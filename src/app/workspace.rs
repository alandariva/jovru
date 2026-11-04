use super::demo::{self, OPTIMIZED_SQL, SALES, SALES_SQL, sample_result};
use super::layout::{ASSISTANT_WIDTH, EDITOR_MIN_WIDTH, splitter};
use super::result_grid::{ResultGrid, ResultGridEvent};
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
use jykre::completion::{CandidateKind, NameIndex, Symbol};
use jykre::tr;
use jykre::{
    backend, cell_format, connection_store,
    drivers::*,
    error_report::ErrorReport,
    query::{Outcome, QueryRun, RunState, Scope, ShortcutAction, StatementStatus},
    result::{DEFAULT_MEMORY_CEILING, DEFAULT_ROW_LIMIT, ResultData, ResultLimit},
    result_tabs::{Completed, Placement, ResultId, ResultState, ResultTab, ResultTabs},
    run_log::{LogEntry, LogOutcome, RunLog},
    transaction::Transaction,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Key context of the SQL editor, for the run shortcuts.
pub const SQL_EDITOR_CONTEXT: &str = "SqlEditor";

#[derive(Clone, Copy, PartialEq)]
enum ResultView {
    Table,
    Chart,
    Messages,
}

struct Message {
    user: bool,
    text: String,
    sql: Option<&'static str>,
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
    cancellation: CancellationToken,
    numbers_pt_br: bool,
    editor: Entity<EditorState>,
    assistant_split: Entity<ResizableState>,
    query_split: Entity<ResizableState>,
    prompt: Entity<InputState>,
    connection: SharedString,
    dialect: SharedString,
    database: String,
    connection_color: u32,
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
    /// The connection's cache this tab reads names from, and its name
    /// index; empty until the explorer has read the cache.
    cache_id: Option<ConnectionId>,
    names: Arc<NameIndex>,
    /// The columns read so far for the index's relations, and those being
    /// read.
    columns: HashMap<ObjectPath, Vec<MetadataObject>>,
    columns_loading: HashSet<ObjectPath>,
    /// The result grid, its own entity ([ADR-0012](../../docs/adr/0012-result-grid-as-a-cached-entity.md)).
    grid: Entity<ResultGrid>,
    /// Visible cells selected in the grid, as it last reported.
    selected_cells: usize,
    result_view: ResultView,
    assistant_open: bool,
    messages: Vec<Message>,
    /// The run state and the status of each statement in the editor.
    run: QueryRun,
    editor_sql: String,
    /// The line where each statement of `editor_sql` starts and its line
    /// count, kept when the SQL changes so a frame never splits the script.
    statement_lines: Rc<[usize]>,
    line_count: usize,
    /// SQL as last saved to the library, if this tab is saved.
    saved_sql: Option<String>,
    /// The SQL the tab opened with; closing loses nothing while the editor
    /// still holds it.
    opened_sql: Option<String>,
    notice: Option<Feedback>,
    filter_open: bool,
    filter: Entity<InputState>,
    chat_scroll: ScrollHandle,
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
                .default_value(sql)
        });
        set_highlighter(&editor, None, cx);
        let workspace = cx.entity().downgrade();
        editor.update(cx, |editor, _| {
            // Completion, hover and go to definition from the tab's connection.
            let providers = super::sql_providers::SqlProviders::new(workspace.clone());
            let lsp = editor.lsp_mut();
            lsp.completion_provider = Some(providers.clone());
            lsp.hover_provider = Some(providers.clone());
            lsp.definition_provider = Some(providers);
            lsp.show_document = Some(super::sql_providers::show_object(workspace));
        });
        let prompt =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Ask about your data…")));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Filter results…")));
        let prompt_subscription = cx.subscribe_in(
            &prompt,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit_prompt(window, cx);
                }
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
            .map_or_else(|| Rc::new(ResultData::default()), |page| page.data.clone());
        let grid = cx.new(|cx| ResultGrid::new(first, sales, cx));
        // The grid notifies only itself; the workspace keeps the little it
        // shows: the selected count and notices.
        let grid_subscription =
            cx.subscribe(&grid, |this, _, event: &ResultGridEvent, cx| match event {
                ResultGridEvent::SelectionChanged { cells } => {
                    this.selected_cells = *cells;
                    cx.notify();
                }
                ResultGridEvent::Notice(notice) => {
                    this.notice = Some(notice.clone());
                    cx.notify();
                }
            });
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
                cx.emit(WorkspaceEvent::Changed);
            }
            cx.notify();
        });
        let demo = demo::enabled();
        Self {
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
            session: None,
            session_lease: None,
            next_lease: 0,
            fetching: false,
            cancellation: CancellationToken::new(),
            numbers_pt_br: false,
            editor,
            assistant_split,
            query_split,
            prompt,
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
                FAINT
            } else if connection.contains("Databricks") {
                BLUE
            } else if connection.contains("MySQL") {
                0xd995a7
            } else {
                MINT
            },
            title: title.to_string().into(),
            results,
            row_limit: DEFAULT_ROW_LIMIT,
            timeout: ExecutionOptions::default().timeout,
            connection_choices: Vec::new(),
            context: SessionContext::default(),
            context_choices: Vec::new(),
            cache_id: None,
            names: Arc::default(),
            columns: HashMap::new(),
            columns_loading: HashSet::new(),
            grid,
            selected_cells: 0,
            result_view: ResultView::Table,
            assistant_open: true,
            messages: if sql == SALES_SQL {
                vec![
                    Message {
                        user: true,
                        text: tr!("Which categories generated the most revenue last quarter?")
                            .into(),
                        sql: None,
                    },
                    Message {
                        user: false,
                        text: tr!(
                            "I prepared a query for Q3 2026, considering only completed orders."
                        )
                        .into(),
                        sql: Some(SALES_SQL),
                    },
                ]
            } else {
                Vec::new()
            },
            run: QueryRun::new(sql),
            editor_sql: sql.to_string(),
            statement_lines: jykre::sql::statement_lines(sql, None).into(),
            line_count: sql.split('\n').count(),
            saved_sql: None,
            opened_sql: Some(sql.to_string()),
            notice: None,
            filter_open: false,
            chat_scroll: ScrollHandle::new(),
            _subscriptions: vec![
                prompt_subscription,
                filter_subscription,
                grid_subscription,
                resize_subscription,
                query_resize,
                editor_observation,
            ],
        }
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
                None => (Rc::new(ResultData::default()), false, 0, None),
            },
            None => (Rc::new(ResultData::default()), false, 0, None),
        };
        let open: Vec<ResultId> = self.results.tabs().iter().map(|tab| tab.id).collect();
        self.grid.update(cx, |grid, cx| {
            grid.set_page(data, sales, first_row, shown, cx);
            grid.retain_places(&open);
        });
        for page in self.results.take_dropped() {
            if let Ok(page) = Rc::try_unwrap(page) {
                backend::release(page);
            }
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
            self.context = context;
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
        if self.run.is_active() || self.fetching {
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
        if placement != Placement::Hidden {
            self.result_view = ResultView::Table;
        }
        self.notice = None;
        self.started_position = 0;
        self.show_active(cx);
        if let Some(input) = self.input.clone() {
            self.cancellation = CancellationToken::new();
            let cancel = self.cancellation.clone();
            self.run.queue_all(id);
            // The previous run has ended (a stopping one blocks new runs), so
            // its session is back unless it was lost.
            let (session, lease) = self.lease_session(true);
            let options = self.options();
            let before = options.context.clone();
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
                let requests = queries
                    .iter()
                    .map(|(_, sql)| QueryRequest {
                        sql: sql.clone(),
                        options: options.clone(),
                    })
                    .collect();
                // Rows become compact display text on the database runtime, row
                // by row, and the driver cells are dropped there.
                let result = backend::run(backend::query(
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
                    let result = match result {
                        Ok(run) => {
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
                                    this.log.push(sql, outcome, Some(result.elapsed_ms));
                                }
                                if let (Some(error), Some((_, sql))) =
                                    (&batch.error, queries.get(completed))
                                {
                                    this.transaction = this.transaction.after(sql, false, dialect);
                                    this.log.push(sql, logged_failure(error), None);
                                }
                            }
                            failed = failure.clone();
                            let outcome = Outcome::Batch {
                                completed,
                                error: batch.error,
                            };
                            if this.run.finish(id, outcome) {
                                for ((tab, (_, sql)), result) in
                                    tabs.iter().zip(&queries).zip(batch.completed)
                                {
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
                                    cx.emit(WorkspaceEvent::Executed {
                                        title: this.title.to_string(),
                                        connection: this.connection.to_string(),
                                        sql: sql.clone(),
                                    });
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
                                this.log.push(sql, logged_failure(&error), None);
                            }
                            let report = ErrorReport::new(&error);
                            failed = Some(report.clone());
                            this.run.finish(id, Outcome::Failed(error));
                            this.end_unfinished(&tabs, Some(report));
                        }
                    }
                    // A session that did not come back takes its transaction
                    // with it.
                    if current && this.session.is_none() {
                        this.transaction = Transaction::AutoCommit;
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
                        let error = jykre::sql::preview_error(&sql);
                        let shown = this.run.finish_statement(id, index, error);
                        if shown {
                            let data = sample_result(&sql);
                            let outcome = LogOutcome::completed(
                                data.column_count(),
                                data.row_count(),
                                false,
                                None,
                            );
                            this.log.push(&sql, outcome, None);
                            this.results.complete(
                                tab,
                                Completed {
                                    data,
                                    sales: demo::is_sales(&sql),
                                    ..Completed::default()
                                },
                            );
                            cx.emit(WorkspaceEvent::Executed {
                                title: this.title.to_string(),
                                connection: this.connection.to_string(),
                                sql,
                            });
                        } else if let Some(error) = error {
                            this.log
                                .push(&sql, LogOutcome::Failed(error.to_owned()), None);
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

    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.prompt.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        self.ask(text, cx);
        self.prompt
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    fn ask(&mut self, text: String, cx: &mut Context<Self>) {
        let lower = text.to_lowercase();
        let (answer, sql) = if lower.contains("optimi") || lower.contains("slow") {
            (
                tr!(
                    "One alternative is to aggregate the orders before the JOINs. This can reduce intermediate rows. Validate the difference with EXPLAIN ANALYZE on your database; this mock does not measure performance."
                ),
                Some(OPTIMIZED_SQL),
            )
        } else if lower.contains("expla") {
            (
                tr!(
                    "The query filters completed orders between July and September 2026, joins products and categories, and groups the values. COUNT counts orders, SUM computes revenue, and AVG returns the average ticket. The ordering shows the highest revenue first."
                ),
                None,
            )
        } else if lower.contains("chart") || lower.contains("graph") {
            self.result_view = ResultView::Chart;
            (
                tr!(
                    "I opened the bar chart. Electronics leads with R$ 248,760.00. Compare the bar lengths to see how revenue is concentrated by category."
                ),
                None,
            )
        } else if lower.contains("clean") {
            (
                tr!(
                    "To investigate data quality, start by checking null values in product_id, orders without a category, and duplicate IDs. Also compare the orders count before and after the JOINs."
                ),
                None,
            )
        } else {
            (
                tr!(
                    "In this sample, Electronics accounts for 32.4% of revenue. Together with Home and decor, the two categories make up 52.7% of the total. You can explore the chart, ask for an explanation of the query, or try an optimization suggestion."
                ),
                None,
            )
        };
        self.messages.push(Message {
            user: true,
            text,
            sql: None,
        });
        self.messages.push(Message {
            user: false,
            text: if self.sales() { answer.into() } else {
                let (columns, rows) = self.current_page().map_or((0, 0), |page| (page.data.column_count(), page.data.row_count()));
                tr!("The query runs in the context of {0}. The sample preview has {1} columns and {2} rows. Use the editor to adjust the filters and run it to see the sample. This demo's assistant has a complete analysis flow in the Revenue by category tab.", self.connection, columns, rows)
            },
            sql: if self.sales() { sql } else { None },
        });
        self.assistant_open = true;
        self.chat_scroll.scroll_to_bottom();
        cx.notify();
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
            .icon(I::Database)
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
        let parts: Vec<&str> = [&self.context.catalog, &self.context.schema]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        if parts.is_empty() {
            tr!("Session default").into()
        } else {
            parts.join(".")
        }
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
                    let text: Vec<&str> = [&choice.catalog, &choice.schema]
                        .into_iter()
                        .flatten()
                        .map(String::as_str)
                        .collect();
                    menu = menu.item(
                        PopupMenuItem::new(text.join("."))
                            .checked(*choice == current)
                            .on_click(move |_, _, cx| {
                                let _ = workspace.update(cx, |this, cx| {
                                    this.context = chosen.clone();
                                    cx.emit(WorkspaceEvent::Changed);
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            })
    }

    /// Uses `names` for completion, hover and go to definition. A newer
    /// index (or another connection's) drops the columns read for the old
    /// one.
    pub fn set_names(&mut self, id: ConnectionId, names: Arc<NameIndex>) {
        if self.cache_id != Some(id) || self.names.version != names.version {
            self.columns.clear();
            self.columns_loading.clear();
        }
        self.cache_id = Some(id);
        self.names = names;
    }

    /// The identifier quote and the connection, for reading the cache.
    fn cache_target(&self) -> Option<(ConnectionId, ConnectionConfig, char)> {
        let input = self.input.as_ref()?;
        let quote = builtin_registry()
            .and_then(|registry| registry.get(input.config.driver))
            .map(|driver| driver.descriptor().capabilities.identifier_quote)
            .unwrap_or('"');
        Some((self.cache_id?, input.config.clone(), quote))
    }

    /// The editor's suggestions at `offset`. Relations whose columns were
    /// not read yet show as loading and are read in the background, so the
    /// next keystroke completes them; typing never waits.
    pub(super) fn completion_items(
        &mut self,
        sql: &str,
        offset: usize,
        text: &gpui_kit::component::Rope,
        cx: &mut Context<Self>,
    ) -> Vec<lsp_types::CompletionItem> {
        use gpui_kit::component::RopeExt as _;
        let Some((_, _, quote)) = self.cache_target() else {
            return Vec::new();
        };
        let Some(request) = jykre::completion::request(sql, offset, &self.names, &self.context)
        else {
            return Vec::new();
        };
        let columns = &self.columns;
        let (found, missing) = jykre::completion::candidates(
            &request,
            &self.names,
            &self.context,
            quote,
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
                }),
                detail: Some(candidate.detail),
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
            self.load_columns(missing, cx);
        }
        items
    }

    /// Reads the columns of `relations` from the cache, off the UI thread.
    fn load_columns(&mut self, relations: Vec<ObjectPath>, cx: &mut Context<Self>) {
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
                            this.columns.insert(relation, columns);
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
    pub(super) fn symbol_at(
        &mut self,
        sql: &str,
        offset: usize,
        cx: &mut Context<Self>,
    ) -> Option<(Symbol, std::ops::Range<usize>)> {
        self.cache_target()?;
        let columns = &self.columns;
        let found =
            jykre::completion::symbol_at(sql, offset, &self.names, &self.context, &|relation| {
                columns.get(relation).cloned()
            });
        if found.is_none() {
            // The name may be a column of a relation not read yet.
            let request = jykre::completion::request(sql, offset, &self.names, &self.context);
            if let Some(jykre::completion::Request {
                wanted: jykre::completion::Wanted::Columns { relations },
                ..
            }) = request
            {
                let missing = relations
                    .into_iter()
                    .filter(|relation| !self.columns.contains_key(relation))
                    .collect();
                self.load_columns(missing, cx);
            }
        }
        found
    }

    /// What the hover shows for a symbol: a column's type, nullability,
    /// key, default and comment, or a relation's kind and name.
    pub(super) fn describe(&self, symbol: &Symbol) -> Option<String> {
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
    pub(super) fn reveal(&mut self, path: ObjectPath, cx: &mut Context<Self>) {
        cx.emit(WorkspaceEvent::Reveal {
            connection: self.connection.to_string(),
            path,
        });
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
            .border_color(rgb(BORDER))
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
                    .child(button("explain", I::ScanLine, tr!("Explain")).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.ask(tr!("Explain this query").into(), cx)
                        }),
                    ))
                    .child(button("format", I::TextAlignStart, tr!("Format")).on_click(
                        cx.listener(|this, _, window, cx| this.format_sql(window, cx)),
                    ))
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
                                    tr!("Remove from saved queries")
                                }
                                Some(_) => tr!("Update saved query"),
                                None => tr!("Save query"),
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            let sql = this.editor.read(cx).text().to_string();
                            // A saved tab whose SQL changed since then is updated
                            // instead of removed, so later edits are not lost.
                            let (saved, notice) = match &this.saved_sql {
                                Some(saved) if *saved == sql => (
                                    false,
                                    Feedback::new(
                                        Tone::Info,
                                        tr!("Query removed from saved queries."),
                                    ),
                                ),
                                Some(_) => (
                                    true,
                                    Feedback::new(Tone::Success, tr!("Saved query updated.")),
                                ),
                                None => (
                                    true,
                                    Feedback::new(
                                        Tone::Success,
                                        tr!("Query saved in this session."),
                                    ),
                                ),
                            };
                            this.saved_sql = saved.then(|| sql.clone());
                            cx.emit(WorkspaceEvent::Saved {
                                saved,
                                title: this.title.to_string(),
                                connection: this.connection.to_string(),
                                sql,
                            });
                            this.notice = Some(notice);
                            cx.notify();
                        })),
                    )
                    .child(divider())
                    .child(
                        icon_button(
                            "toggle-ai",
                            I::PanelRight,
                            tr!("Show or hide the assistant"),
                        )
                        .when(self.assistant_open, |s| s.bg(rgb(SURFACE)))
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

    fn editor_panel(&self, cx: &mut Context<Self>) -> Div {
        column()
            .size_full()
            .bg(rgb(BG))
            .child(
                row()
                    .h(px(38.))
                    .flex_none()
                    .px_4()
                    .justify_between()
                    .child(
                        row()
                            .gap_2()
                            .child(icon(I::Database, self.connection_color))
                            .child(label(self.connection.clone()))
                            .child(label("/"))
                            .map(|s| {
                                if self.input.is_some() {
                                    s.child(self.context_menu(cx))
                                } else {
                                    s.child(label(self.database.clone()))
                                        .child(label("/"))
                                        .child(label(if self.demo { "public" } else { "SQL" }))
                                }
                            }),
                    )
                    .child(
                        row()
                            .child(dot(if self.input.is_some() || self.demo {
                                MINT
                            } else {
                                AMBER
                            }))
                            .child(label(if self.input.is_some() {
                                tr!("Real connection")
                            } else if self.demo {
                                tr!("Demo")
                            } else {
                                tr!("No connection")
                            })),
                    ),
            )
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
                    .child(label(format!("SQL · {}", self.dialect)))
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
            (ResultView::Chart, I::ChartColumn, tr!("Chart")),
            (ResultView::Messages, I::Terminal, tr!("Messages")),
        ];
        let active = self.results.active_index();
        column()
            .size_full()
            .border_t_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                row()
                    .h(px(42.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(BORDER))
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
                                    row()
                                        .id(("result-tab", index))
                                        .role(Role::Tab)
                                        .aria_selected(index == active)
                                        .aria_label(tr!("Result {0}: {1}", tab.number, status))
                                        .focusable()
                                        .tab_index(0)
                                        .focus_visible(|s| s.bg(rgb(HOVER)))
                                        .flex_none()
                                        .h(px(28.))
                                        .pl_2()
                                        .pr_1()
                                        .gap_1()
                                        .rounded(px(5.))
                                        .cursor_pointer()
                                        .text_size(px(12.))
                                        .text_color(rgb(if index == active { TEXT } else { MUTED }))
                                        .when(index == active, |s| s.bg(rgb(SURFACE)))
                                        .hover(|s| s.bg(rgb(HOVER)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.results.select(index);
                                            this.show_active(cx);
                                        }))
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
                                        .child(tr!("Result {0}", tab.number))
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
                            .children(views.into_iter().enumerate().map(
                                |(i, (view, symbol, title))| {
                                    icon_button(("result-mode", i), symbol, title)
                                        .aria_selected(self.result_view == view)
                                        .when(self.result_view == view, |s| {
                                            s.bg(rgb(SURFACE)).text_color(rgb(TEXT))
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.result_view = view;
                                            cx.notify();
                                        }))
                                },
                            ))
                            .child(divider())
                            .child(
                                button("format-numbers", I::Settings, tr!("pt-BR numbers"))
                                    .when(self.numbers_pt_br, |s| {
                                        s.bg(rgb(HOVER)).text_color(rgb(MINT))
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.numbers_pt_br = !this.numbers_pt_br;
                                        let numbers_pt_br = this.numbers_pt_br;
                                        this.grid.update(cx, |grid, cx| {
                                            grid.set_number_format(numbers_pt_br, cx)
                                        });
                                        cx.notify();
                                    })),
                            )
                            .child(
                                icon_button("filter-results", I::ListFilter, tr!("Filter results"))
                                    .aria_selected(self.filter_open)
                                    .when(self.filter_open, |s| s.bg(rgb(HOVER)))
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
                            .child(
                                icon_button(
                                    "export-results",
                                    I::Download,
                                    tr!("Copy this page as CSV"),
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        let Some(page) =
                                            this.current_page().filter(|_| this.has_results())
                                        else {
                                            this.notice = Some(Feedback::new(
                                                Tone::Info,
                                                tr!("Run a query before copying the results."),
                                            ));
                                            cx.notify();
                                            return;
                                        };
                                        let _timer = jykre::logging::UiTimer::start("copy CSV");
                                        let csv = cell_format::result_csv(&page.data);
                                        cx.write_to_clipboard(ClipboardItem::new_string(csv));
                                        this.notice = Some(Feedback::new(
                                            Tone::Success,
                                            tr!("CSV copied. Paste it into a spreadsheet or file."),
                                        ));
                                        cx.notify();
                                    },
                                )),
                            )
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
            return centered()
                .child(icon(I::Terminal, FAINT))
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
                .child(spinning_icon("running-spinner", I::LoaderCircle, MINT))
                .child(label(if self.input.is_some() {
                    tr!("Running on the server…")
                } else {
                    tr!("Preparing the demo…")
                }))
                .into_any_element(),
            ResultState::Failed(report) => self.result_error(tab, report, cx),
            ResultState::Cancelled => centered()
                .child(icon(I::Ban, AMBER))
                .child(label(tr!("Execution cancelled")))
                .into_any_element(),
            ResultState::Done => match self.result_view {
                ResultView::Chart => self.result_chart(cx),
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
                    .child(icon(I::CircleCheck, MINT))
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
                row().gap_2().child(icon(I::CircleX, RED)).child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(RED))
                        .child(report.title.clone()),
                ),
            )
            .child(
                div()
                    .pl(px(24.))
                    .font_family("Cascadia Mono")
                    .text_size(px(12.))
                    .line_height(relative(1.5))
                    .text_color(rgb(TEXT))
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
                        .text_color(rgb(MUTED))
                        .child(action),
                )
            })
            .child(
                row()
                    .pl(px(24.))
                    .gap_2()
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
        let (symbol, color, text, detail): (I, u32, String, Option<String>) =
            if matches!(state, RunState::Running { .. }) {
                (I::LoaderCircle, AMBER, tr!("Running…").into(), None)
            } else if *state == RunState::Cancelling {
                (I::LoaderCircle, AMBER, tr!("Cancelling…").into(), None)
            } else if self.fetching {
                (
                    I::LoaderCircle,
                    AMBER,
                    tr!("Reading the next page…").into(),
                    None,
                )
            } else if let Some(tab) = tab {
                match &tab.state {
                    ResultState::Failed(report) => match state {
                        RunState::Failed { error } => {
                            (I::CircleX, RED, report.title.clone(), Some(error.clone()))
                        }
                        _ => (I::CircleX, RED, report.title.clone(), None),
                    },
                    ResultState::Cancelled => {
                        (I::Ban, AMBER, tr!("Execution cancelled").into(), None)
                    }
                    ResultState::Running => (I::LoaderCircle, AMBER, tr!("Running…").into(), None),
                    ResultState::Done => page_summary(tab),
                }
            } else {
                (I::Clock, FAINT, tr!("Waiting to run").into(), None)
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
            .border_color(rgb(BORDER))
            .justify_between()
            .child(
                row()
                    .id("run-status")
                    .role(Role::Status)
                    .aria_label(text.clone())
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
                            s.child(label("·"))
                                .child(label(format!("{} ms", tab.elapsed_ms)))
                        },
                    ),
            )
            .child(
                row()
                    .gap_2()
                    .when(self.has_results() && !self.run.is_running(), |s| {
                        let selected = self.selected_cells;
                        s.when(selected > 1, |s| {
                            s.child(label(tr!("{0} cells selected", selected)))
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
                            FAINT,
                        ))
                    }),
            )
    }

    /// The result on screen is the demo's, with a chart and an analysis.
    fn sales(&self) -> bool {
        self.results.active().is_some_and(|tab| tab.sales)
    }

    fn result_chart(&self, _: &mut Context<Self>) -> AnyElement {
        if !self.sales() {
            return column()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .p_5()
                .child(icon(I::ChartColumn, MINT))
                .child(
                    div()
                        .text_size(px(14.))
                        .child(tr!("Give your results a view")),
                )
                .child(label(tr!(
                    "Chart available only in the Revenue by category demo."
                )))
                .into_any_element();
        }
        column()
            .id("revenue-chart")
            .size_full()
            .overflow_y_scroll()
            .p_5()
            .gap_3()
            .child(
                row()
                    .justify_between()
                    .child(
                        column()
                            .gap_1()
                            .child(div().text_size(px(14.)).child(tr!("Revenue by category")))
                            .child(label(tr!("July — September 2026 · BRL"))),
                    )
                    .child(badge("R$ 769.000,00", MINT)),
            )
            .children(SALES.iter().enumerate().map(|(index, data)| {
                row()
                    .h(px(24.))
                    .flex_none()
                    .gap_3()
                    .child(
                        div()
                            .w(px(125.))
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child(data.category),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h(px(16.))
                            .rounded(px(3.))
                            .bg(rgb(SURFACE))
                            .child(
                                div()
                                    .w(relative(data.amount / SALES[0].amount))
                                    .h_full()
                                    .rounded(px(3.))
                                    .bg(rgb(if index == 0 { MINT } else { 0x4c8274 })),
                            ),
                    )
                    .child(
                        div()
                            .w(px(94.))
                            .text_right()
                            .font_family("Cascadia Mono")
                            .text_size(px(12.))
                            .child(data.revenue),
                    )
            }))
            .into_any_element()
    }

    /// The log of what ran in this tab, newest first, with when each
    /// statement ended, how and how long it took. It outlives the result
    /// tabs, so a replaced tab does not take an `UPDATE`'s outcome with it.
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
            .child(icon(I::Database, AMBER))
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
                                    .border_color(rgb(BORDER))
                                    .focus_visible(|s| s.border_color(rgb(MINT)))
                                    .h(px(32.))
                                    .px(px(9.))
                                    .rounded(px(5.))
                                    .cursor_pointer()
                                    .text_size(px(14.))
                                    .text_color(rgb(MUTED))
                                    .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                    .child(icon(I::Database, MUTED))
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
                FAINT,
                tr!("Auto-commit"),
                tr!("Each statement commits on its own. BEGIN opens a transaction."),
            ),
            Transaction::Open {
                changes: false,
                manual: true,
            } => (
                I::LockOpen,
                AMBER,
                tr!("Manual commit"),
                tr!(
                    "Autocommit is off: statements wait for COMMIT. Closing the tab rolls them back."
                ),
            ),
            Transaction::Open { changes: false, .. } => (
                I::LockOpen,
                AMBER,
                tr!("Transaction open"),
                tr!("A transaction is open in this tab's session. Closing the tab rolls it back."),
            ),
            Transaction::Open { changes: true, .. } => (
                I::CircleDot,
                AMBER,
                tr!("Uncommitted changes"),
                tr!(
                    "Statements in the open transaction changed data that is not committed. Commit or roll back; closing the tab rolls it back."
                ),
            ),
            Transaction::Failed { .. } => (
                I::CircleAlert,
                RED,
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

    /// Formats the SQL by the tab's dialect. Only whitespace and keyword
    /// case change, so the caret, the selection and the caret's place on
    /// screen are kept.
    fn format_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (sql, selection, reversed, scroll) = {
            let editor = self.editor.read(cx);
            (
                editor.text().to_string(),
                editor.selected_range(),
                editor.is_selection_reversed(),
                editor.scroll_offset(),
            )
        };
        let formatted = jykre::sql::format(&sql, self.run.dialect());
        if formatted == sql {
            self.notice = Some(Feedback::new(Tone::Info, tr!("SQL is already formatted.")));
            cx.notify();
            return;
        }
        let map = |offset| jykre::sql::map_offset(&sql, &formatted, offset);
        let range = map(selection.start)..map(selection.end);
        let caret = |text: &str, range: &Range<usize>| {
            let at = if reversed { range.start } else { range.end };
            text.get(..at)
                .map_or(0, |before| before.matches('\n').count())
        };
        let moved = caret(&formatted, &range) as f32 - caret(&sql, &selection) as f32;
        self.editor.update(cx, |editor, cx| {
            // `replace_all` keeps the undo history, so Ctrl+Z restores the
            // previous layout; it also moves to the start, undone here.
            editor.replace_all(formatted.clone(), window, cx);
            editor.set_selected_range_with_direction(range, reversed, cx);
            if let Some(line_height) = editor.line_height() {
                editor.set_scroll_offset(point(scroll.x, scroll.y - line_height * moved), cx);
            }
        });
        self.notice = Some(Feedback::new(
            Tone::Success,
            tr!("SQL formatted. Use Ctrl+Z to undo."),
        ));
        cx.notify();
    }

    fn assistant(&self, cx: &mut Context<Self>) -> Div {
        column().size_full().bg(rgb(0x191d23)).border_l_1().border_color(rgb(BORDER))
            .child(row().h(px(48.)).flex_none().px_4().justify_between().border_b_1().border_color(rgb(BORDER))
                .child(row().child(icon(I::Sparkles, MINT)).child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(tr!("Assistant")))
                    .child(badge("IA", MINT)))
                .child(row().gap_0()
                    .child(icon_button("reset-chat", I::SquarePen, tr!("New conversation"))
                        .on_click(cx.listener(|this, _, _, cx| { this.messages.clear(); cx.notify(); })))
                    .child(icon_button("close-ai", I::PanelRightClose, tr!("Collapse assistant"))
                        .on_click(cx.listener(|this, _, _, cx| { this.assistant_open = false; cx.notify(); })))))
            .child(row().px_4().h(px(37.)).flex_none().gap_2().border_b_1().border_color(rgb(BORDER))
                .child(icon(I::Layers, FAINT)).child(label(tr!("Context")))
                .child(badge(tr!("Current SQL"), MUTED)).child(badge(if self.sales() { tr!("3 tables") } else { tr!("1 query") }, MUTED)))
            .child(column().id("ai-conversation").flex_1().overflow_y_scroll().track_scroll(&self.chat_scroll).vertical_scrollbar(&self.chat_scroll).px_4().py_4().gap_5()
                .child(row().gap_2().child(div().h(px(1.)).flex_1().bg(rgb(BORDER)))
                    .child(div().text_size(px(12.)).text_color(rgb(FAINT)).child(tr!("Context of this query")))
                    .child(div().h(px(1.)).flex_1().bg(rgb(BORDER))))
                .when(self.messages.is_empty(), |s| s.child(column().py_5().gap_3()
                    .child(icon(I::Sparkles, MINT)).child(div().text_size(px(16.)).child(tr!("One question, a new path.")))
                    .child(label(tr!("Explore data, understand queries, and turn questions into SQL.")))))
                .children(self.messages.iter().enumerate().map(|(index, message)| {
                    column().gap_2()
                        .child(row().gap_2()
                            .child(div().size(px(20.)).rounded(px(5.)).bg(rgb(if message.user { HOVER } else { 0x263b34 }))
                                .flex().items_center().justify_center().child(icon(if message.user { I::UserRound } else { I::Sparkles }, if message.user { MUTED } else { MINT })))
                            .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(if message.user { MUTED } else { MINT })).child(if message.user { tr!("You") } else { "Jykre Studio" })))
                        .child(div().text_size(px(14.)).line_height(relative(1.6)).text_color(rgb(if message.user { TEXT } else { 0xaeb8c5 })).child(message.text.clone()))
                        .when_some(message.sql, |s, suggested| s.child(column().mt_1().rounded(px(6.)).border_1().border_color(rgb(BORDER)).bg(rgb(BG)).overflow_hidden()
                            .child(row().px_3().h(px(28.)).justify_between().border_b_1().border_color(rgb(BORDER)).child(label(tr!("SQL · suggestion")))
                                .child(icon_button(("copy-ai", index), I::Copy, tr!("Copy suggested SQL"))
                                    .on_click(cx.listener(move |this, _, _, cx| { cx.write_to_clipboard(ClipboardItem::new_string(suggested.into())); this.notice = Some(Feedback::new(Tone::Success, tr!("Suggested SQL copied."))); cx.notify(); }))))
                            .child(div().px_3().py_3().font_family("Cascadia Mono").text_size(px(12.)).line_height(relative(1.6)).text_color(rgb(BLUE))
                                .child(if suggested == OPTIMIZED_SQL { "WITH sales AS (\n  SELECT product_id,\n         SUM(total) AS revenue\n  FROM orders\n  …\n)" } else { "SELECT c.name AS category,\n       SUM(o.total) AS revenue\nFROM orders o\nJOIN products p ON …\nGROUP BY c.name;" }))
                            .child(button(("apply-ai", index), I::CornerDownLeft, tr!("Insert into editor")).w_full().justify_center().border_t_1().border_color(rgb(BORDER)).text_color(rgb(MINT))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.editor.update(cx, |editor, cx| editor.replace_all(suggested, window, cx));
                                    this.notice = Some(Feedback::new(Tone::Info, tr!("Suggestion inserted. Review the SQL before running it."))); cx.notify();
                                })))))
                }))
                .when(self.has_results() && self.sales(), |s| s.child(column().gap_2().p_3().rounded(px(7.)).border_1().border_color(rgb(0x34473f)).bg(rgb(0x1c2a26))
                    .child(row().child(icon(I::Lightbulb, MINT)).child(div().text_size(px(14.)).text_color(rgb(MINT)).font_weight(FontWeight::MEDIUM).child(tr!("A look at the data"))))
                    .child(div().text_size(px(14.)).line_height(relative(1.55)).text_color(rgb(0xb8c8c1)).child(tr!("Electronics and Home account for 52.7% of the quarter's revenue.")))
                    .child(button("ai-show-chart", I::ChartColumn, tr!("View distribution")).px_0().text_color(rgb(MINT))
                        .on_click(cx.listener(|this, _, _, cx| { this.result_view = ResultView::Chart; cx.notify(); }))))))
            .child(column().flex_none().p_3().gap_2().border_t_1().border_color(rgb(BORDER))
                .child(row().gap_1().children([("explain-chip", tr!("Explain SQL")), ("optimize-chip", tr!("Optimize")), ("chart-chip", tr!("Create chart"))].into_iter().map(|(id, text)| {
                    div().id(id).role(Role::Button).aria_label(text).focusable().tab_index(0).focus_visible(|s| s.border_color(rgb(MINT))).px_2().py(px(5.)).rounded(px(5.)).border_1().border_color(rgb(BORDER))
                        .text_size(px(12.)).text_color(rgb(MUTED)).cursor_pointer().hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                        .child(text).on_click(cx.listener(move |this, _, _, cx| this.ask(text.into(), cx)))
                })))
                .child(column().p_2().gap_2().rounded(px(7.)).border_1().border_color(rgb(0x3b4945)).bg(rgb(BG))
                    .child(Input::new(&self.prompt).appearance(false).bordered(false).small().w_full().aria_label(tr!("Message to the AI assistant")))
                    .child(row().justify_between().child(row().gap_1().child(icon(I::AtSign, FAINT)).child(label(tr!("Query context"))))
                        .child(icon_button("send-ai", I::ArrowUp, tr!("Send message")).bg(rgb(0x304a40))
                            .on_click(cx.listener(|this, _, window, cx| this.submit_prompt(window, cx))))))
                .child(div().text_center().text_size(px(12.)).text_color(rgb(FAINT)).child(tr!("Simulated AI · Always review the suggestions."))))
    }
}

