//! **Import from file…** ([ADR-0040](../../../docs/adr/0040-importing-files-into-a-table.md)):
//! the dialog that chooses a CSV or `.xlsx` file, its sheet, header row,
//! delimiter and encoding, the table (from the connection's metadata
//! cache), how the file's columns fill the table's, and what an empty CSV
//! field means, with a preview of the values that would be bound.
//!
//! Everything it decides is `jykre::import`'s; the dialog only holds the
//! choices and shows what the library read, off the UI thread. **Import**
//! runs `jykre::import::session::run` on the runtime, in a session of its
//! own, all or nothing, and says how it ended ([`ImportDialog::run`]).
use super::*;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState};
use jykre::import::mapping::{Mapping, MatchBy, TargetColumn};
use jykre::import::pipeline::{ImportPlan, Outcome, PROGRESS_AFTER, Progress, Shown, TimeLimit};
use jykre::import::session::ImportJob;
use jykre::import::{
    CsvOptions, Delimiter, EmptyFields, FileKind, ImportValue, Preview, ReadOptions, XlsxOptions,
    delimited::ENCODINGS,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Width of a preview column.
const PREVIEW_COLUMN: f32 = 150.;
/// Preview columns drawn at most; the rest are counted.
const PREVIEW_COLUMNS: usize = 40;
const ROW_HEIGHT: f32 = 30.;

/// One choice of a select: its position in the dialog's list.
#[derive(Clone)]
struct Choice {
    index: usize,
    title: SharedString,
}

impl SelectItem for Choice {
    type Value = usize;
    fn value(&self) -> &usize {
        &self.index
    }
    fn title(&self) -> SharedString {
        self.title.clone()
    }
}

/// Work the dialog waits for.
enum Load<T> {
    Idle,
    Loading { started: Instant },
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }
}

/// What the dialog asks of the window.
pub(super) enum ImportEvent {
    Close,
    /// Import now: the window asks for the password and, on a Production
    /// connection, confirms first, then calls [`ImportDialog::run`].
    Start,
    /// Open `sql`, a draft `CREATE TABLE`, in a new query tab titled
    /// `title`.
    Draft {
        sql: String,
        title: String,
    },
}

/// The import, from **Import** to its outcome.
enum ImportRun {
    Idle,
    Running {
        started: Instant,
        cancel: CancellationToken,
        progress: tokio::sync::watch::Receiver<Progress>,
    },
    /// How it ended, in words: the outcome and, under it, why.
    Finished {
        tone: Tone,
        text: String,
        detail: Option<String>,
    },
}

impl ImportRun {
    /// Forgets how the last import ended: once the file, the sheet, the
    /// table or the mapping changed, that outcome was about another import.
    /// An import that runs is kept.
    fn forget_outcome(&mut self) {
        if matches!(self, Self::Finished { .. }) {
            *self = Self::Idle;
        }
    }
}

/// The open dialog and what keeps it subscribed.
pub(super) struct ImportHost {
    dialog: Entity<ImportDialog>,
    _events: Subscription,
}

