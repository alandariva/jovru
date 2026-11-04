//! A report's tab ([ADR-0024](../../docs/adr/0024-reports.md)): its
//! definition (title, description, SQL, parameters, chart) while editing,
//! and the parameter form, chart and table it shows when run.
//!
//! The tab keeps a session of its own between runs (ADR-0014) and runs
//! every statement of its SQL, showing the last one's result up to the
//! connection's row limit. Parameter values are bound by the driver, never
//! written into the SQL (ADR-0023).
use super::report_chart::ReportChart;
use super::result_grid::ResultGrid;
use super::sql_editor::{SqlCatalogs, SqlEditor, SqlEditorEvent};
use super::ui::*;
use super::workspace::SQL_EDITOR_CONTEXT;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::drivers::*;
use jykre::reports::params::{self, ParameterProblem};
use jykre::reports::{ChartKind, ChartSpec, ConnectionRef, Report, chart, export};
use jykre::result::{DEFAULT_MEMORY_CEILING, ResultData};
use jykre::{backend, connection_store, tr};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

/// Whether the definition is open for editing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportMode {
    Edit,
    View,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

/// Where the report runs, as the host knows it.
#[derive(Clone)]
pub struct ReportTarget {
    pub name: String,
    pub id: Option<ConnectionId>,
    pub input: Option<ResolvedConnection>,
    pub row_limit: u32,
    pub environment: Option<jykre::connection_appearance::Environment>,
}

/// The editable parts of a parameter's specification.
struct SpecRow {
    name: String,
    label: Entity<InputState>,
    default: Entity<InputState>,
}

/// A parameter's value field in the form.
struct ValueField {
    name: String,
    input: Entity<InputState>,
}

pub enum ReportEvent {
    /// The definition or the run changed.
    Changed,
    /// A run needs the connection's password; the host asks and calls
    /// [`ReportView::resume_pending`].
    PasswordRequired,
    /// A run on a production connection holds risky statements; the host
    /// asks and calls [`ReportView::confirm_risk`].
    ConfirmRisk {
        connection: String,
        description: String,
    },
    /// Store this definition; the host answers with [`ReportView::saved`].
    Save(Box<Report>),
    /// The user chose another connection.
    ConnectionRequested(String),
}

impl EventEmitter<ReportEvent> for ReportView {}

