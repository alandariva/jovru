use super::assistant_panel::{
    AssistantPanel, AssistantPanelEvent, FixTarget, Mode as AssistantMode, QueryTicket,
};
use super::demo::{self, SALES_SQL, sample_result};
use super::layout::{ASSISTANT_WIDTH, EDITOR_MIN_WIDTH, splitter};
use super::result_grid::{GridSql, ResultGrid, ResultGridEvent, Summary};
use super::sql_editor::{NameIndex, SqlCatalogs, SqlEditor, SqlEditorEvent};
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::{
    ActiveTheme, Sizable,
    button::{Button, ButtonVariants},
    menu::{DropdownMenu as _, PopupMenuItem},
    tooltip::Tooltip,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::tr;
use jykre::{
    assistant::{
        apply,
        facts::{ErrorFacts, RunFacts, SchemaScope},
        prompts,
        released::{self, QueryPreview},
    },
    backend, cell_format, connection_store,
    drivers::*,
    error_report::ErrorReport,
    query::{Outcome, QueryRun, RunState, Scope, ShortcutAction, StatementStatus},
    result::{DEFAULT_MEMORY_CEILING, DEFAULT_ROW_LIMIT, ResultData, ResultLimit},
    result_tabs::{Completed, DataView, Placement, ResultId, ResultState, ResultTab, ResultTabs},
    run_log::{LogEntry, LogOutcome, RunLog},
    transaction::Transaction,
};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

mod copy_menu;
mod export;
mod parameters;
use export::{ExportBar, ExportState};
use parameters::ParameterForm;

/// Key context of the SQL editor, for the run shortcuts.
pub const SQL_EDITOR_CONTEXT: &str = "SqlEditor";

#[derive(Clone, Copy, PartialEq)]
enum ResultView {
    Table,
    Messages,
}

pub struct Workspace {
    input: Option<ResolvedConnection>,
    /// The application runs the demo: a tab without a connection runs on
    /// sample data. Otherwise such a tab asks for a connection.
    demo: bool,
    /// The connection's settings changed while the session was open: the
    /// next run closes it and opens one with the new settings.
    stale_session: bool,
    /// The driver runs transactions, so the toolbar shows their state.
    transactions: bool,
    /// The transaction state of the session, from the statements it ran.
    transaction: Transaction,
    /// What ran in this tab, shown in Messages; it outlives result tabs.
    log: RunLog,
    log_scroll: UniformListScrollHandle,
    /// The position in the running plan of the statement on the server.
    started_position: usize,
    /// A run was asked for without a connection: the results area offers
    /// the connections to choose from.
    choosing_connection: bool,
    /// A run waiting for the password of the connection.
    pending_run: Option<(Scope, Placement)>,
    /// A run on a production connection that waits for the user to confirm
    /// its risky statements.
    risky_run: Option<(Scope, Placement)>,
    /// The waiting risky run was confirmed: the next start skips the check.
    risk_confirmed: bool,
    /// The environment of the tab's connection; production asks before
    /// risky statements.
    environment: Option<jykre::connection_appearance::Environment>,
    /// The tab's session, kept between runs so `USE`, `SET`, temporary
    /// tables and transactions carry over. Out with the run while one is
    /// running; `None` until the first run or after it was lost.
    session: Option<Box<dyn Session>>,
    /// The lease of the run or page read that has the session out. A session
    /// that comes back under any other lease (a superseded run, or one begun
    /// on another connection) is closed.
    session_lease: Option<u64>,
    next_lease: u64,
    /// The next page of a result is being read; runs wait for it.
    fetching: bool,
    /// The export bar and the export it runs ([Results](../../docs/results.md#export)).
    export: ExportBar,
    /// The fields of the SQL's `:name` parameters, above the editor.
    parameters: ParameterForm,
    cancellation: CancellationToken,
    numbers_pt_br: bool,
    editor: Entity<EditorState>,
    assistant_split: Entity<ResizableState>,
    query_split: Entity<ResizableState>,
    /// The assistant, its own cached entity (ADR-0028).
    assistant_panel: Entity<AssistantPanel>,
    /// The assistant's own session, for the queries the user lets it run:
    /// apart from the tab's, so they never touch its transaction, temporary
    /// tables or `USE`.
    assistant_session: Option<Box<dyn Session>>,
    /// Bumped when the connection changes: a session coming back from the
    /// previous one is closed.
    assistant_lease: u64,
    /// The assistant's query waiting for the connection's password.
    pending_assistant: Option<AssistantQuery>,
    connection: SharedString,
    dialect: SharedString,
    database: String,
    /// The hue of the tab's connection; `None` without one.
    connection_color: Option<jykre::connection_appearance::ConnectionColor>,
    title: SharedString,
    /// One result tab per statement run, each read page by page.
    results: ResultTabs,
    /// Rows kept per statement, from the connection.
    row_limit: u32,
    /// How long one statement may run; `None` waits for it as long as it
    /// takes.
    timeout: Option<Duration>,
    /// The connections this tab can switch to, by name.
    connection_choices: Vec<String>,
    /// Where unqualified names resolve in this tab: chosen in the header,
    /// then as the session reports it after each run. Applied to the
    /// session only when it changes, and again to a session that reopened.
    context: SessionContext,
    /// The namespaces the header offers, from the connection's cache.
    context_choices: Vec<SessionContext>,
    /// Highlighting, completion, hover, go to definition and Format: the
    /// editor's language features, told what they need by the tab.
    sql_editor: Entity<SqlEditor>,
    /// The result grid, its own entity ([ADR-0012](../../docs/adr/0012-result-grid-as-a-cached-entity.md)).
    grid: Entity<ResultGrid>,
    /// Visible cells selected in the grid, as it last reported.
    selected_cells: usize,
    /// The statistics of the selected cells, as the grid last reported.
    selection_summary: Summary,
    result_view: ResultView,
    assistant_open: bool,
    /// The connection whose metadata cache the assistant's tools read.
    cache_id: Option<ConnectionId>,
    /// The run state and the status of each statement in the editor.
    run: QueryRun,
    editor_sql: String,
    /// The line where each statement of `editor_sql` starts and its line
    /// count, kept when the SQL changes so a frame never splits the script.
    statement_lines: Rc<[usize]>,
    line_count: usize,
    /// SQL as last saved to the library, if this tab is saved.
    saved_sql: Option<String>,
    /// The saved query this tab edits, by its identity in the library.
    saved_id: Option<u64>,
    /// The table or view the tab was opened for: opening it again shows
    /// this tab.
    object: Option<ObjectPath>,
    /// The statement the tab reads its table's rows with, when it was opened
    /// as the table's Data view: `SELECT * FROM` it, without a `LIMIT`. The
    /// result tab of that statement is called Data.
    data_sql: Option<String>,
    /// Rows the open filter shows on the page, while a filter is open.
    filtered_rows: Option<usize>,
    /// A task redraws the footer every second while a run is active, so it
    /// shows the elapsed time.
    clock: bool,
    /// The SQL the tab opened with; closing loses nothing while the editor
    /// still holds it.
    opened_sql: Option<String>,
    notice: Option<Feedback>,
    filter_open: bool,
    filter: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(
        title: &str,
        connection: &str,
        sql: &str,
        assistant_split: Entity<ResizableState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .placeholder(tr!(
                    "Write SQL here. Ctrl+Enter runs the statement at the cursor."
                ))
                .default_value(sql)
        });
        // Highlighting, completion, hover, go to definition and Format.
        let sql_editor = cx.new(|cx| SqlEditor::new(&editor, cx));
        let sql_editor_subscription = cx.subscribe(
            &sql_editor,
            |this, _, event: &SqlEditorEvent, cx| match event {
                SqlEditorEvent::Reveal(path) => cx.emit(WorkspaceEvent::Reveal {
                    connection: this.connection.to_string(),
                    path: path.clone(),
                }),
                SqlEditorEvent::Notice(notice) => {
                    this.notice = Some(notice.clone());
                    cx.notify();
                }
            },
        );
        let assistant_panel = cx.new(|cx| AssistantPanel::new(demo::enabled(), window, cx));
        if sql == SALES_SQL {
            assistant_panel.update(cx, |panel, cx| {
                panel.seed_demo(
                    tr!("Which categories generated the most revenue last quarter?").into(),
                    tr!("I prepared a query for Q3 2026, considering only completed orders.")
                        .into(),
                    Some(SALES_SQL),
                    cx,
                )
            });
        }
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Filter results…")));
        let assistant_subscription = cx.subscribe_in(
            &assistant_panel,
            window,
            |this, _, event: &AssistantPanelEvent, window, cx| {
                this.on_assistant_event(event, window, cx)
            },
        );
        let filter_subscription = cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) && this.filter_open {
                let filter = this.filter.read(cx).value().to_string();
                this.grid
                    .update(cx, |grid, cx| grid.set_filter(&filter, cx));
            }
        });
        let sales = demo::is_sales(sql);
        let mut results = ResultTabs::new(DEFAULT_MEMORY_CEILING);
        // The demo's sales tab opens with its result and chart.
        if sql == SALES_SQL {
            let id = results.begin(&[sql.to_owned()], Placement::New)[0];
            results.complete(
                id,
                Completed {
                    data: sample_result(sql),
                    sales,
                    ..Completed::default()
                },
            );
        }
        let first = results
            .active()
            .and_then(ResultTab::current)
            .map_or_else(|| Arc::new(ResultData::default()), |page| page.data.clone());
        let grid = cx.new(|cx| ResultGrid::new(first, sales, cx));
        // The grid notifies only itself; the workspace keeps the little it
        // shows: the selected count and notices.
        let grid_subscription =
            cx.subscribe(&grid, |this, _, event: &ResultGridEvent, cx| match event {
                ResultGridEvent::SelectionChanged { cells } => {
                    this.selected_cells = *cells;
                    cx.notify();
                }
                ResultGridEvent::RowsShown { rows } => {
                    this.filtered_rows = *rows;
                    cx.notify();
                }
                ResultGridEvent::Notice(notice) => {
                    this.notice = Some(notice.clone());
                    cx.notify();
                }
                ResultGridEvent::Summary(summary) => {
                    this.selection_summary = summary.clone();
                    cx.notify();
                }
                ResultGridEvent::RecordsShown | ResultGridEvent::PanelChanged => cx.notify(),
            });
        // The copy button names the format Ctrl+C copies in, which any tab's
        // menu can change.
        let copy_observation =
            cx.observe_global::<super::copying::CopyPreferences>(|_, cx| cx.notify());
        let resize_subscription = cx.observe(&assistant_split, |_, _, cx| cx.notify());
        let query_split = cx.new(|_| ResizableState::default());
        let query_resize = cx.observe(&query_split, |_, _, cx| cx.notify());
        // Editing the SQL never discards results, messages or a running
        // execution. Only the gutter status of statements that changed resets.
        let editor_observation = cx.observe(&editor, |this, editor, cx| {
            let sql = editor.read(cx).text().to_string();
            if this.editor_sql != sql {
                this.run.edit(&sql);
                this.statement_lines = jykre::sql::statement_lines(&sql, this.run.dialect()).into();
                this.line_count = sql.split('\n').count();
                this.editor_sql = sql;
                this.sync_parameters();
                cx.emit(WorkspaceEvent::Changed);
            }
            cx.notify();
        });
        let demo = demo::enabled();
        let mut workspace = Self {
            input: None,
            demo,
            stale_session: false,
            transactions: false,
            transaction: Transaction::AutoCommit,
            log: RunLog::default(),
            log_scroll: UniformListScrollHandle::new(),
            started_position: 0,
            choosing_connection: false,
            pending_run: None,
            risky_run: None,
            risk_confirmed: false,
            environment: None,
            session: None,
            session_lease: None,
            next_lease: 0,
            fetching: false,
            export: ExportBar::default(),
            parameters: ParameterForm::default(),
            cancellation: CancellationToken::new(),
            numbers_pt_br: false,
            editor,
            assistant_split,
            query_split,
            assistant_panel,
            filter,
            connection: connection.to_string().into(),
            // The demo guesses its database from the connection's name; a
            // real tab shows only what its connection says.
            dialect: if demo {
                demo::dialect(connection).into()
            } else {
                "—".into()
            },
            database: if !demo {
                "—".into()
            } else if connection.contains("Databricks") {
                "analytics".into()
            } else if connection.contains("MySQL") {
                "finance".into()
            } else {
                "commerce".into()
            },
            connection_color: if !demo {
                None
            } else if connection.contains("Databricks") {
                Some(jykre::connection_appearance::ConnectionColor::Blue)
            } else if connection.contains("MySQL") {
                Some(jykre::connection_appearance::ConnectionColor::Pink)
            } else {
                Some(jykre::connection_appearance::ConnectionColor::Mint)
            },
            title: title.to_string().into(),
            results,
            row_limit: DEFAULT_ROW_LIMIT,
            timeout: ExecutionOptions::default().timeout,
            connection_choices: Vec::new(),
            context: SessionContext::default(),
            context_choices: Vec::new(),
            sql_editor,
            grid,
            selected_cells: 0,
            selection_summary: Summary::default(),
            result_view: ResultView::Table,
            assistant_open: true,
            assistant_session: None,
            assistant_lease: 0,
            pending_assistant: None,
            cache_id: None,
            run: QueryRun::new(sql),
            editor_sql: sql.to_string(),
            statement_lines: jykre::sql::statement_lines(sql, None).into(),
            line_count: sql.split('\n').count(),
            saved_sql: None,
            saved_id: None,
            object: None,
            data_sql: None,
            filtered_rows: None,
            clock: false,
            opened_sql: Some(sql.to_string()),
            notice: None,
            filter_open: false,
            _subscriptions: vec![
                assistant_subscription,
                filter_subscription,
                grid_subscription,
                resize_subscription,
                query_resize,
                editor_observation,
                sql_editor_subscription,
                copy_observation,
            ],
        };
        workspace.sync_parameters();
        workspace
    }

    /// Ctrl+Enter: the selected text when there is a selection, otherwise the
    /// statement at the cursor, in place of the result tab on screen. A run
    /// shortcut never cancels: while a run is active the host asks first
    /// ([`Workspace::shortcut`]).
    pub fn execute_current(&mut self, cx: &mut Context<Self>) {
        let scope = self.current_scope(cx);
        self.execute_queries(scope, Placement::ReplaceActive, cx);
    }

    /// Ctrl+Shift+Enter: as [`Workspace::execute_current`], in a new result
    /// tab.
    pub fn execute_current_in_new_result(&mut self, cx: &mut Context<Self>) {
        let scope = self.current_scope(cx);
        self.execute_queries(scope, Placement::New, cx);
    }

    /// Ctrl+Alt+Enter: every statement in the tab, in order, whatever the
    /// cursor or selection, one result tab each in place of the unpinned
    /// ones.
    pub fn execute_all(&mut self, cx: &mut Context<Self>) {
        self.execute_queries(Scope::All, Placement::ReplaceAll, cx);
    }

    /// What a run shortcut does now: run, ask before cancelling, or wait for
    /// the run that is stopping.
    pub fn shortcut(&self) -> ShortcutAction {
        self.run.shortcut()
    }

    /// Cancels the running run, after the user confirmed it.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.stop_execution(cx);
    }

    /// The selection when there is one, otherwise the statement at the
    /// cursor.
    fn current_scope(&self, cx: &App) -> Scope {
        let editor = self.editor.read(cx);
        let selection = editor.selected_range();
        if selection.is_empty() {
            Scope::Cursor(editor.cursor())
        } else {
            Scope::Selection(selection)
        }
    }

    /// The Run button: it turns into Stop while a run is running, as its
    /// label shows, so a click on Stop cancels.
    fn toggle_current(&mut self, cx: &mut Context<Self>) {
        if self.run.is_running() {
            self.stop_execution(cx);
        } else {
            self.execute_current(cx);
        }
    }

    /// Runs the statement of the result tab on screen again, as it ran, into
    /// that same tab. The editor's cursor plays no part, so a refresh never
    /// runs another statement.
    fn run_again(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.results.active() else {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("Run a query before running it again."),
            ));
            cx.notify();
            return;
        };
        let scope = Scope::Again {
            sql: tab.sql.clone(),
            origin: tab.origin,
        };
        let placement = Placement::Replace(tab.id);
        self.execute_queries(scope, placement, cx);
    }

    /// Ends the session's transaction with `COMMIT` or `ROLLBACK`. The
    /// outcome goes to Messages and the indicator; no result tab opens.
    fn end_transaction(&mut self, command: &str, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let scope = Scope::Again {
            sql: command.to_owned(),
            origin: None,
        };
        self.execute_queries(scope, Placement::Hidden, cx);
    }

    /// The previous run was asked to stop but has not ended: it may still
    /// change data, so nothing new starts until it does.
    fn still_cancelling(&mut self, cx: &mut Context<Self>) -> bool {
        if self.run.state() != &RunState::Cancelling {
            return false;
        }
        self.notice = Some(Feedback::new(Tone::Info, stopping_notice()));
        cx.notify();
        true
    }

    /// Once the stopping run has ended, its notice no longer applies.
    fn clear_stopping_notice(&mut self) {
        if !self.run.is_active()
            && self
                .notice
                .as_ref()
                .is_some_and(|notice| notice.text == stopping_notice())
        {
            self.notice = None;
        }
    }

    /// Asks the run to stop. The footer reports the cancellation; there is
    /// no second message.
    fn stop_execution(&mut self, cx: &mut Context<Self>) {
        if self.run.cancel() {
            self.cancellation.cancel();
            cx.notify();
        }
    }

    /// Records a statement that ended: in Messages and, unless it came from
    /// the toolbar's transaction controls (`history` false), in the history,
    /// whether it succeeded, failed or was cancelled.
    fn ran(
        &mut self,
        sql: &str,
        outcome: LogOutcome,
        elapsed_ms: Option<u128>,
        history: bool,
        cx: &mut Context<Self>,
    ) {
        if history {
            use jykre::workspace_state::RunStatus;
            let (status, detail) = match &outcome {
                LogOutcome::Rows { rows, more: true } => (
                    RunStatus::Succeeded,
                    tr!("{0} rows on the first page", cell_format::count(*rows)),
                ),
                LogOutcome::Rows { rows: 1, .. } => (RunStatus::Succeeded, tr!("1 row").into()),
                LogOutcome::Rows { rows, .. } => (
                    RunStatus::Succeeded,
                    tr!("{0} rows", cell_format::count(*rows)),
                ),
                LogOutcome::Affected(affected) => {
                    (RunStatus::Succeeded, tr!("{0} rows affected", affected))
                }
                LogOutcome::Completed => (RunStatus::Succeeded, tr!("Statement completed").into()),
                LogOutcome::Failed(error) => (RunStatus::Failed, error.clone()),
                LogOutcome::Cancelled => (RunStatus::Cancelled, tr!("Execution cancelled").into()),
            };
            cx.emit(WorkspaceEvent::Executed(
                jykre::workspace_state::QueryEntry::ran(
                    &self.title,
                    &self.connection,
                    sql,
                    status,
                    Some(detail),
                    elapsed_ms.map(|elapsed| elapsed.min(u64::MAX as u128) as u64),
                ),
            ));
        }
        self.log.push(sql, outcome, elapsed_ms);
    }

    /// Redraws the footer every second while a run is active or stopping,
    /// so it shows how long it has taken. The task ends with the run.
    fn start_clock(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.clock, true) {
            return;
        }
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let ticking = weak
                    .update(cx, |this, cx| {
                        if this.run.is_active() || this.is_exporting() {
                            cx.notify();
                            true
                        } else {
                            this.clock = false;
                            false
                        }
                    })
                    .unwrap_or(false);
                if !ticking {
                    break;
                }
            }
        })
        .detach();
    }

    /// The toolbar's Explain: the database's plan for the statement at the
    /// cursor, or for the selected statement, read with the dialect's
    /// `EXPLAIN` into a new result tab. The statement itself does not run.
    fn explain(&mut self, cx: &mut Context<Self>) {
        if self.input.is_none() {
            let text = if self.demo {
                tr!("Explain reads the plan from the database. Connect to a server to see it.")
            } else {
                tr!("Choose a connection for this tab to see the plan of a query.")
            };
            self.notice = Some(Feedback::new(Tone::Info, text));
            cx.notify();
            return;
        }
        let selection = {
            let editor = self.editor.read(cx);
            let selected = editor.selected_range();
            if selected.is_empty() {
                editor.cursor()..editor.cursor()
            } else {
                selected
            }
        };
        match jykre::sql::explain(&self.editor_sql, selection, self.run.dialect()) {
            Ok(sql) => {
                let scope = Scope::Again { sql, origin: None };
                self.execute_queries(scope, Placement::New, cx);
            }
            Err(refused) => {
                use jykre::sql::ExplainRefused;
                let text = match refused {
                    ExplainRefused::NoDialect => {
                        tr!("This connection's dialect does not say how to read a plan.")
                    }
                    ExplainRefused::NoStatement => {
                        tr!("Put the cursor on the statement to explain, or select it.")
                    }
                    ExplainRefused::Several => {
                        tr!("Select one statement to explain, not several.")
                    }
                    ExplainRefused::NotExplainable => tr!(
                        "Explain shows the plan of a query or a data change (SELECT, WITH, INSERT, UPDATE, DELETE, MERGE)."
                    ),
                };
                self.notice = Some(Feedback::new(Tone::Info, text));
                cx.notify();
            }
        }
    }

    /// The save button: saves the SQL in the library, or updates the saved
    /// query this tab edits. It never removes anything; removing is done in
    /// the library's list.
    fn save_query(&mut self, cx: &mut Context<Self>) {
        let sql = self.editor.read(cx).text().to_string();
        if self.saved_id.is_some() && self.saved_sql.as_deref() == Some(sql.as_str()) {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("This query is already saved."),
            ));
            cx.notify();
            return;
        }
        cx.emit(WorkspaceEvent::Saved {
            id: self.saved_id,
            title: self.title.to_string(),
            connection: self.connection.to_string(),
            sql,
        });
    }

    /// The SQL a report takes from this tab: the selection, else the
    /// statement at the cursor, else the whole script. `None` (and a notice)
    /// without any.
    fn report_sql(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let sql = self.editor.read(cx).text().to_string();
        let chosen = self
            .run
            .statements_for(&sql, &self.current_scope(cx))
            .join("\n");
        let sql = if chosen.trim().is_empty() {
            sql
        } else {
            chosen
        };
        if sql.trim().is_empty() {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("Write the SQL the report runs first."),
            ));
            cx.notify();
            return None;
        }
        Some(sql)
    }

    /// **Save as report**: the tab's SQL, in the tab's database, becomes the
    /// first panel of a draft report.
    fn save_as_report(&mut self, cx: &mut Context<Self>) {
        if let Some(sql) = self.report_sql(cx) {
            cx.emit(WorkspaceEvent::SaveAsReport {
                title: self.title.to_string(),
                connection: self.connection.to_string(),
                sql,
                context: self.context.clone(),
            });
        }
    }

    /// **Add to a report**: the tab's SQL, on the tab's connection and in its
    /// database, becomes a new panel of a saved report chosen in a dialog.
    fn add_to_report(&mut self, cx: &mut Context<Self>) {
        if let Some(sql) = self.report_sql(cx) {
            cx.emit(WorkspaceEvent::AddToReport {
                connection: self.connection.to_string(),
                sql,
                context: self.context.clone(),
            });
        }
    }

    /// The library saved this tab's SQL as the query `id`.
    pub fn saved(&mut self, id: u64, sql: String, cx: &mut Context<Self>) {
        let updated = self.saved_id == Some(id);
        self.saved_id = Some(id);
        self.saved_sql = Some(sql);
        self.notice = Some(Feedback::new(
            Tone::Success,
            if updated {
                tr!("Saved query updated.")
            } else {
                tr!("Query saved in the library.")
            },
        ));
        cx.emit(WorkspaceEvent::Changed);
        cx.notify();
    }

    /// Opens the tab as the saved query `id`, whose SQL it holds.
    pub fn open_saved(&mut self, id: u64, sql: &str) {
        self.saved_id = Some(id);
        self.saved_sql = Some(sql.to_owned());
    }

    /// Gives the tab another title, as when its saved query is named or
    /// renamed. The host renames the tab in the strip.
    pub fn set_title(&mut self, title: &str, cx: &mut Context<Self>) {
        if self.title.as_ref() != title {
            self.title = title.to_owned().into();
            cx.emit(WorkspaceEvent::Changed);
            cx.notify();
        }
    }

    /// The saved query this tab edits, if any.
    pub fn saved_id(&self) -> Option<u64> {
        self.saved_id
    }

    /// The saved query `id` was removed from the library: the tab is a
    /// draft again, so closing it asks before losing its SQL.
    pub fn forget_saved(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.saved_id == Some(id) {
            self.saved_id = None;
            self.saved_sql = None;
            cx.emit(WorkspaceEvent::Changed);
            cx.notify();
        }
    }

    /// The toolbar's Statistics button: shows the table with the statistics
    /// panel (`show`), or closes the panel.
    fn show_statistics(&mut self, show: bool, cx: &mut Context<Self>) {
        self.result_view = ResultView::Table;
        self.grid.update(cx, |grid, cx| {
            if grid.statistics_shown() != show {
                grid.toggle_statistics(cx);
            }
        });
        cx.notify();
    }

    /// The gear of the result toolbar: how the grid shows values. Its
    /// options are toggles that leave the menu's button as an icon.
    fn display_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let numbers_pt_br = self.numbers_pt_br;
        Button::new("display-menu")
            .ghost()
            .small()
            .icon(I::Settings)
            .when(numbers_pt_br, |s| s.text_color(rgb(colors().mint)))
            .accessibility_label(tr!("Display settings"))
            .tooltip(tr!("Display settings"))
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                let workspace = workspace.clone();
                menu.item(
                    PopupMenuItem::new(tr!("pt-BR numbers"))
                        .checked(numbers_pt_br)
                        .on_click(move |_, _, cx| {
                            let _ = workspace.update(cx, |this, cx| {
                                this.numbers_pt_br = !this.numbers_pt_br;
                                let numbers_pt_br = this.numbers_pt_br;
                                this.grid.update(cx, |grid, cx| {
                                    grid.set_number_format(numbers_pt_br, cx)
                                });
                                cx.notify();
                            });
                        }),
                )
            })
    }

    /// **Copy this page as CSV**. A copy estimated above 50 MiB asks first
    /// (unless `confirmed`), since building and holding it takes time and
    /// memory ([Results](../../docs/results.md#copying)).
    fn copy_page_csv(&mut self, confirmed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.current_page().filter(|_| self.has_results()) else {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("Run a query before copying the results."),
            ));
            cx.notify();
            return;
        };
        let bytes = jykre::grid::csv_bytes(&page.data);
        if !confirmed && bytes > jykre::grid::COPY_CONFIRM_BYTES {
            let workspace = cx.entity().downgrade();
            super::confirm(
                window,
                cx,
                super::Confirmation {
                    title: tr!("Copy about {0} MB?", bytes / (1024 * 1024)),
                    description: tr!(
                        "The page is large: copying it can take a moment and holds all of it in the clipboard."
                    )
                    .to_string(),
                    ok: tr!("Copy"),
                    cancel: tr!("Cancel"),
                    focus: super::ConfirmFocus::Dialog,
                },
                move |window, cx| {
                    let _ = workspace.update(cx, |this, cx| this.copy_page_csv(true, window, cx));
                },
            );
            return;
        }
        let _timer = jykre::logging::UiTimer::start("copy CSV");
        let csv = cell_format::result_csv(&page.data);
        cx.write_to_clipboard(ClipboardItem::new_string(csv));
        self.notice = Some(Feedback::new(
            Tone::Success,
            tr!("CSV copied. Paste it into a spreadsheet or file."),
        ));
        cx.notify();
    }

    /// The table or view this tab was opened for.
    pub fn object(&self) -> Option<&ObjectPath> {
        self.object.as_ref()
    }

    pub fn set_object(&mut self, object: Option<ObjectPath>, cx: &mut Context<Self>) {
        self.object = object;
        self.update_grid_sql(cx);
    }

    /// The statement the tab reads its table's rows with, when it is the
    /// table's Data view.
    pub fn data_sql(&self) -> Option<&str> {
        self.data_sql.as_deref()
    }

    /// The table's **Data** view: the tab reads its table's rows with `sql`
    /// (every row, without a `LIMIT`), page by page: the first page has the
    /// connection's row limit and the next ones are read from the result the
    /// session keeps open. Opened again, a tab that holds the rows, or is
    /// reading them, shows them; otherwise they are read again.
    pub fn view_data(&mut self, sql: String, cx: &mut Context<Self>) {
        if self.data_sql.as_deref() != Some(sql.as_str()) {
            self.data_sql = Some(sql);
            cx.emit(WorkspaceEvent::Changed);
        }
        self.read_data(cx);
    }

    /// Shows the Data result, or reads it. Only a statement the editor
    /// holds runs, so what reads the rows is always on screen and its
    /// statement's status shows in the gutter.
    fn read_data(&mut self, cx: &mut Context<Self>) {
        let Some(sql) = self.data_sql.clone() else {
            return;
        };
        self.result_view = ResultView::Table;
        let placement = match self.results.data_view(&sql) {
            DataView::Show(index) => {
                self.results.select(index);
                self.show_active(cx);
                return;
            }
            DataView::Read(placement) => placement,
        };
        let origin = jykre::sql::statements(&self.editor_sql, self.run.dialect())
            .into_iter()
            .find(|range| self.editor_sql.get(range.clone()) == Some(sql.as_str()))
            .map(|range| range.start);
        let Some(origin) = origin else {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!(
                    "The editor no longer holds the query that reads this table, so nothing was read. Run the SQL in the editor."
                ),
            ));
            cx.notify();
            return;
        };
        let scope = Scope::Again {
            sql,
            origin: Some(origin),
        };
        self.execute_queries(scope, placement, cx);
    }

    /// Tells the grid how copies as SQL write names, strings and the table
    /// the tab was opened for.
    fn update_grid_sql(&mut self, cx: &mut Context<Self>) {
        let mut sql = GridSql {
            dialect: self.run.dialect(),
            ..GridSql::default()
        };
        if let Some(quote) = self.sql_editor.read(cx).name_quoter() {
            sql.quote_name = quote;
        }
        sql.table = self.object.as_ref().and_then(|path| {
            let input = self.input.as_ref()?;
            let driver = builtin_registry()
                .and_then(|registry| registry.get(input.config.driver))
                .ok()?;
            Some(driver.relation_name(path))
        });
        self.grid.update(cx, |grid, _| grid.set_sql(sql));
    }

    /// The result tab on screen has rows or a completed statement to show.
    fn has_results(&self) -> bool {
        self.results
            .active()
            .is_some_and(|tab| tab.state == ResultState::Done)
    }

    /// The page on screen, if any.
    fn current_page(&self) -> Option<&jykre::result_tabs::Page> {
        self.results.active().and_then(ResultTab::current)
    }

    /// Shows the active result tab's page in the grid, and frees on the
    /// database runtime the pages nothing holds any more.
    fn show_active(&mut self, cx: &mut Context<Self>) {
        let (data, sales, first_row, shown) = match self.results.active() {
            Some(tab) => match tab.current() {
                Some(page) => (
                    page.data.clone(),
                    tab.sales,
                    page.first_row,
                    Some((tab.id, tab.page_number())),
                ),
                None => (Arc::new(ResultData::default()), false, 0, None),
            },
            None => (Arc::new(ResultData::default()), false, 0, None),
        };
        let open: Vec<ResultId> = self.results.tabs().iter().map(|tab| tab.id).collect();
        self.grid.update(cx, |grid, cx| {
            grid.set_page(data, sales, first_row, shown, cx);
            grid.retain_places(&open);
        });
        // Whoever holds a page last frees it; here, that is the runtime
        // ([ADR-0022](../../docs/adr/0022-result-pages-shared-across-threads.md)).
        for page in self.results.take_dropped() {
            backend::release(page);
        }
        cx.notify();
    }

    /// A new lease for taking the session out with a run or a page read.
    /// For a run (`fresh`), a session opened with settings that changed
    /// since is closed, so the run opens one with the new settings; a page
    /// read keeps it, since the result it reads lives there.
    fn lease_session(&mut self, fresh: bool) -> (Option<Box<dyn Session>>, u64) {
        if fresh
            && std::mem::take(&mut self.stale_session)
            && let Some(session) = self.session.take()
        {
            backend::close_in_background(session);
            self.transaction = Transaction::AutoCommit;
        }
        self.next_lease += 1;
        self.session_lease = Some(self.next_lease);
        (self.session.take(), self.next_lease)
    }

    /// Keeps a session that comes back under the current lease and closes
    /// any other.
    fn return_session(&mut self, lease: u64, session: Option<Box<dyn Session>>) {
        let Some(session) = session else {
            return;
        };
        if self.session_lease == Some(lease) {
            if let Some(previous) = self.session.replace(session) {
                backend::close_in_background(previous);
            }
        } else {
            backend::close_in_background(session);
        }
    }

    fn options(&self) -> ExecutionOptions {
        ExecutionOptions {
            max_rows: self.row_limit,
            timeout: self.timeout,
            context: self.context.clone(),
        }
    }

    /// Shows the context the session reported after a run or a last page.
    fn session_context(
        &mut self,
        lease: u64,
        context: Option<SessionContext>,
        cx: &mut Context<Self>,
    ) {
        if let Some(context) = context.filter(|_| self.session_lease == Some(lease))
            && context != self.context
        {
            self.sql_editor
                .update(cx, |editor, _| editor.set_context(context.clone()));
            self.context = context;
            self.push_assistant_context(cx);
            cx.emit(WorkspaceEvent::Changed);
        }
    }

    /// Shows the next page of the active result tab, reading it from the
    /// open result when it is not held yet.
    fn next_page(&mut self, cx: &mut Context<Self>) {
        if !self.results.next_page() {
            self.show_active(cx);
            return;
        }
        if self.run.is_active() || self.fetching || self.export_uses_session() {
            return;
        }
        let Some(tab) = self.results.active() else {
            return;
        };
        let (id, columns) = (tab.id, tab.columns());
        let (session, lease) = self.lease_session(false);
        let Some(session) = session else {
            self.results.page_failed(id);
            self.show_active(cx);
            return;
        };
        self.fetching = true;
        self.cancellation = CancellationToken::new();
        let cancel = self.cancellation.clone();
        let options = self.options();
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(backend::fetch(
                session,
                columns,
                options,
                DEFAULT_MEMORY_CEILING,
                cancel,
            ))
            .await;
            let mut pending = Some(result);
            let _ = weak.update(cx, |this, cx| {
                let Some(result) = pending.take() else {
                    return;
                };
                this.fetching = false;
                let page = match result {
                    Ok(run) => {
                        this.session_context(lease, run.context, cx);
                        if this.session_lease == Some(lease) {
                            this.transaction = this.transaction.reported(run.transaction);
                        }
                        this.return_session(lease, run.session);
                        if this.session.is_none() && this.session_lease == Some(lease) {
                            this.session_notice(false, true);
                            this.transaction = Transaction::AutoCommit;
                        }
                        run.page
                    }
                    Err(error) => Err(error),
                };
                match page {
                    Ok(page) => this.results.add_page(
                        id,
                        Completed {
                            data: page.data,
                            limit: page.limit,
                            has_more: page.has_more,
                            elapsed_ms: page.elapsed_ms,
                            ..Completed::default()
                        },
                    ),
                    Err(error) => {
                        this.results.page_failed(id);
                        this.notice = Some(Feedback::new(
                            Tone::Error,
                            tr!("The next page could not be read: {0}", error),
                        ));
                    }
                }
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

    fn previous_page(&mut self, cx: &mut Context<Self>) {
        self.results.previous_page();
        self.show_active(cx);
    }

    fn execute_queries(&mut self, scope: Scope, placement: Placement, cx: &mut Context<Self>) {
        if self.still_cancelling(cx) {
            return;
        }
        if self.fetching {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("Reading the next page. Run again when it arrives."),
            ));
            cx.notify();
            return;
        }
        if self.export_uses_session() {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!(
                    "An export is reading from the session. Wait for it, or cancel it in the export bar."
                ),
            ));
            cx.notify();
            return;
        }
        if self.run.is_running() {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("A query is already running in this tab. Wait for it, or stop it first."),
            ));
            cx.notify();
            return;
        }
        if self.input.is_none() && !self.demo {
            // Nothing runs without a connection: the results area offers
            // the connections instead.
            self.choosing_connection = true;
            self.result_view = ResultView::Table;
            self.notice = Some(Feedback::new(
                Tone::Warning,
                tr!("Choose a connection for this tab."),
            ));
            cx.notify();
            return;
        }
        if self
            .input
            .as_ref()
            .is_some_and(connection_store::needs_password)
        {
            // The host asks for the password, then resumes this run.
            self.pending_run = Some((scope, placement));
            cx.emit(WorkspaceEvent::PasswordRequired);
            return;
        }
        let sql = self.editor.read(cx).text().to_string();
        let confirmed = std::mem::take(&mut self.risk_confirmed);
        // The values of the statements' `:name` markers, bound by the driver
        // (ADR-0026). A missing or malformed one runs nothing.
        let options = self.options();
        let statements = self.run.statements_for(&sql, &scope);
        let Some(requests) = self.parameter_requests(&statements, &options, cx) else {
            return;
        };
        if self.environment == Some(jykre::connection_appearance::Environment::Production)
            && !confirmed
        {
            let risks = jykre::guard::risks(&statements, self.run.dialect());
            if !risks.is_empty() {
                self.risky_run = Some((scope, placement));
                cx.emit(WorkspaceEvent::ConfirmRisk {
                    connection: self.connection.to_string(),
                    description: risk_description(&risks, statements.len()),
                });
                return;
            }
        }
        let off_statement = self.run.off_statement(&sql, &scope);
        let Some(plan) = self.run.start(&sql, scope) else {
            self.notice = Some(Feedback::new(
                Tone::Info,
                if off_statement {
                    tr!(
                        "The cursor is not on a statement. Put it on the statement to run, or select the SQL."
                    )
                } else {
                    tr!("There is no SQL statement to execute.")
                },
            ));
            cx.notify();
            return;
        };
        let id = plan.id;
        let queries = plan.statements;
        let texts: Vec<String> = queries.iter().map(|(_, sql)| sql.clone()).collect();
        let tabs = self.results.begin(&texts, placement);
        for (tab, start) in tabs.iter().zip(&plan.starts) {
            self.results.set_origin(*tab, *start);
        }
        for (tab, request) in tabs.iter().zip(&requests) {
            self.results
                .set_parameters(*tab, request.parameters.clone());
        }
        if placement != Placement::Hidden {
            self.result_view = ResultView::Table;
        }
        self.notice = None;
        self.started_position = 0;
        self.show_active(cx);
        if let Some(input) = self.input.clone() {
            self.cancellation = CancellationToken::new();
            let cancel = self.cancellation.clone();
            // Labels the run's telemetry span; unknown until the
            // connection's names are read.
            let connection = self.cache_id;
            self.run.queue_all(id);
            // The previous run has ended (a stopping one blocks new runs), so
            // its session is back unless it was lost.
            let (session, lease) = self.lease_session(true);
            let before = options.context.clone();
            // COMMIT and ROLLBACK from the toolbar are logged in Messages,
            // not in the history of what the user ran.
            let history = placement != Placement::Hidden;
            self.start_clock(cx);
            // The gutter follows the server: each statement shows as running
            // when the backend sends it, the later ones as waiting.
            let (progress, mut started) = tokio::sync::mpsc::unbounded_channel::<usize>();
            cx.spawn(async move |weak, cx| {
                while let Some(position) = started.recv().await {
                    let alive = weak.update(cx, |this, cx| {
                        if this.session_lease == Some(lease) {
                            this.started_position = position;
                        }
                        this.run.statement_started(id, position);
                        cx.notify();
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            })
            .detach();
            cx.spawn(async move |weak, cx| {
                // Rows become compact display text on the database runtime, row
                // by row, and the driver cells are dropped there.
                let result = backend::run(backend::query(
                    connection,
                    input,
                    session,
                    requests,
                    DEFAULT_MEMORY_CEILING,
                    cancel,
                    Some(progress),
                ))
                .await;
                let mut pending = Some(result);
                let _ = weak.update(cx, |this, cx| {
                    let _timer = jykre::logging::UiTimer::start("publish result");
                    let Some(result) = pending.take() else {
                        return;
                    };
                    // The session and its log still belong to this run; a
                    // superseded run's session is closed on its return.
                    let current = this.session_lease == Some(lease);
                    let dialect = this.run.dialect();
                    let mut reported = None;
                    let result = match result {
                        Ok(run) => {
                            reported = run.transaction;
                            if current {
                                this.session_notice(run.reopened, run.ended);
                                if run.reopened {
                                    this.transaction = Transaction::AutoCommit;
                                }
                            }
                            this.session_context(lease, run.context, cx);
                            this.return_session(lease, run.session);
                            run.batch
                        }
                        Err(error) => Err(error),
                    };
                    let failed: Option<ErrorReport>;
                    if let Ok(batch) = &result {
                        this.structure_changed(
                            &queries[..batch.completed.len().min(queries.len())],
                            &before,
                            cx,
                        );
                    }
                    match result {
                        Ok(batch) => {
                            let completed = batch.completed.len();
                            let failure = batch.error.as_ref().map(ErrorReport::new);
                            if current {
                                // Every statement that ran is logged and moves the
                                // transaction, even in a run cancelled meanwhile.
                                for (result, (_, sql)) in batch.completed.iter().zip(&queries) {
                                    this.transaction = this.transaction.after(sql, true, dialect);
                                    let outcome = LogOutcome::completed(
                                        result.data.column_count(),
                                        result.data.row_count(),
                                        result.has_more || result.limit.is_some(),
                                        result.outcome.rows_affected,
                                    );
                                    this.ran(sql, outcome, Some(result.elapsed_ms), history, cx);
                                }
                                if let (Some(error), Some((_, sql))) =
                                    (&batch.error, queries.get(completed))
                                {
                                    this.transaction = this.transaction.after(sql, false, dialect);
                                    this.ran(sql, logged_failure(error), None, history, cx);
                                }
                            }
                            failed = failure.clone();
                            let outcome = Outcome::Batch {
                                completed,
                                error: batch.error,
                            };
                            if this.run.finish(id, outcome) {
                                for (tab, result) in tabs.iter().zip(batch.completed) {
                                    this.results.complete(
                                        *tab,
                                        Completed {
                                            data: result.data,
                                            limit: result.limit,
                                            has_more: result.has_more,
                                            rows_affected: result.outcome.rows_affected,
                                            elapsed_ms: result.elapsed_ms,
                                            sales: false,
                                        },
                                    );
                                }
                                let ran = completed + usize::from(failure.is_some());
                                this.results.settle(&tabs[..ran.min(tabs.len())]);
                                if let Some(failure) = failure
                                    && let Some(tab) = tabs.get(completed)
                                {
                                    this.results.fail(*tab, failure);
                                    this.mark_error(*tab, cx);
                                }
                                this.results.discard(&tabs[ran.min(tabs.len())..]);
                            } else {
                                this.end_unfinished(&tabs, None);
                            }
                        }
                        Err(error) => {
                            if current && let Some((_, sql)) = queries.get(this.started_position) {
                                this.transaction = this.transaction.after(sql, false, dialect);
                                this.ran(sql, logged_failure(&error), None, history, cx);
                            }
                            let report = ErrorReport::new(&error);
                            failed = Some(report.clone());
                            this.run.finish(id, Outcome::Failed(error));
                            this.end_unfinished(&tabs, Some(report));
                        }
                    }
                    // What the server reports wins over what the statements
                    // suggest; a session that did not come back takes its
                    // transaction with it.
                    if current {
                        this.transaction = this.transaction.reported(reported);
                        if this.session.is_none() {
                            this.transaction = Transaction::AutoCommit;
                        }
                    }
                    // A COMMIT or ROLLBACK has no result tab to show its error.
                    if placement == Placement::Hidden
                        && let Some(report) = failed
                    {
                        this.notice = Some(Feedback::new(Tone::Error, report.summary()));
                    }
                    this.clear_stopping_notice();
                    this.show_active(cx);
                });
                // The tab closed while the run was out: its session goes too.
                if let Some(Ok(run)) = pending
                    && let Some(session) = run.session
                {
                    backend::close_in_background(session);
                }
            })
            .detach();
            return;
        }
        self.run.queue_all(id);
        self.start_clock(cx);
        cx.spawn(async move |weak, cx| {
            for (position, (index, sql)) in queries.into_iter().enumerate() {
                let tab = tabs[position];
                let active = weak
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.run.begin_statement(id, index)
                    })
                    .unwrap_or(false);
                if !active {
                    let _ = weak.update(cx, |this, cx| {
                        this.end_unfinished(&tabs, None);
                        this.show_active(cx);
                    });
                    return;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(550))
                    .await;
                let keep_running = weak
                    .update(cx, |this, cx| {
                        let error = demo::preview_error(&sql);
                        let shown = this.run.finish_statement(id, index, error);
                        if shown {
                            let data = sample_result(&sql);
                            let outcome = LogOutcome::completed(
                                data.column_count(),
                                data.row_count(),
                                false,
                                None,
                            );
                            this.ran(&sql, outcome, None, true, cx);
                            this.results.complete(
                                tab,
                                Completed {
                                    data,
                                    sales: demo::is_sales(&sql),
                                    ..Completed::default()
                                },
                            );
                        } else if let Some(error) = error {
                            this.ran(&sql, LogOutcome::Failed(error.to_owned()), None, true, cx);
                            this.results.fail(tab, ErrorReport::database(error));
                            this.results.discard(&tabs[position + 1..]);
                        } else {
                            this.end_unfinished(&tabs, None);
                        }
                        this.show_active(cx);
                        shown
                    })
                    .unwrap_or(false);
                if !keep_running {
                    return;
                }
            }
            let _ = weak.update(cx, |this, cx| {
                this.run.complete(id);
                cx.notify();
            });
        })
        .detach();
    }

    /// The tabs of a run that ended without results: cancelled, superseded or
    /// failed as a whole. A whole-run error shows in its first tab.
    fn end_unfinished(&mut self, tabs: &[ResultId], failure: Option<ErrorReport>) {
        if let RunState::Failed { .. } = self.run.state()
            && let Some(failure) = failure
            && let Some(first) = tabs.first()
        {
            self.results.fail(*first, failure);
            self.results.discard(&tabs[1..]);
            return;
        }
        for tab in tabs {
            self.results.cancel(*tab);
        }
    }

    /// Where the failed statement of result `id` stops, as a byte offset in
    /// the editor: the statement must still be where it ran, or appear once
    /// in the editor.
    fn error_offset(&self, id: ResultId) -> Option<usize> {
        let tab = self.results.get(id)?;
        let ResultState::Failed(report) = &tab.state else {
            return None;
        };
        let offset = report.offset_in(&tab.sql)?;
        let sql = &self.editor_sql;
        let start = tab
            .origin
            .filter(|start| sql.get(*start..*start + tab.sql.len()) == Some(tab.sql.as_str()))
            .or_else(|| {
                let mut found = sql.match_indices(tab.sql.as_str()).map(|(at, _)| at);
                let first = found.next()?;
                found.next().is_none().then_some(first)
            })?;
        Some(start + offset)
    }

    /// Puts the cursor where the server stopped, when the editor still holds
    /// the statement ([UX](../../docs/ux.md#error-messages)). Nothing is
    /// selected, so the next key never replaces text.
    fn mark_error(&mut self, id: ResultId, cx: &mut Context<Self>) {
        if let Some(at) = self.error_offset(id) {
            self.editor
                .update(cx, |editor, cx| editor.set_selected_range(at..at, cx));
        }
    }

    /// Moves the cursor to the error's position and focuses the editor.
    fn go_to_error(&mut self, id: ResultId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(at) = self.error_offset(id) else {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("The statement changed in the editor since it ran."),
            ));
            cx.notify();
            return;
        };
        let before = &self.editor_sql[..at];
        let line = before.matches('\n').count();
        let character = before
            .rsplit_once('\n')
            .map_or(before, |(_, line)| line)
            .chars()
            .count();
        self.editor.update(cx, |editor, cx| {
            editor.set_cursor_position(
                gpui_kit::component::input::Position::new(line as u32, character as u32),
                window,
                cx,
            );
        });
    }

    /// The connection this tab runs on. Choosing another asks the host,
    /// which confirms with the user before moving the tab.
    fn connection_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let current = self.connection.to_string();
        let choices = self.connection_choices.clone();
        Button::new("tab-connection")
            .ghost()
            .small()
            .icon(I::Server)
            .label(current.clone())
            .dropdown_caret(true)
            .accessibility_label(tr!("Connection: {0}", current))
            .tooltip(tr!("Connection of this tab"))
            .dropdown_menu(move |mut menu, _, _| {
                for name in &choices {
                    let workspace = workspace.clone();
                    let chosen = name.clone();
                    menu = menu.item(
                        PopupMenuItem::new(name.clone())
                            .checked(*name == current)
                            .on_click(move |_, _, cx| {
                                let _ = workspace.update(cx, |this, cx| {
                                    if *this.connection != *chosen {
                                        cx.emit(WorkspaceEvent::ConnectionRequested(
                                            chosen.clone(),
                                        ));
                                    }
                                });
                            }),
                    );
                }
                menu
            })
    }

    /// Tells the explorer which namespaces the DDL among `statements` (the
    /// ones that succeeded) made stale. Unqualified names count in the
    /// context before the run and in the one after it.
    fn structure_changed(
        &mut self,
        statements: &[(usize, String)],
        before: &SessionContext,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = &self.input else {
            return;
        };
        let Ok(driver) = builtin_registry().and_then(|registry| registry.get(input.config.driver))
        else {
            return;
        };
        let descriptor = driver.descriptor();
        let texts: Vec<String> = statements.iter().map(|(_, sql)| sql.clone()).collect();
        let namespaces = jykre::ddl::stale_namespaces(
            &texts,
            &descriptor.capabilities.namespace_levels(),
            descriptor.dialect,
            &[before.clone(), self.context.clone()],
        );
        if !namespaces.is_empty() {
            cx.emit(WorkspaceEvent::StructureChanged {
                connection: self.connection.to_string(),
                namespaces,
            });
        }
    }

    /// Where unqualified names resolve, as the header shows it.
    fn context_label(&self) -> String {
        jykre::ddl::context_label(&self.context)
    }

    /// The header's context selector: the schema (PostgreSQL), database
    /// (MySQL) or catalog and schema (Databricks) unqualified names resolve
    /// in. Choosing one applies it from the next run.
    fn context_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let current = self.context.clone();
        let choices = self.context_choices.clone();
        let label = self.context_label();
        Button::new("tab-context")
            .ghost()
            .small()
            .icon(I::Layers)
            .label(label.clone())
            .dropdown_caret(true)
            .accessibility_label(tr!("Context: {0}", label))
            .tooltip(tr!("Where unqualified names resolve in this tab"))
            .dropdown_menu(move |mut menu, _, _| {
                if choices.is_empty() {
                    menu = menu.label(tr!("Refresh the metadata to list namespaces"));
                }
                for choice in &choices {
                    let workspace = workspace.clone();
                    let chosen = choice.clone();
                    menu = menu.item(
                        PopupMenuItem::new(jykre::ddl::context_label(choice))
                            .checked(*choice == current)
                            .on_click(move |_, _, cx| {
                                let _ = workspace.update(cx, |this, cx| {
                                    this.sql_editor
                                        .update(cx, |editor, _| editor.set_context(chosen.clone()));
                                    this.context = chosen.clone();
                                    this.push_assistant_context(cx);
                                    cx.emit(WorkspaceEvent::Changed);
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            })
    }

    /// What each dialect has built in, for the editor's language features.
    pub fn set_catalogs(&mut self, catalogs: Arc<SqlCatalogs>, cx: &mut Context<Self>) {
        self.sql_editor
            .update(cx, |editor, cx| editor.set_catalogs(catalogs, cx));
        self.update_grid_sql(cx);
    }

    /// Uses `names` for completion, hover and go to definition.
    pub fn set_names(&mut self, id: ConnectionId, names: Arc<NameIndex>, cx: &mut Context<Self>) {
        self.sql_editor
            .update(cx, |editor, _| editor.set_names(id, names));
        if self.cache_id != Some(id) {
            self.cache_id = Some(id);
            self.push_assistant_target(cx);
        }
    }

    /// Sets the namespaces the header offers.
    pub fn set_context_choices(&mut self, choices: Vec<SessionContext>, cx: &mut Context<Self>) {
        if self.context_choices != choices {
            self.context_choices = choices;
            cx.notify();
        }
    }

    /// Sets the connections the toolbar offers.
    pub fn set_connection_choices(&mut self, choices: Vec<String>, cx: &mut Context<Self>) {
        if self.connection_choices != choices {
            self.connection_choices = choices;
            cx.notify();
        }
    }

    /// The statement timeout of this tab, chosen from a short list.
    fn timeout_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = cx.entity().downgrade();
        let current = self.timeout;
        Button::new("statement-timeout")
            .ghost()
            .small()
            .icon(I::Clock)
            .label(timeout_label(current))
            .dropdown_caret(true)
            .accessibility_label(tr!("Statement timeout: {0}", timeout_label(current)))
            .tooltip(tr!("Statement timeout"))
            .dropdown_menu(move |mut menu, _, _| {
                for option in TIMEOUTS {
                    let workspace = workspace.clone();
                    menu = menu.item(
                        PopupMenuItem::new(timeout_label(option))
                            .checked(option == current)
                            .on_click(move |_, _, cx| {
                                let _ = workspace.update(cx, |this, cx| {
                                    this.timeout = option;
                                    cx.emit(WorkspaceEvent::Changed);
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            })
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        row()
            .h(px(48.))
            .flex_none()
            .px_4()
            .justify_between()
            .border_b_1()
            .border_color(rgb(colors().border))
            .child(
                row()
                    .gap_3()
                    .child(
                        primary_button(
                            "execute",
                            match self.run.state() {
                                RunState::Running { .. } => I::Square,
                                RunState::Cancelling => I::LoaderCircle,
                                _ => I::Play,
                            },
                            match self.run.state() {
                                RunState::Running { .. } => tr!("Stop"),
                                // Waits for the stopping run before another starts.
                                RunState::Cancelling => tr!("Stopping…"),
                                _ => tr!("Run"),
                            },
                        )
                        .when(self.run.state() == &RunState::Cancelling, |s| {
                            s.opacity(0.6).cursor_default()
                        })
                        .tooltip(|window, cx| {
                            Tooltip::new(tr!(
                                "Run the statement at the cursor or the selection, in place of the result on screen · Ctrl+Enter\nIn a new result · Ctrl+Shift+Enter\nRun all statements · Ctrl+Alt+Enter"
                            ))
                            .build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_current(cx))),
                    )
                    .child(divider())
                    .child(
                        button("explain", I::ScanLine, tr!("Explain"))
                            .tooltip(|window, cx| {
                                Tooltip::new(tr!(
                                    "Show the database's plan for the statement at the cursor or the selection, in a new result, without running it"
                                ))
                                .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.explain(cx))),
                    )
                    .child(
                        button("format", I::TextAlignStart, tr!("Format"))
                            .tooltip(|window, cx| {
                                Tooltip::new(tr!(
                                    "Format the selection, or the whole script when nothing is selected"
                                ))
                                .build(window, cx)
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.format_sql(window, cx)),
                            ),
                    )
                    .child(divider())
                    .child(self.connection_menu(cx))
                    .child(self.timeout_menu(cx)),
            )
            .child(
                row()
                    .child(self.transaction_controls(cx))
                    .child(
                        icon_button(
                            "save-query",
                            if self.saved_sql.as_deref() == Some(self.editor_sql.as_str()) {
                                I::BookmarkCheck
                            } else {
                                I::Bookmark
                            },
                            match &self.saved_sql {
                                Some(saved) if *saved == self.editor_sql => {
                                    tr!("Saved in the library")
                                }
                                Some(_) => tr!("Update saved query"),
                                None => tr!("Save query"),
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.save_query(cx))),
                    )
                    .child({
                        let workspace = cx.entity().downgrade();
                        Button::new("save-report")
                            .ghost()
                            .small()
                            .icon(I::FileChartColumn)
                            .dropdown_caret(true)
                            .accessibility_label(tr!("Make a report from this query"))
                            .tooltip(tr!("Make a report from this query"))
                            .dropdown_menu(move |menu, _, _| {
                                let new_report = workspace.clone();
                                let existing = workspace.clone();
                                menu.item(PopupMenuItem::new(tr!("Save as a new report")).on_click(
                                    move |_, _, cx| {
                                        let _ = new_report
                                            .update(cx, |this, cx| this.save_as_report(cx));
                                    },
                                ))
                                .item(PopupMenuItem::new(tr!("Add to a report…")).on_click(
                                    move |_, _, cx| {
                                        let _ =
                                            existing.update(cx, |this, cx| this.add_to_report(cx));
                                    },
                                ))
                            })
                    })
                    .child(divider())
                    .child(
                        icon_button(
                            "toggle-ai",
                            I::PanelRight,
                            tr!("Show or hide the assistant"),
                        )
                        .when(self.assistant_open, |s| s.bg(rgb(colors().surface)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.assistant_open = !this.assistant_open;
                            cx.notify();
                        })),
                    ),
            )
    }

    /// Run buttons beside the first line of each statement. Only statements
    /// on screen get a button, and their lines come from `statement_lines`,
    /// so a frame costs the same for a short query or a long script.
    fn query_gutter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.editor.clone();
        let lines = self.statement_lines.clone();
        let workspace = cx.weak_entity();
        canvas(
            move |bounds, window, cx| {
                let on_screen: Vec<(usize, Pixels)> = {
                    let state = editor.read(cx);
                    // Paint-time bounds still belong to the previous frame here.
                    // Use the editor's fresh prepaint snapshot so the gutter and
                    // its hitboxes follow the same scroll offset as the SQL.
                    let Some(geometry) = state.prepaint_caret_geometry() else {
                        return Vec::new();
                    };
                    let height = geometry.line_height();
                    let first = geometry.input_bounds().top()
                        + geometry.scroll_offset().y
                        + (height - px(22.)) / 2.;
                    let map = state.display_map();
                    lines
                        .iter()
                        .enumerate()
                        .filter(|(_, line)| !map.is_buffer_line_hidden(**line))
                        .map(|(index, line)| {
                            (
                                index,
                                first + height * map.buffer_line_to_display_row(*line) as f32,
                            )
                        })
                        // Statements are in line order: after the first one
                        // below the editor, none is on screen.
                        .take_while(|(_, top)| *top < bounds.bottom())
                        .filter(|(_, top)| {
                            *top >= bounds.top() && *top + px(22.) <= bounds.bottom()
                        })
                        .collect()
                };
                let mut visible = Vec::with_capacity(on_screen.len());
                for (index, top) in on_screen {
                    let Some(this) = workspace.upgrade() else {
                        break;
                    };
                    let status = this.read(cx).run.status(index);
                    let mut button = run_button(index, status, workspace.clone());
                    button.layout_as_root(AvailableSpace::min_size(), window, cx);
                    button.prepaint_at(point(bounds.left(), top), window, cx);
                    visible.push(button);
                }
                visible
            },
            |_, mut buttons, window, cx| {
                for button in &mut buttons {
                    button.paint(window, cx);
                }
            },
        )
        .w(px(24.))
        .h_full()
        .flex_none()
    }

    /// Whether the tab holds a session on its connection, after the
    /// context in the editor header. A click on **Connected** disconnects.
    fn session_status(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.input.is_none() {
            return row()
                .child(dot(if self.demo {
                    colors().mint
                } else {
                    colors().amber
                }))
                .child(label(if self.demo {
                    tr!("Demo")
                } else {
                    tr!("No connection")
                }))
                .into_any_element();
        }
        let status = |id: &'static str, color: u32, text: &'static str, tip: &'static str| {
            row()
                .id(id)
                .role(Role::Status)
                .aria_label(text)
                .px_2()
                .h(px(26.))
                .rounded(px(5.))
                .child(dot(color))
                .child(label(text))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        };
        match SessionIndicator::of(self.session.is_some(), self.is_busy()) {
            SessionIndicator::InUse => status(
                "session-in-use",
                colors().mint,
                tr!("Connected"),
                tr!("A query is using the session. Stop it to disconnect."),
            )
            .into_any_element(),
            SessionIndicator::Disconnected => status(
                "session-closed",
                colors().faint,
                tr!("Disconnected"),
                tr!("The next run opens a session."),
            )
            .into_any_element(),
            SessionIndicator::Connected => status(
                "disconnect-session",
                colors().mint,
                tr!("Connected"),
                tr!("Disconnect: close this tab's session"),
            )
            .role(Role::Button)
            .aria_label(tr!("Connected. Disconnect"))
            .focusable()
            .tab_index(0)
            .border_1()
            .border_color(transparent_black())
            .focus_visible(|s| s.border_color(rgb(colors().mint)))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(colors().hover)))
            .child(icon(I::Unplug, colors().faint).size(px(14.)))
            .on_click(cx.listener(|this, _, window, cx| this.disconnect(false, window, cx)))
            .into_any_element(),
        }
    }

    /// Closes the tab's session, as a click on **Connected** does. Results
    /// stay on screen; the next run opens a new session, where the tab's
    /// context is applied again. An open transaction is rolled back, so that
    /// asks first (unless `confirmed`).
    fn disconnect(&mut self, confirmed: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("A query is using the session. Stop it before disconnecting."),
            ));
            cx.notify();
            return;
        }
        if self.session.is_none() {
            return;
        }
        if !confirmed && self.transaction.is_open() {
            let workspace = cx.entity().downgrade();
            super::confirm(
                window,
                cx,
                super::Confirmation {
                    title: tr!("Disconnect and roll back?").to_owned(),
                    description: tr!(
                        "This tab has an open transaction. Disconnecting closes the session, which rolls it back."
                    )
                    .to_owned(),
                    ok: tr!("Disconnect"),
                    cancel: tr!("Cancel"),
                    focus: super::ConfirmFocus::Dialog,
                },
                move |window, cx| {
                    let _ = workspace.update(cx, |this, cx| this.disconnect(true, window, cx));
                },
            );
            return;
        }
        self.stale_session = false;
        // Closing the session rolls back its transaction.
        self.transaction = Transaction::AutoCommit;
        if let Some(session) = self.session.take() {
            backend::close_in_background(session);
        }
        self.notice = Some(Feedback::new(
            Tone::Info,
            tr!(
                "Disconnected. The next run opens a new session, and session settings such as the current schema start over."
            ),
        ));
        cx.notify();
    }

    fn editor_panel(&self, cx: &mut Context<Self>) -> Div {
        column()
            .size_full()
            .bg(rgb(colors().bg))
            .child(
                row()
                    .h(px(38.))
                    .flex_none()
                    .px_4()
                    .justify_between()
                    .child(
                        row()
                            .gap_2()
                            .child(icon(
                                I::Server,
                                self.connection_color
                                    .map_or(colors().faint, connection_color),
                            ))
                            .child(label(self.connection.clone()))
                            .when_some(self.environment, |s, environment| {
                                s.child(environment_badge(environment))
                            })
                            // Only what is real: the context the session
                            // reports (or chosen for its next run). A tab
                            // without a connection has no context to show;
                            // the demo shows its sample database.
                            .map(|s| {
                                if self.input.is_some() {
                                    s.child(label("/")).child(self.context_menu(cx))
                                } else if self.demo {
                                    s.child(label("/"))
                                        .child(label(self.database.clone()))
                                        .child(label("/"))
                                        .child(label("public"))
                                } else {
                                    s
                                }
                            }),
                    )
                    .child(self.session_status(cx)),
            )
            .children(self.parameter_form(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .overflow_hidden()
                    .relative()
                    .pl(px(24.))
                    .child(
                        // The run shortcuts take precedence over the editor's own
                        // Ctrl+Enter binding inside this context.
                        div()
                            .key_context(SQL_EDITOR_CONTEXT)
                            .on_action(cx.listener(|this, _: &super::ToggleComment, window, cx| {
                                this.sql_editor
                                    .update(cx, |editor, cx| editor.toggle_comment(window, cx));
                            }))
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(
                                Editor::new(&self.editor)
                                    .appearance(false)
                                    .bordered(false)
                                    .font_family("Cascadia Code")
                                    .text_size(cx.theme().mono_font_size)
                                    .w_full()
                                    .h_full()
                                    .aria_label(tr!("SQL query editor")),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(px(8.))
                            .top_0()
                            .h_full()
                            .child(deferred(self.query_gutter(cx))),
                    ),
            )
            .child(
                row()
                    .h(px(26.))
                    .flex_none()
                    .px_4()
                    .justify_between()
                    .child(label(if self.dialect.as_ref() == "—" {
                        "SQL".to_owned()
                    } else {
                        format!("SQL · {}", self.dialect)
                    }))
                    .child(
                        row()
                            .gap_3()
                            .child(label("UTF-8"))
                            .child(label(tr!("Spaces: 4")))
                            .child(label(match self.line_count {
                                1 => tr!("1 line").to_string(),
                                lines => tr!("{0} lines", lines),
                            })),
                    ),
            )
    }

    fn result_panel(&self, cx: &mut Context<Self>) -> Div {
        let views = [
            (ResultView::Table, I::Table2, tr!("Results")),
            (ResultView::Messages, I::Terminal, tr!("Messages")),
        ];
        let active = self.results.active_index();
        column()
            .size_full()
            .border_t_1()
            .border_color(rgb(colors().border))
            .bg(rgb(colors().panel))
            .child(
                row()
                    .h(px(42.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(colors().border))
                    // One tab per statement result; the close button keeps
                    // the tab strip from growing without end.
                    .child(
                        row()
                            .id("result-tabs")
                            .role(Role::TabList)
                            .aria_label(tr!("Results"))
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .overflow_x_scroll()
                            .children(self.results.tabs().iter().enumerate().map(
                                |(index, tab)| {
                                    let (symbol, color, status) = result_status(tab);
                                    let running = tab.state == ResultState::Running;
                                    let title = tab.title(self.data_sql.as_deref());
                                    let drag = ResultTabDrag {
                                        id: tab.id,
                                        title: title.clone().into(),
                                    };
                                    row()
                                        // Follows the tab when it moves, so
                                        // the focus moves with it.
                                        .id(("result-tab", tab.id.get() as usize))
                                        .role(Role::Tab)
                                        .aria_selected(index == active)
                                        .aria_label(tr!("{0}: {1}", title, status))
                                        .focusable()
                                        .tab_index(0)
                                        .focus_visible(|s| s.bg(rgb(colors().hover)))
                                        .flex_none()
                                        .h(px(28.))
                                        .pl_2()
                                        .pr_1()
                                        .gap_1()
                                        .rounded(px(5.))
                                        .cursor_pointer()
                                        .text_size(px(12.))
                                        .text_color(rgb(if index == active {
                                            colors().text
                                        } else {
                                            colors().muted
                                        }))
                                        .when(index == active, |s| s.bg(rgb(colors().surface)))
                                        .hover(|s| s.bg(rgb(colors().hover)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.results.select(index);
                                            this.show_active(cx);
                                        }))
                                        // Dragged onto another tab, it takes
                                        // that tab's place; Ctrl Shift and an
                                        // arrow move the focused one.
                                        .on_drag(drag, |drag, _, _, cx| {
                                            cx.new(|_| ResultTabPreview {
                                                title: drag.title.clone(),
                                            })
                                        })
                                        .drag_over::<ResultTabDrag>(|style, _, _, _| {
                                            style
                                                .bg(rgb(colors().hover))
                                                .border_l_2()
                                                .border_color(rgb(colors().mint))
                                        })
                                        .on_drop(cx.listener(
                                            move |this, drag: &ResultTabDrag, _, cx| {
                                                if let Some(from) = this
                                                    .results
                                                    .tabs()
                                                    .iter()
                                                    .position(|tab| tab.id == drag.id)
                                                {
                                                    this.results.move_tab(from, index);
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                        .on_key_down(cx.listener(
                                            move |this, event: &KeyDownEvent, window, cx| {
                                                let modifiers = &event.keystroke.modifiers;
                                                if !(modifiers.control && modifiers.shift) {
                                                    return;
                                                }
                                                let to = match event.keystroke.key.as_str() {
                                                    "left" => index.checked_sub(1),
                                                    "right" => Some(index + 1),
                                                    _ => None,
                                                };
                                                if let Some(to) = to {
                                                    this.results.move_tab(index, to);
                                                    window.prevent_default();
                                                    cx.stop_propagation();
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                        .map(|s| {
                                            if running {
                                                s.child(spinning_icon(
                                                    ("result-tab-spinner", index),
                                                    symbol,
                                                    color,
                                                ))
                                            } else {
                                                s.child(icon(symbol, color))
                                            }
                                        })
                                        .child(title)
                                        .when(!running, |s| {
                                            s.child(pin_button(index, tab.pinned).on_click(
                                                cx.listener(move |this, _, _, cx| {
                                                    this.results.toggle_pin(index);
                                                    cx.notify();
                                                    cx.stop_propagation();
                                                }),
                                            ))
                                        })
                                        .when(!running, |s| {
                                            s.child(
                                                icon_button(
                                                    ("close-result", index),
                                                    I::X,
                                                    tr!("Close result"),
                                                )
                                                .size(px(20.))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.results.close(index);
                                                    this.show_active(cx);
                                                    cx.stop_propagation();
                                                })),
                                            )
                                        })
                                },
                            )),
                    )
                    .child(
                        row()
                            .flex_none()
                            .gap_1()
                            .children(views.into_iter().enumerate().flat_map(
                                |(i, (view, symbol, title))| {
                                    let mode = icon_button(("result-mode", i), symbol, title)
                                        .aria_selected(self.result_view == view)
                                        .when(self.result_view == view, |s| {
                                            s.bg(rgb(colors().surface))
                                                .text_color(rgb(colors().text))
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.result_view = view;
                                            cx.notify();
                                        }));
                                    // Beside Results: the side panel's statistics
                                    // of the selection, else of the whole page.
                                    let statistics = (view == ResultView::Table).then(|| {
                                        let shown = self.result_view == ResultView::Table
                                            && self.grid.read(cx).statistics_shown();
                                        icon_button(
                                            "result-statistics",
                                            I::ChartColumn,
                                            tr!("Statistics"),
                                        )
                                        .aria_selected(shown)
                                        .when(shown, |s| {
                                            s.bg(rgb(colors().surface))
                                                .text_color(rgb(colors().mint))
                                        })
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.show_statistics(!shown, cx)
                                            }),
                                        )
                                    });
                                    std::iter::once(mode).chain(statistics)
                                },
                            ))
                            .child(divider())
                            .child(self.display_menu(cx))
                            .child({
                                let records = self.grid.read(cx).records();
                                icon_button(
                                    "records-view",
                                    I::Rows3,
                                    tr!("Show the selected rows as records"),
                                )
                                .aria_selected(records)
                                .when(records, |s| s.bg(rgb(colors().hover)))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.grid
                                            .update(cx, |grid, cx| grid.set_records(!records, cx));
                                    },
                                ))
                            })
                            .child(
                                icon_button("filter-results", I::ListFilter, tr!("Filter results"))
                                    .aria_selected(self.filter_open)
                                    .when(self.filter_open, |s| s.bg(rgb(colors().hover)))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.filter_open = !this.filter_open;
                                        if this.filter_open {
                                            this.filter
                                                .update(cx, |input, cx| input.focus(window, cx));
                                        }
                                        // A closed filter bar must not keep hiding rows.
                                        let filter = if this.filter_open {
                                            this.filter.read(cx).value().to_string()
                                        } else {
                                            String::new()
                                        };
                                        this.grid
                                            .update(cx, |grid, cx| grid.set_filter(&filter, cx));
                                        cx.notify();
                                    })),
                            )
                            .child(self.copy_menu(cx))
                            .child(
                                icon_button(
                                    "copy-page-csv",
                                    I::Sheet,
                                    tr!("Copy this page as CSV"),
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.copy_page_csv(false, window, cx),
                                )),
                            )
                            .child({
                                let open = !matches!(self.export.state, ExportState::Closed);
                                icon_button(
                                    "export-results",
                                    I::Download,
                                    tr!("Export the result to a file (Ctrl+Shift+E)"),
                                )
                                .aria_selected(open)
                                .when(open, |s| s.bg(rgb(colors().hover)))
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_export(cx)))
                            })
                            .child(
                                icon_button(
                                    "refresh-results",
                                    I::RefreshCw,
                                    tr!("Run this result's statement again"),
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.run_again(cx))),
                            ),
                    ),
            )
            .when(self.filter_open, |s| {
                s.child(
                    column()
                        .px_3()
                        .py_2()
                        .gap_1()
                        .child(Input::new(&self.filter).small().prefix(I::Search).w_full())
                        .when_some(self.filter_scope(), |s, scope| s.child(label(scope))),
                )
            })
            .when_some(self.export_bar(cx), |s, bar| s.child(bar))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.result_body(cx)),
            )
            .child(self.result_footer(cx))
    }

    /// The active result tab: rows, the statement's outcome, its error or
    /// the empty state.
    fn result_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let centered = || {
            column()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .p_4()
        };
        if self.result_view == ResultView::Messages {
            return self.result_messages(cx);
        }
        if self.choosing_connection && self.input.is_none() {
            return self.connection_chooser(cx);
        }
        let Some(tab) = self.results.active() else {
            // A table's Data view not read yet (reopened after a restart)
            // offers to read its rows.
            if self.data_sql.is_some() {
                return centered()
                    .child(icon(I::Table2, colors().faint))
                    .child(div().text_size(px(14.)).child(tr!("Ready to explore")))
                    .child(label(tr!(
                        "Read the table's rows, a page at a time, with the query above."
                    )))
                    .child(
                        button("empty-view-data", I::Play, tr!("View data"))
                            .on_click(cx.listener(|this, _, _, cx| this.read_data(cx))),
                    )
                    .into_any_element();
            }
            return centered()
                .child(icon(I::Terminal, colors().faint))
                .child(div().text_size(px(14.)).child(tr!("Ready to explore")))
                .child(label(tr!("Run the query to see the results.")))
                .child(
                    button("empty-run", I::Play, tr!("Run query"))
                        .on_click(cx.listener(|this, _, _, cx| this.execute_current(cx))),
                )
                .into_any_element();
        };
        match &tab.state {
            ResultState::Running => centered()
                .child(spinning_icon(
                    "running-spinner",
                    I::LoaderCircle,
                    colors().mint,
                ))
                .child(label(if self.input.is_some() {
                    tr!("Running on the server…")
                } else {
                    tr!("Preparing the demo…")
                }))
                .into_any_element(),
            ResultState::Failed(report) => self.result_error(tab, report, cx),
            ResultState::Cancelled => centered()
                .child(icon(I::Ban, colors().amber))
                .child(label(tr!("Execution cancelled")))
                .into_any_element(),
            ResultState::Done => match self.result_view {
                ResultView::Messages => self.result_messages(cx),
                ResultView::Table
                    if self
                        .current_page()
                        .is_some_and(|page| page.data.column_count() > 0) =>
                {
                    // A cached view reuses the grid's last frame while only
                    // the workspace changes.
                    self.grid
                        .clone()
                        .cached(StyleRefinement::default().size_full())
                        .into_any_element()
                }
                ResultView::Table => centered()
                    .child(icon(I::CircleCheck, colors().mint))
                    .child(label(match tab.rows_affected {
                        Some(affected) => tr!("{0} rows affected", affected),
                        None => tr!("Statement completed").into(),
                    }))
                    .into_any_element(),
            },
        }
    }

    /// A failed statement: the title, the database's own message, its codes
    /// and position, a suggested action and **Copy details**.
    fn result_error(
        &self,
        tab: &ResultTab,
        report: &ErrorReport,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tab.id;
        let position = report
            .line_and_column(&tab.sql)
            .map(|(line, column)| tr!("Line {0}, column {1} of the statement", line, column));
        let codes = match (report.codes(), position.clone()) {
            (Some(codes), Some(position)) => Some(format!("{codes} · {position}")),
            (codes, position) => codes.or(position),
        };
        let details = report.details(&tr!("Run {0} · result {1}", self.run_number(), tab.number));
        column()
            .id("result-error")
            .role(Role::Alert)
            .aria_label(format!("{}. {}", report.title, report.message))
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_2()
            .child(
                row().gap_2().child(icon(I::CircleX, colors().red)).child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(colors().red))
                        .child(report.title.clone()),
                ),
            )
            .child(
                div()
                    .pl(px(24.))
                    .font_family("Cascadia Mono")
                    .text_size(px(12.))
                    .line_height(relative(1.5))
                    .text_color(rgb(colors().text))
                    .child(report.message.clone()),
            )
            .when_some(codes, |s, codes| {
                s.child(div().pl(px(24.)).child(label(codes)))
            })
            .when_some(report.action.clone(), |s, action| {
                s.child(
                    div()
                        .pl(px(24.))
                        .text_size(px(12.))
                        .text_color(rgb(colors().muted))
                        .child(action),
                )
            })
            .child(
                row()
                    .pl(px(24.))
                    .gap_2()
                    .when(self.assistant_ready(cx), |s| {
                        s.child(
                            button("fix-with-ai", I::Sparkles, tr!("Fix with AI")).on_click(
                                cx.listener(move |this, _, _, cx| this.fix_with_ai(id, cx)),
                            ),
                        )
                    })
                    .child(button("copy-error", I::Copy, tr!("Copy details")).on_click(
                        cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(details.clone()));
                            this.notice =
                                Some(Feedback::new(Tone::Success, tr!("Error details copied.")));
                            cx.notify();
                        }),
                    ))
                    .when(position.is_some(), |s| {
                        s.child(
                            button("go-to-error", I::CornerDownLeft, tr!("Go to the position"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.go_to_error(id, window, cx)
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The number of the last run of this tab, for error details.
    fn run_number(&self) -> u64 {
        self.run.current_id().get()
    }

    /// The single place that reports how the result on screen ended, with
    /// its pages.
    fn result_footer(&self, cx: &mut Context<Self>) -> Div {
        let tab = self.results.active();
        let state = self.run.state();
        // After two seconds a run shows how long it has taken, and a
        // cancellation that takes long says the server has not stopped yet
        // ([UX](../../docs/ux.md#always-visible-states)).
        let elapsed = |duration: Duration| {
            (duration >= Duration::from_secs(2))
                .then(|| cell_format::duration(duration.as_millis() as u64))
        };
        let (symbol, color, text, detail): (I, u32, String, Option<String>) = if matches!(
            state,
            RunState::Running { .. }
        ) {
            let text = match self.run.running_for().and_then(elapsed) {
                Some(elapsed) => tr!("Running… {0}", elapsed),
                None => tr!("Running…").into(),
            };
            let detail = self.timeout.map(|timeout| {
                tr!(
                    "Stops at the statement timeout of {0}",
                    timeout_label(Some(timeout))
                )
            });
            (I::LoaderCircle, colors().amber, text, detail)
        } else if *state == RunState::Cancelling {
            let waited = self.run.cancelling_for().unwrap_or_default();
            if waited >= CANCEL_SLOW {
                (
                        I::LoaderCircle,
                        colors().amber,
                        tr!(
                            "Still cancelling… {0}",
                            cell_format::duration(waited.as_millis() as u64)
                        ),
                        Some(
                            tr!(
                                "The server has not stopped the statement yet. The driver gives up on the cancellation after a few seconds and closes the session."
                            )
                            .into(),
                        ),
                    )
            } else {
                (
                    I::LoaderCircle,
                    colors().amber,
                    tr!("Cancelling…").into(),
                    None,
                )
            }
        } else if self.fetching {
            (
                I::LoaderCircle,
                colors().amber,
                tr!("Reading the next page…").into(),
                None,
            )
        } else if let Some(tab) = tab {
            match &tab.state {
                ResultState::Failed(report) => match state {
                    RunState::Failed { error } => (
                        I::CircleX,
                        colors().red,
                        report.title.clone(),
                        Some(error.clone()),
                    ),
                    _ => (I::CircleX, colors().red, report.title.clone(), None),
                },
                ResultState::Cancelled => (
                    I::Ban,
                    colors().amber,
                    tr!("Execution cancelled").into(),
                    None,
                ),
                ResultState::Running => (
                    I::LoaderCircle,
                    colors().amber,
                    tr!("Running…").into(),
                    None,
                ),
                ResultState::Done => page_summary(tab, self.filtered_rows),
            }
        } else {
            (I::Clock, colors().faint, tr!("Waiting to run").into(), None)
        };
        // The status region is announced when it changes: the clock is shown,
        // not read out every second.
        let announced = match state {
            RunState::Running { .. } => tr!("Running…").to_string(),
            RunState::Cancelling => tr!("Cancelling…").to_string(),
            _ => text.clone(),
        };
        let spinning = self.run.is_active() || self.fetching;
        let pages = tab.filter(|tab| {
            tab.state == ResultState::Done
                && (tab.has_next() || tab.has_previous() || tab.page_number() > 1)
        });
        row()
            .h(px(36.))
            .flex_none()
            .px_3()
            .border_t_1()
            .border_color(rgb(colors().border))
            .justify_between()
            .child(
                row()
                    .id("run-status")
                    .role(Role::Status)
                    .aria_label(announced)
                    .gap_2()
                    .when_some(detail, |s, detail| {
                        s.tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx))
                    })
                    .map(|s| {
                        if spinning {
                            s.child(spinning_icon("run-status-spinner", symbol, color))
                        } else {
                            s.child(icon(symbol, color))
                        }
                    })
                    .child(div().text_size(px(12.)).text_color(rgb(color)).child(text))
                    .when_some(
                        tab.filter(|tab| tab.state == ResultState::Done && !spinning),
                        |s, tab| {
                            s.child(label("·")).child(label(cell_format::duration(
                                tab.elapsed_ms.min(u64::MAX as u128) as u64,
                            )))
                        },
                    ),
            )
            .child(
                row()
                    .gap_2()
                    .when(self.has_results() && !self.run.is_running(), |s| {
                        let selected = self.selected_cells;
                        s.when(selected > 1, |s| {
                            s.child(label(tr!(
                                "{0} cells selected",
                                cell_format::count(selected)
                            )))
                            .when_some(self.selection_stats(), |s, (text, detail)| {
                                s.child(label("·")).child(
                                    div()
                                        .id("selection-stats")
                                        .role(Role::Status)
                                        .aria_label(detail.clone())
                                        .max_w(px(520.))
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(12.))
                                        .text_color(rgb(colors().muted))
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(detail.clone()).build(window, cx)
                                        })
                                        .child(text),
                                )
                            })
                            .child(label("·"))
                        })
                    })
                    .when_some(pages, |s, tab| {
                        s.child(
                            icon_button("previous-page", I::ChevronLeft, tr!("Previous page"))
                                .when(!tab.has_previous() || spinning, |s| s.opacity(0.4))
                                .on_click(cx.listener(|this, _, _, cx| this.previous_page(cx))),
                        )
                        .child(label(tr!("Page {0}", tab.page_number())))
                        .child(
                            icon_button("next-page", I::ChevronRight, tr!("Next page"))
                                .when(!tab.has_next() || spinning, |s| s.opacity(0.4))
                                .on_click(cx.listener(|this, _, _, cx| this.next_page(cx))),
                        )
                        .child(label("·"))
                    })
                    .child(label(if self.input.is_some() {
                        tr!("Server results")
                    } else if self.demo {
                        tr!("Demo data")
                    } else {
                        tr!("No connection")
                    }))
                    .when(self.input.is_some() || self.demo, |s| {
                        s.child(badge(
                            if self.input.is_some() { "SQL" } else { "MOCK" },
                            colors().faint,
                        ))
                    }),
            )
    }

    /// What the footer says about the numbers among the selected cells, and
    /// the longer text of its tooltip: sum, average, median, minimum and
    /// maximum, exact and in the number format on screen.
    fn selection_stats(&self) -> Option<(String, String)> {
        match &self.selection_summary {
            Summary::None => None,
            Summary::Computing => Some((
                tr!("Calculating…").to_owned(),
                tr!("Reading the selected cells to sum them…").to_owned(),
            )),
            Summary::Ready(summary) => {
                let numbers = summary.numbers.as_ref()?;
                let number = |text: Option<&str>| {
                    text.map_or_else(
                        || "—".to_owned(),
                        |text| {
                            cell_format::display(
                                Some(text),
                                cell_format::ValueFamily::Number,
                                self.numbers_pt_br,
                            )
                        },
                    )
                };
                let (sum, average, median) = (
                    number(numbers.sum.as_deref()),
                    number(numbers.average.as_deref()),
                    number(numbers.median.as_deref()),
                );
                let (min, max) = (number(Some(&numbers.min)), number(Some(&numbers.max)));
                Some((
                    tr!(
                        "Sum {0} · Average {1} · Median {2} · Min {3} · Max {4}",
                        sum,
                        average,
                        median,
                        min,
                        max
                    ),
                    tr!(
                        "{0} numbers of {1} values: sum {2}, average {3}, median {4}, minimum {5}, maximum {6}. Exact, from the numbers as the database wrote them.",
                        cell_format::count(numbers.count),
                        cell_format::count(summary.values),
                        sum,
                        average,
                        median,
                        min,
                        max
                    ),
                ))
            }
        }
    }

    fn result_messages(&self, cx: &mut Context<Self>) -> AnyElement {
        column()
            .id("result-messages")
            .size_full()
            .p_5()
            .gap_3()
            .child(label(if self.input.is_some() {
                match self.timeout {
                    Some(timeout) => tr!(
                        "Server execution · {0} rows per page · {1} timeout",
                        cell_format::count(self.row_limit as usize),
                        timeout_label(Some(timeout))
                    ),
                    None => tr!(
                        "Server execution · {0} rows per page · no timeout",
                        cell_format::count(self.row_limit as usize)
                    ),
                }
            } else if self.demo {
                tr!("Visual demo results.").into()
            } else {
                tr!("No connection").into()
            }))
            .map(|s| {
                if self.log.is_empty() {
                    s.child(label(tr!("Statements you run in this tab appear here.")))
                } else {
                    // Virtualized: the log holds up to hundreds of entries.
                    let now = SystemTime::now();
                    s.child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(
                                uniform_list(
                                    "run-log",
                                    self.log.len(),
                                    cx.processor(move |this, range: Range<usize>, _, _| {
                                        range
                                            .filter_map(|index| {
                                                Some(log_entry(index, this.log.newest(index)?, now))
                                            })
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .size_full()
                                .track_scroll(&self.log_scroll),
                            )
                            .vertical_scrollbar(&self.log_scroll),
                    )
                }
            })
            .into_any_element()
    }

    /// Shown when the tab was asked to run without a connection: running
    /// needs one, so the connections are offered here.
    fn connection_chooser(&self, cx: &mut Context<Self>) -> AnyElement {
        column()
            .id("connection-chooser")
            .role(Role::Alert)
            .aria_label(tr!("Choose a connection for this tab."))
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .p_4()
            .child(icon(I::Server, colors().amber))
            .child(
                div()
                    .text_size(px(14.))
                    .child(tr!("Choose a connection for this tab.")),
            )
            .child(label(tr!(
                "The tab has no connection, so its SQL cannot run. Its SQL stays when you choose one."
            )))
            .map(|s| {
                if self.connection_choices.is_empty() {
                    s.child(label(tr!(
                        "No connections yet. Add one in the explorer."
                    )))
                } else {
                    s.child(
                        row().flex_wrap().justify_center().gap_2().children(
                            self.connection_choices.iter().enumerate().map(|(index, name)| {
                                let chosen = name.clone();
                                row()
                                    .id(("choose-connection", index))
                                    .role(Role::Button)
                                    .aria_label(tr!("Run this tab on {0}", name))
                                    .focusable()
                                    .tab_index(0)
                                    .border_1()
                                    .border_color(rgb(colors().border))
                                    .focus_visible(|s| s.border_color(rgb(colors().mint)))
                                    .h(px(32.))
                                    .px(px(9.))
                                    .rounded(px(5.))
                                    .cursor_pointer()
                                    .text_size(px(14.))
                                    .text_color(rgb(colors().muted))
                                    .hover(|s| s.bg(rgb(colors().hover)).text_color(rgb(colors().text)))
                                    .child(icon(I::Server, colors().muted))
                                    .child(name.clone())
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(WorkspaceEvent::ConnectionRequested(
                                            chosen.clone(),
                                        ));
                                    }))
                            }),
                        ),
                    )
                }
            })
            .into_any_element()
    }

    /// What the filter searches, when it is open: only the rows held for
    /// the page on screen, which may not be the whole result.
    fn filter_scope(&self) -> Option<String> {
        let tab = self.results.active()?;
        let page = tab.current()?;
        let rows = cell_format::count(page.data.row_count());
        let partial =
            tab.has_next() || tab.page_number() > 1 || tab.rest_closed() || tab.limit.is_some();
        Some(if partial {
            tr!(
                "Filters this page only ({0} rows). Other pages and rows past the limit are not searched.",
                rows
            )
        } else {
            tr!("Filters the {0} rows of this result.", rows)
        })
    }

    /// The state of the session's transaction, with Commit and Rollback
    /// while one is open. Shown for drivers that run transactions.
    fn transaction_controls(&self, cx: &mut Context<Self>) -> Div {
        if !self.transactions || self.input.is_none() {
            return row();
        }
        let (symbol, color, text, detail) = match self.transaction {
            Transaction::AutoCommit => (
                I::CheckCheck,
                colors().faint,
                tr!("Auto-commit"),
                tr!("Each statement commits on its own. BEGIN opens a transaction."),
            ),
            Transaction::Open {
                changes: false,
                manual: true,
            } => (
                I::LockOpen,
                colors().amber,
                tr!("Manual commit"),
                tr!(
                    "Autocommit is off: statements wait for COMMIT. Closing the tab rolls them back."
                ),
            ),
            Transaction::Open { changes: false, .. } => (
                I::LockOpen,
                colors().amber,
                tr!("Transaction open"),
                tr!("A transaction is open in this tab's session. Closing the tab rolls it back."),
            ),
            Transaction::Open { changes: true, .. } => (
                I::CircleDot,
                colors().amber,
                tr!("Uncommitted changes"),
                tr!(
                    "Statements in the open transaction changed data that is not committed. Commit or roll back; closing the tab rolls it back."
                ),
            ),
            Transaction::Failed { .. } => (
                I::CircleAlert,
                colors().red,
                tr!("Transaction failed"),
                tr!(
                    "A statement failed inside the transaction. The server ignores the next statements until you roll back."
                ),
            ),
        };
        let open = self.transaction.is_open();
        let busy = self.is_busy();
        row()
            .gap_1()
            .child(
                row()
                    .id("transaction-state")
                    .role(Role::Status)
                    .aria_label(format!("{text}. {detail}"))
                    .gap_1()
                    .px_2()
                    .tooltip(move |window, cx| Tooltip::new(detail).build(window, cx))
                    .child(icon(symbol, color))
                    .child(div().text_size(px(12.)).text_color(rgb(color)).child(text)),
            )
            .when(open, |s| {
                s.child(
                    button("commit", I::Check, tr!("Commit"))
                        .when(busy, |s| s.opacity(0.5).cursor_default())
                        .tooltip(|window, cx| {
                            Tooltip::new(tr!("Commit the open transaction")).build(window, cx)
                        })
                        .on_click(
                            cx.listener(|this, _, _, cx| this.end_transaction("COMMIT;", cx)),
                        ),
                )
                .child(
                    button("rollback", I::Undo2, tr!("Rollback"))
                        .when(busy, |s| s.opacity(0.5).cursor_default())
                        .tooltip(|window, cx| {
                            Tooltip::new(tr!("Roll back the open transaction")).build(window, cx)
                        })
                        .on_click(
                            cx.listener(|this, _, _, cx| this.end_transaction("ROLLBACK;", cx)),
                        ),
                )
            })
            .child(divider())
    }

    /// Formats the selection, or the whole script without one, by the tab's
    /// dialect ([`SqlEditor::format`]); the outcome comes back as a notice.
    fn format_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sql_editor
            .update(cx, |editor, cx| editor.format(window, cx));
    }

    /// What the assistant panel asks of the tab.
    fn on_assistant_event(
        &mut self,
        event: &AssistantPanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            AssistantPanelEvent::Submit { text, mode } => {
                // The request's first message holds only what the facts
                // allow-list lets the tab tell (ADR-0028).
                let prompt = match mode {
                    AssistantMode::Ask => prompts::chat_message(
                        text,
                        self.results.active().and_then(RunFacts::of),
                        &self.context,
                    ),
                    AssistantMode::WriteSql => {
                        let current = self.report_sql(cx);
                        prompts::write_message(text, current.as_deref(), &self.context)
                    }
                };
                let display = text.clone();
                self.assistant_panel
                    .update(cx, |panel, cx| panel.start(display, prompt, None, cx));
            }
            AssistantPanelEvent::Insert(sql) => {
                // It goes where the cursor is, or in place of the selection;
                // the rest of the SQL stays, and Ctrl+Z takes it out.
                self.editor.update(cx, |editor, cx| {
                    editor.replace(sql.as_str(), window, cx);
                    editor.focus(window, cx);
                });
                self.notice = Some(Feedback::new(
                    Tone::Info,
                    tr!("Suggestion inserted at the cursor. Review the SQL before running it."),
                ));
            }
            AssistantPanelEvent::Replace {
                statement,
                origin,
                sql,
            } => match apply::locate_statement(&self.editor_sql, statement, *origin) {
                Some(range) => {
                    let text = apply::fit_replacement(statement, sql);
                    // Selecting the statement and replacing it is one undo step.
                    self.editor.update(cx, |editor, cx| {
                        editor.set_selected_range(range, cx);
                        editor.replace(text.as_str(), window, cx);
                        editor.focus(window, cx);
                    });
                    self.notice = Some(Feedback::new(
                        Tone::Info,
                        tr!("Statement replaced. Review it before running; Ctrl+Z undoes it."),
                    ));
                }
                None => {
                    self.notice = Some(Feedback::new(
                        Tone::Warning,
                        tr!(
                            "The failed statement changed in the editor; use Insert at the cursor instead."
                        ),
                    ));
                }
            },
            AssistantPanelEvent::Notice(notice) => self.notice = Some(notice.clone()),
            AssistantPanelEvent::OpenPreferences => cx.emit(WorkspaceEvent::OpenPreferences),
            AssistantPanelEvent::Close => self.assistant_open = false,
            AssistantPanelEvent::RunQuery {
                sql,
                ticket,
                cancel,
            } => self.run_assistant_query(
                AssistantQuery {
                    sql: sql.clone(),
                    ticket: *ticket,
                    cancel: cancel.clone(),
                },
                cx,
            ),
        }
        cx.notify();
    }

    /// Runs a statement the user approved for the assistant, in the
    /// assistant's own session, and hands the panel its result cut to what
    /// may be sent ([`QueryPreview`]); the panel then asks the user whether
    /// to send it. It is not a result tab and not in the history.
    fn run_assistant_query(&mut self, query: AssistantQuery, cx: &mut Context<Self>) {
        let Some(input) = self.input.clone() else {
            let error = DriverError::new(
                ErrorCode::Connection,
                tr!("Choose a connection for this tab."),
            );
            let preview = QueryPreview::from_error(&error, &query.sql);
            self.assistant_panel.update(cx, |panel, cx| {
                panel.query_done(query.ticket, Some(preview), cx)
            });
            return;
        };
        if connection_store::needs_password(&input) {
            // The host asks for the password, then resumes this query.
            self.pending_assistant = Some(query);
            cx.emit(WorkspaceEvent::PasswordRequired);
            return;
        }
        let AssistantQuery {
            sql,
            ticket,
            cancel,
        } = query;
        let session = self.assistant_session.take();
        let lease = self.assistant_lease;
        let connection = self.cache_id;
        let context = self.context.clone();
        let request = QueryRequest::new(
            sql.clone(),
            ExecutionOptions {
                max_rows: released::READ_ROWS,
                ..self.options()
            },
        );
        cx.spawn(async move |weak, cx| {
            // The result becomes a preview on the runtime; its rows are
            // dropped there.
            let outcome = backend::run(async move {
                let stopped = cancel.clone();
                let run = backend::query_in_context(
                    connection,
                    input,
                    context,
                    session,
                    vec![request],
                    ASSISTANT_MEMORY_CEILING,
                    cancel,
                )
                .await;
                let (preview, session) = match run {
                    Ok(run) => {
                        let preview = match &run.batch {
                            Ok(batch) => QueryPreview::from_batch(batch, &sql),
                            Err(error) => QueryPreview::from_error(error, &sql),
                        };
                        (preview, run.session)
                    }
                    Err(error) => (QueryPreview::from_error(&error, &sql), None),
                };
                Ok(((!stopped.is_cancelled()).then_some(preview), session))
            })
            .await;
            let (preview, session) = outcome.unwrap_or((None, None));
            let mut session = session;
            let _ = weak.update(cx, |this, cx| {
                if this.assistant_lease == lease && this.assistant_session.is_none() {
                    this.assistant_session = session.take();
                }
                this.assistant_panel
                    .update(cx, |panel, cx| panel.query_done(ticket, preview, cx));
            });
            if let Some(session) = session {
                backend::close_in_background(session);
            }
        })
        .detach();
    }

    /// Closes the assistant's session: the connection changed or went away.
    fn drop_assistant_session(&mut self) {
        self.assistant_lease += 1;
        if let Some(session) = self.assistant_session.take() {
            backend::close_in_background(session);
        }
    }

    /// Tells the panel which connection's cache its tools read, and where
    /// names resolve.
    fn push_assistant_target(&mut self, cx: &mut Context<Self>) {
        let target = match (&self.input, self.cache_id) {
            (Some(input), Some(connection)) => Some(SchemaScope {
                dir: connection_store::data_dir(),
                connection,
                config: input.config.clone(),
                engine: self.dialect.to_string(),
                dialect: self.run.dialect(),
                context: self.context.clone(),
            }),
            _ => None,
        };
        let label = self.connection.clone();
        self.assistant_panel
            .update(cx, |panel, cx| panel.set_target(target, label, cx));
    }

    fn push_assistant_context(&mut self, cx: &mut Context<Self>) {
        let context = self.context.clone();
        self.assistant_panel
            .update(cx, |panel, _| panel.set_context(context));
    }

    /// Whether **Fix with AI** is offered: a real tab with the assistant set
    /// up.
    fn assistant_ready(&self, cx: &App) -> bool {
        !self.demo
            && self.input.is_some()
            && super::assistant_settings::preferences(cx)
                .config()
                .is_some()
    }

    /// Asks the assistant to fix the failed statement of result `id`: the
    /// statement and the server's error, nothing from any result.
    fn fix_with_ai(&mut self, id: ResultId, cx: &mut Context<Self>) {
        let Some(tab) = self.results.get(id) else {
            return;
        };
        let ResultState::Failed(report) = &tab.state else {
            return;
        };
        let facts = ErrorFacts::new(report, &tab.sql);
        let prompt = prompts::fix_message(&tab.sql, &facts, &self.context);
        let fix = FixTarget {
            statement: tab.sql.clone(),
            origin: tab.origin,
        };
        self.assistant_open = true;
        self.assistant_panel.update(cx, |panel, cx| {
            panel.start(tr!("Fix this statement").to_owned(), prompt, Some(fix), cx)
        });
        cx.notify();
    }
}