pub(super) struct ImportDialog {
    connection: String,
    id: ConnectionId,
    config: ConnectionConfig,
    driver: Arc<dyn Driver>,
    /// Where a new table goes when no table is chosen.
    namespace: Option<ObjectPath>,
    focus: FocusHandle,
    needs_focus: bool,
    file: Option<PathBuf>,
    kind: FileKind,
    /// Reading the file's kind, sheets and byte-order mark.
    inspection: Load<()>,
    sheets: Vec<String>,
    sheet_select: Entity<SelectState<Vec<Choice>>>,
    delimiter: Delimiter,
    /// Position in [`ENCODINGS`].
    encoding: usize,
    encoding_select: Entity<SelectState<SearchableVec<Choice>>>,
    header: Entity<InputState>,
    header_error: Option<String>,
    empty: EmptyFields,
    /// The connection's cached tables, by qualified title.
    tables: Vec<ObjectPath>,
    /// Every relation the cache knows (tables, views…), so a drafted table
    /// does not take one's name.
    relations: Vec<ObjectPath>,
    table: Option<usize>,
    table_select: Entity<SelectState<SearchableVec<Choice>>>,
    targets: Load<Vec<TargetColumn>>,
    preview: Load<Preview>,
    /// Cancels the preview being read.
    reading: Option<CancellationToken>,
    match_by: MatchBy,
    mapping: Option<Mapping>,
    /// The file's columns `mapping` was matched to.
    mapped_columns: Vec<String>,
    /// The preview's rows as they would be bound, for the mapped columns;
    /// computed when the preview or the mapping changes, never per frame.
    shown: Vec<Vec<ImportValue>>,
    /// The mapped columns: the table's name and the file's.
    shown_columns: Vec<(String, String)>,
    problems: Vec<String>,
    time_limit: TimeLimit,
    /// The import itself, once started.
    run: ImportRun,
    /// Bumped by every read, so a late answer for an earlier one is
    /// dropped.
    revision: u64,
    mapping_scroll: UniformListScrollHandle,
    preview_scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ImportEvent> for ImportDialog {}

/// The connection an import goes to and where it was asked for.
struct Opening {
    connection: String,
    id: ConnectionId,
    config: ConnectionConfig,
    driver: Arc<dyn Driver>,
    /// The table chosen in the explorer.
    table: Option<ObjectPath>,
    /// The namespace chosen in the explorer.
    namespace: Option<ObjectPath>,
}

impl ImportDialog {
    fn new(opening: Opening, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Opening {
            connection,
            id,
            config,
            driver,
            table,
            namespace,
        } = opening;
        let sheet_select = cx.new(|cx| SelectState::new(Vec::<Choice>::new(), None, window, cx));
        let encodings: Vec<Choice> = ENCODINGS
            .iter()
            .enumerate()
            .map(|(index, encoding)| Choice {
                index,
                title: encoding.name().into(),
            })
            .collect();
        let encoding_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(encodings),
                Some(gpui_kit::component::IndexPath::default()),
                window,
                cx,
            )
            .searchable(true)
        });
        let table_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(Vec::<Choice>::new()), None, window, cx)
                .searchable(true)
        });
        let header = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder(tr!("No header row"));
            input.set_value("1".to_owned(), window, cx);
            input
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &sheet_select,
                window,
                |this, _, event: &SelectEvent<Vec<Choice>>, _, cx| {
                    if let SelectEvent::Confirm(Some(_)) = event {
                        this.read_preview(cx);
                    }
                },
            ),
            cx.subscribe(
                &encoding_select,
                |this, _, event: &SelectEvent<SearchableVec<Choice>>, cx| {
                    if let SelectEvent::Confirm(Some(index)) = event
                        && *index != this.encoding
                    {
                        this.encoding = *index;
                        this.read_preview(cx);
                    }
                },
            ),
            cx.subscribe(
                &table_select,
                |this, _, event: &SelectEvent<SearchableVec<Choice>>, cx| {
                    let SelectEvent::Confirm(index) = event;
                    if *index != this.table {
                        this.table = *index;
                        this.load_columns(cx);
                    }
                },
            ),
            cx.subscribe(&header, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.read_preview(cx);
                }
            }),
        ];
        let mut dialog = Self {
            connection,
            id,
            config,
            driver,
            namespace,
            focus: cx.focus_handle(),
            needs_focus: true,
            file: None,
            kind: FileKind::Csv,
            inspection: Load::Idle,
            sheets: Vec::new(),
            sheet_select,
            delimiter: Delimiter::default(),
            encoding: 0,
            encoding_select,
            header,
            header_error: None,
            empty: EmptyFields::default(),
            tables: Vec::new(),
            relations: Vec::new(),
            table: None,
            table_select,
            targets: Load::Idle,
            preview: Load::Idle,
            reading: None,
            match_by: MatchBy::Name,
            mapping: None,
            mapped_columns: Vec::new(),
            shown: Vec::new(),
            shown_columns: Vec::new(),
            problems: Vec::new(),
            time_limit: TimeLimit::default(),
            run: ImportRun::Idle,
            revision: 0,
            mapping_scroll: UniformListScrollHandle::new(),
            preview_scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        dialog.load_tables(table, window, cx);
        dialog
    }

    /// Lists the connection's cached tables and chooses `chosen`.
    fn load_tables(
        &mut self,
        chosen: Option<ObjectPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (dir, id, config) = (
            jykre::connection_store::data_dir(),
            self.id,
            self.config.clone(),
        );
        cx.spawn_in(window, async move |weak, cx| {
            let result =
                backend::run(async move { backend::cached_relations(&dir, id, &config).await })
                    .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                let mut tables: Vec<ObjectPath> = match result {
                    Ok((_, relations)) => {
                        this.relations = relations.iter().map(|(path, _)| path.clone()).collect();
                        relations
                            .into_iter()
                            .filter(|(_, kind)| *kind == ObjectKind::Table)
                            .map(|(path, _)| path)
                            .collect()
                    }
                    Err(error) => {
                        this.targets = Load::Failed(tr!(
                            "The tables could not be read from the metadata: {0}",
                            error
                        ));
                        Vec::new()
                    }
                };
                tables.sort_by_key(|path| jykre::explorer::qualified_title(path).to_lowercase());
                let items: Vec<Choice> = tables
                    .iter()
                    .enumerate()
                    .map(|(index, path)| Choice {
                        index,
                        title: jykre::explorer::qualified_title(path).into(),
                    })
                    .collect();
                this.table = chosen
                    .as_ref()
                    .and_then(|chosen| tables.iter().position(|path| path == chosen));
                this.tables = tables;
                let table = this.table;
                this.table_select.update(cx, |select, cx| {
                    select.set_items(SearchableVec::new(items), window, cx);
                    match table {
                        Some(index) => select.set_selected_value(&index, window, cx),
                        None => select.set_selected_index(None, window, cx),
                    }
                });
                this.load_columns(cx);
            });
        })
        .detach();
    }

    /// Reads the chosen table's columns from the cache.
    fn load_columns(&mut self, cx: &mut Context<Self>) {
        let Some(table) = self.table.and_then(|index| self.tables.get(index)).cloned() else {
            self.targets = Load::Idle;
            self.remap(cx);
            return;
        };
        self.targets = Load::Loading {
            started: Instant::now(),
        };
        let (dir, id, config) = (
            jykre::connection_store::data_dir(),
            self.id,
            self.config.clone(),
        );
        let wanted = table.clone();
        cx.spawn(async move |weak, cx| {
            let result =
                backend::run(
                    async move { backend::relation_columns(&dir, id, &config, table).await },
                )
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.table.and_then(|index| this.tables.get(index)) != Some(&wanted) {
                    return;
                }
                this.targets = match result {
                    Ok(objects) => {
                        let mut columns: Vec<_> = objects
                            .into_iter()
                            .filter(|object| object.kind == ObjectKind::Column)
                            .collect();
                        columns.sort_by_key(|column| column.ordinal.unwrap_or(u32::MAX));
                        Load::Ready(columns.iter().map(TargetColumn::from_cached).collect())
                    }
                    Err(error) => Load::Failed(tr!(
                        "The table's columns could not be read from the metadata: {0}",
                        error
                    )),
                };
                this.mapping = None;
                this.remap(cx);
            });
        })
        .detach();
    }

    /// Asks for the file in the system's dialog.
    fn choose_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!("Import").into()),
        });
        cx.spawn_in(window, async move |weak, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = weak.update_in(cx, |this, window, cx| this.inspect(path, window, cx));
        })
        .detach();
    }

    /// Learns the chosen file's kind, sheets and byte-order mark, then
    /// previews it.
    fn inspect(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.run.forget_outcome();
        self.file = Some(path.clone());
        self.preview = Load::Idle;
        self.mapping = None;
        self.revision += 1;
        let revision = self.revision;
        self.inspection = Load::Loading {
            started: Instant::now(),
        };
        self.tick(revision, cx);
        cx.notify();
        cx.spawn_in(window, async move |weak, cx| {
            let result = jykre::import::off_thread(move || jykre::import::inspect(&path)).await;
            let _ = weak.update_in(cx, |this, window, cx| {
                if this.revision != revision {
                    return;
                }
                match result {
                    Ok(info) => {
                        this.inspection = Load::Ready(());
                        this.kind = info.kind;
                        this.delimiter = info.delimiter;
                        this.sheets = info.sheets;
                        if let Some(bom) = info.bom
                            && let Some(index) =
                                ENCODINGS.iter().position(|encoding| *encoding == bom)
                        {
                            this.encoding = index;
                        }
                        this.sync_choices(window, cx);
                        this.read_preview(cx);
                    }
                    Err(error) => {
                        this.inspection = Load::Failed(error.message.clone());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Puts the sheets and the encoding in their selects.
    fn sync_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sheets: Vec<Choice> = self
            .sheets
            .iter()
            .enumerate()
            .map(|(index, name)| Choice {
                index,
                title: name.clone().into(),
            })
            .collect();
        let encoding = self.encoding;
        self.sheet_select.update(cx, |select, cx| {
            let first = (!sheets.is_empty()).then(gpui_kit::component::IndexPath::default);
            select.set_items(sheets, window, cx);
            select.set_selected_index(first, window, cx);
        });
        self.encoding_select.update(cx, |select, cx| {
            select.set_selected_value(&encoding, window, cx);
        });
    }

    /// How to read the file as the dialog stands; `None`, with the reason
    /// shown, while a choice cannot be used.
    fn read_options(&mut self, cx: &mut Context<Self>) -> Option<ReadOptions> {
        let typed = self.header.read(cx).value().trim().to_owned();
        let header_row = if typed.is_empty() {
            None
        } else {
            match typed.parse::<u32>() {
                Ok(row) if row >= 1 => Some(row),
                _ => {
                    self.header_error = Some(
                        tr!("Write the row that names the columns, or leave it empty when there is none.")
                            .to_owned(),
                    );
                    return None;
                }
            }
        };
        self.header_error = None;
        match self.kind {
            FileKind::Csv => Some(ReadOptions::Csv(CsvOptions {
                delimiter: self.delimiter,
                encoding: ENCODINGS.get(self.encoding).copied()?,
                header_row,
                empty: self.empty,
                ..CsvOptions::default()
            })),
            FileKind::Xlsx => {
                let sheet = self
                    .sheet_select
                    .read(cx)
                    .selected_value()
                    .and_then(|index| self.sheets.get(*index))?
                    .clone();
                Some(ReadOptions::Xlsx(XlsxOptions {
                    header_row,
                    ..XlsxOptions::new(sheet)
                }))
            }
        }
    }

    /// Reads the first rows again with the dialog's choices.
    fn read_preview(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.file.clone() else {
            return;
        };
        if !matches!(self.inspection, Load::Ready(())) {
            return;
        }
        if let Some(reading) = self.reading.take() {
            reading.cancel();
        }
        self.run.forget_outcome();
        self.revision += 1;
        let revision = self.revision;
        let Some(options) = self.read_options(cx) else {
            cx.notify();
            return;
        };
        let cancel = CancellationToken::new();
        self.reading = Some(cancel.clone());
        self.preview = Load::Loading {
            started: Instant::now(),
        };
        self.tick(revision, cx);
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let result = jykre::import::off_thread(move || {
                jykre::import::preview(&path, &options, jykre::import::PREVIEW_ROWS, &cancel)
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                if this.revision != revision {
                    return;
                }
                this.reading = None;
                this.preview = match result {
                    Ok(preview) => Load::Ready(preview),
                    Err(error) if error.code == ErrorCode::Cancelled => Load::Idle,
                    Err(error) => Load::Failed(error.message.clone()),
                };
                this.remap(cx);
            });
        })
        .detach();
    }

    /// Stops waiting for the preview.
    fn cancel_reading(&mut self, cx: &mut Context<Self>) {
        if let Some(reading) = self.reading.take() {
            reading.cancel();
        }
        self.revision += 1;
        if matches!(self.inspection, Load::Loading { .. }) {
            self.inspection = Load::Idle;
            self.file = None;
        }
        self.preview = Load::Idle;
        cx.notify();
    }

    /// Redraws while something is read: after 300 ms for its progress,
    /// then each second for its elapsed time, until `revision` is over.
    fn tick(&self, revision: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx| {
            let mut wait = Duration::from_millis(300);
            loop {
                cx.background_executor().timer(wait).await;
                wait = Duration::from_secs(1);
                let waiting = weak
                    .update(cx, |this, cx| {
                        let waiting = this.revision == revision && this.waiting_since().is_some();
                        if waiting {
                            cx.notify();
                        }
                        waiting
                    })
                    .unwrap_or(false);
                if !waiting {
                    return;
                }
            }
        })
        .detach();
    }

    /// When the read the dialog waits for started.
    fn waiting_since(&self) -> Option<Instant> {
        match (&self.inspection, &self.preview) {
            (Load::Loading { started }, _) | (_, Load::Loading { started }) => Some(*started),
            _ => None,
        }
    }

    /// Matches the file's columns to the table's again when either changed,
    /// and derives what the preview and the problems show.
    fn remap(&mut self, cx: &mut Context<Self>) {
        let (Some(preview), Some(targets)) = (self.preview.ready(), self.targets.ready()) else {
            self.mapping = None;
            self.derive();
            cx.notify();
            return;
        };
        // The user's choices stay while only the values change (an empty
        // field's meaning, the encoding); other columns start over, by name
        // with a header row and by position without one.
        if self.mapping.as_ref().map(Mapping::targets) != Some(targets.as_slice())
            || self.mapped_columns != preview.columns
        {
            let has_header = preview.columns.iter().any(|name| !name.is_empty());
            self.match_by = MatchBy::default_for(has_header);
            self.mapping = Some(Mapping::new(
                targets.clone(),
                &preview.columns,
                self.match_by,
            ));
            self.mapped_columns = preview.columns.clone();
        }
        self.derive();
        cx.notify();
    }

    /// The preview's rows as they would be bound, and what stops the
    /// mapping.
    fn derive(&mut self) {
        // Every change of the preview, the table or the mapping comes here.
        self.run.forget_outcome();
        self.shown.clear();
        self.shown_columns.clear();
        self.problems.clear();
        let (Some(preview), Some(mapping)) = (self.preview.ready(), self.mapping.as_ref()) else {
            return;
        };
        let names = file_names(&preview.columns);
        self.shown_columns = mapping
            .targets()
            .iter()
            .enumerate()
            .filter_map(|(index, target)| {
                let source = mapping.source(index)?;
                Some((
                    target.name.clone(),
                    names.get(source).cloned().unwrap_or_default(),
                ))
            })
            .collect();
        self.shown = preview
            .rows
            .iter()
            .map(|record| mapping.project(record.values.clone()))
            .collect();
        self.problems = mapping
            .problems(preview.columns.len())
            .iter()
            .map(|problem| problem.message())
            .collect();
    }

    fn set_source(&mut self, target: usize, source: Option<usize>, cx: &mut Context<Self>) {
        if let Some(mapping) = &mut self.mapping {
            mapping.set(target, source);
        }
        self.derive();
        cx.notify();
    }

    fn set_match_by(&mut self, by: MatchBy, cx: &mut Context<Self>) {
        self.match_by = by;
        if let (Some(mapping), Some(preview)) = (&mut self.mapping, self.preview.ready()) {
            mapping.rematch(&preview.columns, by);
        }
        self.derive();
        cx.notify();
    }

    /// **New table from this file…**: a draft `CREATE TABLE` from the
    /// file's header, in the chosen table's namespace (or the one the
    /// dialog was opened on), quoted by the driver.
    fn draft_table(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.preview.ready() else {
            return;
        };
        let namespace = self
            .table
            .and_then(|index| self.tables.get(index))
            .and_then(|table| table.parent())
            .or_else(|| self.namespace.clone())
            .unwrap_or_default();
        let stem = self
            .file
            .as_ref()
            .and_then(|path| path.file_stem())
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Not a name the cache knows in that namespace; the draft says so
        // when the file's own name was taken.
        let taken: Vec<&str> = self
            .relations
            .iter()
            .filter(|path| path.parent().as_ref() == Some(&namespace))
            .filter_map(ObjectPath::relation)
            .collect();
        let wanted = jykre::import::draft::wanted_name(&stem);
        let table = jykre::import::draft::table_name(&stem, &taken);
        let instead_of = (table != wanted).then_some(wanted.as_str());
        let sql = jykre::import::draft::create_table(
            &*self.driver,
            &namespace,
            &table,
            &preview.columns,
            instead_of,
        );
        cx.emit(ImportEvent::Draft { sql, title: table });
    }

    /// **Import**: once the file, the table and the mapping are ready, the
    /// window asks for what it must (password, Production) and calls
    /// [`ImportDialog::run`].
    fn import(&mut self, cx: &mut Context<Self>) {
        if self.ready_to_import() {
            cx.emit(ImportEvent::Start);
        }
    }

    /// The file, the table, a mapping without problems and nothing running.
    fn ready_to_import(&self) -> bool {
        self.file.is_some()
            && self.selected_table().is_some()
            && self.preview.ready().is_some()
            && self.mapping.is_some()
            && self.problems.is_empty()
            && self.header_error.is_none()
            && !matches!(self.run, ImportRun::Running { .. })
    }

    fn selected_table(&self) -> Option<&ObjectPath> {
        self.table.and_then(|index| self.tables.get(index))
    }

    /// The file's name and the table's, for the Production question.
    pub(super) fn describe(&self) -> (String, String) {
        let file = self
            .file
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let table = self
            .selected_table()
            .map(jykre::explorer::qualified_title)
            .unwrap_or_default();
        (file, table)
    }

    /// An import is running, so the dialog stays until it ends.
    pub(super) fn is_running(&self) -> bool {
        matches!(self.run, ImportRun::Running { .. })
    }

    /// Runs the import with `connection` (its password in memory), in a
    /// session of its own, all or nothing. Progress shows after 300 ms, and
    /// the elapsed time and **Cancel** past 2 s.
    pub(super) fn run(&mut self, connection: ResolvedConnection, cx: &mut Context<Self>) {
        if !self.ready_to_import() {
            return;
        }
        let (Some(path), Some(table), Some(mapping)) = (
            self.file.clone(),
            self.selected_table().cloned(),
            self.mapping.clone(),
        ) else {
            return;
        };
        let Some(options) = self.read_options(cx) else {
            cx.notify();
            return;
        };
        let title = jykre::explorer::qualified_title(&table);
        let job = ImportJob {
            connection,
            table,
            plan: ImportPlan {
                path,
                options,
                mapping,
            },
            limit: self.time_limit.duration(),
        };
        let (sender, receiver) = tokio::sync::watch::channel(Progress::default());
        let cancel = CancellationToken::new();
        self.run = ImportRun::Running {
            started: Instant::now(),
            cancel: cancel.clone(),
            progress: receiver,
        };
        self.tick_run(cx);
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let outcome =
                backend::run(
                    async move { Ok(jykre::import::session::run(job, cancel, sender).await) },
                )
                .await;
            let _ = weak.update(cx, |this, cx| {
                this.run = finished(&outcome, &title);
                // **Cancel import** (the button that had the keyboard) is
                // gone now: the dialog takes the keyboard back, so `Esc`
                // closes it.
                this.needs_focus = true;
                cx.notify();
            });
        })
        .detach();
    }

    /// **Cancel** while importing: the transaction rolls back, and the
    /// outcome says so.
    pub(super) fn cancel_import(&mut self, cx: &mut Context<Self>) {
        if let ImportRun::Running { cancel, .. } = &self.run {
            cancel.cancel();
            cx.notify();
        }
    }

    /// Redraws while the import runs: after 300 ms for its progress, then
    /// four times a second.
    fn tick_run(&self, cx: &mut Context<Self>) {
        redraw_while(cx, Self::is_running);
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if self.is_running() {
            // Closing while it runs cancels it; the outcome stays on screen.
            self.cancel_import(cx);
            return;
        }
        if let Some(reading) = self.reading.take() {
            reading.cancel();
        }
        cx.emit(ImportEvent::Close);
    }

    fn mapping_row(&self, index: usize, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let mapping = self.mapping.as_ref()?;
        let target = mapping.targets().get(index)?;
        let preview = self.preview.ready()?;
        let names = file_names(&preview.columns);
        let source = mapping.source(index);
        let shown = match source {
            Some(column) => names.get(column).cloned().unwrap_or_default(),
            None => tr!("Left out (its default)").to_owned(),
        };
        let view = cx.entity().downgrade();
        let name = target.name.clone();
        Some(
            row()
                .id(("import-mapping", index))
                .h(px(ROW_HEIGHT))
                .px_2()
                .gap_3()
                .child(
                    div()
                        .w(px(220.))
                        .flex_none()
                        .truncate()
                        .text_size(px(13.))
                        .child(name.clone()),
                )
                .child(
                    div()
                        .w(px(140.))
                        .flex_none()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(rgb(colors().faint))
                        .child(target.data_type.clone().unwrap_or_default()),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Button::new(("import-source", index))
                            .ghost()
                            .small()
                            .w_full()
                            .label(shown)
                            .dropdown_caret(true)
                            .accessibility_label(tr!("File column for {0}", name))
                            .dropdown_menu(move |mut menu, _, _| {
                                let pick = |choice: Option<usize>| {
                                    let view = view.clone();
                                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.set_source(index, choice, cx)
                                        });
                                    }
                                };
                                menu = menu.item(
                                    PopupMenuItem::new(tr!("Left out (its default)"))
                                        .checked(source.is_none())
                                        .on_click(pick(None)),
                                );
                                for (column, title) in names.iter().enumerate() {
                                    menu = menu.item(
                                        PopupMenuItem::new(title.clone())
                                            .checked(source == Some(column))
                                            .on_click(pick(Some(column))),
                                    );
                                }
                                menu
                            }),
                    ),
                )
                .when(target.required, |s| {
                    s.child(badge(tr!("Required"), colors().amber))
                }),
        )
    }

    fn preview_row(&self, position: usize) -> Option<Stateful<Div>> {
        let values = self.shown.get(position)?;
        Some(
            row()
                .id(("import-preview-row", position))
                .h(px(ROW_HEIGHT))
                .gap_0()
                .border_b_1()
                .border_color(rgb(colors().grid_line))
                .children(values.iter().take(PREVIEW_COLUMNS).map(|value| {
                    let cell = div()
                        .w(px(PREVIEW_COLUMN))
                        .flex_none()
                        .px_2()
                        .truncate()
                        .font_family(fonts().mono.clone())
                        .text_size(px(12.));
                    match value {
                        ImportValue::Null => cell
                            .italic()
                            .text_color(rgb(colors().value_null))
                            .child("NULL"),
                        ImportValue::Text(text) => cell
                            .text_color(rgb(colors().value_text))
                            .child(text.replace(['\r', '\n'], "↵")),
                        ImportValue::Boolean(flag) => cell
                            .text_color(rgb(colors().value_number))
                            .child(if *flag { "true" } else { "false" }),
                    }
                })),
        )
    }

    fn file_section(&self, cx: &mut Context<Self>) -> Div {
        let file = self
            .file
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| tr!("No file chosen").to_owned());
        let options = match (&self.inspection, self.kind) {
            (Load::Ready(()), FileKind::Csv) => Some(self.csv_options(cx)),
            (Load::Ready(()), FileKind::Xlsx) => Some(
                row()
                    .gap_3()
                    .child(
                        column()
                            .gap_1()
                            .w(px(260.))
                            .child(label(tr!("Sheet")))
                            .child(
                                Select::new(&self.sheet_select)
                                    .small()
                                    .w_full()
                                    .accessibility_label(tr!("Sheet")),
                            ),
                    )
                    .child(self.header_field()),
            ),
            _ => None,
        };
        column()
            .gap_2()
            .child(
                row()
                    .gap_2()
                    .child(
                        button("import-choose-file", I::FolderOpen, tr!("Choose file…"))
                            .on_click(cx.listener(|this, _, window, cx| this.choose_file(window, cx))),
                    )
                    .child(
                        div()
                            .id("import-file")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(rgb(colors().muted))
                            .child(file),
                    ),
            )
            .when_some(options, |s, options| s.child(options))
            .when(self.kind == FileKind::Xlsx && self.preview.ready().is_some_and(|preview| preview.numbers), |s| {
                s.child(feedback_line(&Feedback::new(
                    Tone::Info,
                    tr!(
                        "Excel stores numbers as floating point: a number with more than 15 significant digits was already rounded when the file was saved. The preview shows what will be written."
                    ),
                )))
            })
    }

    fn header_field(&self) -> Div {
        column()
            .gap_1()
            .w(px(180.))
            .child(label(tr!("Header row")))
            .child(
                Input::new(&self.header)
                    .small()
                    .aria_label(tr!("Row that names the columns; empty when there is none")),
            )
    }

    fn csv_options(&self, cx: &mut Context<Self>) -> Div {
        let delimiter_name = |delimiter: Delimiter| match delimiter {
            Delimiter::Comma => tr!(", comma"),
            Delimiter::Semicolon => tr!("; semicolon"),
            Delimiter::Tab => tr!("Tab"),
            Delimiter::Pipe => tr!("| pipe"),
        };
        row()
            .flex_wrap()
            .items_start()
            .gap_3()
            .child(
                column().gap_1().child(label(tr!("Delimiter"))).child(
                    row()
                        .gap_1()
                        .children(Delimiter::ALL.into_iter().enumerate().map(
                            |(index, delimiter)| {
                                super::assistant_settings::choice(
                                    ("import-delimiter", index),
                                    delimiter_name(delimiter),
                                    self.delimiter == delimiter,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if this.delimiter != delimiter {
                                            this.delimiter = delimiter;
                                            this.read_preview(cx);
                                        }
                                    },
                                ))
                            },
                        )),
                ),
            )
            .child(
                column()
                    .gap_1()
                    .w(px(200.))
                    .child(label(tr!("Encoding")))
                    .child(
                        Select::new(&self.encoding_select)
                            .small()
                            .w_full()
                            .accessibility_label(tr!("Encoding")),
                    ),
            )
            .child(self.header_field())
            .child(
                column().gap_1().child(label(tr!("Empty fields"))).child(
                    row().gap_1().children(
                        [
                            (EmptyFields::Null, tr!("NULL")),
                            (EmptyFields::EmptyText, tr!("Empty text")),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (empty, text))| {
                            super::assistant_settings::choice(
                                ("import-empty", index),
                                text,
                                self.empty == empty,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if this.empty != empty {
                                        this.empty = empty;
                                        this.read_preview(cx);
                                    }
                                },
                            ))
                        }),
                    ),
                ),
            )
    }

    fn table_section(&self, cx: &mut Context<Self>) -> Div {
        let can_draft = self
            .preview
            .ready()
            .is_some_and(|preview| !preview.columns.is_empty());
        column()
            .gap_1()
            .child(label(tr!("Table")))
            .child(
                row()
                    .gap_2()
                    .child(
                        div().flex_1().min_w_0().child(
                            Select::new(&self.table_select)
                                .small()
                                .w_full()
                                .placeholder(tr!("Choose a table"))
                                .accessibility_label(tr!("Table to import into")),
                        ),
                    )
                    .child(
                        button(
                            "import-new-table",
                            I::SquareTerminal,
                            tr!("New table from this file…"),
                        )
                        .when(!can_draft, |s| s.opacity(0.5).cursor_default())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if can_draft {
                                this.draft_table(cx);
                            }
                        })),
                    ),
            )
            .when(!can_draft, |s| {
                s.child(label(tr!(
                    "The table must exist. New table from this file drafts its CREATE TABLE in a query tab, from the file's header."
                )))
            })
    }

    fn mapping_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let message = match (&self.targets, &self.preview) {
            (Load::Failed(error), _) => Some(Feedback::new(Tone::Error, error.clone())),
            (_, Load::Failed(error)) => Some(Feedback::new(Tone::Error, error.clone())),
            _ => None,
        };
        if let Some(message) = message {
            return feedback_line(&message)
                .id("import-error")
                .role(Role::Alert)
                .into_any_element();
        }
        let Some(mapping) = &self.mapping else {
            return label(tr!("Choose a file and a table to match their columns."))
                .into_any_element();
        };
        let count = mapping.targets().len();
        column()
            .gap_1()
            .child(
                row()
                    .justify_between()
                    .child(label(tr!(
                        "Columns of the table, and the file column that fills each"
                    )))
                    .child(
                        row().gap_1().children(
                            [
                                (MatchBy::Name, tr!("Match by name")),
                                (MatchBy::Position, tr!("Match by position")),
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(index, (by, text))| {
                                super::assistant_settings::choice(
                                    ("import-match", index),
                                    text,
                                    self.match_by == by,
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.set_match_by(by, cx)),
                                )
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .relative()
                    .h(px((count.min(6) as f32 * ROW_HEIGHT).max(ROW_HEIGHT)))
                    .child(
                        uniform_list(
                            "import-mapping-rows",
                            count,
                            cx.processor(move |this, range: Range<usize>, _, cx| {
                                range
                                    .filter_map(|index| this.mapping_row(index, cx))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full()
                        .track_scroll(&self.mapping_scroll),
                    )
                    .vertical_scrollbar(&self.mapping_scroll),
            )
            .into_any_element()
    }

    fn preview_section(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(started) = self.waiting_since() {
            let elapsed = started.elapsed();
            if elapsed < Duration::from_millis(300) {
                return div().into_any_element();
            }
            return row()
                .id("import-reading")
                .role(Role::Status)
                .gap_2()
                .child(spinning_icon(
                    "import-spinner",
                    I::LoaderCircle,
                    colors().mint,
                ))
                .child(label(if elapsed >= Duration::from_secs(2) {
                    tr!("Reading the file… {0} s", elapsed.as_secs())
                } else {
                    tr!("Reading the file…").to_owned()
                }))
                .when(elapsed >= Duration::from_secs(2), |s| {
                    s.child(
                        button("import-cancel-reading", I::X, tr!("Cancel"))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_reading(cx))),
                    )
                })
                .into_any_element();
        }
        if let Load::Failed(error) = &self.inspection {
            return feedback_line(&Feedback::new(Tone::Error, error.clone()))
                .id("import-file-error")
                .role(Role::Alert)
                .into_any_element();
        }
        if self.shown_columns.is_empty() {
            return div().into_any_element();
        }
        let columns = self.shown_columns.len().min(PREVIEW_COLUMNS);
        let width = columns as f32 * PREVIEW_COLUMN;
        let more = self.preview.ready().is_some_and(|preview| preview.more);
        let hidden = self.shown_columns.len() - columns;
        column()
            .gap_1()
            .child(label(if more {
                tr!(
                    "The first {0} rows, as they would be written",
                    self.shown.len()
                )
            } else {
                match self.shown.len() {
                    1 => tr!("The only row, as it would be written").to_owned(),
                    rows => tr!("All {0} rows, as they would be written", rows),
                }
            }))
            .child(
                div()
                    .id("import-preview")
                    .overflow_x_scroll()
                    .border_1()
                    .border_color(rgb(colors().border))
                    .rounded(px(6.))
                    .bg(rgb(colors().grid_bg))
                    .child(
                        column()
                            .w(px(width))
                            .child(
                                row()
                                    .h(px(40.))
                                    .gap_0()
                                    .bg(rgb(colors().grid_header))
                                    .children(self.shown_columns.iter().take(columns).map(
                                        |(target, source)| {
                                            column()
                                                .w(px(PREVIEW_COLUMN))
                                                .flex_none()
                                                .px_2()
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_size(px(12.))
                                                        .text_color(rgb(colors().grid_header_text))
                                                        .child(target.clone()),
                                                )
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_size(px(11.))
                                                        .text_color(rgb(colors().grid_type_text))
                                                        .child(tr!("from {0}", source)),
                                                )
                                        },
                                    )),
                            )
                            .child(
                                div().h(px(220.)).child(
                                    uniform_list(
                                        "import-preview-rows",
                                        self.shown.len(),
                                        cx.processor(move |this, range: Range<usize>, _, _| {
                                            range
                                                .filter_map(|position| this.preview_row(position))
                                                .collect::<Vec<_>>()
                                        }),
                                    )
                                    .size_full()
                                    .track_scroll(&self.preview_scroll),
                                ),
                            ),
                    ),
            )
            .when(hidden > 0, |s| {
                s.child(label(if hidden == 1 {
                    tr!("1 more column is not shown.").to_owned()
                } else {
                    tr!("{0} more columns are not shown.", hidden)
                }))
            })
            .into_any_element()
    }

    /// The import's progress after 300 ms, its elapsed time and **Cancel**
    /// past 2 s, then how it ended.
    fn run_status(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.run {
            ImportRun::Idle => div().into_any_element(),
            ImportRun::Running {
                started, progress, ..
            } => running_status(*started, *progress.borrow(), Self::cancel_import, cx),
            ImportRun::Finished { tone, text, detail } => column()
                .id("import-outcome")
                .role(if *tone == Tone::Error {
                    Role::Alert
                } else {
                    Role::Status
                })
                .gap_1()
                .child(feedback_line(&Feedback::new(*tone, text.clone())))
                .when_some(detail.clone(), |s, detail| {
                    s.child(
                        div()
                            .pl_6()
                            .font_family(fonts().mono.clone())
                            .text_size(px(12.))
                            .text_color(rgb(colors().muted))
                            .child(detail),
                    )
                })
                .into_any_element(),
        }
    }

    fn footer(&self, cx: &mut Context<Self>) -> Div {
        let running = self.is_running();
        let finished = matches!(self.run, ImportRun::Finished { .. });
        let ready = self.ready_to_import();
        column()
            .gap_2()
            .when(!self.problems.is_empty(), |s| {
                s.child(
                    column()
                        .id("import-problems")
                        .role(Role::Status)
                        .gap_1()
                        .children(self.problems.iter().map(|problem| {
                            feedback_line(&Feedback::new(Tone::Warning, problem.clone()))
                        })),
                )
            })
            .when_some(self.header_error.clone(), |s, error| {
                s.child(
                    feedback_line(&Feedback::new(Tone::Error, error))
                        .id("import-header-error")
                        .role(Role::Alert),
                )
            })
            .child(self.run_status(cx))
            .child(label(tr!(
                "The rows are added to the table in one transaction: all of them, or none if anything fails or you cancel. Values are sent as written and converted by the database."
            )))
            .child(
                row()
                    .justify_between()
                    .child(
                        row()
                            .gap_1()
                            .child(label(tr!("Time limit")))
                            .children(TimeLimit::ALL.into_iter().enumerate().map(|(index, limit)| {
                                super::assistant_settings::choice(
                                    ("import-limit", index),
                                    limit.label(),
                                    self.time_limit == limit,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.time_limit = limit;
                                    cx.notify();
                                }))
                            })),
                    )
                    .child(
                        row()
                            .gap_2()
                            .when(!running, |s| {
                                s.child(
                                    button(
                                        "import-close",
                                        I::X,
                                        if finished { tr!("Close") } else { tr!("Cancel") },
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                                )
                            })
                            .child(
                                primary_button("import-run", I::Upload, tr!("Import"))
                                    .when(!ready, |s| s.opacity(0.5).cursor_default())
                                    .on_click(cx.listener(|this, _, _, cx| this.import(cx))),
                            ),
                    ),
            )
    }
}

