//! Editing a result's rows (ADR-0039): reading what the result can be
//! written back to, the one confirmation before a write, the write itself
//! in the tab's session, and the statements generated for a database that
//! is not written directly (Databricks).
//!
//! The pending edits and the request they become live in
//! `jykre::grid_edit`; the grid draws them and takes the editing keys. This
//! view asks the session, confirms, writes and says how it went.
use super::super::result_grid::Editing;
use super::*;
use gpui_kit::component::WindowExt as _;
use jykre::grid_edit::{self, ChangeKind, EditSession, EditValue, Origin};
use jykre::parameters::{KindChoice, TabParameter};
use jykre_core::write::{self, ReferenceAction, SideEffect, WriteMode};
use std::time::Instant;

/// Changes the confirmation lists one by one, and statements it shows.
const LISTED_CHANGES: usize = 50;
const LISTED_STATEMENTS: usize = 20;

/// What editing is doing with the tab's session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EditJob {
    /// Reading what a result can be written back to.
    Resolving,
    /// Writing the pending edits.
    Writing,
}

/// What the confirmation of a write shows, kept for its rebuilds.
#[derive(Clone)]
struct WritePreview {
    table: String,
    changes: usize,
    lines: Vec<(String, Vec<String>)>,
    statements: Vec<String>,
    commit: String,
    side_effects: Vec<String>,
    production: bool,
    connection: String,
}

/// A value as the confirmation shows it: NULL as NULL, apart from the text
/// `NULL`, which is quoted like every text.
fn shown_value(value: Option<&str>) -> String {
    match value {
        None => "NULL".to_owned(),
        Some(text) => {
            let mut short: String = text.chars().take(80).collect();
            if short.len() < text.len() {
                short.push('…');
            }
            format!("“{}”", short.replace(['\n', '\r'], "↵"))
        }
    }
}

fn edit_value(value: &EditValue) -> String {
    shown_value(value.text())
}

impl Workspace {
    /// Changed rows waiting to be written in this tab's results: closing the
    /// tab, or a run that replaces their result, loses them.
    pub fn pending_edits(&self, cx: &App) -> usize {
        self.grid.read(cx).pending_edits(None)
    }