/// The memory one query of the assistant may read: far more than the
/// [`released::MAX_ROWS`] rows it may send, far less than a tab's result.
const ASSISTANT_MEMORY_CEILING: usize = 4 << 20;

/// A statement the user approved for the assistant, on its way.
struct AssistantQuery {
    sql: String,
    ticket: QueryTicket,
    cancel: CancellationToken,
}

/// What the editor header says about the tab's session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionIndicator {
    /// A session is open and idle: a click disconnects.
    Connected,
    /// A run or a page read has the session out (or is opening one).
    InUse,
    /// No session: the next run opens one.
    Disconnected,
}

impl SessionIndicator {
    fn of(session: bool, busy: bool) -> Self {
        if busy {
            Self::InUse
        } else if session {
            Self::Connected
        } else {
            Self::Disconnected
        }
    }
}

pub enum WorkspaceEvent {
    /// The assistant needs setting up: show Preferences.
    OpenPreferences,
    /// State worth saving changed (SQL, timeout, result tabs).
    Changed,
    /// A run needs the connection's password, which was not remembered.
    /// The host asks for it and calls [`Workspace::resume_pending`].
    PasswordRequired,
    /// A run on a production connection holds statements that change every
    /// row of a table or drop data. The host asks and, on a yes, calls
    /// [`Workspace::confirm_risk`].
    ConfirmRisk {
        connection: String,
        description: String,
    },
    /// The user chose another connection for this tab in the toolbar.
    ConnectionRequested(String),
    /// Save the SQL in the library: as the saved query `id` when the tab
    /// edits one, otherwise as a new one, which the host first asks a name
    /// and tags for. The host answers with [`Workspace::saved`].
    Saved {
        id: Option<u64>,
        title: String,
        connection: String,
        sql: String,
    },
    /// A statement ended, in success, failure or cancellation: an entry for
    /// the history.
    Executed(jykre::workspace_state::QueryEntry),
    /// Start a report from this SQL (the selection, the statement at the
    /// cursor or the whole script); the host opens it as a draft.
    SaveAsReport {
        title: String,
        connection: String,
        sql: String,
        /// The tab's database, where the report's panel runs.
        context: SessionContext,
    },
    /// Add this SQL as a panel of a saved report of the same connection,
    /// chosen by the user.
    AddToReport {
        connection: String,
        sql: String,
        context: SessionContext,
    },
    /// Statements that ran changed the structure of these last-level
    /// namespaces (`CREATE`, `ALTER`, `DROP`, ...), so their metadata is
    /// stale.
    StructureChanged {
        connection: String,
        namespaces: Vec<ObjectPath>,
    },
    /// Go to definition chose this object: the explorer shows it.
    Reveal {
        connection: String,
        path: ObjectPath,
    },
}
impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn rebind_connection(
        &mut self,
        name: String,
        input: Option<ResolvedConnection>,
        row_limit: u32,
        cx: &mut Context<Self>,
    ) {
        self.connection = name.into();
        self.choosing_connection = false;
        self.pending_run = None;
        // The table it was opened for belongs to the previous connection.
        self.object = None;
        self.data_sql = None;
        if let Some(input) = input {
            self.set_connection(input, row_limit, cx);
        } else {
            self.abandon_run();
            self.drop_assistant_session();
            self.input = None;
            self.sql_editor
                .update(cx, |editor, _| editor.set_connection(None));
            self.results.clear();
            self.show_active(cx);
        }
        self.update_grid_sql(cx);
        self.notice = Some(Feedback::new(
            Tone::Warning,
            tr!("The tab's connection changed. Review the SQL before running it."),
        ));
        cx.notify();
    }

    pub fn connection_name(&self) -> &str {
        self.connection.as_ref()
    }

    /// What this tab keeps between sessions.
    pub fn state(&self) -> jykre::workspace_state::TabState {
        jykre::workspace_state::TabState {
            title: self.title.to_string(),
            connection: self.connection.to_string(),
            sql: self.editor_sql.clone(),
            saved_sql: self.saved_sql.clone(),
            saved_id: self.saved_id,
            timeout_seconds: jykre::workspace_state::TabState::store_timeout(self.timeout),
            opened_sql: self.opened_sql.clone(),
            catalog: self.context.catalog.clone(),
            schema: self.context.schema.clone(),
            object: self.object.clone(),
            parameters: self.parameters.fields.clone(),
            data_sql: self.data_sql.clone(),
        }
    }

    /// Closing the tab would lose SQL that is not in the library.
    pub fn has_unsaved_sql(&self) -> bool {
        jykre::workspace_state::has_unsaved_sql(
            &self.editor_sql,
            self.saved_sql.as_deref(),
            self.opened_sql.as_deref(),
        )
    }

    /// Restores what [`Workspace::state`] kept.
    pub fn restore(&mut self, state: &jykre::workspace_state::TabState, cx: &mut Context<Self>) {
        self.saved_sql = state.saved_sql.clone();
        self.saved_id = state.saved_id;
        self.object = state.object.clone();
        self.data_sql = state.data_sql.clone();
        self.opened_sql = state.opened_sql.clone();
        self.restore_parameters(&state.parameters);
        self.update_grid_sql(cx);
        self.timeout = state.timeout(self.timeout);
        // Applied to the session the next run opens.
        self.context = SessionContext {
            catalog: state.catalog.clone(),
            schema: state.schema.clone(),
        };
        let context = self.context.clone();
        self.sql_editor
            .update(cx, |editor, _| editor.set_context(context));
        self.push_assistant_context(cx);
    }

    /// What the query tab shows before its title: the run in progress, or
    /// how the last one ended.
    pub fn tab_status(&self) -> jykre::grouped_tabs::TabStatus {
        self.run.state().tab_status()
    }

    /// A statement, a page read or an export that reads from the session is
    /// in progress.
    pub fn is_busy(&self) -> bool {
        self.run.is_active() || self.fetching || self.export_uses_session()
    }

    /// Applies a saved connection's new name, password, row limit or
    /// settings. Nothing on screen is lost: results stay, and a session
    /// opened with other settings is replaced on the next run. Only a run
    /// in progress or an open transaction is ended, which the host confirmed
    /// with the user before saving.
    pub fn update_connection(
        &mut self,
        name: String,
        input: ResolvedConnection,
        row_limit: u32,
        cx: &mut Context<Self>,
    ) {
        self.connection = name.into();
        // The new limit applies from the next run.
        self.row_limit = row_limit;
        // The assistant's next query opens a session with the new settings.
        self.drop_assistant_session();
        let same_server = self
            .input
            .as_ref()
            .is_some_and(|current| current.config == input.config);
        if same_server || self.input.is_none() {
            if self.input.is_none() {
                self.describe_connection(&input, cx);
            }
            self.input = Some(input);
            cx.notify();
            return;
        }
        if self.is_busy() || self.transaction.is_open() {
            self.abandon_run();
        } else if self.session.is_some() {
            self.stale_session = true;
        }
        self.describe_connection(&input, cx);
        self.input = Some(input);
        self.forget_server(cx);
        self.notice = Some(Feedback::new(
            Tone::Info,
            tr!(
                "The connection settings changed. The next run opens a session with them; the results on screen came from the previous settings."
            ),
        ));
        cx.notify();
    }

    /// A transaction is open in the tab's session: closing, moving or
    /// reconfiguring the tab rolls it back.
    pub fn has_open_transaction(&self) -> bool {
        self.transaction.is_open()
    }

    /// Runs what waited for the connection's password, now that the host
    /// has it.
    pub fn resume_pending(&mut self, cx: &mut Context<Self>) {
        if let Some((scope, placement)) = self.pending_run.take() {
            self.execute_queries(scope, placement, cx);
        }
        if let Some(query) = self.pending_assistant.take() {
            self.run_assistant_query(query, cx);
        }
    }

    /// Runs what waited for the user to confirm its risky statements.
    pub fn confirm_risk(&mut self, cx: &mut Context<Self>) {
        if let Some((scope, placement)) = self.risky_run.take() {
            self.risk_confirmed = true;
            self.execute_queries(scope, placement, cx);
            self.risk_confirmed = false;
        }
    }

    /// The colour of the tab's connection and its environment, as the
    /// explorer shows them.
    pub fn set_appearance(
        &mut self,
        color: Option<jykre::connection_appearance::ConnectionColor>,
        environment: Option<jykre::connection_appearance::Environment>,
        cx: &mut Context<Self>,
    ) {
        self.connection_color = color;
        self.environment = environment;
        let production = environment == Some(jykre::connection_appearance::Environment::Production);
        self.assistant_panel
            .update(cx, |panel, cx| panel.set_production(production, cx));
        cx.notify();
    }

    /// The tab's connection was deleted. A run in progress stops and the
    /// session closes (rolling back an open transaction, which the host
    /// confirmed with the user); the SQL and the results stay, and nothing
    /// runs until another connection is chosen.
    pub fn connection_deleted(&mut self, cx: &mut Context<Self>) {
        self.abandon_run();
        self.drop_assistant_session();
        self.input = None;
        self.transactions = false;
        self.pending_run = None;
        self.risky_run = None;
        self.notice = Some(Feedback::new(
            Tone::Warning,
            tr!(
                "The connection {0} was deleted. Choose another before running this SQL.",
                self.connection
            ),
        ));
        cx.notify();
    }

    /// Forgets the run that waited for a password the user did not give;
    /// the assistant's query waiting for it does not run.
    pub fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        self.pending_run = None;
        if let Some(query) = self.pending_assistant.take() {
            self.assistant_panel
                .update(cx, |panel, cx| panel.query_done(query.ticket, None, cx));
        }
    }

    /// Opens the tab without its connection, which no longer exists, and
    /// says so.
    pub fn warn_missing_connection(&mut self, cx: &mut Context<Self>) {
        self.notice = Some(Feedback::new(
            Tone::Warning,
            tr!(
                "The connection {0} no longer exists. Choose another before running this SQL.",
                self.connection
            ),
        ));
        cx.notify();
    }

    /// Stops the run and forgets it: its outcome no longer applies. The
    /// tab's session is closed, and one still out with the run is closed
    /// when it comes back.
    fn abandon_run(&mut self) {
        self.cancellation.cancel();
        // An export that has the session out stops with it; one that only
        // writes the rows held goes on.
        if self.export_uses_session() {
            self.export.cancel();
        }
        self.run.reset();
        self.results.cancel_running();
        self.session_lease = None;
        self.fetching = false;
        self.stale_session = false;
        // Closing the session rolls back its transaction.
        self.transaction = Transaction::AutoCommit;
        if let Some(session) = self.session.take() {
            backend::close_in_background(session);
        }
    }

    /// Tells the user when the session state they may rely on (current
    /// schema, temporary tables, an open transaction) started over.
    fn session_notice(&mut self, reopened: bool, ended: bool) {
        if ended {
            self.notice = Some(Feedback::new(
                Tone::Warning,
                tr!(
                    "The session was closed. The next run opens a new one, and session settings such as the current schema start over."
                ),
            ));
        } else if reopened {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!(
                    "The previous session no longer answered, so this run used a new one. Session settings such as the current schema started over."
                ),
            ));
        }
    }

    /// Binds the tab to another server. Any run is abandoned and the
    /// previous result cleared.
    pub fn set_connection(
        &mut self,
        input: ResolvedConnection,
        row_limit: u32,
        cx: &mut Context<Self>,
    ) {
        self.abandon_run();
        self.row_limit = row_limit;
        self.choosing_connection = false;
        self.describe_connection(&input, cx);
        self.input = Some(input);
        self.forget_server(cx);
        self.assistant_open = false;
        self.results.clear();
        self.show_active(cx);
    }

    /// Shows what the connection is (its dialect, database and whether it
    /// runs transactions) and splits and highlights the SQL by its dialect.
    fn describe_connection(&mut self, input: &ResolvedConnection, cx: &mut Context<Self>) {
        let descriptor = builtin_registry()
            .and_then(|registry| registry.get(input.config.driver))
            .map(|driver| driver.descriptor());
        let dialect = descriptor
            .as_ref()
            .ok()
            .map(|descriptor| descriptor.dialect);
        if let Ok(descriptor) = &descriptor {
            self.dialect = descriptor.display_name.clone().into();
            self.transactions = descriptor.capabilities.transactions;
        }
        self.database = input.config.options["database"]
            .as_str()
            .or(input.config.options["defaultCatalog"].as_str())
            .unwrap_or("—")
            .to_owned();
        self.sql_editor.update(cx, |editor, cx| {
            editor.set_connection(Some(&input.config));
            editor.set_dialect(dialect, cx);
        });
        if dialect != self.run.dialect() {
            self.run.set_dialect(dialect);
            self.statement_lines = jykre::sql::statement_lines(&self.editor_sql, dialect).into();
            // A colon means another thing in each dialect.
            self.sync_parameters();
        }
        self.update_grid_sql(cx);
    }

    /// Another server: its namespaces and the names read from its cache are
    /// not this one's.
    fn forget_server(&mut self, cx: &mut Context<Self>) {
        self.drop_assistant_session();
        self.context = SessionContext::default();
        self.context_choices.clear();
        self.sql_editor
            .update(cx, |editor, _| editor.forget_server());
        self.cache_id = None;
        self.push_assistant_target(cx);
    }

    /// Puts the keyboard in the SQL editor.
    /// Shows the connections to choose from in the results area, for a tab
    /// opened without one.
    pub fn ask_for_connection(&mut self, cx: &mut Context<Self>) {
        if self.input.is_none() && !self.demo {
            self.choosing_connection = true;
            self.result_view = ResultView::Table;
            cx.notify();
        }
    }

    /// The tab has a connection to run on (the demo's simulated one too),
    /// so it may hold a session and results.
    pub fn has_connection(&self) -> bool {
        self.input.is_some() || self.demo
    }

    pub fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
    }

    pub fn toggle_assistant(&mut self, cx: &mut Context<Self>) {
        self.assistant_open = !self.assistant_open;
        cx.notify();
    }

    fn query_area(&self, cx: &mut Context<Self>) -> Div {
        column()
            .size_full()
            .overflow_hidden()
            .child(self.toolbar(cx))
            .child(
                div().flex_1().min_h_0().child(
                    v_resizable("query-results-layout")
                        .with_state(&self.query_split)
                        .with_handle_appearance(splitter(
                            &self.query_split,
                            0,
                            tr!("SQL editor height. Use the arrows to adjust."),
                            200.0..900.0,
                        ))
                        .child(
                            resizable_panel()
                                .size(px(360.))
                                .size_range(px(200.)..Pixels::MAX)
                                .child(self.editor_panel(cx)),
                        )
                        .child(
                            resizable_panel()
                                .size_range(px(210.)..Pixels::MAX)
                                .child(self.result_panel(cx)),
                        ),
                ),
            )
            .when_some(self.notice.clone(), |s, notice| {
                s.child(
                    row()
                        .id("query-notice")
                        .role(if notice.tone == Tone::Error {
                            Role::Alert
                        } else {
                            Role::Status
                        })
                        .aria_label(notice.text.clone())
                        .h(px(38.))
                        .flex_none()
                        .px_3()
                        .bg(rgb(notice.tone.background()))
                        .justify_between()
                        .child(
                            row()
                                .flex_1()
                                .child(icon(notice.tone.icon(), notice.tone.color()))
                                .child(
                                    div()
                                        .id("query-notice-text")
                                        .flex_1()
                                        .text_size(px(14.))
                                        .text_color(rgb(notice.tone.color()))
                                        .truncate()
                                        .tooltip({
                                            let text = notice.text.clone();
                                            move |window, cx| {
                                                Tooltip::new(text.clone()).build(window, cx)
                                            }
                                        })
                                        .child(notice.text),
                                ),
                        )
                        .child(
                            icon_button("dismiss-notice", I::X, tr!("Close notice")).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.notice = None;
                                    cx.notify();
                                }),
                            ),
                        ),
                )
            })
    }
}