/// What a running import shows: nothing for 300 ms, then the rows written
/// and the share of the file read, and past 2 s the elapsed time and
/// **Cancel import**, which calls `cancel`. The dialog draws it, and so
/// does the performance run (`--test-performance`), which measures when it
/// appears.
pub(super) fn running_status<V: 'static>(
    started: Instant,
    progress: Progress,
    cancel: fn(&mut V, &mut Context<V>),
    cx: &mut Context<V>,
) -> AnyElement {
    let elapsed = started.elapsed();
    let shown = Shown::after(elapsed);
    if shown == Shown::Nothing {
        return div().into_any_element();
    }
    let rows = jykre::cell_format::count(progress.rows_written as usize);
    let text = match progress.read {
        Some(share) => tr!(
            "Importing… rows written: {0}, {1} % of the file read",
            rows,
            (share * 100.0).floor() as u32
        ),
        None => tr!("Importing… rows written: {0}", rows),
    };
    row()
        .id("import-progress")
        .role(Role::Status)
        .gap_2()
        .child(spinning_icon(
            "import-run-spinner",
            I::LoaderCircle,
            colors().mint,
        ))
        .child(div().text_size(px(13.)).child(text))
        .when(shown == Shown::ProgressAndCancel, |s| {
            s.child(label(jykre::cell_format::duration(
                elapsed.as_millis() as u64
            )))
            .child(
                button("import-cancel", I::X, tr!("Cancel import"))
                    .on_click(cx.listener(move |this, _, _, cx| cancel(this, cx))),
            )
        })
        .into_any_element()
}

