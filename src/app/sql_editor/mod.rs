//! The SQL editor's language features, wired to the editor: highlighting,
//! completion, hover, go to definition and Format.
//!
//! This adapter is the only place that knows both the editor (gpui-kit) and
//! the language features (`jykre-sql-language`). The workspace sees
//! [`SqlEditor`] alone and tells it what it needs (the dialect, the
//! connection, the names of its cache and the tab's context), so the
//! features can be removed or replaced here without touching what runs
//! queries ([ADR-0020](../../../docs/adr/0020-editor-language-features-in-their-own-crates.md)).
mod go_to_line;
mod highlighter;
mod providers;

pub use go_to_line::GoToLine;

use super::ui::{Feedback, Tone};
use gpui_kit::component::input::EditorState;
use gpui_kit::*;
use jykre::drivers::*;
use jykre::{backend, tr};
use jykre_sql_language::catalog::{self, DialectCatalog, DialectFiles};
use jykre_sql_language::completion::{CandidateKind, Relation, Request, Source, Symbol, Wanted};
use jykre_sql_language::format::{FormatError, Formatted};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub use jykre_sql_language::catalog::SqlCatalogs;
pub use jykre_sql_language::completion::NameIndex;

/// What the workspace hears from the editor's features.
pub enum SqlEditorEvent {
    /// Go to definition asks the explorer to show this object.
    Reveal(ObjectPath),
    /// How Format went, for the notice bar.
    Notice(Feedback),
    /// A Format started, ended or ran another second past 2 s: the button
    /// that shows it and cancels it is drawn again.
    Formatting,
}

/// How long a Format runs before its button shows the time.
const FORMAT_ELAPSED_AFTER: Duration = Duration::from_secs(2);

/// A Format running off the UI thread.
struct FormatJob {
    started: Instant,
    /// Asked by the formatter before each statement.
    stop: Arc<AtomicBool>,
    /// Tells this Format's outcome from a cancelled or later one's.
    revision: u64,
}

/// Quotes a name for SQL the application writes, when the dialect needs it.
pub type QuoteName = std::rc::Rc<dyn Fn(&str) -> String>;

/// The language features of one tab's editor and what they need to know
/// about the tab.
pub struct SqlEditor {
    editor: WeakEntity<EditorState>,
    dialect: Option<SqlDialect>,
    /// What each dialect has built in, read at start from the catalog
    /// directory; the built-in words until then.
    catalogs: Arc<SqlCatalogs>,
    /// The connection whose cache gives the names, and its identifier quote.
    connection: Option<(ConnectionConfig, char)>,
    /// Where unqualified names resolve, as the tab last said.
    context: SessionContext,
    /// The connection's cache the names come from, and its name index;
    /// empty until the explorer has read the cache.
    cache_id: Option<ConnectionId>,
    names: Arc<NameIndex>,
    /// The columns read so far for the index's relations, and those being
    /// read.
    columns: Arc<HashMap<ObjectPath, Vec<MetadataObject>>>,
    columns_loading: HashSet<ObjectPath>,
    /// The Format running off the UI thread, if any.
    formatting: Option<FormatJob>,
    format_revision: u64,
}

impl EventEmitter<SqlEditorEvent> for SqlEditor {}

impl SqlEditor {
    /// Installs highlighting, completion, hover and go to definition on
    /// `editor`, with generic rules until a connection gives a dialect.
    pub fn new(editor: &Entity<EditorState>, cx: &mut Context<Self>) -> Self {
        let catalogs: Arc<SqlCatalogs> = Arc::default();
        highlighter::install(editor, None, catalogs.get(None), cx);
        providers::install(editor, cx.entity().downgrade(), cx);
        Self {
            editor: editor.downgrade(),
            dialect: None,
            catalogs,
            connection: None,
            context: SessionContext::default(),
            cache_id: None,
            names: Arc::default(),
            columns: Arc::default(),
            columns_loading: HashSet::new(),
            formatting: None,
            format_revision: 0,
        }
    }