/// The pin of a result tab: mint while pinned, when no run replaces it.
fn pin_button(index: usize, pinned: bool) -> Stateful<Div> {
    let title = if pinned {
        tr!("Unpin result")
    } else {
        tr!("Pin result: no run replaces it")
    };
    div()
        .id(("pin-result", index))
        .role(Role::Button)
        .aria_label(title)
        .aria_toggled(if pinned {
            Toggled::True
        } else {
            Toggled::False
        })
        .focusable()
        .tab_index(0)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(|s| s.border_color(rgb(colors().mint)))
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .cursor_pointer()
        .tooltip(move |window, cx| Tooltip::new(title).build(window, cx))
        .hover(|s| s.bg(rgb(colors().hover)))
        .child(icon(
            I::Pin,
            if pinned {
                colors().mint
            } else {
                colors().faint
            },
        ))
}

/// How a statement that failed or was cancelled reads in Messages.
fn logged_failure(error: &DriverError) -> LogOutcome {
    if error.code == ErrorCode::Cancelled {
        return LogOutcome::Cancelled;
    }
    let report = ErrorReport::new(error);
    LogOutcome::Failed(format!(
        "{}: {}",
        report.title,
        report.message.lines().next().unwrap_or_default()
    ))
}

/// One line of the run log: how the statement ended, how long it took and
/// how long ago, with the statement below.
fn log_entry(index: usize, entry: &LogEntry, now: SystemTime) -> Stateful<Div> {
    let (symbol, color, outcome) = match &entry.outcome {
        LogOutcome::Rows { rows, more } => (
            I::CircleCheck,
            colors().mint,
            match (*rows, *more) {
                (rows, true) => tr!("{0} rows on the first page", cell_format::count(rows)),
                (1, false) => tr!("1 row").to_string(),
                (rows, false) => tr!("{0} rows", cell_format::count(rows)),
            },
        ),
        LogOutcome::Affected(affected) => (
            I::CircleCheck,
            colors().mint,
            tr!("{0} rows affected", affected),
        ),
        LogOutcome::Completed => (
            I::CircleCheck,
            colors().mint,
            tr!("Statement completed").into(),
        ),
        LogOutcome::Failed(error) => (I::CircleX, colors().red, error.clone()),
        LogOutcome::Cancelled => (I::Ban, colors().amber, tr!("Execution cancelled").into()),
    };
    let age = jykre::explorer::format_age(now.duration_since(entry.at).unwrap_or_default());
    let timing = match entry.elapsed_ms {
        Some(elapsed) => format!(
            "{} · {age}",
            cell_format::duration(elapsed.min(u64::MAX as u128) as u64)
        ),
        None => age,
    };
    column()
        .id(("run-log-entry", index))
        .role(Role::ListItem)
        .aria_label(format!("{outcome} · {timing} · {}", entry.sql))
        .h(px(48.))
        .flex_none()
        .gap_1()
        .child(
            row()
                .gap_2()
                .child(icon(symbol, color))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.))
                        .text_color(rgb(color))
                        .child(outcome),
                )
                .child(label(timing)),
        )
        .child(
            div()
                .pl(px(24.))
                .text_size(px(12.))
                .text_color(rgb(colors().faint))
                .font_family("Cascadia Code")
                .truncate()
                .child(entry.sql.clone()),
        )
}