/// Redraws `V` while `running` says an import runs: after 300 ms for its
/// progress, then four times a second.
pub(super) fn redraw_while<V: 'static>(cx: &mut Context<V>, running: fn(&V) -> bool) {
    cx.spawn(async move |weak, cx| {
        let mut wait = PROGRESS_AFTER;
        loop {
            cx.background_executor().timer(wait).await;
            wait = Duration::from_millis(250);
            let running = weak
                .update(cx, |this, cx| {
                    let running = running(this);
                    if running {
                        cx.notify();
                    }
                    running
                })
                .unwrap_or(false);
            if !running {
                return;
            }
        }
    })
    .detach();
}

/// How an import ended, in words (ADR-0040, section 4).
fn finished(outcome: &DriverResult<Outcome>, table: &str) -> ImportRun {
    match outcome {
        Ok(outcome @ Outcome::Imported { .. }) => ImportRun::Finished {
            tone: Tone::Success,
            text: outcome.message(table),
            detail: None,
        },
        Ok(outcome @ Outcome::NothingImported { reason }) => ImportRun::Finished {
            tone: if reason.code == ErrorCode::Cancelled {
                Tone::Info
            } else {
                Tone::Error
            },
            text: outcome.message(table),
            detail: Some(if reason.code == ErrorCode::Cancelled {
                tr!("The import was cancelled.").to_owned()
            } else {
                reason.message.clone()
            }),
        },
        Ok(outcome @ Outcome::Unknown { .. }) => ImportRun::Finished {
            tone: Tone::Warning,
            text: outcome.message(table),
            detail: None,
        },
        Err(error) => ImportRun::Finished {
            tone: Tone::Error,
            text: tr!(
                "The import stopped unexpectedly. Check {0} before importing again.",
                table
            ),
            detail: Some(error.message.clone()),
        },
    }
}