    /// Highlights and formats by the rules of `dialect` (`None`: generic
    /// rules, for a tab without a connection).
    pub fn set_dialect(&mut self, dialect: Option<SqlDialect>, cx: &mut Context<Self>) {
        if self.dialect == dialect {
            return;
        }
        self.dialect = dialect;
        self.highlight(cx);
    }

    /// Highlights by the tab's dialect and that dialect's catalog.
    fn highlight(&self, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.upgrade() {
            highlighter::install(&editor, self.dialect, self.catalogs.get(self.dialect), cx);
        }
    }

    /// What each dialect has built in, once the catalogs are read.
    pub fn set_catalogs(&mut self, catalogs: Arc<SqlCatalogs>, cx: &mut Context<Self>) {
        self.catalogs = catalogs;
        self.highlight(cx);
    }

    /// The connection the tab runs on, whose driver gives the identifier
    /// quote; `None` without one.
    pub fn set_connection(&mut self, config: Option<&ConnectionConfig>) {
        self.connection = config.map(|config| {
            let quote = builtin_registry()
                .and_then(|registry| registry.get(config.driver))
                .map(|driver| driver.descriptor().capabilities.identifier_quote)
                .unwrap_or('"');
            (config.clone(), quote)
        });
    }

    /// Quotes a name for SQL the application writes from results (copies
    /// as `INSERT`s), as the suggestions quote it: only when the dialect
    /// needs it. `None` without a dialect.
    pub fn name_quoter(&self) -> Option<QuoteName> {
        let dialect = self.dialect?;
        let quote = self.connection.as_ref().map_or('"', |(_, quote)| *quote);
        let catalog = self.catalogs.get(Some(dialect));
        Some(std::rc::Rc::new(move |name: &str| {
            jykre_sql_language::completion::quote_name(name, dialect, quote, &catalog)
        }))
    }

    /// Where unqualified names resolve in the tab.
    pub fn set_context(&mut self, context: SessionContext) {
        self.context = context;
    }

    /// Uses `names` for completion, hover and go to definition. A newer
    /// index (or another connection's) drops the columns read for the old
    /// one.
    pub fn set_names(&mut self, id: ConnectionId, names: Arc<NameIndex>) {
        if self.cache_id != Some(id) || self.names.version != names.version {
            self.columns = Arc::default();
            self.columns_loading.clear();
        }
        self.cache_id = Some(id);
        self.names = names;
    }

    /// Another server: the names read from its cache and its context are
    /// not this one's.
    pub fn forget_server(&mut self) {
        self.context = SessionContext::default();
        self.cache_id = None;
        self.names = Arc::default();
        self.columns = Arc::default();
        self.columns_loading.clear();
    }

    /// The cache to read and the identifier quote, when the tab has both a
    /// connection and its names.
    fn cache_target(&self) -> Option<(ConnectionId, ConnectionConfig, char)> {
        let (config, quote) = self.connection.as_ref()?;
        Some((self.cache_id?, config.clone(), *quote))
    }
}

/// What a completion request needs, taken on the UI thread so the work runs
/// off it.
pub(super) struct Snapshot {
    names: Arc<NameIndex>,
    context: SessionContext,
    columns: Arc<HashMap<ObjectPath, Vec<MetadataObject>>>,
    catalog: Arc<DialectCatalog>,
    quote: char,
}