/// Why a run did not start: the previous one is still stopping.
fn stopping_notice() -> String {
    tr!("The previous run is still stopping. Run again when it ends.").into()
}

/// The icon, colour and words for how a result tab ended.
fn result_status(tab: &ResultTab) -> (I, u32, String) {
    match &tab.state {
        ResultState::Running => (I::LoaderCircle, colors().amber, tr!("Running…").into()),
        ResultState::Failed(report) => (
            I::CircleX,
            colors().red,
            format!(
                "{}: {}",
                report.title,
                report.message.lines().next().unwrap_or_default()
            ),
        ),
        ResultState::Cancelled => (I::Ban, colors().amber, tr!("Execution cancelled").into()),
        ResultState::Done => {
            let (_, _, text, _) = page_summary(tab, None);
            (I::CircleCheck, colors().mint, text)
        }
    }
}

/// The footer text for a completed result's page: its rows (as a range when
/// the result has several pages, as "3 of 1,000" while the filter shows
/// `filtered` of them), the rows affected and the limit that ended reading,
/// with an explanation for the tooltip.
fn page_summary(tab: &ResultTab, filtered: Option<usize>) -> (I, u32, String, Option<String>) {
    let Some(page) = tab.current() else {
        return (
            I::CircleCheck,
            colors().mint,
            match tab.rows_affected {
                Some(affected) => tr!("{0} rows affected", affected),
                None => tr!("Statement completed").into(),
            },
            None,
        );
    };
    let paged = tab.page_number() > 1 || tab.has_next();
    let rows =
        jykre::result_tabs::rows_label(page.data.row_count(), page.first_row, paged, filtered);
    match tab.limit {
        Some(ResultLimit::Memory(bytes)) => (
            I::TriangleAlert,
            colors().amber,
            tr!("{0} · memory limit reached", rows),
            Some(tr!(
                "The page reached the memory limit of {0} MB before the row limit, so the rest of the result was not read. Refine the SQL to get other rows.",
                bytes / (1024 * 1024)
            )),
        ),
        _ if tab.rest_closed() => (
            I::TriangleAlert,
            colors().amber,
            tr!("{0} · more rows were not read", rows),
            Some(
                tr!(
                    "The session ran another statement, which closed this result. Run the statement again to read further pages."
                )
                .into(),
            ),
        ),
        _ => {
            let text = match tab.rows_affected {
                Some(affected) => tr!("{0} · {1} rows affected", rows, affected),
                None => rows,
            };
            (I::CircleCheck, colors().mint, text, None)
        }
    }
}