/// The names the dialog shows for the file's columns: the header's, or
/// "Column N" for an empty one.
fn file_names(columns: &[String]) -> Vec<String> {
    columns
        .iter()
        .enumerate()
        .map(|(index, name)| {
            if name.is_empty() {
                tr!("Column {0}", index + 1)
            } else {
                name.clone()
            }
        })
        .collect()
}

impl Render for ImportDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.needs_focus) {
            self.focus.focus(window, cx);
        }
        let title = tr!("Import from file");
        modal_backdrop("import-backdrop").child(
            column()
                .id("import-dialog")
                .role(Role::Dialog)
                .aria_label(title)
                .w(px(880.))
                .max_h(relative(0.92))
                .mt(px(32.))
                .rounded(px(12.))
                .border_1()
                .border_color(rgb(colors().border_strong))
                .bg(rgb(colors().panel))
                .shadow_lg()
                .overflow_hidden()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    row()
                        .flex_none()
                        .px_5()
                        .pt_4()
                        .justify_between()
                        .child(
                            column()
                                .child(
                                    div()
                                        .text_size(px(18.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(title),
                                )
                                .child(label(tr!("Into a table of {0}", self.connection))),
                        )
                        .child(
                            icon_button("import-x", I::X, tr!("Close"))
                                .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                        ),
                )
                .child(
                    column()
                        .id("import-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px_5()
                        .py_3()
                        .gap_4()
                        .child(self.file_section(cx))
                        .child(self.table_section(cx))
                        .child(self.mapping_section(cx))
                        .child(self.preview_section(cx)),
                )
                .child(
                    div()
                        .flex_none()
                        .px_5()
                        .pb_4()
                        .pt_2()
                        .border_t_1()
                        .border_color(rgb(colors().border))
                        .child(self.footer(cx)),
                )
                // Tab stays inside the dialog while it is open.
                .focus_trap("import-focus-trap", &self.focus),
        )
    }
}

