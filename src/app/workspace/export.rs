//! The export bar of a query tab: the options, the system's save dialog, the
//! export's progress and how it ended ([Results](../../../docs/reference/results.md#export)).
//!
//! The bar exports the result tab on screen. What it writes and how runs in
//! `jykre::export` and `backend::export`; this view only chooses, starts,
//! shows and cancels.
use super::*;
use jykre::backend::{ExportProgress, ExportRead, ExportRequest, ExportRun};
use jykre::export::{
    ExportFormat, ExportOptions, ExportPreferences, Quoting, Separator, SqlTarget,
    suggested_file_name,
};
use jykre::result::ResultColumn;
use jykre::result_tabs::{ExportSource, Missing};
use std::path::PathBuf;
use std::time::Instant;

/// How an export reads the rows of the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Only the rows the tab holds.
    Held,
    /// The rows held, then the rest the session keeps open.
    Rest,
    /// The statement runs again and everything it returns is written.
    Again,
}

/// What the export bar shows.
pub(super) enum ExportState {
    Closed,
    /// The options for the result on screen.
    Choosing,
    /// The system's save dialog is open.
    Picking,
    Writing {
        started: Instant,
        progress: ExportProgress,
        /// The rows to write, when the tab holds all of them.
        total: Option<u64>,
        /// The file's name, without its folder.
        name: String,
        /// The export reads from the tab's session, which nothing else may
        /// use meanwhile.
        session: bool,
    },
    Cancelling {
        session: bool,
    },
    /// How the last export ended, until the bar is closed.
    Ended {
        feedback: Feedback,
        /// The file written, for **Show in folder**.
        path: Option<PathBuf>,
    },
}

/// The export bar of a tab and the export it runs.
pub(super) struct ExportBar {
    pub(super) state: ExportState,
    preferences: ExportPreferences,
    /// The user changed an option since the bar opened, so the preferences
    /// read meanwhile do not overwrite it.
    touched: bool,
    cancel: CancellationToken,
    /// Bumped by every export, so one that ends after another began
    /// changes nothing on screen.
    generation: u64,
}

impl Default for ExportBar {
    fn default() -> Self {
        Self {
            state: ExportState::Closed,
            preferences: ExportPreferences::default(),
            touched: false,
            cancel: CancellationToken::new(),
            generation: 0,
        }
    }
}

impl ExportBar {
    /// Stops the export in progress, if any.
    pub(super) fn cancel(&mut self) {
        self.cancel.cancel();
    }
}

fn format_name(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Csv => tr!("CSV"),
        ExportFormat::Xlsx => tr!("Excel (XLSX)"),
        ExportFormat::Json => tr!("JSON"),
        ExportFormat::JsonLines => tr!("JSON Lines"),
        ExportFormat::Insert => tr!("SQL INSERT"),
        ExportFormat::Markdown => tr!("Markdown"),
    }
}

fn separator_name(separator: Separator) -> &'static str {
    match separator {
        Separator::Semicolon => tr!("Semicolon (;)"),
        Separator::Comma => tr!("Comma (,)"),
        Separator::Tab => tr!("Tab"),
    }
}

/// A check box of the bar.
fn toggle(id: &'static str, on: bool, title: &'static str) -> Stateful<Div> {
    button(id, if on { I::SquareCheck } else { I::Square }, title)
        .role(Role::CheckBox)
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .when(on, |s| s.text_color(rgb(colors().mint)))
}