impl Snapshot {
    /// The editor's suggestions at `offset` of `text`, and the relations
    /// whose columns are not read yet (shown as loading until they are).
    pub(super) fn completions(
        &self,
        text: &gpui_kit::component::Rope,
        offset: usize,
    ) -> (Vec<lsp_types::CompletionItem>, Vec<ObjectPath>) {
        use gpui_kit::component::RopeExt as _;
        let sql = text.to_string();
        let Some(request) =
            jykre_sql_language::completion::request(&sql, offset, &self.names, &self.context)
        else {
            return (Vec::new(), Vec::new());
        };
        let columns = &self.columns;
        let (found, missing) = jykre_sql_language::completion::candidates(
            &request,
            &self.names,
            &self.context,
            self.quote,
            &self.catalog,
            &|relation| columns.get(relation).cloned(),
        );
        let range = lsp_types::Range {
            start: text.offset_to_position(request.replace.start),
            end: text.offset_to_position(request.replace.end),
        };
        let mut items: Vec<_> = found
            .into_iter()
            .map(|candidate| lsp_types::CompletionItem {
                label: candidate.label,
                kind: Some(match candidate.kind {
                    CandidateKind::Namespace => lsp_types::CompletionItemKind::MODULE,
                    CandidateKind::Relation => lsp_types::CompletionItemKind::CLASS,
                    CandidateKind::View => lsp_types::CompletionItemKind::INTERFACE,
                    CandidateKind::Column => lsp_types::CompletionItemKind::FIELD,
                    CandidateKind::Cte => lsp_types::CompletionItemKind::REFERENCE,
                    CandidateKind::Keyword => lsp_types::CompletionItemKind::KEYWORD,
                    CandidateKind::Function => lsp_types::CompletionItemKind::FUNCTION,
                    CandidateKind::Type => lsp_types::CompletionItemKind::TYPE_PARAMETER,
                }),
                detail: Some(candidate.detail),
                documentation: candidate.documentation.map(|value| {
                    lsp_types::Documentation::MarkupContent(lsp_types::MarkupContent {
                        kind: lsp_types::MarkupKind::Markdown,
                        value,
                    })
                }),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range,
                    new_text: candidate.insert,
                })),
                ..lsp_types::CompletionItem::default()
            })
            .collect();
        if !missing.is_empty() {
            items.push(lsp_types::CompletionItem {
                label: tr!("Loading columns…").into(),
                detail: Some(tr!("Type again to see them").into()),
                // Choosing it changes nothing.
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range,
                    new_text: request.prefix.clone(),
                })),
                ..lsp_types::CompletionItem::default()
            });
        }
        (items, missing)
    }
}

impl SqlEditor {
    /// What a completion request needs. Keywords and functions are
    /// suggested even before the cache's names are read; the quote is the
    /// connection's, or `"` without one.
    pub(super) fn snapshot(&self) -> Snapshot {
        Snapshot {
            names: self.names.clone(),
            context: self.context.clone(),
            columns: self.columns.clone(),
            catalog: self.catalogs.get(self.dialect),
            quote: self.connection.as_ref().map_or('"', |(_, quote)| *quote),
        }
    }

    /// Reads the columns of `relations` from the cache, off the UI thread.
    pub(super) fn load_columns(&mut self, relations: Vec<ObjectPath>, cx: &mut Context<Self>) {
        let Some((id, config, _)) = self.cache_target() else {
            return;
        };
        let version = self.names.version;
        for relation in relations {
            if !self.columns_loading.insert(relation.clone()) {
                continue;
            }
            let config = config.clone();
            let dir = jykre::connection_store::data_dir();
            cx.spawn(async move |weak, cx| {
                let read = relation.clone();
                let result = backend::run(async move {
                    backend::relation_columns(&dir, id, &config, read).await
                })
                .await;
                let _ = weak.update(cx, |this, _| {
                    this.columns_loading.remove(&relation);
                    if this.names.version != version || this.cache_id != Some(id) {
                        return;
                    }
                    match result {
                        Ok(columns) => {
                            Arc::make_mut(&mut this.columns).insert(relation, columns);
                        }
                        Err(error) => {
                            tracing::debug!(error_code = ?error.code, "columns unavailable")
                        }
                    }
                });
            })
            .detach();
        }
    }