impl AppView {
    /// An import runs in one transaction (decision I-1), so it is offered
    /// only on connections whose driver has transactions.
    pub(super) fn can_import(&self, index: usize) -> bool {
        self.connections
            .get(index)
            .and_then(|connection| connection.input.as_ref())
            .and_then(|input| {
                builtin_registry()
                    .and_then(|registry| registry.get(input.config.driver))
                    .ok()
            })
            .is_some_and(|driver| driver.descriptor().capabilities.transactions)
    }

    /// Opens the import dialog on the connection at `index`, with `table`
    /// chosen, or for a new table in `namespace`.
    pub(super) fn open_import(
        &mut self,
        index: usize,
        table: Option<ObjectPath>,
        namespace: Option<ObjectPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dialog_open() || self.password_prompt.is_some() || self.connection_open {
            return;
        }
        let Some(connection) = self.connections.get(index) else {
            return;
        };
        let (Some(input), Some(profile)) = (connection.input.as_ref(), connection.profile.as_ref())
        else {
            return;
        };
        let Ok(driver) = builtin_registry().and_then(|registry| registry.get(input.config.driver))
        else {
            return;
        };
        self.command_open = false;
        let (name, id, config, key) = (
            connection.name.clone(),
            profile.id,
            input.config.clone(),
            connection.key,
        );
        let opening = Opening {
            connection: name,
            id,
            config,
            driver,
            table,
            namespace,
        };
        let dialog = cx.new(|cx| ImportDialog::new(opening, window, cx));
        let events = cx.subscribe_in(
            &dialog,
            window,
            move |this, _, event: &ImportEvent, window, cx| match event {
                ImportEvent::Close => this.close_import(window, cx),
                ImportEvent::Start => this.start_import(key, window, cx),
                ImportEvent::Draft { sql, title } => {
                    let (sql, title) = (sql.clone(), title.clone());
                    this.close_import(window, cx);
                    this.open_draft(key, &title, &sql, window, cx);
                }
            },
        );
        self.import_dialog = Some(ImportHost {
            dialog,
            _events: events,
        });
        cx.notify();
    }