/// After this long, a cancellation the driver has not finished is shown as
/// slow, with what happens next.
const CANCEL_SLOW: Duration = Duration::from_secs(5);

/// The statement timeouts a tab can choose; the default is five minutes.
const TIMEOUTS: [Option<Duration>; 6] = [
    Some(Duration::from_secs(30)),
    Some(Duration::from_secs(60)),
    Some(Duration::from_secs(300)),
    Some(Duration::from_secs(900)),
    Some(Duration::from_secs(3600)),
    None,
];

/// How a statement timeout reads in the toolbar and messages.
fn timeout_label(timeout: Option<Duration>) -> String {
    match timeout.map(|timeout| timeout.as_secs()) {
        None => tr!("No timeout").into(),
        Some(seconds) if seconds % 3600 == 0 => tr!("{0} h", seconds / 3600),
        Some(seconds) if seconds % 60 == 0 => tr!("{0} min", seconds / 60),
        Some(seconds) => tr!("{0} s", seconds),
    }
}

/// The gutter button of the statement at `index`: it runs that statement,
/// or stops the run while the statement is running.
fn run_button(
    index: usize,
    status: StatementStatus,
    workspace: WeakEntity<Workspace>,
) -> AnyElement {
    let (symbol, color, title) = match status {
        StatementStatus::Ready => (I::Play, colors().mint, tr!("Run this query")),
        StatementStatus::Pending => (
            I::Clock,
            colors().faint,
            tr!("Waiting for the statements before it · stop the run"),
        ),
        StatementStatus::Running => (I::Square, colors().amber, tr!("Stop execution")),
        StatementStatus::Success => (I::CircleCheck, colors().mint, tr!("Success · run again")),
        StatementStatus::Error => (I::CircleX, colors().red, tr!("Error · run again")),
        StatementStatus::Cancelled => (I::Play, colors().faint, tr!("Cancelled · run again")),
    };
    div()
        .id(("query-run", index))
        .role(Role::Button)
        .aria_label(tr!("Query {0}: {1}", index + 1, title))
        .cursor_pointer()
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .hover(|s| s.bg(rgb(colors().hover)))
        .tooltip(move |window, cx| Tooltip::new(title).build(window, cx))
        .child(icon(symbol, color))
        .on_click(move |_, _, cx| {
            let _ = workspace.update(cx, |this, cx| {
                if matches!(
                    this.run.status(index),
                    StatementStatus::Running | StatementStatus::Pending
                ) {
                    this.stop_execution(cx);
                } else {
                    this.execute_queries(Scope::Statement(index), Placement::ReplaceActive, cx);
                }
            });
        })
        .into_any_element()
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::probe::rendered("Workspace");
        self.ensure_parameter_inputs(window, cx);
        let query = self.query_area(cx);
        let content = if self.assistant_open {
            h_resizable("assistant-layout")
                .with_state(&self.assistant_split)
                .with_handle_appearance(splitter(
                    &self.assistant_split,
                    1,
                    tr!("Assistant width. Use the arrows to adjust."),
                    280.0..560.0,
                ))
                .child(
                    resizable_panel()
                        .size_range(px(EDITOR_MIN_WIDTH)..Pixels::MAX)
                        .child(query),
                )
                .child(
                    resizable_panel()
                        .size(px(ASSISTANT_WIDTH))
                        .size_range(px(280.)..px(560.))
                        .fixed()
                        // A streamed answer redraws the panel alone.
                        .child(
                            self.assistant_panel
                                .clone()
                                .cached(StyleRefinement::default().size_full()),
                        ),
                )
                .into_any_element()
        } else {
            query.into_any_element()
        };
        column()
            .id("query-workspace")
            .size_full()
            .bg(rgb(colors().bg))
            .child(content)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.export.cancel();
        if let Some(session) = self.session.take() {
            backend::close_in_background(session);
        }
        if let Some(session) = self.assistant_session.take() {
            backend::close_in_background(session);
        }
    }
}