impl Workspace {
    /// The export button and Ctrl+Shift+E: opens the export bar for the
    /// result on screen, or closes it. While an export runs, the bar stays.
    pub fn toggle_export(&mut self, cx: &mut Context<Self>) {
        match self.export.state {
            ExportState::Picking | ExportState::Writing { .. } | ExportState::Cancelling { .. } => {
                return;
            }
            ExportState::Choosing | ExportState::Ended { .. } => {
                self.export.state = ExportState::Closed;
                cx.notify();
                return;
            }
            ExportState::Closed => {}
        }
        self.export.state = ExportState::Choosing;
        self.export.touched = false;
        self.result_view = ResultView::Table;
        // Another tab may have exported since: the last choices are read
        // again each time the bar opens.
        let dir = connection_store::data_dir();
        cx.spawn(async move |weak, cx| {
            let loaded =
                backend::run(async move { jykre::export::load_preferences(&dir).await }).await;
            let _ = weak.update(cx, |this, cx| match loaded {
                Ok(preferences) if !this.export.touched => {
                    this.export.preferences = preferences;
                    cx.notify();
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(error_code = ?error.code, "export preferences unavailable");
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// An export is writing a file or stopping.
    pub fn is_exporting(&self) -> bool {
        matches!(
            self.export.state,
            ExportState::Writing { .. } | ExportState::Cancelling { .. }
        )
    }

    /// An export has the tab's session out, so no run or page read may
    /// start.
    pub(super) fn export_uses_session(&self) -> bool {
        matches!(
            self.export.state,
            ExportState::Writing { session: true, .. } | ExportState::Cancelling { session: true }
        )
    }

    fn change_export_options(
        &mut self,
        change: impl FnOnce(&mut ExportOptions),
        cx: &mut Context<Self>,
    ) {
        change(&mut self.export.preferences.options);
        self.export.touched = true;
        cx.notify();
    }

    /// Whether running the statement of `tab` again may read its whole
    /// result for an export: a single query that only reads, on a
    /// connection.
    fn can_run_again(&self, tab: &ResultTab) -> bool {
        self.input.is_some() && jykre::sql::reads_only(&tab.sql, self.run.dialect())
    }

    /// Starts exporting the result on screen. `held_only` writes only the
    /// rows the tab holds when part of the result can no longer be read;
    /// otherwise such a result runs its statement again, after asking.
    fn start_export(&mut self, held_only: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.results.active() else {
            return;
        };
        let source = tab.export_source();
        let id = tab.id;
        let again = self.can_run_again(tab);
        let rest = source == ExportSource::Rest && self.session.is_some();
        let mode = match source {
            ExportSource::Nothing => {
                self.export.state = ExportState::Ended {
                    feedback: Feedback::new(Tone::Info, tr!("This result has no rows to export.")),
                    path: None,
                };
                cx.notify();
                return;
            }
            ExportSource::Held => Mode::Held,
            ExportSource::Rest if rest => Mode::Rest,
            // A statement that may write is never run again.
            _ if held_only || !again => Mode::Held,
            _ => Mode::Again,
        };
        if mode != Mode::Held && self.is_busy() {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("A query is using the session. Wait for it, or stop it, before exporting."),
            ));
            cx.notify();
            return;
        }
        if mode == Mode::Again {
            if self
                .input
                .as_ref()
                .is_some_and(connection_store::needs_password)
            {
                self.notice = Some(Feedback::new(
                    Tone::Info,
                    tr!("Run a query first, so the connection has its password, then export."),
                ));
                cx.notify();
                return;
            }
            let workspace = cx.entity().downgrade();
            super::super::confirm(
                window,
                cx,
                super::super::Confirmation {
                    title: tr!("Run the statement again to export it?").to_string(),
                    description: tr!(
                        "This tab no longer holds the whole result, so the statement runs again on the server and everything it returns goes to the file. The rows can differ from the ones on screen."
                    )
                    .to_string(),
                    ok: tr!("Run and export"),
                    cancel: tr!("Cancel"),
                    focus: super::super::ConfirmFocus::Dialog,
                },
                move |_, cx| {
                    let _ = workspace.update(cx, |this, cx| this.pick_export_file(id, Mode::Again, cx));
                },
            );
            return;
        }
        self.pick_export_file(id, mode, cx);
    }

    /// Opens the system's save dialog for the export of result `id`.
    fn pick_export_file(&mut self, id: ResultId, mode: Mode, cx: &mut Context<Self>) {
        let format = self.export.preferences.options.format;
        let stem = self
            .object
            .as_ref()
            .and_then(ObjectPath::relation)
            .map_or_else(|| self.title.to_string(), str::to_owned);
        let name = suggested_file_name(&stem, format);
        let directory = self
            .export
            .preferences
            .directory
            .clone()
            .unwrap_or_else(jykre::export::default_directory);
        self.export.state = ExportState::Picking;
        cx.notify();
        let chosen = cx.prompt_for_new_path(&directory, Some(name.as_str()));
        cx.spawn(async move |weak, cx| {
            let chosen = chosen.await;
            let _ = weak.update(cx, |this, cx| {
                if !matches!(this.export.state, ExportState::Picking) {
                    return;
                }
                match chosen {
                    Ok(Ok(Some(path))) => this.begin_export(id, mode, path, cx),
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "save dialog failed");
                        this.export.state = ExportState::Ended {
                            feedback: Feedback::new(
                                Tone::Error,
                                tr!("The save dialog could not open. Nothing was exported."),
                            ),
                            path: None,
                        };
                        cx.notify();
                    }
                    // Closed without a file: back to the options.
                    _ => {
                        this.export.state = ExportState::Choosing;
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// How `INSERT`s name the table and the columns, as **Copy as** does.
    fn export_sql(&self, columns: &[ResultColumn], cx: &App) -> SqlTarget {
        let quote = self
            .sql_editor
            .read(cx)
            .name_quoter()
            .unwrap_or_else(|| super::super::result_grid::GridSql::default().quote_name);
        let table = self
            .object
            .as_ref()
            .and_then(|path| {
                let input = self.input.as_ref()?;
                let driver = builtin_registry()
                    .and_then(|registry| registry.get(input.config.driver))
                    .ok()?;
                Some(driver.relation_name(path))
            })
            .unwrap_or_else(|| "table_name".to_owned());
        SqlTarget {
            dialect: self.run.dialect(),
            table,
            columns: columns.iter().map(|column| quote(&column.name)).collect(),
        }
    }

    /// Writes result `id` to `path`, reading it as `mode` says.
    fn begin_export(&mut self, id: ResultId, mode: Mode, path: PathBuf, cx: &mut Context<Self>) {
        let changed = |this: &mut Self, cx: &mut Context<Self>| {
            this.export.state = ExportState::Ended {
                feedback: Feedback::new(
                    Tone::Info,
                    tr!("The result changed while the file was chosen. Nothing was exported."),
                ),
                path: None,
            };
            cx.notify();
        };
        let Some(tab) = self.results.get(id) else {
            return changed(self, cx);
        };
        let source = tab.export_source();
        let still = match mode {
            Mode::Held => source != ExportSource::Nothing,
            Mode::Rest => source == ExportSource::Rest && self.session.is_some() && !self.is_busy(),
            Mode::Again => self.input.is_some() && !self.is_busy(),
        };
        if !still {
            return changed(self, cx);
        }
        let columns = tab.columns();
        let pages = if mode == Mode::Again {
            Vec::new()
        } else {
            tab.held_pages()
        };
        let total = (mode == Mode::Held).then(|| tab.held_rows() as u64);
        let statement = tab.sql.clone();
        let parameters = tab.parameters.clone();
        let sql = self.export_sql(&columns, cx);
        let options = self.export.preferences.options;
        self.export.preferences.directory = path.parent().map(PathBuf::from);
        let preferences = self.export.preferences.clone();
        let dir = connection_store::data_dir();
        let ticket = jykre::export::PREFERENCE_WRITES.ask();
        cx.background_spawn(async move {
            let saved = backend::run(async move {
                jykre::export::save_preferences(&dir, preferences, ticket).await
            })
            .await;
            if let Err(error) = saved {
                tracing::debug!(error_code = ?error.code, "export preferences not saved");
            }
        })
        .detach();
        let (read, lease) = match mode {
            Mode::Held => (ExportRead::Held, None),
            Mode::Rest => {
                let (session, lease) = self.lease_session(false);
                let Some(session) = session else {
                    self.session_lease = None;
                    return changed(self, cx);
                };
                (
                    ExportRead::Rest {
                        session,
                        options: self.options(),
                    },
                    Some(lease),
                )
            }
            Mode::Again => {
                let Some(input) = self.input.clone() else {
                    return changed(self, cx);
                };
                let (session, lease) = self.lease_session(true);
                (
                    ExportRead::Again {
                        input,
                        session,
                        sql: statement.clone(),
                        parameters: parameters.clone(),
                        options: self.options(),
                    },
                    Some(lease),
                )
            }
        };
        self.export.generation += 1;
        let generation = self.export.generation;
        self.export.cancel = CancellationToken::new();
        let cancel = self.export.cancel.clone();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.export.state = ExportState::Writing {
            started: Instant::now(),
            progress: ExportProgress::default(),
            total,
            name,
            session: lease.is_some(),
        };
        self.start_clock(cx);
        cx.notify();
        let (progress, mut watched) = tokio::sync::watch::channel(ExportProgress::default());
        cx.spawn(async move |weak, cx| {
            while watched.changed().await.is_ok() {
                let progress = *watched.borrow_and_update();
                let alive = weak.update(cx, |this, cx| {
                    if this.export.generation == generation
                        && let ExportState::Writing {
                            progress: shown, ..
                        } = &mut this.export.state
                    {
                        *shown = progress;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        let request = ExportRequest {
            target: path.clone(),
            options,
            columns,
            sql,
            pages,
            read,
        };
        cx.spawn(async move |weak, cx| {
            let run =
                backend::run(
                    async move { Ok(backend::export(request, cancel, Some(progress)).await) },
                )
                .await;
            let mut pending = Some(run);
            let _ = weak.update(cx, |this, cx| {
                if let Some(run) = pending.take() {
                    let ended = Ended {
                        generation,
                        lease,
                        mode,
                        id,
                        path,
                        statement,
                    };
                    this.export_ended(ended, run, cx);
                }
            });
            // The tab closed while the export was out: its session goes too.
            if let Some(Ok(run)) = pending
                && let Some(session) = run.session
            {
                backend::close_in_background(session);
            }
        })
        .detach();
    }

    /// Asks the export to stop. A part being written finishes first; the
    /// file is then removed and the destination keeps what it had.
    fn cancel_export(&mut self, cx: &mut Context<Self>) {
        if let ExportState::Writing { session, .. } = self.export.state {
            self.export.cancel.cancel();
            self.export.state = ExportState::Cancelling { session };
            cx.notify();
        }
    }

    /// The export ended: the session comes back to the tab and the bar says
    /// how it went.
    fn export_ended(&mut self, ended: Ended, run: DriverResult<ExportRun>, cx: &mut Context<Self>) {
        let Ended {
            generation,
            lease,
            mode,
            id,
            path,
            statement,
        } = ended;
        let outcome = match run {
            Ok(run) => {
                match lease {
                    Some(lease) => {
                        let current = self.session_lease == Some(lease);
                        if current {
                            self.session_notice(run.reopened, run.ended);
                            if run.reopened {
                                self.transaction = Transaction::AutoCommit;
                            }
                            self.transaction = self.transaction.reported(run.transaction);
                        }
                        self.session_context(lease, run.context, cx);
                        self.return_session(lease, run.session);
                        if current && self.session.is_none() {
                            self.transaction = Transaction::AutoCommit;
                        }
                    }
                    None => {
                        if let Some(session) = run.session {
                            backend::close_in_background(session);
                        }
                    }
                }
                run.outcome
            }
            Err(error) => Err(error),
        };
        // What the export read went to the file, not to this tab (D4).
        if mode == Mode::Rest {
            self.results.rest_exported(id);
            self.show_active(cx);
        }
        // A statement run again ran on the server: Messages says so.
        if mode == Mode::Again {
            let logged = match &outcome {
                Ok(exported) => LogOutcome::Rows {
                    rows: usize::try_from(exported.rows).unwrap_or(usize::MAX),
                    more: false,
                },
                Err(error) if error.code == ErrorCode::Cancelled => LogOutcome::Cancelled,
                Err(error) => logged_failure(error),
            };
            let elapsed = outcome.as_ref().ok().map(|exported| exported.elapsed_ms);
            self.ran(&statement, logged, elapsed, false, cx);
        }
        if self.export.generation != generation {
            return;
        }
        self.clear_stopping_notice();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.export.state = match outcome {
            Ok(exported) => {
                let rows = cell_format::count(usize::try_from(exported.rows).unwrap_or(usize::MAX));
                let size = cell_format::size(exported.bytes);
                let elapsed =
                    cell_format::duration(exported.elapsed_ms.min(u64::MAX as u128) as u64);
                let text = if exported.truncated {
                    tr!(
                        "Exported {0} rows to {1} ({2}, {3}). The server stopped the result before its end, so the file holds only the rows it returned.",
                        rows,
                        name,
                        size,
                        elapsed
                    )
                } else if mode == Mode::Rest {
                    tr!(
                        "Exported {0} rows to {1} ({2}, {3}). The rest of the result went to the file, so this tab no longer pages past the rows it holds.",
                        rows,
                        name,
                        size,
                        elapsed
                    )
                } else {
                    tr!(
                        "Exported {0} rows to {1} ({2}, {3}).",
                        rows,
                        name,
                        size,
                        elapsed
                    )
                };
                let tone = if exported.truncated {
                    Tone::Warning
                } else {
                    Tone::Success
                };
                ExportState::Ended {
                    feedback: Feedback::new(tone, text),
                    path: Some(path),
                }
            }
            Err(error) if error.code == ErrorCode::Cancelled => ExportState::Ended {
                feedback: Feedback::new(
                    Tone::Info,
                    tr!(
                        "Export cancelled. No file was written, and {0} was left as it was.",
                        name
                    ),
                ),
                path: None,
            },
            Err(error) => ExportState::Ended {
                feedback: Feedback::new(
                    Tone::Error,
                    tr!(
                        "The export failed: {0} No file was written, and {1} was left as it was.",
                        error,
                        name
                    ),
                ),
                path: None,
            },
        };
        cx.notify();
    }

    /// What the bar says about where the rows come from, and which buttons
    /// it offers.
    fn export_source_text(&self, tab: &ResultTab) -> String {
        let rows = cell_format::count(tab.held_rows());
        match tab.export_source() {
            ExportSource::Held => tr!("Exports the {0} rows of this result.", rows),
            ExportSource::Rest if self.session.is_some() || self.export_uses_session() => tr!(
                "Exports the {0} rows on screen, then reads the rest of the result from the server. Afterwards this tab no longer pages past the rows it holds.",
                rows
            ),
            ExportSource::Nothing => tr!("This result has no rows to export.").to_owned(),
            source => {
                let why = match source {
                    ExportSource::Partial(Missing::Released) => {
                        tr!("The first pages of this result were released to save memory.")
                    }
                    _ => tr!("The rest of this result is no longer open on the server."),
                };
                if self.can_run_again(tab) {
                    tr!(
                        "{0} Exporting runs the statement again to read all of it, or exports only the {1} rows this tab holds.",
                        why,
                        rows
                    )
                } else {
                    tr!(
                        "{0} Only a statement that only reads can run again, so the export holds the {1} rows this tab has.",
                        why,
                        rows
                    )
                }
            }
        }
    }

    /// The bar under the result toolbar, or `None` while it is closed.
    pub(super) fn export_bar(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let bar = || {
            row()
                .id("export-bar")
                .flex_wrap()
                .px_3()
                .py_2()
                .gap_2()
                .border_b_1()
                .border_color(rgb(colors().border))
        };
        let close = |cx: &mut Context<Self>| {
            icon_button("export-close", I::X, tr!("Close the export bar")).on_click(cx.listener(
                |this, _, _, cx| {
                    this.export.state = ExportState::Closed;
                    cx.notify();
                },
            ))
        };
        let bar = match &self.export.state {
            ExportState::Closed => return None,
            ExportState::Choosing | ExportState::Picking => {
                let picking = matches!(self.export.state, ExportState::Picking);
                let tab = self.results.active();
                let source = tab.map_or(ExportSource::Nothing, ResultTab::export_source);
                let options = self.export.preferences.options;
                let partial = match source {
                    ExportSource::Partial(_) => true,
                    ExportSource::Rest => self.session.is_none(),
                    _ => false,
                };
                let again = partial && tab.is_some_and(|tab| self.can_run_again(tab));
                bar()
                    .child(self.format_menu(options, cx))
                    .when(options.format == ExportFormat::Csv, |s| {
                        s.child(self.separator_menu(options, cx))
                            .child(
                                toggle(
                                    "export-quote-all",
                                    options.quoting == Quoting::All,
                                    tr!("Quote every value"),
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.change_export_options(
                                            |options| {
                                                options.quoting = match options.quoting {
                                                    Quoting::All => Quoting::Needed,
                                                    Quoting::Needed => Quoting::All,
                                                };
                                            },
                                            cx,
                                        );
                                    },
                                )),
                            )
                            .child(
                                toggle("export-bom", options.bom, tr!("Byte order mark (BOM)"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.change_export_options(
                                            |options| options.bom = !options.bom,
                                            cx,
                                        );
                                    })),
                            )
                    })
                    .when(options.format.has_optional_header(), |s| {
                        s.child(
                            toggle("export-header", options.header, tr!("Column names first"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.change_export_options(
                                        |options| options.header = !options.header,
                                        cx,
                                    );
                                })),
                        )
                    })
                    .child(divider())
                    .child(div().flex_1().min_w(px(200.)).child(label(tab.map_or_else(
                        || tr!("Run a query to export its result.").to_owned(),
                        |tab| self.export_source_text(tab),
                    ))))
                    .map(|s| {
                        if picking {
                            return s.child(label(tr!("Choose where to save the file…")));
                        }
                        if source == ExportSource::Nothing {
                            return s;
                        }
                        let primary = if again {
                            tr!("Run again and export…")
                        } else if partial {
                            tr!("Export the rows held…")
                        } else {
                            tr!("Export…")
                        };
                        s.child(
                            primary_button("export-start", I::Download, primary).on_click(
                                cx.listener(move |this, _, window, cx| {
                                    this.start_export(!again && partial, window, cx)
                                }),
                            ),
                        )
                        .when(again, |s| {
                            s.child(
                                button("export-held", I::Download, tr!("Export the rows held…"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start_export(true, window, cx)
                                    })),
                            )
                        })
                    })
                    .when(!picking, |s| s.child(close(cx)))
            }
            ExportState::Writing {
                started,
                progress,
                total,
                name,
                ..
            } => {
                let rows = cell_format::count(usize::try_from(progress.rows).unwrap_or(usize::MAX));
                let written = match total {
                    Some(total) => tr!(
                        "{0} of {1} rows",
                        rows,
                        cell_format::count(usize::try_from(*total).unwrap_or(usize::MAX))
                    ),
                    None => tr!("{0} rows", rows),
                };
                let mut text = tr!("Exporting to {0}… {1}", name, written);
                if progress.bytes > 0 {
                    text = tr!("{0} · {1}", text, cell_format::size(progress.bytes));
                }
                let elapsed = started.elapsed();
                if elapsed >= Duration::from_secs(2) {
                    text = tr!(
                        "{0} · {1}",
                        text,
                        cell_format::duration(elapsed.as_millis() as u64)
                    );
                }
                bar()
                    .child(spinning_icon(
                        "export-spinner",
                        I::LoaderCircle,
                        colors().mint,
                    ))
                    .child(
                        div()
                            .id("export-progress")
                            .role(Role::Status)
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(rgb(colors().muted))
                            .child(text),
                    )
                    .child(
                        button("export-cancel", I::X, tr!("Cancel export"))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_export(cx))),
                    )
            }
            ExportState::Cancelling { .. } => bar()
                .child(spinning_icon(
                    "export-spinner",
                    I::LoaderCircle,
                    colors().amber,
                ))
                .child(
                    div()
                        .id("export-progress")
                        .role(Role::Status)
                        .flex_1()
                        .text_size(px(12.))
                        .text_color(rgb(colors().muted))
                        .child(tr!("Cancelling the export…")),
                ),
            ExportState::Ended { feedback, path } => bar()
                .child(
                    div()
                        .id("export-outcome")
                        .role(if feedback.tone == Tone::Error {
                            Role::Alert
                        } else {
                            Role::Status
                        })
                        .flex_1()
                        .min_w_0()
                        .child(feedback_line(feedback)),
                )
                .when_some(path.clone(), |s, path| {
                    s.child(
                        button("export-reveal", I::FolderOpen, tr!("Show in folder"))
                            .on_click(move |_, _, cx| cx.reveal_path(&path)),
                    )
                })
                .child(close(cx)),
        };
        Some(bar)
    }

    fn format_menu(&self, options: ExportOptions, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let current = options.format;
        Button::new("export-format")
            .ghost()
            .small()
            .label(format_name(current))
            .dropdown_caret(true)
            .accessibility_label(tr!("Export format: {0}", format_name(current)))
            .tooltip(tr!("Export format"))
            .dropdown_menu(move |mut menu, _, _| {
                for format in ExportFormat::ALL {
                    let workspace = workspace.clone();
                    menu = menu.item(
                        PopupMenuItem::new(format_name(format))
                            .checked(format == current)
                            .on_click(move |_, _, cx| {
                                let _ = workspace.update(cx, |this, cx| {
                                    this.change_export_options(
                                        |options| options.format = format,
                                        cx,
                                    );
                                });
                            }),
                    );
                }
                menu
            })
    }

    fn separator_menu(&self, options: ExportOptions, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let current = options.separator;
        Button::new("export-separator")
            .ghost()
            .small()
            .label(separator_name(current))
            .dropdown_caret(true)
            .accessibility_label(tr!("Separator: {0}", separator_name(current)))
            .tooltip(tr!("Separator"))
            .dropdown_menu(move |mut menu, _, _| {
                for separator in Separator::ALL {
                    let workspace = workspace.clone();
                    menu = menu.item(
                        PopupMenuItem::new(separator_name(separator))
                            .checked(separator == current)
                            .on_click(move |_, _, cx| {
                                let _ = workspace.update(cx, |this, cx| {
                                    this.change_export_options(
                                        |options| options.separator = separator,
                                        cx,
                                    );
                                });
                            }),
                    );
                }
                menu
            })
    }
}

/// What an export that ended needs to report back.
struct Ended {
    generation: u64,
    /// The lease of the session the export took, if it took one.
    lease: Option<u64>,
    mode: Mode,
    id: ResultId,
    path: PathBuf,
    /// The statement of the result, for Messages when it ran again.
    statement: String,
}