    /// The grid asked to edit result tab `id`, whose table is not known
    /// yet. A driver that cannot tell keeps it read-only; otherwise the
    /// tab's session is asked, after saying so when that closes the rest
    /// of the result.
    pub(super) fn edit_requested(
        &mut self,
        id: ResultId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let forget = |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            this.grid
                .update(cx, |grid, cx| grid.set_editing(id, None, window, cx));
        };
        let Some(tab) = self.results.get(id) else {
            forget(self, window, cx);
            return;
        };
        let writes = self.input.as_ref().is_some_and(|input| {
            builtin_registry()
                .and_then(|registry| registry.get(input.config.driver))
                .is_ok_and(|driver| driver.descriptor().capabilities.row_writes)
        });
        if !writes {
            let reason = write::ReadOnlyReason::DriverCannotTell;
            self.grid.update(cx, |grid, cx| {
                grid.set_editing(id, Some(Editing::ReadOnly(reason)), window, cx)
            });
            return;
        }
        if self.is_busy() {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("The session is busy. Edit when the query or the page read ends."),
            ));
            forget(self, window, cx);
            cx.notify();
            return;
        }
        // The session still keeps the rest of the result open for paging.
        if tab.more_rows() && !tab.rest_closed() {
            let one = tab.rows_read() == 1;
            let rows = cell_format::count(tab.rows_read());
            let workspace = cx.entity().downgrade();
            let cancelled = workspace.clone();
            window.open_alert_dialog(cx, move |alert, _, _| {
                let workspace = workspace.clone();
                let cancelled = cancelled.clone();
                alert
                    .title(tr!("Edit the rows read so far?"))
                    .description(if one {
                        tr!(
                            "Editing reads the table's key from the database in this tab's session, which closes the rest of this result: only the 1 row read so far can be edited. Run the query again to read the others."
                        )
                        .to_owned()
                    } else {
                        tr!(
                            "Editing reads the table's key from the database in this tab's session, which closes the rest of this result: only the {0} rows read so far can be edited. Run the query again to read the others.",
                            rows
                        )
                    })
                    .confirm()
                    .ok_text(tr!("Edit"))
                    .cancel_text(tr!("Cancel"))
                    .width(px(460.))
                    .on_ok(move |_, window, cx| {
                        let _ = workspace.update(cx, |this, cx| {
                            this.read_edit_target(id, window, cx)
                        });
                        true
                    })
                    .on_cancel(move |_, window, cx| {
                        let _ = cancelled.update(cx, |this, cx| {
                            this.grid
                                .update(cx, |grid, cx| grid.set_editing(id, None, window, cx));
                        });
                        true
                    })
            });
            return;
        }
        self.read_edit_target(id, window, cx);
    }

    /// Asks the tab's session what result tab `id` can be written back to.
    fn read_edit_target(&mut self, id: ResultId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.results.get(id) else {
            return;
        };
        let statement = tab.sql.clone();
        let columns: Vec<ColumnInfo> = tab
            .columns()
            .iter()
            .map(jykre::result::ResultColumn::info)
            .collect();
        let binary = combined_binary(tab);
        let (session, lease) = self.lease_session(false);
        let Some(session) = session else {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!(
                    "The session that read this result is closed. Run the query again to edit its rows."
                ),
            ));
            self.grid
                .update(cx, |grid, cx| grid.set_editing(id, None, window, cx));
            cx.notify();
            return;
        };
        // Asking closes a result still open for paging.
        self.results.rest_exported(id);
        self.edit_job = Some(EditJob::Resolving);
        self.cancellation = CancellationToken::new();
        let cancel = self.cancellation.clone();
        cx.notify();
        cx.spawn_in(window, async move |weak, cx| {
            let run = backend::run(async move {
                Ok(backend::edit_target(session, statement, columns, cancel).await)
            })
            .await;
            let mut pending = Some(run);
            let _ = weak.update_in(cx, |this, window, cx| {
                let Some(run) = pending.take() else {
                    return;
                };
                this.edit_job = None;
                let target = match run {
                    Ok(run) => {
                        this.return_session(lease, run.session);
                        run.target
                    }
                    Err(error) => Err(error),
                };
                let editing = match target {
                    Ok(write::EditTarget::Editable(table)) => Some(Editing::Session(
                        std::sync::Arc::new(EditSession::new(table, binary)),
                    )),
                    Ok(write::EditTarget::ReadOnly(reason)) => Some(Editing::ReadOnly(reason)),
                    Ok(_) => None,
                    Err(error) => {
                        this.notice = Some(Feedback::new(
                            Tone::Error,
                            tr!(
                                "Could not read what this result can be written back to: {0}",
                                error
                            ),
                        ));
                        None
                    }
                };
                this.grid
                    .update(cx, |grid, cx| grid.set_editing(id, editing, window, cx));
                this.show_active(cx);
            });
            if let Some(Ok(run)) = pending
                && let Some(session) = run.session
            {
                backend::close_in_background(session);
            }
        })
        .detach();
    }

    /// The pending edits of the result tab on screen, when it has some.
    fn active_session<'a>(&self, cx: &'a App) -> Option<(ResultId, &'a EditSession)> {
        let id = self.results.active()?.id;
        match self.grid.read(cx).editing(id)? {
            Editing::Session(session) => Some((id, session)),
            _ => None,
        }
    }

    /// The driver of the tab's connection, for quoting the statements shown.
    fn edit_driver(&self) -> Option<std::sync::Arc<dyn Driver>> {
        let input = self.input.as_ref()?;
        builtin_registry()
            .and_then(|registry| registry.get(input.config.driver))
            .ok()
    }

    /// **Write…**: one confirmation listing what will be written, then the
    /// write; a database written through statements gets them in a new tab.
    pub(super) fn write_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("The session is busy. Write when the query or the page read ends."),
            ));
            cx.notify();
            return;
        }
        let Some((id, session)) = self.active_session(cx) else {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("There are no edits to write in this result."),
            ));
            cx.notify();
            return;
        };
        let request = session.request(self.timeout);
        let (request, origins) = match request {
            Ok(request) => request,
            Err(refused) => {
                self.notice = Some(Feedback::new(Tone::Info, grid_edit::refused_text(&refused)));
                cx.notify();
                return;
            }
        };
        if request.target.writes == WriteMode::Script {
            self.generate_edits(id, request, cx);
            return;
        }
        let Some(driver) = self.edit_driver() else {
            return;
        };
        let table = driver.relation_name(&request.target.table);
        let quote = |name: &str| driver.quote_identifier(name);
        let lines = session
            .lines(LISTED_CHANGES)
            .into_iter()
            .map(|line| {
                let key = line
                    .key
                    .iter()
                    .map(|(name, value)| format!("{name} = {}", shown_value(value.as_deref())))
                    .collect::<Vec<_>>()
                    .join(", ");
                let title = match line.kind {
                    ChangeKind::Update => tr!("Change the row where {0}", key),
                    ChangeKind::Delete => tr!("Delete the row where {0}", key),
                    ChangeKind::Insert => tr!("Insert a row").to_owned(),
                };
                let cells = line
                    .cells
                    .iter()
                    .map(|(name, before, after)| match before {
                        Some(before) => format!(
                            "{name}: {} → {}",
                            shown_value(before.as_deref()),
                            edit_value(after)
                        ),
                        None => format!("{name}: {}", edit_value(after)),
                    })
                    .collect();
                (title, cells)
            })
            .collect();
        let statements = request
            .changes
            .iter()
            .take(LISTED_STATEMENTS)
            .filter_map(|change| write::preview(&table, &request.target, change, &quote).ok())
            .collect();
        let commit = if self.transaction.is_open() {
            tr!(
                "A transaction is open in this tab: the changes stay in it, not committed, until you commit or roll it back in the toolbar."
            )
        } else {
            tr!(
                "They are written in one transaction and committed only if every change affects exactly one row; otherwise nothing is written."
            )
        }
        .to_owned();
        let side_effects = request
            .target
            .side_effects
            .iter()
            .map(|effect| match effect {
                SideEffect::Trigger { name } => tr!("Trigger {0} runs on this table.", name),
                SideEffect::Reference {
                    table,
                    name,
                    on_delete,
                    on_update,
                } => {
                    let action = |action: &Option<ReferenceAction>| match action {
                        Some(ReferenceAction::Cascade) => Some(tr!("cascades")),
                        Some(ReferenceAction::SetNull) => Some(tr!("sets NULL")),
                        Some(ReferenceAction::SetDefault) => Some(tr!("sets the default")),
                        None => None,
                    };
                    match (action(on_delete), action(on_update)) {
                        (Some(delete), Some(update)) => tr!(
                            "Foreign key {0} of {1}: on delete it {2}, on a key change it {3}.",
                            name,
                            table,
                            delete,
                            update
                        ),
                        (Some(delete), None) => tr!(
                            "Foreign key {0} of {1}: on delete it {2}.",
                            name,
                            table,
                            delete
                        ),
                        (None, Some(update)) => tr!(
                            "Foreign key {0} of {1}: on a key change it {2}.",
                            name,
                            table,
                            update
                        ),
                        (None, None) => String::new(),
                    }
                }
                _ => String::new(),
            })
            .filter(|line| !line.is_empty())
            .collect();
        let preview = WritePreview {
            table,
            changes: request.changes.len(),
            lines,
            statements,
            commit,
            side_effects,
            production: self.environment
                == Some(jykre::connection_appearance::Environment::Production),
            connection: self.connection.to_string(),
        };
        let workspace = cx.entity().downgrade();
        let request = std::rc::Rc::new(std::cell::RefCell::new(Some((request, origins))));
        window.open_alert_dialog(cx, move |alert, _, _| {
            let workspace = workspace.clone();
            let request = request.clone();
            alert
                .title(if preview.changes == 1 {
                    tr!("Write 1 change to {0}?", preview.table)
                } else {
                    tr!(
                        "Write {0} changes to {1}?",
                        cell_format::count(preview.changes),
                        preview.table
                    )
                })
                .description(write_preview(&preview))
                .confirm()
                .ok_text(if preview.production {
                    tr!("Write to production")
                } else {
                    tr!("Write")
                })
                .cancel_text(tr!("Cancel"))
                .width(px(620.))
                .on_ok(move |_, _, cx| {
                    if let Some((request, origins)) = request.borrow_mut().take() {
                        let _ = workspace
                            .update(cx, |this, cx| this.start_write(id, request, origins, cx));
                    }
                    true
                })
        });
    }

    /// Writes `request` in the tab's session, as a run of the tab: it takes
    /// the session, answers Stop, and is logged in Messages with its counts.
    fn start_write(
        &mut self,
        id: ResultId,
        request: write::WriteRequest,
        origins: Vec<Origin>,
        cx: &mut Context<Self>,
    ) {
        if self.is_busy() {
            return;
        }
        let (session, lease) = self.lease_session(false);
        let Some(session) = session else {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("The session that read this result is closed. Run the query again, then edit."),
            ));
            cx.notify();
            return;
        };
        let table = self
            .edit_driver()
            .map(|driver| driver.relation_name(&request.target.table))
            .unwrap_or_default();
        let changes = request.changes.len();
        let label = if changes == 1 {
            tr!("Write to {0}: 1 change", table)
        } else {
            tr!(
                "Write to {0}: {1} changes",
                table,
                cell_format::count(changes)
            )
        };
        self.edit_job = Some(EditJob::Writing);
        self.cancellation = CancellationToken::new();
        let cancel = self.cancellation.clone();
        self.notice = Some(Feedback::new(
            Tone::Info,
            if changes == 1 {
                tr!("Writing 1 change to {0}…", table)
            } else {
                tr!(
                    "Writing {0} changes to {1}…",
                    cell_format::count(changes),
                    table
                )
            },
        ));
        self.start_clock(cx);
        let started = Instant::now();
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let run = backend::run(async move {
                Ok(backend::write(session, request, cancel).await)
            })
            .await;
            let mut pending = Some(run);
            let _ = weak.update(cx, |this, cx| {
                let Some(run) = pending.take() else {
                    return;
                };
                this.edit_job = None;
                let elapsed = Some(started.elapsed().as_millis());
                let outcome = match run {
                    Ok(run) => {
                        if this.session_lease == Some(lease) {
                            this.transaction = this.transaction.reported(run.transaction);
                        }
                        this.return_session(lease, run.session);
                        if this.session.is_none() && this.session_lease == Some(lease) {
                            this.transaction = Transaction::AutoCommit;
                        }
                        run.outcome
                    }
                    Err(error) => Err(error),
                };
                match outcome {
                    Ok(write::WriteOutcome::Written { rows, committed }) => {
                        this.grid
                            .update(cx, |grid, cx| grid.update_session(id, EditSession::written, cx));
                        this.ran(&label, LogOutcome::Affected(rows), elapsed, false, cx);
                        this.notice = Some(Feedback::new(
                            Tone::Success,
                            match (committed, rows == 1) {
                                (true, true) => tr!(
                                    "Wrote 1 row to {0}. The grid shows what was written; ↻ reads the row back as the database stored it.",
                                    table
                                ),
                                (true, false) => tr!(
                                    "Wrote {0} rows to {1}. The grid shows what was written; ↻ reads the rows back as the database stored them.",
                                    cell_format::count(rows as usize),
                                    table
                                ),
                                (false, true) => tr!(
                                    "Wrote 1 row to {0} in the open transaction. It is not committed: commit or roll back in the toolbar.",
                                    table
                                ),
                                (false, false) => tr!(
                                    "Wrote {0} rows to {1} in the open transaction. They are not committed: commit or roll back in the toolbar.",
                                    cell_format::count(rows as usize),
                                    table
                                ),
                            },
                        ));
                    }
                    Ok(write::WriteOutcome::RolledBack { index, matched }) => {
                        this.grid.update(cx, |grid, cx| {
                            grid.update_session(id, |session| session.rolled_back(&origins, index), cx)
                        });
                        let text = tr!(
                            "Change {0} affected {1} rows instead of one, so nothing was written. Its row is marked; read the rows again (↻) to see the table as it is.",
                            index + 1,
                            cell_format::count(matched as usize)
                        );
                        this.ran(&label, LogOutcome::Failed(text.clone()), elapsed, false, cx);
                        this.notice = Some(Feedback::new(Tone::Error, text));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let text = if error.code == ErrorCode::Cancelled {
                            tr!("The write was stopped. Nothing was written.").to_owned()
                        } else if error.code == ErrorCode::Connection {
                            error.message.clone()
                        } else {
                            tr!("Nothing was written: {0}", error)
                        };
                        let outcome = if error.code == ErrorCode::Cancelled {
                            LogOutcome::Cancelled
                        } else {
                            LogOutcome::Failed(text.clone())
                        };
                        this.ran(&label, outcome, elapsed, false, cx);
                        this.notice = Some(Feedback::new(Tone::Error, text));
                    }
                }
                cx.notify();
            });
            if let Some(Ok(run)) = pending
                && let Some(session) = run.session
            {
                backend::close_in_background(session);
            }
        })
        .detach();
    }

    /// The statements that write the edits, for a database that is not
    /// written directly (ADR-0039, section 9): a new query tab on the same
    /// connection and context, with the values in its parameter fields.
    fn generate_edits(
        &mut self,
        id: ResultId,
        request: write::WriteRequest,
        cx: &mut Context<Self>,
    ) {
        let script = match &self.session {
            Some(session) => session.write_script(&request),
            None => {
                self.notice = Some(Feedback::new(
                    Tone::Info,
                    tr!(
                        "The session that read this result is closed. Run the query again, then edit."
                    ),
                ));
                cx.notify();
                return;
            }
        };
        let script = match script {
            Ok(script) => script,
            Err(error) => {
                self.notice = Some(Feedback::new(
                    Tone::Error,
                    tr!("The statements could not be generated: {0}", error),
                ));
                cx.notify();
                return;
            }
        };
        let table = self
            .edit_driver()
            .map(|driver| driver.relation_name(&request.target.table))
            .unwrap_or_default();
        let key = request
            .target
            .key
            .iter()
            .filter_map(|column| request.target.column(*column))
            .map(|column| column.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let header = [
            if script.statements.len() == 1 {
                tr!(
                    "{0}: 1 change generated from a result, to review and run.",
                    table
                )
            } else {
                tr!(
                    "{0}: {1} changes generated from a result, to review and run.",
                    table,
                    cell_format::count(script.statements.len())
                )
            },
            tr!(
                "Each statement is its own commit: there is no transaction around them, so if one fails the statements before it stay written."
            )
            .to_owned(),
            tr!(
                "Nothing checks that a statement changes exactly one row. Rows are found by {0}; a change made meanwhile to the same row is overwritten.",
                key
            ),
            tr!("The values are in the parameter fields above; none is written into the SQL.")
                .to_owned(),
        ];
        let mut sql = header
            .iter()
            .map(|line| format!("-- {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        sql.push_str("\n\n");
        sql.push_str(&script.statements.join("\n"));
        sql.push('\n');
        let parameters = script
            .parameters
            .into_iter()
            .map(|parameter| TabParameter {
                name: parameter.name,
                kind: KindChoice::Fixed(parameter.kind),
                null: parameter.value.is_none(),
                text: parameter.value.unwrap_or_default(),
            })
            .collect();
        cx.emit(WorkspaceEvent::OpenGenerated {
            title: tr!("Changes to {0}", table),
            connection: self.connection.to_string(),
            sql,
            parameters,
            context: self.context.clone(),
        });
        self.grid.update(cx, |grid, cx| {
            grid.update_session(id, EditSession::discard, cx)
        });
        self.notice = Some(Feedback::new(
            Tone::Success,
            tr!(
                "The statements are in a new tab, with their values in its parameter fields. Review them and run them there."
            ),
        ));
        cx.notify();
    }

    /// A tab opened with generated statements: the parameter fields take
    /// their values, and names resolve where the result was read.
    pub fn open_generated(
        &mut self,
        parameters: &[TabParameter],
        context: SessionContext,
        cx: &mut Context<Self>,
    ) {
        self.restore_parameters(parameters);
        self.context = context.clone();
        self.sql_editor
            .update(cx, |editor, _| editor.set_context(context));
        self.push_assistant_context(cx);
        cx.emit(WorkspaceEvent::Changed);
        cx.notify();
    }

    /// **Discard**: drops the pending edits of the result on screen, asking
    /// first.
    pub(super) fn discard_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, session)) = self.active_session(cx) else {
            return;
        };
        let pending = session.pending();
        if pending == 0 {
            return;
        }
        let workspace = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let workspace = workspace.clone();
            alert
                .title(if pending == 1 {
                    tr!("Discard 1 pending change?").to_owned()
                } else {
                    tr!("Discard {0} pending changes?", cell_format::count(pending))
                })
                .description(tr!("Nothing was written; the edits are lost."))
                .confirm()
                .ok_text(tr!("Discard"))
                .cancel_text(tr!("Keep editing"))
                .width(px(440.))
                .on_ok(move |_, _, cx| {
                    let _ = workspace.update(cx, |this, cx| {
                        this.grid.update(cx, |grid, cx| {
                            grid.update_session(id, EditSession::discard, cx)
                        });
                    });
                    true
                })
        });
    }

    /// Names the columns selected in the grid as the key, for a table that
    /// declares none.
    pub(super) fn choose_key(&mut self, cx: &mut Context<Self>) {
        let Some((id, _)) = self.active_session(cx) else {
            return;
        };
        let columns = self.grid.read(cx).selected_columns();
        let mut refused = None;
        self.grid.update(cx, |grid, cx| {
            grid.update_session(id, |session| refused = session.set_key(columns).err(), cx)
        });
        self.notice = Some(match refused {
            Some(refused) => Feedback::new(Tone::Info, grid_edit::refused_text(&refused)),
            None => Feedback::new(
                Tone::Success,
                tr!("The selected columns identify the rows. Edit the cells now."),
            ),
        });
        cx.notify();
    }

    /// Runs what waited for the user to accept losing pending edits.
    pub fn confirm_pending_edits(&mut self, cx: &mut Context<Self>) {
        if let Some((scope, placement)) = self.edits_run.take() {
            self.edits_confirmed = true;
            self.execute_queries(scope, placement, cx);
            self.edits_confirmed = false;
        }
    }

    /// The bar under the results' toolbar while the result on screen is
    /// being edited, or why it cannot be.
    pub(super) fn edit_bar(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let id = self.results.active()?.id;
        let bar = || {
            row()
                .id("edit-bar")
                .role(Role::Status)
                .flex_none()
                .min_h(px(36.))
                .px_3()
                .py_1()
                .gap_2()
                .border_b_1()
                .border_color(rgb(colors().border))
                .bg(rgb(colors().surface))
        };
        match self.grid.read(cx).editing(id)? {
            Editing::Resolving => Some(
                bar()
                    .child(spinning_icon(
                        "edit-resolving",
                        I::LoaderCircle,
                        colors().amber,
                    ))
                    .child(label(tr!(
                        "Reading what this result can be written back to…"
                    ))),
            ),
            Editing::ReadOnly(reason) => Some(
                bar().child(icon(I::Lock, colors().faint)).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(label(grid_edit::read_only_text(reason))),
                ),
            ),
            Editing::Session(session) => {
                let target = session.target();
                let table = self
                    .edit_driver()
                    .map(|driver| driver.relation_name(&target.table))
                    .unwrap_or_default();
                let pending = session.pending();
                let script = target.writes == WriteMode::Script;
                let writing = self.edit_job == Some(EditJob::Writing);
                let status = if session.needs_key() {
                    tr!(
                        "Editing {0}. It declares no key: select the cells of the columns that identify a row, then choose them as the key.",
                        table
                    )
                } else if pending == 0 {
                    tr!("Editing {0}. No pending changes.", table)
                } else if pending == 1 {
                    tr!("Editing {0}. 1 pending change, not written.", table)
                } else {
                    tr!(
                        "Editing {0}. {1} pending changes, not written.",
                        table,
                        cell_format::count(pending)
                    )
                };
                let key = |action| super::super::shortcuts::label_of(action, cx);
                Some(
                    bar()
                        .child(icon(I::Pencil, colors().amber))
                        .child(div().flex_1().min_w_0().truncate().child(label(status)))
                        .when(session.needs_key(), |s| {
                            s.child(
                                button("edit-choose-key", I::KeyRound, tr!("Use as key"))
                                    .on_click(cx.listener(|this, _, _, cx| this.choose_key(cx))),
                            )
                        })
                        .child(
                            icon_button(
                                "edit-insert-row",
                                I::Plus,
                                tr!("Insert a row ({0})", key(<super::super::shortcuts::GridInsertRow as Action>::name_for_type())),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.grid.update(cx, |grid, cx| grid.insert_row(window, cx));
                            })),
                        )
                        .child(
                            icon_button(
                                "edit-set-null",
                                I::CircleSlash,
                                tr!("Set the selected cells to NULL ({0})", key(<super::super::shortcuts::GridSetNull as Action>::name_for_type())),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.grid.update(cx, |grid, cx| grid.set_null(window, cx));
                            })),
                        )
                        .child(
                            icon_button(
                                "edit-delete-rows",
                                I::Trash,
                                tr!("Delete the selected rows, or keep them again ({0})", key(<super::super::shortcuts::GridDeleteRows as Action>::name_for_type())),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.grid.update(cx, |grid, cx| grid.delete_rows(window, cx));
                            })),
                        )
                        .child(divider())
                        .child(
                            button("edit-discard", I::Undo2, tr!("Discard"))
                                .when(pending == 0 || writing, |s| s.opacity(0.5))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.discard_edits(window, cx)
                                })),
                        )
                        .map(|s| {
                            if writing {
                                s.child(
                                    button("edit-stop", I::Square, tr!("Stop"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.stop_execution(cx)
                                        })),
                                )
                            } else {
                                s.child(
                                    button(
                                        "edit-write",
                                        if script { I::FileCode } else { I::Save },
                                        if script {
                                            tr!("Generate SQL…")
                                        } else {
                                            tr!("Write…")
                                        },
                                    )
                                    .when(pending == 0, |s| s.opacity(0.5))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.write_edits(window, cx)
                                    })),
                                )
                            }
                        }),
                )
            }
        }
    }
}