pub enum WorkspaceEvent {
    /// State worth saving changed (SQL, timeout, result tabs).
    Changed,
    /// A run needs the connection's password, which was not remembered.
    /// The host asks for it and calls [`Workspace::resume_pending`].
    PasswordRequired,
    /// The user chose another connection for this tab in the toolbar.
    ConnectionRequested(String),
    Saved {
        saved: bool,
        title: String,
        connection: String,
        sql: String,
    },
    Executed {
        title: String,
        connection: String,
        sql: String,
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
        if let Some(input) = input {
            self.set_connection(input, row_limit, cx);
        } else {
            self.abandon_run();
            self.input = None;
            self.results.clear();
            self.show_active(cx);
        }
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
            timeout_seconds: jykre::workspace_state::TabState::store_timeout(self.timeout),
            opened_sql: self.opened_sql.clone(),
            catalog: self.context.catalog.clone(),
            schema: self.context.schema.clone(),
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
    pub fn restore(&mut self, state: &jykre::workspace_state::TabState) {
        self.saved_sql = state.saved_sql.clone();
        self.opened_sql = state.opened_sql.clone();
        self.timeout = state.timeout(self.timeout);
        // Applied to the session the next run opens.
        self.context = SessionContext {
            catalog: state.catalog.clone(),
            schema: state.schema.clone(),
        };
    }

    /// A statement or a page read is in progress.
    pub fn is_busy(&self) -> bool {
        self.run.is_active() || self.fetching
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
        self.forget_server();
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
    }

    /// Forgets the run that waited for a password the user did not give.
    pub fn cancel_pending(&mut self) {
        self.pending_run = None;
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
        self.forget_server();
        self.assistant_open = false;
        self.results.clear();
        self.show_active(cx);
        self.messages.clear();
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
        if dialect != self.run.dialect() {
            self.run.set_dialect(dialect);
            self.statement_lines = jykre::sql::statement_lines(&self.editor_sql, dialect).into();
            set_highlighter(&self.editor, dialect, cx);
        }
    }

    /// Another server: its namespaces and the names read from its cache are
    /// not this one's.
    fn forget_server(&mut self) {
        self.context = SessionContext::default();
        self.context_choices.clear();
        self.cache_id = None;
        self.names = Arc::default();
        self.columns.clear();
        self.columns_loading.clear();
    }

    /// Puts the keyboard in the SQL editor.
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

/// Highlights the editor by the rules of `dialect`.
fn set_highlighter(editor: &Entity<EditorState>, dialect: Option<SqlDialect>, cx: &mut App) {
    editor.update(cx, |editor, cx| {
        editor.set_highlighter_factory(
            Rc::new(move |_| Some(Box::new(super::sql::SqlHighlighter::new(dialect)))),
            cx,
        )
    });
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
        .focus_visible(|s| s.border_color(rgb(MINT)))
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .cursor_pointer()
        .tooltip(move |window, cx| Tooltip::new(title).build(window, cx))
        .hover(|s| s.bg(rgb(HOVER)))
        .child(icon(I::Pin, if pinned { MINT } else { FAINT }))
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
            MINT,
            match (*rows, *more) {
                (rows, true) => tr!("{0} rows on the first page", cell_format::count(rows)),
                (1, false) => tr!("1 row").to_string(),
                (rows, false) => tr!("{0} rows", cell_format::count(rows)),
            },
        ),
        LogOutcome::Affected(affected) => {
            (I::CircleCheck, MINT, tr!("{0} rows affected", affected))
        }
        LogOutcome::Completed => (I::CircleCheck, MINT, tr!("Statement completed").into()),
        LogOutcome::Failed(error) => (I::CircleX, RED, error.clone()),
        LogOutcome::Cancelled => (I::Ban, AMBER, tr!("Execution cancelled").into()),
    };
    let age = jykre::explorer::format_age(now.duration_since(entry.at).unwrap_or_default());
    let timing = match entry.elapsed_ms {
        Some(elapsed) => format!("{elapsed} ms · {age}"),
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
                .text_color(rgb(FAINT))
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
        ResultState::Running => (I::LoaderCircle, AMBER, tr!("Running…").into()),
        ResultState::Failed(report) => (
            I::CircleX,
            RED,
            format!(
                "{}: {}",
                report.title,
                report.message.lines().next().unwrap_or_default()
            ),
        ),
        ResultState::Cancelled => (I::Ban, AMBER, tr!("Execution cancelled").into()),
        ResultState::Done => {
            let (_, _, text, _) = page_summary(tab);
            (I::CircleCheck, MINT, text)
        }
    }
}