pub struct ReportView {
    /// The working definition: parameters and chart live here; title,
    /// description and SQL in their fields.
    report: Report,
    /// The definition as stored; `None` for a draft.
    saved: Option<Report>,
    mode: ReportMode,
    title: Entity<InputState>,
    description: Entity<InputState>,
    editor: Entity<EditorState>,
    sql_editor: Entity<SqlEditor>,
    editor_sql: String,
    spec_rows: Vec<SpecRow>,
    values: Vec<ValueField>,
    errors: HashMap<String, ParameterProblem>,
    target: Option<ReportTarget>,
    dialect: Option<SqlDialect>,
    connection_choices: Vec<String>,
    session: Option<Box<dyn Session>>,
    lease: Option<u64>,
    next_lease: u64,
    phase: Phase,
    cancellation: CancellationToken,
    /// A run waits for the connection's password.
    pending: bool,
    /// A run waits for the user to confirm its risky statements.
    risky: bool,
    risk_confirmed: bool,
    result: Option<Arc<ResultData>>,
    /// The result stopped at the row limit or the memory ceiling.
    limited: bool,
    elapsed_ms: Option<u128>,
    /// Each parameter's label and the value the last run bound.
    bound: Vec<(String, Option<String>)>,
    /// The statements of the last run, with their markers.
    ran: Vec<String>,
    show_ran: bool,
    /// The numeric columns of the result, for the chart's values.
    numeric: Vec<String>,
    /// The chart [`chart::suggest`] proposes for the result, computed with it.
    suggestion: Option<ChartSpec>,
    chart_problem: Option<String>,
    grid: Entity<ResultGrid>,
    chart: Entity<ReportChart>,
    chart_shown: bool,
    notice: Option<Feedback>,
    /// What listens to the parameter fields, replaced with them.
    field_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl ReportView {
    pub fn new(
        report: Report,
        mode: ReportMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let saved = (report.id != 0).then(|| report.clone());
        let title = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("Report title"))
                .default_value(report.title.clone())
        });
        let description = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("What the report shows (optional)"))
                .default_value(report.description.clone())
        });
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .placeholder(tr!(
                    "SELECT … WHERE day >= :start. Each :name becomes a field to fill before running."
                ))
                .default_value(report.sql.clone())
        });
        let sql_editor = cx.new(|cx| SqlEditor::new(&editor, cx));
        let editor_sql = report.sql.clone();
        let mut subscriptions = vec![
            cx.subscribe(&sql_editor, |this, _, event: &SqlEditorEvent, cx| {
                if let SqlEditorEvent::Notice(notice) = event {
                    this.notice = Some(notice.clone());
                    cx.notify();
                }
            }),
            cx.observe_in(&editor, window, |this, editor, window, cx| {
                let sql = editor.read(cx).text().to_string();
                if sql != this.editor_sql {
                    this.editor_sql = sql;
                    this.sync_parameters(window, cx);
                    cx.emit(ReportEvent::Changed);
                }
            }),
        ];
        for field in [&title, &description] {
            subscriptions.push(cx.subscribe(field, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.emit(ReportEvent::Changed);
                    cx.notify();
                }
            }));
        }
        let grid = cx.new(|cx| ResultGrid::new(Arc::default(), false, cx));
        let chart = cx.new(|_| ReportChart::new());
        let mut view = Self {
            report,
            saved,
            mode,
            title,
            description,
            editor,
            sql_editor,
            editor_sql,
            spec_rows: Vec::new(),
            values: Vec::new(),
            errors: HashMap::new(),
            target: None,
            dialect: None,
            connection_choices: Vec::new(),
            session: None,
            lease: None,
            next_lease: 0,
            phase: Phase::Idle,
            cancellation: CancellationToken::new(),
            pending: false,
            risky: false,
            risk_confirmed: false,
            result: None,
            limited: false,
            elapsed_ms: None,
            bound: Vec::new(),
            ran: Vec::new(),
            show_ran: false,
            numeric: Vec::new(),
            chart_problem: None,
            grid,
            chart,
            chart_shown: false,
            notice: None,
            suggestion: None,
            field_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        };
        view.rebuild_fields(window, cx);
        view
    }

    /// The parameters follow the SQL's markers: a new marker gets a field, a
    /// removed one loses its own, and the others keep what they hold.
    fn sync_parameters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.store_spec_fields(cx);
        let names = params::names(&self.editor_sql, self.dialect);
        let current: Vec<&str> = self
            .report
            .parameters
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        if names.iter().map(String::as_str).eq(current.iter().copied()) {
            return;
        }
        self.report.parameters = params::reconcile(&self.report.parameters, &names);
        self.rebuild_fields(window, cx);
        cx.notify();
    }

    /// Fields for every parameter: its label and default while editing, its
    /// value in the form. A value already typed stays.
    fn rebuild_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.typed_values(cx);
        self.field_subscriptions.clear();
        self.spec_rows = self
            .report
            .parameters
            .iter()
            .map(|spec| SpecRow {
                name: spec.name.clone(),
                label: cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(spec.name.clone())
                        .default_value(spec.label.clone())
                }),
                default: cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(placeholder(spec.kind))
                        .default_value(spec.default.clone().unwrap_or_default())
                }),
            })
            .collect();
        for row in &self.spec_rows {
            for field in [&row.label, &row.default] {
                self.field_subscriptions.push(cx.subscribe(
                    field,
                    |_, _, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change) {
                            cx.emit(ReportEvent::Changed);
                        }
                    },
                ));
            }
        }
        self.values = self
            .report
            .parameters
            .iter()
            .map(|spec| {
                let value = typed
                    .get(&spec.name)
                    .cloned()
                    .or_else(|| spec.default.clone())
                    .unwrap_or_default();
                ValueField {
                    name: spec.name.clone(),
                    input: cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder(placeholder(spec.kind))
                            .default_value(value)
                    }),
                }
            })
            .collect();
        for field in &self.values {
            self.field_subscriptions.push(cx.subscribe(
                &field.input,
                |this, _, event: &InputEvent, cx| match event {
                    InputEvent::PressEnter { .. } => this.run(cx),
                    InputEvent::Change if !this.errors.is_empty() => {
                        this.errors.clear();
                        cx.notify();
                    }
                    _ => {}
                },
            ));
        }
    }

    /// Copies what the label and default fields hold into the definition.
    fn store_spec_fields(&mut self, cx: &App) {
        for row in &self.spec_rows {
            if let Some(spec) = self
                .report
                .parameters
                .iter_mut()
                .find(|spec| spec.name == row.name)
            {
                spec.label = row.label.read(cx).value().trim().to_owned();
                let default = row.default.read(cx).value().to_string();
                spec.default = (!default.trim().is_empty()).then_some(default);
            }
        }
    }

    fn typed_values(&self, cx: &App) -> HashMap<String, String> {
        self.values
            .iter()
            .map(|field| (field.name.clone(), field.input.read(cx).value().to_string()))
            .collect()
    }

    /// The definition as the fields hold it.
    pub fn definition(&self, cx: &App) -> Report {
        let mut report = self.report.clone();
        report.title = self.title.read(cx).value().trim().to_owned();
        report.description = self.description.read(cx).value().to_string();
        report.sql = self.editor.read(cx).text().to_string();
        for row in &self.spec_rows {
            if let Some(spec) = report
                .parameters
                .iter_mut()
                .find(|spec| spec.name == row.name)
            {
                spec.label = row.label.read(cx).value().trim().to_owned();
                let default = row.default.read(cx).value().to_string();
                spec.default = (!default.trim().is_empty()).then_some(default);
            }
        }
        if let Some(target) = &self.target {
            report.connection = ConnectionRef {
                id: target.id,
                name: target.name.clone(),
            };
        }
        report
    }

    pub fn title(&self, cx: &App) -> String {
        let title = self.title.read(cx).value().trim().to_owned();
        if title.is_empty() {
            tr!("Untitled report").to_owned()
        } else {
            title
        }
    }

    pub fn saved_id(&self) -> Option<u64> {
        self.saved.as_ref().map(|report| report.id)
    }

    /// The connection the report names, for finding it in the host.
    pub fn connection(&self) -> ConnectionRef {
        match &self.target {
            Some(target) => ConnectionRef {
                id: target.id,
                name: target.name.clone(),
            },
            None => self.report.connection.clone(),
        }
    }

    pub fn connection_name(&self) -> &str {
        self.target
            .as_ref()
            .map_or(self.report.connection.name.as_str(), |target| {
                target.name.as_str()
            })
    }

    /// The definition differs from the stored one (a draft always does once
    /// it holds SQL).
    pub fn has_unsaved_changes(&self, cx: &App) -> bool {
        let current = self.definition(cx);
        match &self.saved {
            None => !current.sql.trim().is_empty(),
            Some(saved) => {
                current.title != saved.title
                    || current.description != saved.description
                    || current.sql != saved.sql
                    || current.parameters != saved.parameters
                    || current.chart != saved.chart
                    || current.connection.name != saved.connection.name
            }
        }
    }

    pub fn is_busy(&self) -> bool {
        self.phase == Phase::Running
    }

    pub fn tab_status(&self) -> jykre::grouped_tabs::TabStatus {
        use jykre::grouped_tabs::TabStatus;
        match self.phase {
            Phase::Idle => TabStatus::Idle,
            Phase::Running => TabStatus::Running,
            Phase::Succeeded => TabStatus::Succeeded,
            Phase::Failed => TabStatus::Failed,
            Phase::Cancelled => TabStatus::Cancelled,
        }
    }

    /// Puts the keyboard where the tab is worked: the SQL while editing, the
    /// first parameter otherwise.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        match (self.mode, self.values.first()) {
            (ReportMode::View, Some(field)) => {
                field.input.update(cx, |input, cx| input.focus(window, cx));
            }
            _ => self
                .editor
                .update(cx, |editor, cx| editor.focus(window, cx)),
        }
    }

    pub fn set_catalogs(&mut self, catalogs: Arc<SqlCatalogs>, cx: &mut Context<Self>) {
        self.sql_editor
            .update(cx, |editor, cx| editor.set_catalogs(catalogs, cx));
    }

    pub fn set_connection_choices(&mut self, choices: Vec<String>, cx: &mut Context<Self>) {
        if self.connection_choices != choices {
            self.connection_choices = choices;
            cx.notify();
        }
    }

    /// Runs on `target` from now on. Another server closes the session; the
    /// same one with a new password or settings keeps the result on screen.
    pub fn set_target(&mut self, target: Option<ReportTarget>, cx: &mut Context<Self>) {
        let same = match (&self.target, &target) {
            (Some(old), Some(new)) => {
                old.name == new.name
                    && old.input.as_ref().map(|input| &input.config)
                        == new.input.as_ref().map(|input| &input.config)
            }
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.abandon(cx);
        }
        let dialect = target
            .as_ref()
            .and_then(|target| target.input.as_ref())
            .and_then(|input| {
                builtin_registry()
                    .and_then(|registry| registry.get(input.config.driver))
                    .ok()
                    .map(|driver| driver.descriptor().dialect)
            });
        let config = target
            .as_ref()
            .and_then(|target| target.input.as_ref())
            .map(|input| input.config.clone());
        self.sql_editor.update(cx, |editor, cx| {
            editor.set_connection(config.as_ref());
            editor.set_dialect(dialect, cx);
        });
        self.dialect = dialect;
        self.target = target;
        cx.notify();
    }

    /// Stops a run and closes the session, as when the connection changes.
    fn abandon(&mut self, cx: &mut Context<Self>) {
        self.cancellation.cancel();
        self.lease = None;
        if let Some(session) = self.session.take() {
            backend::close_in_background(session);
        }
        if self.phase == Phase::Running {
            self.phase = Phase::Cancelled;
        }
        self.pending = false;
        self.risky = false;
        cx.notify();
    }

    /// The host stored the definition as `report`.
    pub fn saved(&mut self, report: Report, cx: &mut Context<Self>) {
        self.report.id = report.id;
        self.report.created_at = report.created_at;
        self.report.updated_at = report.updated_at;
        self.saved = Some(report);
        self.notice = Some(Feedback::new(Tone::Success, tr!("Report saved.")));
        cx.notify();
    }

    /// The stored report was removed: the tab is a draft again.
    pub fn forget_saved(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.saved_id() == Some(id) {
            self.saved = None;
            self.report.id = 0;
            cx.notify();
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let report = self.definition(cx);
        if report.title.is_empty() {
            self.notice = Some(Feedback::new(
                Tone::Warning,
                tr!("Give the report a title before saving it."),
            ));
            cx.notify();
            return;
        }
        if report.sql.trim().is_empty() {
            self.notice = Some(Feedback::new(
                Tone::Warning,
                tr!("Write the report's SQL before saving it."),
            ));
            cx.notify();
            return;
        }
        cx.emit(ReportEvent::Save(Box::new(report)));
    }

    fn set_mode(&mut self, mode: ReportMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.store_spec_fields(cx);
        self.mode = mode;
        self.focus(window, cx);
        cx.notify();
    }

    fn options(&self) -> ExecutionOptions {
        ExecutionOptions {
            max_rows: self
                .target
                .as_ref()
                .map_or(jykre::result::DEFAULT_ROW_LIMIT, |target| target.row_limit),
            ..ExecutionOptions::default()
        }
    }

    /// Runs every statement of the SQL with the values in the form.
    pub fn run(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Running {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("The report is running. Wait for it, or stop it first."),
            ));
            cx.notify();
            return;
        }
        let Some(target) = self.target.clone() else {
            self.notice = Some(Feedback::new(
                Tone::Warning,
                tr!("Choose a connection for this report."),
            ));
            cx.notify();
            return;
        };
        let Some(input) = target.input.clone() else {
            self.notice = Some(Feedback::new(
                Tone::Warning,
                tr!(
                    "The connection {0} is not available. Choose another one.",
                    target.name
                ),
            ));
            cx.notify();
            return;
        };
        if connection_store::needs_password(&input) {
            self.pending = true;
            cx.emit(ReportEvent::PasswordRequired);
            return;
        }
        self.store_spec_fields(cx);
        let sql = self.editor.read(cx).text().to_string();
        let typed = self.typed_values(cx);
        let requests = match params::requests(
            &sql,
            self.dialect,
            &self.report.parameters,
            &typed,
            &self.options(),
        ) {
            Ok(requests) => requests,
            Err(errors) => {
                self.errors = errors
                    .into_iter()
                    .map(|error| (error.name, error.problem))
                    .collect();
                self.mode = ReportMode::View;
                self.notice = Some(Feedback::new(
                    Tone::Warning,
                    tr!("Fill in the parameters marked below."),
                ));
                cx.notify();
                return;
            }
        };
        self.errors.clear();
        if requests.is_empty() {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("There is no SQL statement to execute."),
            ));
            cx.notify();
            return;
        }
        let statements: Vec<String> = requests.iter().map(|request| request.sql.clone()).collect();
        let confirmed = std::mem::take(&mut self.risk_confirmed);
        if target.environment == Some(jykre::connection_appearance::Environment::Production)
            && !confirmed
        {
            let risks = jykre::guard::risks(&statements, self.dialect);
            if !risks.is_empty() {
                self.risky = true;
                cx.emit(ReportEvent::ConfirmRisk {
                    connection: target.name.clone(),
                    description: super::workspace::risk_description(&risks, statements.len()),
                });
                return;
            }
        }
        self.bound = self
            .report
            .parameters
            .iter()
            .map(|spec| {
                let typed = typed.get(&spec.name).map_or("", String::as_str);
                (
                    spec.shown_label().to_owned(),
                    params::value(spec, typed).ok().flatten(),
                )
            })
            .collect();
        self.ran = statements;
        self.phase = Phase::Running;
        self.notice = None;
        self.mode = ReportMode::View;
        self.cancellation = CancellationToken::new();
        let cancel = self.cancellation.clone();
        self.next_lease += 1;
        let lease = self.next_lease;
        self.lease = Some(lease);
        let session = self.session.take();
        cx.emit(ReportEvent::Changed);
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(backend::query(
                input,
                session,
                requests,
                DEFAULT_MEMORY_CEILING,
                cancel,
                None,
            ))
            .await;
            let mut pending = Some(result);
            let _ = weak.update(cx, |this, cx| {
                if let Some(result) = pending.take() {
                    this.finish(lease, result, cx);
                }
            });
            // The tab closed meanwhile: its session closes with it.
            if let Some(Ok(run)) = pending
                && let Some(session) = run.session
            {
                backend::close_in_background(session);
            }
        })
        .detach();
    }

    fn finish(
        &mut self,
        lease: u64,
        result: DriverResult<backend::SessionRun>,
        cx: &mut Context<Self>,
    ) {
        let current = self.lease == Some(lease);
        let batch = match result {
            Ok(run) => {
                if let Some(session) = run.session {
                    if current {
                        if let Some(previous) = self.session.replace(session) {
                            backend::close_in_background(previous);
                        }
                    } else {
                        backend::close_in_background(session);
                    }
                }
                run.batch
            }
            Err(error) => Err(error),
        };
        if !current {
            return;
        }
        self.lease = None;
        match batch {
            Ok(batch) => {
                let failed = batch.error;
                if let Some(last) = batch.completed.into_iter().last() {
                    self.limited = last.has_more || last.limit.is_some();
                    self.elapsed_ms = Some(last.elapsed_ms);
                    self.show_result(Arc::new(last.data), cx);
                }
                match failed {
                    Some(error) => self.failed(&error),
                    None => self.phase = Phase::Succeeded,
                }
            }
            Err(error) => self.failed(&error),
        }
        cx.emit(ReportEvent::Changed);
        cx.notify();
    }

    fn failed(&mut self, error: &DriverError) {
        if error.code == ErrorCode::Cancelled {
            self.phase = Phase::Cancelled;
            self.notice = Some(Feedback::new(Tone::Warning, tr!("The report was stopped.")));
        } else {
            self.phase = Phase::Failed;
            self.notice = Some(Feedback::new(
                Tone::Error,
                tr!("The report failed: {0}", error),
            ));
        }
    }

    fn show_result(&mut self, data: Arc<ResultData>, cx: &mut Context<Self>) {
        self.numeric = chart::numeric_columns(&data);
        self.suggestion = chart::suggest(&data);
        if self.report.chart.is_none() && self.saved.is_none() {
            self.report.chart = self.suggestion.clone();
        }
        self.grid.update(cx, |grid, cx| {
            grid.set_page(data.clone(), false, 0, None, cx)
        });
        if let Some(previous) = self.result.replace(data)
            && let Ok(previous) = Arc::try_unwrap(previous)
        {
            backend::release(previous);
        }
        self.derive_chart(cx);
    }

    /// Computes what the chart draws, when the result or the chart changes.
    fn derive_chart(&mut self, cx: &mut Context<Self>) {
        use jykre::reports::chart::ChartProblem;
        let derived = match (&self.result, &self.report.chart) {
            (Some(result), Some(spec)) => Some(chart::derive(result, spec)),
            _ => None,
        };
        let (data, problem) = match derived {
            None => (None, None),
            Some(Ok(data)) => (Some(Rc::new(data)), None),
            Some(Err(problem)) => (
                None,
                Some(match problem {
                    ChartProblem::NoValues => {
                        tr!("Choose a column of values for the chart.").to_owned()
                    }
                    ChartProblem::MissingColumn(name) => {
                        tr!("The result has no column {0} for the chart.", name)
                    }
                    ChartProblem::NotNumeric(name) => {
                        tr!("The column {0} does not hold numbers.", name)
                    }
                    ChartProblem::PieNeedsOneValue => {
                        tr!("A pie shows a single column of values.").to_owned()
                    }
                }),
            ),
        };
        self.chart_shown = data.is_some();
        self.chart_problem = problem;
        self.chart.update(cx, |chart, cx| chart.set_data(data, cx));
    }

    fn set_chart(&mut self, chart: Option<ChartSpec>, cx: &mut Context<Self>) {
        self.report.chart = chart;
        self.derive_chart(cx);
        cx.emit(ReportEvent::Changed);
        cx.notify();
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Running {
            self.cancellation.cancel();
            cx.notify();
        }
    }

    /// The password is in memory: the waiting run starts.
    pub fn resume_pending(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.pending) {
            self.run(cx);
        }
    }

    pub fn cancel_pending(&mut self) {
        self.pending = false;
    }

    /// The user confirmed the risky statements: the waiting run starts.
    pub fn confirm_risk(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.risky) {
            self.risk_confirmed = true;
            self.run(cx);
        }
    }

    /// The connection was given its password or new settings: the next run
    /// uses them, on the same session when the server is the same.
    pub fn update_input(&mut self, input: ResolvedConnection, cx: &mut Context<Self>) {
        if let Some(target) = &mut self.target {
            target.input = Some(input);
        }
        cx.notify();
    }

    fn copy(&mut self, tabs: bool, confirmed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self
            .result
            .clone()
            .filter(|result| result.column_count() > 0)
        else {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("Run the report before copying its result."),
            ));
            cx.notify();
            return;
        };
        let bytes = jykre::grid::csv_bytes(&result);
        if !confirmed && bytes > jykre::grid::COPY_CONFIRM_BYTES {
            let view = cx.entity().downgrade();
            super::confirm(
                window,
                cx,
                super::Confirmation {
                    title: tr!("Copy about {0} MB?", bytes / (1024 * 1024)),
                    description: tr!(
                        "The result is large: copying it can take a moment and holds all of it in the clipboard."
                    )
                    .to_string(),
                    ok: tr!("Copy"),
                    cancel: tr!("Cancel"),
                    focus: super::ConfirmFocus::Dialog,
                },
                move |window, cx| {
                    let _ = view.update(cx, |this, cx| this.copy(tabs, true, window, cx));
                },
            );
            return;
        }
        let text = if tabs {
            let headers: Vec<&str> = result.columns().iter().map(|c| c.name.as_str()).collect();
            let rows: Vec<Vec<Option<&str>>> = (0..result.row_count())
                .map(|row| result.row(row).collect())
                .collect();
            jykre::cell_format::copy_cells(Some(&headers), &rows)
        } else {
            jykre::cell_format::result_csv(&result)
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.notice = Some(Feedback::new(
            Tone::Success,
            if tabs {
                tr!("Copied {0} rows for a spreadsheet.", result.row_count())
            } else {
                tr!("Copied {0} rows as CSV.", result.row_count())
            },
        ));
        cx.notify();
    }

    /// Writes the print document where the user chooses and opens it in the
    /// browser, which prints it or saves it as PDF.
    fn export(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self
            .result
            .clone()
            .filter(|result| result.column_count() > 0)
        else {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("Run the report before exporting it."),
            ));
            cx.notify();
            return;
        };
        let title = self.title(cx);
        let description = self.description.read(cx).value().to_string();
        let chart = self
            .report
            .chart
            .as_ref()
            .and_then(|spec| chart::derive(&result, spec).ok());
        let html = export::html(&export::Document {
            title: &title,
            description: &description,
            connection: self.connection_name(),
            parameters: self.bound.clone(),
            result: &result,
            limited: self.limited,
            chart: chart.as_ref(),
            generated: export::now_utc(),
        });
        let directory = dirs::document_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_default();
        let name = export::file_name(&title, "html");
        let chosen = cx.prompt_for_new_path(&directory, Some(&name));
        cx.spawn(async move |weak, cx| {
            let path = match chosen.await {
                Ok(Ok(Some(path))) => export::with_extension(&path, "html"),
                // The user closed the dialog.
                Ok(Ok(None)) => return,
                _ => {
                    let _ = weak.update(cx, |this, cx| {
                        this.notice = Some(Feedback::new(
                            Tone::Error,
                            tr!("The save dialog is not available on this system."),
                        ));
                        cx.notify();
                    });
                    return;
                }
            };
            let target = path.clone();
            let written = backend::run(async move {
                export::write(target, html)
                    .await
                    .map_err(|error| DriverError::new(ErrorCode::Internal, error.to_string()))
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                match written {
                    Ok(()) => {
                        cx.open_with_system(&path);
                        this.notice = Some(Feedback::new(
                            Tone::Success,
                            tr!(
                                "Saved {0}. It opens in the browser, which prints it or saves it as PDF.",
                                path.display()
                            ),
                        ));
                    }
                    Err(error) => {
                        this.notice = Some(Feedback::new(
                            Tone::Error,
                            tr!("The report could not be saved: {0}", error),
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn connection_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let current = self.connection_name().to_owned();
        let shown = if current.is_empty() {
            tr!("Choose a connection").to_owned()
        } else {
            current.clone()
        };
        let choices = self.connection_choices.clone();
        Button::new("report-connection")
            .ghost()
            .small()
            .icon(I::Server)
            .label(shown.clone())
            .dropdown_caret(true)
            .accessibility_label(tr!("Connection: {0}", shown))
            .tooltip(tr!("Connection of this report"))
            .dropdown_menu(move |mut menu, _, _| {
                for name in &choices {
                    let view = view.clone();
                    let chosen = name.clone();
                    menu = menu.item(
                        PopupMenuItem::new(name.clone())
                            .checked(*name == current)
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    if this.connection_name() != chosen {
                                        cx.emit(ReportEvent::ConnectionRequested(chosen.clone()));
                                    }
                                });
                            }),
                    );
                }
                menu
            })
    }

    fn toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let editing = self.mode == ReportMode::Edit;
        let unsaved = self.has_unsaved_changes(cx);
        let running = self.phase == Phase::Running;
        let view = cx.entity().downgrade();
        let copy_view = view.clone();
        let _ = window;
        row()
            .h(px(48.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(icon(I::FileChartColumn, MINT))
            .child(if editing {
                div()
                    .w(px(280.))
                    .child(
                        Input::new(&self.title)
                            .small()
                            .aria_label(tr!("Report title")),
                    )
                    .into_any_element()
            } else {
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(self.title(cx))
                    .into_any_element()
            })
            .child(self.connection_menu(cx))
            .child(div().flex_1())
            .child(if editing {
                button("report-view", I::Eye, tr!("Done editing")).on_click(
                    cx.listener(|this, _, window, cx| this.set_mode(ReportMode::View, window, cx)),
                )
            } else {
                button("report-edit", I::Pencil, tr!("Edit")).on_click(
                    cx.listener(|this, _, window, cx| this.set_mode(ReportMode::Edit, window, cx)),
                )
            })
            .child(
                icon_button(
                    "report-save",
                    if unsaved { I::Save } else { I::BookmarkCheck },
                    if unsaved {
                        tr!("Save report")
                    } else {
                        tr!("Saved in the reports")
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
            .child(
                Button::new("report-copy")
                    .ghost()
                    .small()
                    .icon(I::Copy)
                    .dropdown_caret(true)
                    .accessibility_label(tr!("Copy the result"))
                    .tooltip(tr!("Copy the result"))
                    .dropdown_menu(move |menu, _, _| {
                        let csv = copy_view.clone();
                        let tsv = copy_view.clone();
                        menu.item(PopupMenuItem::new(tr!("Copy as CSV")).on_click(
                            move |_, window, cx| {
                                let _ =
                                    csv.update(cx, |this, cx| this.copy(false, false, window, cx));
                            },
                        ))
                        .item(
                            PopupMenuItem::new(tr!("Copy for a spreadsheet")).on_click(
                                move |_, window, cx| {
                                    let _ = tsv
                                        .update(cx, |this, cx| this.copy(true, false, window, cx));
                                },
                            ),
                        )
                    }),
            )
            .child(
                icon_button(
                    "report-export",
                    I::Printer,
                    tr!("Export for printing or PDF…"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.export(cx))),
            )
            .child(divider())
            .child(if running {
                button("report-stop", I::Square, tr!("Stop"))
                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
            } else {
                primary_button("report-run", I::Play, tr!("Run"))
                    .on_click(cx.listener(|this, _, _, cx| this.run(cx)))
            })
    }

    fn definition_panel(&self, cx: &mut Context<Self>) -> Div {
        column()
            .flex_none()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                Input::new(&self.description)
                    .small()
                    .aria_label(tr!("Report description")),
            )
            .child(
                div()
                    .h(px(220.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .key_context(SQL_EDITOR_CONTEXT)
                    .child(Editor::new(&self.editor).h_full()),
            )
            .when(!self.spec_rows.is_empty(), |s| {
                s.child(label(tr!("Parameters"))).children(
                    self.spec_rows
                        .iter()
                        .enumerate()
                        .map(|(index, row)| self.spec_row(index, row, cx)),
                )
            })
    }

    fn spec_row(&self, index: usize, row_fields: &SpecRow, cx: &mut Context<Self>) -> Div {
        let Some(spec) = self
            .report
            .parameters
            .iter()
            .find(|spec| spec.name == row_fields.name)
            .cloned()
        else {
            return div();
        };
        let view = cx.entity().downgrade();
        let name = spec.name.clone();
        row()
            .gap_2()
            .child(
                div()
                    .w(px(120.))
                    .font_family("Cascadia Code")
                    .text_size(px(13.))
                    .text_color(rgb(SQL_PARAMETER))
                    .truncate()
                    .child(format!(":{}", spec.name)),
            )
            .child(
                div().w(px(180.)).child(
                    Input::new(&row_fields.label)
                        .small()
                        .aria_label(tr!("Label of {0}", spec.name)),
                ),
            )
            .child(
                Button::new(("parameter-kind", index))
                    .ghost()
                    .small()
                    .label(kind_name(spec.kind))
                    .dropdown_caret(true)
                    .accessibility_label(tr!("Type of {0}", spec.name))
                    .dropdown_menu(move |mut menu, _, _| {
                        for kind in KINDS {
                            let view = view.clone();
                            let name = name.clone();
                            menu = menu.item(
                                PopupMenuItem::new(kind_name(kind))
                                    .checked(kind == spec.kind)
                                    .on_click(move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.set_kind(&name, kind, window, cx)
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .child(
                div().w(px(160.)).child(
                    Input::new(&row_fields.default)
                        .small()
                        .aria_label(tr!("Default value of {0}", spec.name)),
                ),
            )
            .child({
                let name = spec.name.clone();
                button(
                    ("parameter-required", index),
                    if spec.required {
                        I::SquareCheck
                    } else {
                        I::Square
                    },
                    tr!("Required"),
                )
                .aria_selected(spec.required)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.store_spec_fields(cx);
                    if let Some(spec) = this.report.parameters.iter_mut().find(|s| s.name == name) {
                        spec.required = !spec.required;
                    }
                    cx.emit(ReportEvent::Changed);
                    cx.notify();
                }))
            })
    }

    fn set_kind(
        &mut self,
        name: &str,
        kind: ParameterKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store_spec_fields(cx);
        if let Some(spec) = self
            .report
            .parameters
            .iter_mut()
            .find(|spec| spec.name == name)
        {
            spec.kind = kind;
        }
        self.rebuild_fields(window, cx);
        cx.emit(ReportEvent::Changed);
        cx.notify();
    }

    fn parameter_form(&self) -> Div {
        row()
            .flex_none()
            .flex_wrap()
            .items_end()
            .gap_3()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            .children(self.values.iter().filter_map(|field| {
                let spec = self
                    .report
                    .parameters
                    .iter()
                    .find(|spec| spec.name == field.name)?;
                let problem = self.errors.get(&field.name);
                Some(
                    column()
                        .w(px(200.))
                        .gap_1()
                        .child(label(if spec.required {
                            format!("{} *", spec.shown_label())
                        } else {
                            spec.shown_label().to_owned()
                        }))
                        .child(
                            div()
                                .rounded(px(5.))
                                .border_1()
                                .border_color(rgb(if problem.is_some() { RED } else { BORDER }))
                                .child(
                                    Input::new(&field.input)
                                        .small()
                                        .aria_label(spec.shown_label().to_owned()),
                                ),
                        )
                        .when_some(problem, |s, problem| {
                            s.child(div().text_size(px(12.)).text_color(rgb(RED)).child(
                                match problem {
                                    ParameterProblem::Missing => tr!("Required"),
                                    ParameterProblem::Invalid => invalid_hint(spec.kind),
                                },
                            ))
                        }),
                )
            }))
    }

    fn chart_controls(&self, cx: &mut Context<Self>) -> Div {
        let Some(result) = &self.result else {
            return div();
        };
        let view = cx.entity().downgrade();
        let spec = self.report.chart.clone();
        let columns: Vec<String> = result.columns().iter().map(|c| c.name.clone()).collect();
        let kind_view = view.clone();
        let kind_spec = spec.clone();
        let suggestion = self.suggestion.clone();
        let label_view = view.clone();
        let label_spec = spec.clone();
        row()
            .flex_none()
            .flex_wrap()
            .gap_2()
            .px_3()
            .py_1()
            .child(label(tr!("Chart")))
            .child(
                Button::new("chart-kind")
                    .ghost()
                    .small()
                    .label(
                        spec.as_ref()
                            .map_or(tr!("None"), |spec| chart_name(spec.kind)),
                    )
                    .dropdown_caret(true)
                    .accessibility_label(tr!("Chart type"))
                    .dropdown_menu(move |menu, _, _| {
                        let none_view = kind_view.clone();
                        let mut menu = menu.item(
                            PopupMenuItem::new(tr!("None"))
                                .checked(kind_spec.is_none())
                                .on_click(move |_, _, cx| {
                                    let _ =
                                        none_view.update(cx, |this, cx| this.set_chart(None, cx));
                                }),
                        );
                        for kind in ChartKind::ALL {
                            let view = kind_view.clone();
                            let current = kind_spec.clone();
                            let suggestion = suggestion.clone();
                            menu = menu.item(
                                PopupMenuItem::new(chart_name(kind))
                                    .checked(current.as_ref().is_some_and(|spec| spec.kind == kind))
                                    .on_click(move |_, _, cx| {
                                        let mut spec = current
                                            .clone()
                                            .or_else(|| suggestion.clone())
                                            .unwrap_or(ChartSpec {
                                                kind,
                                                label: String::new(),
                                                values: Vec::new(),
                                            });
                                        spec.kind = kind;
                                        if kind == ChartKind::Pie {
                                            spec.values.truncate(1);
                                        }
                                        let _ = view
                                            .update(cx, |this, cx| this.set_chart(Some(spec), cx));
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .when_some(spec.clone(), |s, current| {
                let label_current = label_spec.clone().unwrap_or(current.clone());
                s.child(label(tr!("Labels")))
                    .child(
                        Button::new("chart-label")
                            .ghost()
                            .small()
                            .label(if current.label.is_empty() {
                                tr!("Choose").to_owned()
                            } else {
                                current.label.clone()
                            })
                            .dropdown_caret(true)
                            .accessibility_label(tr!("Column that labels the chart"))
                            .dropdown_menu(move |mut menu, _, _| {
                                for name in &columns {
                                    let view = label_view.clone();
                                    let spec = label_current.clone();
                                    let chosen = name.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(name.clone())
                                            .checked(spec.label == *name)
                                            .on_click(move |_, _, cx| {
                                                let mut spec = spec.clone();
                                                spec.label = chosen.clone();
                                                let _ = view.update(cx, |this, cx| {
                                                    this.set_chart(Some(spec), cx)
                                                });
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                    .child(label(tr!("Values")))
                    .children(self.numeric.iter().enumerate().map(|(index, name)| {
                        let on = current.values.contains(name);
                        let name = name.clone();
                        let shown = name.clone();
                        let current = current.clone();
                        div()
                            .id(("chart-value", index))
                            .role(Role::CheckBox)
                            .aria_label(tr!("Chart the column {0}", shown))
                            .aria_selected(on)
                            .focusable()
                            .tab_index(0)
                            .focus_visible(|s| s.border_color(rgb(MINT)))
                            .border_1()
                            .border_color(rgb(if on { MINT } else { BORDER }))
                            .rounded(px(4.))
                            .px_2()
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(rgb(if on { MINT } else { MUTED }))
                            .child(shown)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let mut spec = current.clone();
                                if spec.values.contains(&name) {
                                    spec.values.retain(|value| *value != name);
                                } else if spec.kind == ChartKind::Pie {
                                    spec.values = vec![name.clone()];
                                } else {
                                    spec.values.push(name.clone());
                                }
                                this.set_chart(Some(spec), cx);
                            }))
                    }))
            })
            .when_some(self.chart_problem.clone(), |s, problem| {
                s.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(AMBER))
                        .child(problem),
                )
            })
    }

    fn body(&self) -> AnyElement {
        match &self.result {
            // The chart and the grid share the room left: the chart up to
            // 280 px, so neither spills over the footer while editing.
            Some(result) if result.column_count() > 0 => column()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .when(self.chart_shown, |s| {
                    s.child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .max_h(px(280.))
                            .px_3()
                            .py_2()
                            .child(
                                self.chart
                                    .clone()
                                    .cached(StyleRefinement::default().size_full()),
                            ),
                    )
                })
                .child(
                    div().flex_1().min_h_0().child(
                        self.grid
                            .clone()
                            .cached(StyleRefinement::default().size_full()),
                    ),
                )
                .into_any_element(),
            Some(_) => centered(tr!("The last statement returned no rows to show.")),
            None if self.phase == Phase::Running => centered(tr!("Running the report…")),
            None => centered(if self.values.is_empty() {
                tr!("Run the report to see its result.")
            } else {
                tr!("Fill in the parameters and run the report.")
            }),
        }
        .into_any_element()
    }

    fn footer(&self, cx: &mut Context<Self>) -> Div {
        let mut parts = Vec::new();
        if let Some(result) = &self.result {
            parts.push(tr!(
                "{0} rows",
                jykre::cell_format::count(result.row_count())
            ));
        }
        if let Some(elapsed) = self.elapsed_ms {
            parts.push(jykre::cell_format::duration(elapsed as u64));
        }
        column()
            .flex_none()
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(
                row()
                    .h(px(30.))
                    .px_3()
                    .gap_2()
                    .child(label(parts.join(" · ")))
                    .when(self.limited, |s| {
                        s.child(badge(tr!("Stopped at the row limit"), AMBER))
                    })
                    .child(div().flex_1())
                    .when(!self.ran.is_empty(), |s| {
                        s.child(
                            button(
                                "report-ran",
                                I::Code,
                                if self.show_ran {
                                    tr!("Hide the SQL that ran")
                                } else {
                                    tr!("Show the SQL that ran")
                                },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_ran = !this.show_ran;
                                cx.notify();
                            })),
                        )
                    }),
            )
            .when(self.show_ran, |s| {
                s.child(
                    div()
                        .px_3()
                        .pb_2()
                        .font_family("Cascadia Code")
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child(self.ran.join("\n"))
                        .child(
                            div().pt_1().text_color(rgb(FAINT)).child(tr!(
                                "Parameter values are sent apart from the SQL and bound by the database."
                            )),
                        ),
                )
            })
    }
}

impl Render for ReportView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        column()
            .id("report")
            .size_full()
            .bg(rgb(BG))
            .child(self.toolbar(window, cx))
            .when_some(self.notice.clone(), |s, notice| {
                s.child(div().px_3().py_2().child(feedback_line(&notice)))
            })
            .when(self.mode == ReportMode::Edit, |s| {
                s.child(self.definition_panel(cx))
            })
            .when(!self.values.is_empty(), |s| s.child(self.parameter_form()))
            .when(self.result.is_some(), |s| s.child(self.chart_controls(cx)))
            .child(self.body())
            .child(self.footer(cx))
    }
}

impl Drop for ReportView {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(session) = self.session.take() {
            backend::close_in_background(session);
        }
    }
}

const KINDS: [ParameterKind; 5] = [
    ParameterKind::Text,
    ParameterKind::Number,
    ParameterKind::Boolean,
    ParameterKind::Date,
    ParameterKind::Timestamp,
];

fn kind_name(kind: ParameterKind) -> &'static str {
    match kind {
        ParameterKind::Text => tr!("Text"),
        ParameterKind::Number => tr!("Number"),
        ParameterKind::Boolean => tr!("True or false"),
        ParameterKind::Date => tr!("Date"),
        ParameterKind::Timestamp => tr!("Date and time"),
    }
}

fn placeholder(kind: ParameterKind) -> &'static str {
    match kind {
        ParameterKind::Text => tr!("Text"),
        ParameterKind::Number => tr!("e.g. 1250.50"),
        ParameterKind::Boolean => tr!("true or false"),
        ParameterKind::Date => tr!("YYYY-MM-DD"),
        ParameterKind::Timestamp => tr!("YYYY-MM-DD HH:MM:SS"),
    }
}

fn invalid_hint(kind: ParameterKind) -> &'static str {
    match kind {
        ParameterKind::Text => tr!("Invalid text"),
        ParameterKind::Number => tr!("A number, with a dot for decimals"),
        ParameterKind::Boolean => tr!("true or false"),
        ParameterKind::Date => tr!("A date as YYYY-MM-DD"),
        ParameterKind::Timestamp => tr!("A date and time as YYYY-MM-DD HH:MM"),
    }
}

fn chart_name(kind: ChartKind) -> &'static str {
    match kind {
        ChartKind::Bar => tr!("Bars"),
        ChartKind::Line => tr!("Lines"),
        ChartKind::Pie => tr!("Pie"),
    }
}

fn centered(text: impl Into<SharedString>) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(14.))
        .text_color(rgb(MUTED))
        .child(text.into())
        .into_any_element()
}