    /// The dialog's view, while it is open.
    pub(super) fn import_view(&self) -> Option<Entity<ImportDialog>> {
        self.import_dialog.as_ref().map(|host| host.dialog.clone())
    }

    /// Closes the dialog; nothing was written.
    /// Closes the dialog. While an import runs, closing cancels it instead,
    /// and the dialog stays to say what it left.
    pub(super) fn close_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(host) = &self.import_dialog
            && host.dialog.read(cx).is_running()
        {
            host.dialog
                .update(cx, |dialog, cx| dialog.cancel_import(cx));
            return;
        }
        if let Some(host) = self.import_dialog.take() {
            // A preview still being read stops with the dialog.
            host.dialog.update(cx, |dialog, _| {
                if let Some(reading) = dialog.reading.take() {
                    reading.cancel();
                }
            });
            self.focus_active_editor(window, cx);
            cx.notify();
        }
    }

    /// **Import**: asks for the connection's password when it is not in
    /// memory, then, on a connection marked Production, asks first, naming
    /// the connection, the table and the file; then the dialog runs it.
    fn start_import(&mut self, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.connection_index(key) else {
            return;
        };
        let Some(connection) = self.connections.get(index) else {
            return;
        };
        let Some(input) = connection.input.clone() else {
            return;
        };
        if connection_store::needs_password(&input) {
            self.ask_password(
                index,
                Box::new(move |this, window, cx| this.start_import(key, window, cx)),
                Box::new(|_, _| {}),
                cx,
            );
            return;
        }
        let Some(dialog) = self.import_view() else {
            return;
        };
        let production = connection.appearance.environment
            == Some(jykre::connection_appearance::Environment::Production);
        if !production {
            dialog.update(cx, |dialog, cx| dialog.run(input, cx));
            return;
        }
        let (file, table) = dialog.read(cx).describe();
        let name = connection.name.clone();
        confirm(
            window,
            cx,
            Confirmation {
                title: tr!("Import into a production table?").to_owned(),
                description: tr!(
                    "The rows of {0} are added to {1} on {2}, a connection marked Production. They are written in one transaction: all of them, or none.",
                    file,
                    table,
                    name
                ),
                ok: tr!("Import on production"),
                cancel: tr!("Cancel"),
                focus: ConfirmFocus::Dialog,
            },
            move |_, cx| {
                let input = input.clone();
                dialog.update(cx, |dialog, cx| dialog.run(input, cx));
            },
        );
    }

    /// Opens the draft `CREATE TABLE` in a new query tab of the connection
    /// `key`, not run.
    fn open_draft(
        &mut self,
        key: u64,
        title: &str,
        sql: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self
            .connection_index(key)
            .and_then(|index| self.connections.get(index))
            .map(|connection| connection.group)
        else {
            return;
        };
        if let Some(tab) = self.add_query(group, title, sql, window, cx)
            && let Some(panel) = self.panels.get(&tab).cloned()
        {
            panel.update(cx, |panel, cx| panel.focus_editor(window, cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CancellationToken, ImportRun, Instant, Progress, Tone};

    #[test]
    fn a_change_forgets_the_last_outcome_but_never_a_running_import() {
        crate::app::source_text();
        let mut run = ImportRun::Finished {
            tone: Tone::Success,
            text: "Imported 12 rows".into(),
            detail: None,
        };
        run.forget_outcome();
        assert!(matches!(run, ImportRun::Idle));
        run.forget_outcome();
        assert!(matches!(run, ImportRun::Idle));
        let (_sender, progress) = tokio::sync::watch::channel(Progress::default());
        let mut run = ImportRun::Running {
            started: Instant::now(),
            cancel: CancellationToken::new(),
            progress,
        };
        run.forget_outcome();
        assert!(matches!(run, ImportRun::Running { .. }));
    }
}