    /// The relation or column under `offset`, from the names read so far.
    /// A relation whose columns were not read is read for next time.
    fn symbol_at(
        &mut self,
        sql: &str,
        offset: usize,
        cx: &mut Context<Self>,
    ) -> Option<(Symbol, Range<usize>)> {
        self.cache_target()?;
        let columns = &self.columns;
        let found = jykre_sql_language::completion::symbol_at(
            sql,
            offset,
            &self.names,
            &self.context,
            &|relation| columns.get(relation).cloned(),
        );
        if found.is_none() {
            // The name may be a column of a relation not read yet.
            let request =
                jykre_sql_language::completion::request(sql, offset, &self.names, &self.context);
            if let Some(Request {
                wanted: Wanted::Columns { sources, .. },
                ..
            }) = request
            {
                let missing = sources
                    .iter()
                    .flat_map(Source::relations)
                    .filter(|relation| !self.columns.contains_key(*relation))
                    .cloned()
                    .collect();
                self.load_columns(missing, cx);
            }
        }
        found
    }

    /// The built-in function under `offset` and the range of its name, with
    /// what the hover shows: its signatures, description and page.
    fn function_at(&self, sql: &str, offset: usize) -> Option<(String, Range<usize>)> {
        let catalog = self.catalogs.get(self.dialect);
        let (name, range) =
            jykre_sql_language::completion::function_at(sql, offset, self.dialect, &catalog)?;
        let text =
            jykre_sql_language::completion::function_documentation(catalog.functions(&name))?;
        Some((format!("**{name}**\n\n{text}"), range))
    }

    /// What the hover shows for a symbol: a column's type, nullability,
    /// key, default and comment, or a relation's kind and name.
    fn describe(&self, symbol: &Symbol) -> Option<String> {
        match symbol {
            Symbol::Column { relation, name } => {
                let column = self
                    .columns
                    .get(relation)?
                    .iter()
                    .find(|object| object.kind == ObjectKind::Column && object.name == *name)?;
                let details = jykre::explorer::description(column);
                Some(format!(
                    "**{}**\n\n{}",
                    column.name,
                    details.replace('\n', "\n\n")
                ))
            }
            Symbol::Relation(path) => {
                let name: Vec<&str> = path.0.iter().map(|part| part.name.as_str()).collect();
                let kind = path.0.last().map(|part| part.kind)?;
                Some(format!(
                    "**{}**\n\n{}",
                    name.join("."),
                    jykre::explorer::kind_name(kind)
                ))
            }
        }
    }

    /// Asks the explorer to show `path` (go to definition).
    fn reveal(&mut self, path: ObjectPath, cx: &mut Context<Self>) {
        cx.emit(SqlEditorEvent::Reveal(path));
    }