/// How each column's cells arrived across the pages a tab holds: mixed on
/// any page is mixed; bytes on any page are bytes.
fn combined_binary(tab: &ResultTab) -> Vec<Option<bool>> {
    let pages = tab.held_pages();
    let columns = tab.columns().len();
    (0..columns)
        .map(|column| {
            let flags: Vec<Option<bool>> = pages
                .iter()
                .filter_map(|page| page.columns().get(column).map(|column| column.binary))
                .collect();
            if flags.contains(&None) {
                None
            } else {
                Some(flags.contains(&Some(true)))
            }
        })
        .collect()
}

/// What the confirmation of a write lists: the production warning, the
/// commit, each change, the statements with their placeholders, what else
/// the write can change, and that the last write wins.
fn write_preview(preview: &WritePreview) -> Stateful<Div> {
    let heading = |text: String| {
        div()
            .pt_2()
            .text_size(px(12.))
            .text_color(rgb(colors().muted))
            .child(text)
    };
    let mono = |text: String| {
        div()
            .font_family(fonts().mono.clone())
            .text_size(px(12.))
            .text_color(rgb(colors().text))
            .child(text)
    };
    let mut body = column()
        .id("write-preview")
        .max_h(px(380.))
        .overflow_y_scroll()
        .gap_1();
    if preview.production {
        body = body.child(
            row()
                .gap_2()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(colors().red))
                .child(icon(I::ShieldAlert, colors().red))
                .child(label(tr!(
                    "{0} is marked Production: these rows change in production.",
                    preview.connection
                ))),
        );
    }
    body = body.child(label(preview.commit.clone()));
    body = body.child(heading(tr!("Changes").to_owned()));
    for (title, cells) in &preview.lines {
        body = body.child(label(title.clone()));
        for cell in cells {
            body = body.child(div().pl_3().child(mono(cell.clone())));
        }
    }
    let more = preview.changes.saturating_sub(preview.lines.len());
    if more > 0 {
        body = body.child(label(if more == 1 {
            tr!("And 1 more change.").to_owned()
        } else {
            tr!("And {0} more changes.", cell_format::count(more))
        }));
    }
    body = body.child(heading(
        tr!("Statements, with the values above bound apart from the SQL").to_owned(),
    ));
    for statement in &preview.statements {
        body = body.child(mono(statement.clone()));
    }
    if preview.changes > preview.statements.len() {
        let more = preview.changes - preview.statements.len();
        body = body.child(label(if more == 1 {
            tr!("And 1 more statement of the same shapes.").to_owned()
        } else {
            tr!(
                "And {0} more statements of the same shapes.",
                cell_format::count(more)
            )
        }));
    }
    if !preview.side_effects.is_empty() {
        body = body.child(heading(tr!("What else the write can change").to_owned()));
        for effect in &preview.side_effects {
            body = body.child(label(effect.clone()));
        }
    }
    body.child(heading(tr!("Concurrent changes").to_owned())).child(label(tr!(
        "Rows are found by their key only: a change someone else made meanwhile to the same row is overwritten."
    )))
}