/// A result tab being dragged to another place in its bar.
struct ResultTabDrag {
    id: ResultId,
    title: SharedString,
}

/// What follows the pointer while a result tab is dragged.
struct ResultTabPreview {
    title: SharedString,
}

impl Render for ResultTabPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        row()
            .h(px(28.))
            .px_3()
            .rounded(px(5.))
            .bg(rgb(colors().surface))
            .border_1()
            .border_color(rgb(colors().mint))
            .text_size(px(12.))
            .text_color(rgb(colors().text))
            .child(self.title.clone())
    }
}

/// What the confirmation before a risky run on production says: each
/// risky statement's kind, numbered as the run orders them.
pub(super) fn risk_description(risks: &[(usize, jykre::guard::Risk)], total: usize) -> String {
    use jykre::guard::Risk;
    let lines: Vec<String> = risks
        .iter()
        .map(|(index, risk)| {
            let what = match risk {
                Risk::EveryRow { verb } => {
                    tr!("{0} without WHERE changes every row of the table", verb)
                }
                Risk::Drops { verb } => tr!("{0} removes data or structure", verb),
            };
            if total > 1 {
                tr!("Statement {0}: {1}.", index + 1, what)
            } else {
                format!("{what}.")
            }
        })
        .collect();
    tr!(
        "This connection is marked as production. {0} Run it anyway?",
        lines.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::SessionIndicator;

    #[test]
    fn the_header_offers_to_disconnect_only_an_idle_open_session() {
        assert_eq!(
            SessionIndicator::of(true, false),
            SessionIndicator::Connected
        );
        // A run has the session out, or is opening one.
        assert_eq!(SessionIndicator::of(false, true), SessionIndicator::InUse);
        assert_eq!(SessionIndicator::of(true, true), SessionIndicator::InUse);
        assert_eq!(
            SessionIndicator::of(false, false),
            SessionIndicator::Disconnected
        );
    }
}