/// The footer text for a completed result's page: its rows (as a range when
/// the result has several pages), the rows affected and the limit that ended
/// reading, with an explanation for the tooltip.
fn page_summary(tab: &ResultTab) -> (I, u32, String, Option<String>) {
    let Some(page) = tab.current() else {
        return (
            I::CircleCheck,
            MINT,
            match tab.rows_affected {
                Some(affected) => tr!("{0} rows affected", affected),
                None => tr!("Statement completed").into(),
            },
            None,
        );
    };
    let count = page.data.row_count();
    let paged = tab.page_number() > 1 || tab.has_next();
    let rows = if paged && count == 1 {
        tr!("Row {0}", cell_format::count(page.first_row + 1))
    } else if paged && count > 0 {
        tr!(
            "Rows {0}–{1}",
            cell_format::count(page.first_row + 1),
            cell_format::count(page.first_row + count)
        )
    } else {
        match count {
            1 => tr!("1 row").to_string(),
            count => tr!("{0} rows", cell_format::count(count)),
        }
    };
    match tab.limit {
        Some(ResultLimit::Memory(bytes)) => (
            I::TriangleAlert,
            AMBER,
            tr!("{0} · memory limit reached", rows),
            Some(tr!(
                "The page reached the memory limit of {0} MB before the row limit, so the rest of the result was not read. Refine the SQL to get other rows.",
                bytes / (1024 * 1024)
            )),
        ),
        _ if tab.rest_closed() => (
            I::TriangleAlert,
            AMBER,
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
            (I::CircleCheck, MINT, text, None)
        }
    }
}

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
        StatementStatus::Ready => (I::Play, MINT, tr!("Run this query")),
        StatementStatus::Pending => (
            I::Clock,
            FAINT,
            tr!("Waiting for the statements before it · stop the run"),
        ),
        StatementStatus::Running => (I::Square, 0xe7bd7b, tr!("Stop execution")),
        StatementStatus::Success => (I::CircleCheck, MINT, tr!("Success · run again")),
        StatementStatus::Error => (I::CircleX, 0xef8b91, tr!("Error · run again")),
        StatementStatus::Cancelled => (I::Play, FAINT, tr!("Cancelled · run again")),
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
        .hover(|s| s.bg(rgb(HOVER)))
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::probe::rendered("Workspace");
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
                        .child(self.assistant(cx)),
                )
                .into_any_element()
        } else {
            query.into_any_element()
        };
        column()
            .id("query-workspace")
            .size_full()
            .bg(rgb(BG))
            .child(content)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(session) = self.session.take() {
            backend::close_in_background(session);
        }
    }
}