    /// Formats the SQL by the tab's dialect, off the UI thread: the
    /// formatter's grammar takes long to build, and the work grows with the
    /// script. With a selection only the selected SQL is formatted; without
    /// one, the whole script, statement by statement. Only whitespace and
    /// keyword case change, so the caret, the selection and the caret's
    /// place on screen are kept. A result for SQL edited meanwhile is
    /// dropped. While it runs, the same button cancels it.
    pub fn format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.formatting.is_some() {
            self.cancel_format(cx);
            return;
        }
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let (sql, selection) = {
            let editor = editor.read(cx);
            (editor.text().to_string(), editor.selected_range())
        };
        let dialect = self.dialect;
        self.format_revision += 1;
        let revision = self.format_revision;
        let stop = Arc::new(AtomicBool::new(false));
        self.formatting = Some(FormatJob {
            started: Instant::now(),
            stop: stop.clone(),
            revision,
        });
        cx.emit(SqlEditorEvent::Formatting);
        cx.emit(SqlEditorEvent::Notice(Feedback::new(
            Tone::Info,
            if selection.is_empty() {
                tr!("Formatting the SQL…")
            } else {
                tr!("Formatting the selected SQL…")
            },
        )));
        let source = sql.clone();
        let task = cx.background_spawn(async move {
            let stop = || stop.load(Ordering::Relaxed);
            if selection.is_empty() {
                jykre_sql_language::format::format_until(&source, dialect, &stop)
            } else {
                jykre_sql_language::format::format_range_until(&source, selection, dialect, &stop)
                    .map(|(formatted, _)| formatted)
            }
        });
        self.tick_format(revision, cx);
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |this, window, cx| {
                // A cancelled Format, or one a later one replaced, says
                // nothing more.
                if this.formatting.as_ref().map(|job| job.revision) != Some(revision) {
                    return;
                }
                this.formatting = None;
                cx.emit(SqlEditorEvent::Formatting);
                if let Some(notice) = this.apply_format(&sql, result, window, cx) {
                    cx.emit(SqlEditorEvent::Notice(notice));
                }
            });
        })
        .detach();
    }

    /// Stops the running Format: the SQL is left as it was. The formatter
    /// stops before its next statement, off the UI thread.
    pub fn cancel_format(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.formatting.take() else {
            return;
        };
        job.stop.store(true, Ordering::Relaxed);
        cx.emit(SqlEditorEvent::Formatting);
        cx.emit(SqlEditorEvent::Notice(Feedback::new(
            Tone::Info,
            tr!("Format cancelled. The SQL was left as it was."),
        )));
    }

    /// How long the running Format has run, or `None` when none runs.
    pub fn formatting_for(&self) -> Option<Duration> {
        self.formatting.as_ref().map(|job| job.started.elapsed())
    }

    /// Past 2 s, asks for the Format button to be drawn again each second
    /// so it shows the time, until this Format ends.
    fn tick_format(&self, revision: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| {
            cx.background_executor().timer(FORMAT_ELAPSED_AFTER).await;
            loop {
                let running = weak
                    .update(cx, |this, cx| {
                        let running =
                            this.formatting.as_ref().map(|job| job.revision) == Some(revision);
                        if running {
                            cx.emit(SqlEditorEvent::Formatting);
                        }
                        running
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        })
        .detach();
    }

    /// The number of lines of the editor's text, as its gutter numbers
    /// them; the rope knows it without reading the text.
    pub fn line_count(&self, cx: &App) -> usize {
        use gpui_kit::component::RopeExt as _;
        self.editor
            .upgrade()
            .map_or(1, |editor| editor.read(cx).text().lines_len())
    }

    /// Closes the editor's search bar, keeping its text for the next one.
    pub fn close_search(&self, cx: &mut App) {
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |editor, cx| editor.close_search(cx));
        }
    }

    /// Gives the keyboard to the editor.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |editor, cx| editor.focus(window, cx));
        }
    }

    /// Go to line: puts the cursor on `target`'s line and column (the end
    /// of the line when it is shorter), scrolled into view, with the
    /// keyboard in the editor. A line past the text goes to the last one,
    /// and the notice says so. Only the rope's line index is read, so the
    /// cost does not grow with the text.
    pub fn go_to(
        &mut self,
        target: jykre::go_to_line::Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let lines = self.line_count(cx);
        let place = jykre::go_to_line::place(target, lines);
        editor.update(cx, |editor, cx| {
            editor.set_cursor_position(
                gpui_kit::component::input::Position::new(
                    u32::try_from(place.line).unwrap_or(u32::MAX),
                    u32::try_from(place.column).unwrap_or(u32::MAX),
                ),
                window,
                cx,
            );
        });
        if place.past_end {
            cx.emit(SqlEditorEvent::Notice(Feedback::new(
                Tone::Info,
                jykre::go_to_line::past_end_notice(target, lines),
            )));
        }
    }

    /// Ctrl+/: comments the lines the selection touches (or the caret's
    /// line) with `--`, or takes the comment out when all of them are
    /// comments already. Only those lines are replaced, in one step that
    /// Ctrl+Z undoes, and the same text stays selected.
    pub fn toggle_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        editor.update(cx, |editor, cx| {
            let sql = editor.text().to_string();
            let reversed = editor.is_selection_reversed();
            let toggled =
                jykre_sql_language::comment::toggle_line_comments(&sql, editor.selected_range());
            // Replacing the selection, made the lines first, is one undo step
            // and leaves every other cursor out.
            editor.set_selected_range(toggled.range, cx);
            editor.replace(toggled.text, window, cx);
            editor.set_selected_range_with_direction(toggled.selection, reversed, cx);
        });
    }

    /// Puts the formatted SQL in the editor, when the editor still holds the
    /// SQL it was made from, and says how it went.
    fn apply_format(
        &mut self,
        sql: &str,
        result: Result<Formatted, FormatError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Feedback> {
        let editor = self.editor.upgrade()?;
        let notice = |tone, text: &str| Some(Feedback::new(tone, text));
        let formatted = match result {
            _ if editor.read(cx).text() != sql => {
                return notice(
                    Tone::Info,
                    tr!("The SQL changed while it was being formatted. Format it again."),
                );
            }
            Err(FormatError::Cancelled) => {
                return notice(
                    Tone::Info,
                    tr!("Format cancelled. The SQL was left as it was."),
                );
            }
            Err(FormatError::Failed) => {
                return notice(
                    Tone::Error,
                    tr!("The SQL could not be formatted. It was left as it was."),
                );
            }
            Ok(formatted) => formatted,
        };
        let partial = formatted.partial;
        let formatted = formatted.sql;
        if formatted == sql {
            return if partial {
                notice(
                    Tone::Warning,
                    tr!(
                        "The SQL does not parse in this connection's dialect, so it was left as it was."
                    ),
                )
            } else {
                notice(Tone::Info, tr!("SQL is already formatted."))
            };
        }
        let (selection, reversed, scroll) = {
            let editor = editor.read(cx);
            (
                editor.selected_range(),
                editor.is_selection_reversed(),
                editor.scroll_offset(),
            )
        };
        let map = |offset| jykre::sql::map_offset(sql, &formatted, offset);
        let range = map(selection.start)..map(selection.end);
        let caret = |text: &str, range: &Range<usize>| {
            let at = if reversed { range.start } else { range.end };
            text.get(..at)
                .map_or(0, |before| before.matches('\n').count())
        };
        let moved = caret(&formatted, &range) as f32 - caret(sql, &selection) as f32;
        editor.update(cx, |editor, cx| {
            // `replace_all` keeps the undo history, so Ctrl+Z restores the
            // previous layout; it also moves to the start, undone here.
            editor.replace_all(formatted.clone(), window, cx);
            editor.set_selected_range_with_direction(range, reversed, cx);
            if let Some(line_height) = editor.line_height() {
                editor.set_scroll_offset(point(scroll.x, scroll.y - line_height * moved), cx);
            }
        });
        if partial {
            notice(
                Tone::Warning,
                &tr!(
                    "SQL formatted, except a part that does not parse in this connection's dialect, which was left as it was. Use {0} to undo.",
                    super::shortcuts::label::<gpui_kit::base::input::Undo>(cx)
                ),
            )
        } else {
            notice(
                Tone::Success,
                &tr!(
                    "SQL formatted. Use {0} to undo.",
                    super::shortcuts::label::<gpui_kit::base::input::Undo>(cx)
                ),
            )
        }
    }
}

/// Reads the SQL catalogs (keywords, functions and types by dialect) from the
/// first catalog directory found: `JYKRE_SQL_CATALOG_DIR`, then `catalog`
/// next to the executable and, in a development build, the repository's
/// `catalog`. The files are read on the runtime and parsed there; a dialect
/// whose files are missing or invalid keeps the built-in words, and so does
/// every dialect when no directory is found.
pub async fn load_catalogs() -> Arc<SqlCatalogs> {
    let variable = std::env::var_os("JYKRE_SQL_CATALOG_DIR").map(PathBuf::from);
    let executable = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let repository = cfg!(debug_assertions).then(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let dirs = catalog::candidate_dirs(variable, executable.as_deref(), repository.as_deref());
    backend::run(async move {
        let started = Instant::now();
        let names: Vec<String> = catalog::DIALECTS
            .iter()
            .flat_map(|(_, dir)| {
                catalog::FILES
                    .iter()
                    .map(move |file| format!("{dir}/{file}"))
            })
            .collect();
        for dir in dirs {
            let texts = backend::read_text_files(&dir, &names, catalog::MAX_FILE_BYTES).await;
            // A directory without any dialect's manifest is not a catalog.
            if texts
                .chunks(catalog::FILES.len())
                .all(|files| files[0].is_none())
            {
                continue;
            }
            let parsed = tokio::task::spawn_blocking(move || parse_catalogs(&texts))
                .await
                .unwrap_or_default();
            tracing::info!(
                dialects = parsed.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "SQL catalogs read"
            );
            return Ok(Arc::new(SqlCatalogs::new(Some(dir), parsed)));
        }
        tracing::info!("no SQL catalog directory found; the editor keeps its built-in words");
        Ok(Arc::new(SqlCatalogs::default()))
    })
    .await
    .unwrap_or_else(|error: DriverError| {
        tracing::warn!(error_code = ?error.code, "SQL catalogs not read; built-in words used");
        Arc::default()
    })
}

/// Parses each dialect's files, in [`catalog::DIALECTS`] order, as
/// [`load_catalogs`] read them; a dialect with a missing or invalid file is
/// left out (and logged), so it keeps the built-in words.
fn parse_catalogs(texts: &[Option<String>]) -> Vec<(SqlDialect, catalog::DialectCatalog)> {
    let mut entries = Vec::new();
    for ((dialect, name), files) in catalog::DIALECTS
        .iter()
        .zip(texts.chunks(catalog::FILES.len()))
    {
        let [Some(manifest), Some(keywords), Some(functions), Some(types)] = files else {
            tracing::warn!(
                dialect = name,
                "SQL catalog files missing; built-in words used"
            );
            continue;
        };
        let files = DialectFiles {
            manifest,
            keywords,
            functions,
            types,
        };
        match catalog::parse(files, None) {
            Ok(parsed) => entries.push((*dialect, parsed)),
            Err(error) => {
                tracing::warn!(dialect = name, error = ?error, "SQL catalog invalid; built-in words used")
            }
        }
    }
    entries
}

/// Builds the editor's index of the connection's cached relations. The cache
/// is read on the runtime and the names are grouped there too, off the UI
/// thread; its version tells a tab whether the index it holds is current.
pub async fn name_index(
    dir: PathBuf,
    id: ConnectionId,
    config: ConnectionConfig,
) -> DriverResult<NameIndex> {
    backend::run(async move {
        let started = Instant::now();
        let descriptor = builtin_registry()?.get(config.driver)?.descriptor();
        let (version, relations) = backend::cached_relations(&dir, id, &config).await?;
        let count = relations.len();
        // Grouping thousands of names is CPU work; it stays on the runtime.
        let index = tokio::task::spawn_blocking(move || {
            NameIndex::new(
                version,
                descriptor.capabilities.namespace_levels(),
                descriptor.dialect,
                relations
                    .into_iter()
                    .map(|(path, kind)| Relation { path, kind })
                    .collect(),
            )
        })
        .await
        .map_err(|_| DriverError::new(ErrorCode::Internal, tr!("Database task failed")))?;
        tracing::debug!(
            connection_id = %id,
            relations = count,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "editor name index built"
        );
        Ok(index)
    })
    .await
}
