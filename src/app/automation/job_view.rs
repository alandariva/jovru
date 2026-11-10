//! A job's tab: its **Editor** (a SQL Job, a program or a chain, as a
//! form), its **Inputs** (the run form, **Run** and **Stop**, the run's
//! progress and the latest SQL result) and its **Runs** (this job's
//! history).
//!
//! The view holds the definition being edited and the run it started; the
//! engine decides and runs. A run goes: the form's values, the password of
//! each connection that needs one (one after another), then one
//! confirmation (production risks, stored outputs, a program's first run,
//! or a chain's whole plan), then the engine. Shortcuts that run SQL in a
//! query tab never run a job.
mod editor;
mod inputs;
mod schedule;

use super::history::{HistoryEvent, HistoryList, LogLines, log_lines, read_log_lines};
use super::program_bits::Stream;
use super::words::{self, Level, Line};
use super::{FRAME, JobSources, Jobs, logic, source_of, whole};
use crate::app::result_grid::ResultGrid;
use crate::app::sql_editor::{GoToLine, SqlCatalogs, SqlEditor, SqlEditorEvent};
use crate::app::ui::*;
use gpui_kit::component::input::{EditorState, InputEvent, InputState};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::automation::chain::form_values;
use jykre::automation::definition::{
    self, ChainStep, JobBody, JobDefinition, OutputSpec, Protocol, Source, SqlOutput,
};
use jykre::automation::{
    Availability, ChainHandle, ChainRequest, ChainStatus, ConnectionInput, Engine, JobId, JobKind,
    JobRequest, Plan, Refusal, RunHandle, RunStatus, SqlJob,
};
use jykre::drivers::*;
use jykre::grouped_tabs::TabStatus;
use jykre::parameters::ParameterProblem;
use jykre::reports::{ParameterSpec, params};
use jykre::{backend, connection_store, tr};
use jykre_storage::StoredVersion;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// What the AppView answers for the view.
pub enum JobEvent {
    /// A run needs this connection's password; the host asks and calls
    /// [`JobView::resume_pending`] or [`JobView::cancel_pending`].
    PasswordRequired(ConnectionId),
    /// One confirmation; the host calls [`JobView::confirmed`] when the
    /// user agrees.
    Confirm {
        title: String,
        description: String,
        ok: &'static str,
        what: Ask,
    },
    History(HistoryEvent),
    /// **Duplicate**: an unsaved copy in a tab of its own.
    Duplicate(Box<JobDefinition>),
    /// The job was deleted: its tab closes.
    Deleted,
}

impl EventEmitter<JobEvent> for JobView {}

/// What a confirmation agrees to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// The run waiting for it (production, a program's first run, a
    /// chain's plan).
    Run,
    DeleteJob,
    DeleteVersion(i64),
    /// Storing the outputs of a job on a production connection (ADR-0035).
    StoreOutputs,
    /// The consent dialog of the job's schedule (section 9.5.6).
    ScheduleConsent,
    DeleteSchedule,
}

/// The tab's inner tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inner {
    Editor,
    Inputs,
    Runs,
    /// A saved job's schedule (section 9.5).
    Schedule,
}

/// What a new view starts from.
pub struct Setup {
    pub engine: Engine,
    pub definition: JobDefinition,
    /// The stored revision; `None` for a job never saved.
    pub revision: Option<i64>,
    pub sources: JobSources,
    pub catalogs: Arc<SqlCatalogs>,
    pub jobs: Jobs,
    pub availability: Availability,
    pub connections: Rc<HashMap<ConnectionId, (String, Option<SqlDialect>)>>,
}

/// A parameter's or an input's editable fields. A SQL Job's names come from
/// its `:name` markers; a program's and a chain's are typed.
struct SpecRow {
    name: Entity<InputState>,
    label: Entity<InputState>,
    default: Entity<InputState>,
    choice: Entity<InputState>,
    choices_open: bool,
}

/// A declared output's fields: its name, and a SQL Job's column position.
struct OutputRow {
    name: Entity<InputState>,
    column: Entity<InputState>,
}

/// A value field of the run form.
struct ValueField {
    name: String,
    input: Entity<InputState>,
}

/// The SQL Job's editor.
struct SqlFields {
    editor: Entity<EditorState>,
    sql_editor: Entity<SqlEditor>,
    /// The editor's **Go to line** prompt (`Ctrl G`).
    go_to_line: Entity<GoToLine>,
    max_rows: Entity<InputState>,
    /// The lines where statements beyond the first start.
    extra: Vec<usize>,
    revision: u64,
}

/// A program's fields.
struct ProgramFields {
    /// The program and its arguments, as one command line.
    command: Entity<InputState>,
    /// The command line the field started with, and the program and
    /// arguments it stands for: left alone, they are kept as stored.
    stored: (String, String, Vec<String>),
    /// The command line as typed, read when it changes.
    read: Result<super::program_bits::Command, super::program_bits::SplitError>,
    entrypoint: Entity<InputState>,
    working_directory: Entity<InputState>,
    unset: Vec<Entity<InputState>>,
    /// The variables of the environment now that the program will not
    /// receive, read again as the lists change.
    removed: Vec<String>,
    set: Vec<(Entity<InputState>, Entity<InputState>)>,
}

/// A chain's fields and what was read about the versions it names.
#[derive(Default)]
struct ChainFields {
    steps: Vec<StepFields>,
    /// The chain checked against its versions (`Engine::plan`).
    plan: Option<Plan>,
    plan_generation: u64,
    /// Bumped by each change typed, so the plan is read again once typing
    /// pauses.
    typing: u64,
    /// The published versions of each SQL Job and program.
    versions: HashMap<JobId, Vec<StoredVersion>>,
}

struct StepFields {
    id: Entity<InputState>,
    /// The constant of each input bound to one.
    constants: HashMap<String, Entity<InputState>>,
}

/// A run waiting for the user, and what it already has.
struct Pending {
    values: HashMap<String, String>,
    /// The one confirmation was given.
    confirmed: bool,
    /// A chain's plan, read once.
    plan: Option<Plan>,
    /// Whether each program the run starts has run before at its revision
    /// or version (Q-10), read once.
    programs_known: Option<Vec<bool>>,
}

/// What the last finished run left to look at, under its account.
enum Shown {
    Nothing,
    /// A SQL run's rows, in the grid.
    Rows {
        rows: usize,
        columns: usize,
        /// The chain's step they came from.
        step: Option<String>,
    },
    /// What a program printed.
    Output(Box<ProgramOutput>),
}

/// A program run's stdout and stderr, read from its logs when it ended.
struct ProgramOutput {
    run: jykre::automation::RunId,
    /// The chain's step that printed it.
    step: Option<String>,
    /// The stream on screen.
    stream: Stream,
    stdout: Option<LogLines>,
    stderr: Option<LogLines>,
    reading: bool,
    scroll: UniformListScrollHandle,
}

/// The run this tab started, while it is active or until another starts.
enum Active {
    Idle,
    Run {
        handle: RunHandle,
        status: RunStatus,
    },
    Chain {
        handle: ChainHandle,
        status: ChainStatus,
        /// The active step's run and its status.
        step: Option<RunHandle>,
        step_status: Option<RunStatus>,
        step_ids: Vec<String>,
    },
}

pub struct JobView {
    engine: Engine,
    kind: JobKind,
    id: JobId,
    /// The stored revision; `None` until first saved.
    revision: Option<i64>,
    /// The definition as stored.
    saved: Option<JobDefinition>,
    /// Structure the fields do not hold (kinds, choices, sources); the
    /// text fields are read into it by [`JobView::current`].
    definition: JobDefinition,
    unsaved: bool,
    inner: Inner,
    name: Entity<InputState>,
    description: Entity<InputState>,
    timeout: Entity<InputState>,
    sql: Option<SqlFields>,
    program: Option<ProgramFields>,
    chain: Option<ChainFields>,
    specs: Vec<SpecRow>,
    outputs: Vec<OutputRow>,
    versions: Vec<StoredVersion>,
    values: Vec<ValueField>,
    value_errors: HashMap<String, ParameterProblem>,
    active: Active,
    pending: Option<Pending>,
    /// What the editor says after Save, Publish or Delete.
    notice: Option<Feedback>,
    /// What the last run said: its outcome, or why it did not run.
    run_lines: Vec<Line>,
    /// The database's error of the last run, shown as every error is.
    run_error: Option<words::RunError>,
    /// The statement of the run on its way, when it is sent as written (no
    /// `:name` parameters): an error's position is read against it.
    ran_sql: Option<String>,
    /// A refused program run names its changed file: **Publish** fixes it.
    changed_file: Option<String>,
    /// The form was prefilled from a chain step's stored outputs.
    prefill_note: Option<String>,
    grid: Entity<ResultGrid>,
    has_result: bool,
    shown: Shown,
    history: Entity<HistoryList>,
    sources: JobSources,
    catalogs: Arc<SqlCatalogs>,
    jobs: Jobs,
    availability: Availability,
    busy: bool,
    editor_scroll: ScrollHandle,
    inputs_scroll: ScrollHandle,
    schedule_scroll: ScrollHandle,
    /// The Schedule tab, made when first opened.
    schedule: Option<schedule::ScheduleFields>,
    focus: FocusHandle,
    field_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
    watch: Option<Task<()>>,
    step_watch: Option<Task<()>>,
    tick: Option<Task<()>>,
}

fn input(
    window: &mut Window,
    cx: &mut Context<JobView>,
    value: impl Into<SharedString>,
    placeholder: impl Into<SharedString>,
) -> Entity<InputState> {
    let value = value.into();
    let placeholder = placeholder.into();
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

fn seconds(ms: u64) -> String {
    (ms / 1000).max(1).to_string()
}

impl JobView {
    pub fn new(setup: Setup, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Setup {
            engine,
            definition,
            revision,
            sources,
            catalogs,
            jobs,
            availability,
            connections,
        } = setup;
        let saved = revision.map(|_| definition.clone());
        let name = input(window, cx, definition.name.clone(), tr!("Job name"));
        let description = input(
            window,
            cx,
            definition.description.clone(),
            tr!("What the job does (optional)"),
        );
        let timeout_ms = definition.timeout().as_millis();
        let timeout = input(
            window,
            cx,
            seconds(u64::try_from(timeout_ms).unwrap_or(u64::MAX)),
            tr!("Seconds"),
        );
        let mut subscriptions = Vec::new();
        for field in [&name, &description, &timeout] {
            subscriptions.push(cx.subscribe(field, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.changed(cx);
                }
            }));
        }
        let history = cx.new(|cx| {
            HistoryList::new(
                engine.clone(),
                Some(definition.id),
                jobs.clone(),
                connections,
                cx,
            )
        });
        subscriptions.push(cx.subscribe(&history, |_, _, event: &HistoryEvent, cx| {
            cx.emit(JobEvent::History(event.clone()));
        }));
        let mut view = Self {
            kind: definition.kind(),
            id: definition.id,
            engine,
            revision,
            saved,
            definition,
            unsaved: revision.is_none(),
            inner: Inner::Editor,
            name,
            description,
            timeout,
            sql: None,
            program: None,
            chain: None,
            specs: Vec::new(),
            outputs: Vec::new(),
            versions: Vec::new(),
            values: Vec::new(),
            value_errors: HashMap::new(),
            active: Active::Idle,
            pending: None,
            notice: None,
            run_lines: Vec::new(),
            run_error: None,
            ran_sql: None,
            changed_file: None,
            prefill_note: None,
            grid: cx.new(|cx| ResultGrid::new(Arc::default(), false, cx)),
            has_result: false,
            shown: Shown::Nothing,
            history,
            sources,
            catalogs,
            jobs,
            availability,
            busy: false,
            editor_scroll: ScrollHandle::new(),
            inputs_scroll: ScrollHandle::new(),
            schedule_scroll: ScrollHandle::new(),
            schedule: None,
            focus: cx.focus_handle(),
            field_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
            watch: None,
            step_watch: None,
            tick: None,
        };
        view.make_kind_fields(window, cx);
        view.rebuild(window, cx);
        view.load_versions(cx);
        view.refresh_plan(cx);
        view
    }

    // -----------------------------------------------------------------------
    // What the host reads and hands over.

    pub fn job_id(&self) -> JobId {
        self.id
    }

    /// Shows an inner tab, for the performance run (`--test-performance`).
    pub(super) fn benchmark_show(
        &mut self,
        inner: Inner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_inner(inner, window, cx);
    }

    /// The job's **Runs** list.
    pub(super) fn benchmark_history(&self) -> Entity<HistoryList> {
        self.history.clone()
    }

    /// Whether this tab's run is queued or active, and whether its result
    /// is in the grid.
    pub(super) fn benchmark_running(&self) -> (bool, bool) {
        (self.running(), self.has_result)
    }

    /// The job's name, for its tab group.
    pub fn name(&self) -> String {
        let name = self.definition.name.trim();
        if name.is_empty() {
            tr!("Untitled job").to_owned()
        } else {
            name.to_owned()
        }
    }

    /// The tab's title: the name, with `●` while the job is not saved.
    pub fn title(&self) -> String {
        let name = self.name();
        if self.unsaved {
            format!("{name} ●")
        } else {
            name
        }
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.unsaved
    }

    /// The tab's mark: a spinner while its run is active, then how it ended.
    pub fn tab_status(&self) -> TabStatus {
        let ended = |state: jykre::automation::RunState| match state {
            jykre::automation::RunState::Succeeded => TabStatus::Succeeded,
            jykre::automation::RunState::Failed => TabStatus::Failed,
            _ => TabStatus::Cancelled,
        };
        match &self.active {
            Active::Idle => TabStatus::Idle,
            Active::Run { status, .. } => match status {
                RunStatus::Queued { .. } | RunStatus::Active(_) => TabStatus::Running,
                RunStatus::Finished(outcome) => ended(outcome.state),
                RunStatus::Refused(_) => TabStatus::Failed,
            },
            Active::Chain { status, .. } => match status {
                ChainStatus::Running { .. } => TabStatus::Running,
                ChainStatus::Finished(outcome) => ended(outcome.state),
                ChainStatus::Refused(_) => TabStatus::Failed,
            },
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.inner {
            Inner::Editor => self.name.update(cx, |input, cx| input.focus(window, cx)),
            Inner::Inputs => match self.values.first() {
                Some(field) => field.input.update(cx, |input, cx| input.focus(window, cx)),
                None => window.focus(&self.focus, cx),
            },
            Inner::Runs => self
                .history
                .update(cx, |history, cx| history.focus(window, cx)),
            Inner::Schedule => match &self.schedule {
                Some(fields) => fields.times.update(cx, |input, cx| input.focus(window, cx)),
                None => window.focus(&self.focus, cx),
            },
        }
    }

    pub fn set_sources(&mut self, sources: JobSources, cx: &mut Context<Self>) {
        self.sources = sources;
        self.apply_language(cx);
        cx.notify();
    }

    pub fn set_jobs(&mut self, jobs: Jobs, cx: &mut Context<Self>) {
        self.jobs = jobs.clone();
        self.history
            .update(cx, |history, cx| history.set_jobs(jobs, cx));
        cx.notify();
    }

    pub fn set_availability(&mut self, availability: Availability, cx: &mut Context<Self>) {
        self.availability = availability;
        cx.notify();
    }

    /// The engine changed: versions and the history are read again.
    pub fn engine_changed(&mut self, cx: &mut Context<Self>) {
        self.refresh_schedule(cx);
        self.load_versions(cx);
        self.refresh_plan(cx);
        self.history.update(cx, |history, cx| history.refresh(cx));
    }

    fn writable(&self) -> bool {
        self.availability == Availability::Ready
    }

    fn set_inner(&mut self, inner: Inner, window: &mut Window, cx: &mut Context<Self>) {
        self.inner = inner;
        if inner == Inner::Runs {
            self.history.update(cx, |history, cx| history.refresh(cx));
        }
        if inner == Inner::Schedule && self.schedule.is_none() {
            self.load_schedule(window, cx);
        }
        self.focus(window, cx);
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // The definition.

    /// The SQL editor's dialect: the chosen connection's.
    fn dialect(&self) -> Option<SqlDialect> {
        let JobBody::Sql(body) = &self.definition.body else {
            return None;
        };
        body.connection
            .and_then(|id| source_of(&self.sources.sources, id))
            .and_then(|source| source.dialect)
    }

    fn make_kind_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.definition.body.clone() {
            JobBody::Sql(body) => {
                let editor = cx.new(|cx| {
                    EditorState::new(window, cx)
                        .language("sql")
                        .line_number(true)
                        .placeholder(tr!(
                            "One statement, e.g. SELECT address FROM t WHERE key = :key. Each :name becomes an input of the run form."
                        ))
                        .default_value(body.sql.clone())
                });
                let sql_editor = cx.new(|cx| SqlEditor::new(&editor, cx));
                self._subscriptions.push(cx.subscribe(
                    &sql_editor,
                    |this, _, event: &SqlEditorEvent, cx| {
                        if let SqlEditorEvent::Notice(notice) = event {
                            this.notice = Some(notice.clone());
                            cx.notify();
                        }
                    },
                ));
                self._subscriptions.push(cx.observe_in(
                    &editor,
                    window,
                    |this, editor, window, cx| {
                        let sql = editor.read(cx).text().to_string();
                        let current = match &this.definition.body {
                            JobBody::Sql(body) => body.sql.clone(),
                            _ => return,
                        };
                        if sql != current {
                            if let JobBody::Sql(body) = &mut this.definition.body {
                                body.sql = sql;
                            }
                            this.sql_changed(window, cx);
                            this.changed(cx);
                        }
                    },
                ));
                let max_rows = input(window, cx, body.max_rows.to_string(), tr!("Rows"));
                self._subscriptions.push(cx.subscribe(
                    &max_rows,
                    |this, _, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change) {
                            this.changed(cx);
                        }
                    },
                ));
                let go_to_line = cx.new(|_| GoToLine::new(&sql_editor));
                self.sql = Some(SqlFields {
                    editor,
                    sql_editor,
                    go_to_line,
                    max_rows,
                    extra: Vec::new(),
                    revision: 0,
                });
                self.apply_language(cx);
                self.sql_changed(window, cx);
            }
            JobBody::Process(body) => {
                let text = super::program_bits::command_text(&body.program, &body.arguments);
                let command = input(
                    window,
                    cx,
                    text.clone(),
                    tr!("For example: python3 /home/me/jobs/export.py --format csv"),
                );
                self._subscriptions.push(cx.subscribe(
                    &command,
                    |this, _, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change) {
                            this.command_changed(cx);
                        }
                    },
                ));
                let entrypoint = input(
                    window,
                    cx,
                    body.entrypoint.clone().unwrap_or_default(),
                    tr!("The file a version pins; the program itself when empty"),
                );
                let working_directory = input(
                    window,
                    cx,
                    body.working_directory.clone().unwrap_or_default(),
                    tr!("The entrypoint's folder"),
                );
                for field in [&entrypoint, &working_directory] {
                    self._subscriptions.push(cx.subscribe(
                        field,
                        |this, _, event: &InputEvent, cx| {
                            if matches!(event, InputEvent::Change) {
                                this.changed(cx);
                            }
                        },
                    ));
                }
                self.program = Some(ProgramFields {
                    read: super::program_bits::read_command(&text),
                    command,
                    stored: (text, body.program.clone(), body.arguments.clone()),
                    entrypoint,
                    working_directory,
                    unset: Vec::new(),
                    removed: super::program_bits::removed_variables(&body.environment),
                    set: Vec::new(),
                });
            }
            JobBody::Chain(_) => self.chain = Some(ChainFields::default()),
        }
    }

    /// Hands the editor the dialect, names and catalogs of the job's
    /// connection.
    fn apply_language(&mut self, cx: &mut Context<Self>) {
        let JobBody::Sql(body) = &self.definition.body else {
            return;
        };
        let source = body
            .connection
            .and_then(|id| source_of(&self.sources.sources, id))
            .cloned();
        let context = body.context.clone();
        let catalogs = self.catalogs.clone();
        let Some(fields) = &self.sql else {
            return;
        };
        fields.sql_editor.update(cx, |sql_editor, cx| {
            let config = source
                .as_ref()
                .and_then(|source| source.input.as_ref())
                .map(|input| input.config.clone());
            sql_editor.set_connection(config.as_ref());
            sql_editor.set_dialect(source.as_ref().and_then(|source| source.dialect), cx);
            sql_editor.set_catalogs(catalogs, cx);
            match source.as_ref().and_then(|source| source.names.clone()) {
                Some((id, names)) => sql_editor.set_names(id, names),
                None => sql_editor.forget_server(),
            }
            sql_editor.set_context(context);
        });
    }

    /// The SQL changed: its statements and `:name` markers are read off the
    /// UI thread; the parameters follow the markers.
    fn sql_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let JobBody::Sql(body) = &self.definition.body else {
            return;
        };
        let Some(fields) = self.sql.as_mut() else {
            return;
        };
        fields.revision += 1;
        let revision = fields.revision;
        let sql = body.sql.clone();
        let dialect = self.dialect();
        cx.spawn_in(window, async move |weak, cx| {
            let (extra, names) = cx
                .background_executor()
                .spawn(async move {
                    (
                        logic::extra_statements(&sql, dialect),
                        params::names_all([(sql.as_str(), dialect)]),
                    )
                })
                .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                let Some(fields) = this.sql.as_mut() else {
                    return;
                };
                if fields.revision != revision {
                    return;
                }
                fields.extra = extra;
                let current: Vec<String> = this
                    .definition
                    .inputs()
                    .iter()
                    .map(|spec| spec.name.clone())
                    .collect();
                if current != names {
                    this.store_fields(cx);
                    if let JobBody::Sql(body) = &mut this.definition.body {
                        body.parameters = params::reconcile(&body.parameters, &names);
                    }
                    this.rebuild(window, cx);
                    this.changed(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The definition as the fields hold it now.
    fn current(&self, cx: &App) -> JobDefinition {
        let mut definition = self.definition.clone();
        definition.name = self.name.read(cx).value().trim().to_owned();
        definition.description = self.description.read(cx).value().to_string();
        let timeout_ms =
            whole(&self.timeout.read(cx).value()).map_or(0, |seconds| seconds.saturating_mul(1000));
        let specs: Vec<(String, String, String)> = self
            .specs
            .iter()
            .map(|row| {
                (
                    row.name.read(cx).value().trim().to_owned(),
                    row.label.read(cx).value().trim().to_owned(),
                    row.default.read(cx).value().to_string(),
                )
            })
            .collect();
        let apply_specs = |specs_out: &mut Vec<ParameterSpec>, typed_names: bool| {
            for (spec, (name, label, default)) in specs_out.iter_mut().zip(&specs) {
                if typed_names {
                    spec.name = name.clone();
                }
                spec.label = label.clone();
                spec.default = (!default.trim().is_empty()).then(|| default.clone());
            }
        };
        let output_names: Vec<(String, Option<u64>)> = self
            .outputs
            .iter()
            .map(|row| {
                (
                    row.name.read(cx).value().trim().to_owned(),
                    whole(&row.column.read(cx).value()),
                )
            })
            .collect();
        match &mut definition.body {
            JobBody::Sql(body) => {
                body.timeout_ms = timeout_ms;
                if let Some(fields) = &self.sql {
                    body.sql = fields.editor.read(cx).text().to_string();
                    body.max_rows = whole(&fields.max_rows.read(cx).value())
                        .and_then(|rows| u32::try_from(rows).ok())
                        .unwrap_or(0);
                }
                apply_specs(&mut body.parameters, false);
                for (output, (name, column)) in body.outputs.iter_mut().zip(&output_names) {
                    output.name = name.clone();
                    output.column = column
                        .and_then(|column| u32::try_from(column).ok())
                        .unwrap_or(0);
                }
            }
            JobBody::Process(body) => {
                body.timeout_ms = timeout_ms;
                if let Some(fields) = &self.program {
                    let (text, program, arguments) = &fields.stored;
                    if fields.command.read(cx).value().as_ref() == text.as_str() {
                        body.program.clone_from(program);
                        body.arguments.clone_from(arguments);
                    } else if let Ok(command) = &fields.read {
                        body.program = command.program().to_owned();
                        body.arguments = command.arguments().to_vec();
                    }
                    let optional = |field: &Entity<InputState>| {
                        let text = field.read(cx).value().trim().to_owned();
                        (!text.is_empty()).then_some(text)
                    };
                    body.entrypoint = optional(&fields.entrypoint);
                    body.working_directory = optional(&fields.working_directory);
                    body.environment.unset = fields
                        .unset
                        .iter()
                        .map(|field| field.read(cx).value().trim().to_owned())
                        .collect();
                    body.environment.set = fields
                        .set
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.read(cx).value().trim().to_owned(),
                                value.read(cx).value().to_string(),
                            )
                        })
                        .collect();
                }
                apply_specs(&mut body.inputs, true);
                for (output, (name, _)) in body.outputs.iter_mut().zip(&output_names) {
                    output.name = name.clone();
                }
            }
            JobBody::Chain(body) => {
                body.timeout_ms = timeout_ms;
                apply_specs(&mut body.inputs, true);
                if let Some(fields) = &self.chain {
                    for (step, step_fields) in body.steps.iter_mut().zip(&fields.steps) {
                        step.id = step_fields.id.read(cx).value().trim().to_owned();
                        for (input, source) in &mut step.inputs {
                            if let Source::Value(value) = source
                                && let Some(field) = step_fields.constants.get(input)
                            {
                                *value = field.read(cx).value().to_string();
                            }
                        }
                    }
                }
            }
        }
        definition
    }

    /// Reads the fields into the structure, before the structure changes.
    fn store_fields(&mut self, cx: &App) {
        self.definition = self.current(cx);
    }

    /// Something was typed or chosen: whether the job differs from the
    /// stored one is worked out now, not while drawing.
    fn changed(&mut self, cx: &mut Context<Self>) {
        let current = self.current(cx);
        self.unsaved = self.command_problem().is_some()
            || match &self.saved {
                Some(saved) => !saved.same_definition(&current),
                None => true,
            };
        if let (Some(program), JobBody::Process(body)) = (self.program.as_mut(), &current.body) {
            program.removed = super::program_bits::removed_variables(&body.environment);
        }
        self.definition.name = current.name;
        self.plan_soon(cx);
        cx.notify();
    }

    /// The command line was typed: it is read into the program and its
    /// arguments now, not while drawing.
    fn command_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(fields) = self.program.as_mut() {
            let text = fields.command.read(cx).value().to_string();
            fields.read = super::program_bits::read_command(&text);
        }
        self.changed(cx);
    }

    /// Why the command line cannot be read, when it cannot: nothing is
    /// saved or run from a command cut in the middle of a quote.
    fn command_problem(&self) -> Option<String> {
        let error = self.program.as_ref()?.read.as_ref().err()?;
        Some(super::program_bits::command_problem(*error))
    }

    /// A chain's step IDs and constants were typed: its plan is read again
    /// once typing pauses for 300 ms.
    fn plan_soon(&mut self, cx: &mut Context<Self>) {
        let Some(chain) = self.chain.as_mut() else {
            return;
        };
        chain.typing += 1;
        let typing = chain.typing;
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this
                    .chain
                    .as_ref()
                    .is_some_and(|chain| chain.typing == typing)
                {
                    this.refresh_plan(cx);
                }
            });
        })
        .detach();
    }

    /// The parameters' (or inputs') specification rows, a program's lists,
    /// a chain's steps, the outputs and the run form, from the structure.
    fn rebuild(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.typed_values(cx);
        self.field_subscriptions.clear();
        let specs: Vec<ParameterSpec> = match &self.definition.body {
            JobBody::Sql(body) => body.parameters.clone(),
            JobBody::Process(body) => body.inputs.clone(),
            JobBody::Chain(body) => body.inputs.clone(),
        };
        let open: Vec<String> = self
            .specs
            .iter()
            .filter(|row| row.choices_open)
            .map(|row| row.name.read(cx).value().to_string())
            .collect();
        self.specs = specs
            .iter()
            .map(|spec| SpecRow {
                name: input(window, cx, spec.name.clone(), tr!("name")),
                label: input(window, cx, spec.label.clone(), spec.name.clone()),
                default: input(
                    window,
                    cx,
                    spec.default.clone().unwrap_or_default(),
                    crate::app::report_view::placeholder(spec.kind),
                ),
                choice: input(window, cx, "", tr!("Add a choice")),
                choices_open: open.contains(&spec.name),
            })
            .collect();
        let outputs: Vec<(String, u32)> = match &self.definition.body {
            JobBody::Sql(body) => body
                .outputs
                .iter()
                .map(|output| (output.name.clone(), output.column))
                .collect(),
            JobBody::Process(body) => body
                .outputs
                .iter()
                .map(|output| (output.name.clone(), 0))
                .collect(),
            JobBody::Chain(_) => Vec::new(),
        };
        self.outputs = outputs
            .into_iter()
            .map(|(name, column)| OutputRow {
                name: input(window, cx, name, tr!("output_name")),
                column: input(
                    window,
                    cx,
                    if column == 0 {
                        String::new()
                    } else {
                        column.to_string()
                    },
                    tr!("Column"),
                ),
            })
            .collect();
        let mut fields: Vec<Entity<InputState>> = Vec::new();
        for row in &self.specs {
            fields.extend([row.name.clone(), row.label.clone(), row.default.clone()]);
        }
        for row in &self.outputs {
            fields.extend([row.name.clone(), row.column.clone()]);
        }
        if let (Some(program), JobBody::Process(body)) = (&mut self.program, &self.definition.body)
        {
            program.unset = body
                .environment
                .unset
                .iter()
                .map(|name| input(window, cx, name.clone(), tr!("VARIABLE")))
                .collect();
            program.set = body
                .environment
                .set
                .iter()
                .map(|(name, value)| {
                    (
                        input(window, cx, name.clone(), tr!("VARIABLE")),
                        input(window, cx, value.clone(), tr!("Value (never a secret)")),
                    )
                })
                .collect();
            fields.extend(program.unset.iter().cloned());
            for (name, value) in &program.set {
                fields.extend([name.clone(), value.clone()]);
            }
        }
        if let (Some(chain), JobBody::Chain(body)) = (&mut self.chain, &self.definition.body) {
            chain.steps = body
                .steps
                .iter()
                .map(|step| StepFields {
                    id: input(window, cx, step.id.clone(), tr!("step_id")),
                    constants: step
                        .inputs
                        .iter()
                        .filter_map(|(name, source)| match source {
                            Source::Value(value) => Some((
                                name.clone(),
                                input(window, cx, value.clone(), tr!("The constant")),
                            )),
                            _ => None,
                        })
                        .collect(),
                })
                .collect();
            for step in &chain.steps {
                fields.push(step.id.clone());
                fields.extend(step.constants.values().cloned());
            }
        }
        for field in fields {
            self.field_subscriptions.push(cx.subscribe(
                &field,
                |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.changed(cx);
                    }
                },
            ));
        }
        // Enter in a list's new choice adds it.
        for row in &self.specs {
            let choice = row.choice.clone();
            let name_field = row.name.clone();
            self.field_subscriptions.push(cx.subscribe_in(
                &choice,
                window,
                move |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        let name = name_field.read(cx).value().to_string();
                        this.add_choice(&name, window, cx);
                    }
                },
            ));
        }
        self.rebuild_values(typed, window, cx);
        cx.notify();
    }

    /// The run form: one field per input, keeping what was typed.
    fn rebuild_values(
        &mut self,
        typed: HashMap<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let specs = self.current(cx).inputs().to_vec();
        self.values = specs
            .iter()
            .map(|spec| {
                let value = typed
                    .get(&spec.name)
                    .cloned()
                    .or_else(|| spec.default.clone())
                    .unwrap_or_default();
                ValueField {
                    name: spec.name.clone(),
                    input: input(
                        window,
                        cx,
                        value,
                        crate::app::report_view::placeholder(spec.kind),
                    ),
                }
            })
            .collect();
        for field in &self.values {
            self.field_subscriptions.push(cx.subscribe(
                &field.input,
                |this, _, event: &InputEvent, cx| match event {
                    InputEvent::PressEnter { .. } => this.run(cx),
                    InputEvent::Change if !this.value_errors.is_empty() => {
                        this.value_errors.clear();
                        cx.notify();
                    }
                    _ => {}
                },
            ));
        }
    }

    fn typed_values(&self, cx: &App) -> HashMap<String, String> {
        self.values
            .iter()
            .map(|field| (field.name.clone(), field.input.read(cx).value().to_string()))
            .collect()
    }

    /// The specifications of the inputs, mutable.
    fn specs_mut(&mut self) -> &mut Vec<ParameterSpec> {
        match &mut self.definition.body {
            JobBody::Sql(body) => &mut body.parameters,
            JobBody::Process(body) => &mut body.inputs,
            JobBody::Chain(body) => &mut body.inputs,
        }
    }

    fn add_choice(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self
            .specs
            .iter()
            .find(|row| row.name.read(cx).value() == name)
        else {
            return;
        };
        let choice = row.choice.read(cx).value().to_string();
        let field = row.choice.clone();
        if choice.trim().is_empty() {
            return;
        }
        self.store_fields(cx);
        if let Some(spec) = self.specs_mut().iter_mut().find(|spec| spec.name == name)
            && spec.choices.len() < jykre::reports::MAX_CHOICES
            && !spec.choices.contains(&choice)
        {
            spec.choices.push(choice);
        }
        field.update(cx, |input, cx| input.set_value("", window, cx));
        self.changed(cx);
    }

    // -----------------------------------------------------------------------
    // Saving, publishing, deleting.

    fn save(&mut self, cx: &mut Context<Self>) {
        if !self.writable() {
            self.notice = Some(Feedback::new(
                Tone::Warning,
                tr!("Automation is read-only or off here, so nothing can be saved."),
            ));
            cx.notify();
            return;
        }
        if let Some(problem) = self.command_problem() {
            self.notice = Some(Feedback::new(Tone::Warning, problem));
            cx.notify();
            return;
        }
        let definition = self.current(cx);
        let problems = definition::problems(&definition);
        if !problems.is_empty() {
            self.notice = Some(Feedback::new(
                Tone::Warning,
                problems
                    .iter()
                    .map(words::problem)
                    .collect::<Vec<_>>()
                    .join(" "),
            ));
            cx.notify();
            return;
        }
        let engine = self.engine.clone();
        let revision = self.revision;
        self.busy = true;
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let stored = definition.clone();
            let saved =
                backend::run(async move { Ok(engine.save_job(&stored, revision).await) }).await;
            let _ = weak.update(cx, |this, cx| {
                this.busy = false;
                match saved {
                    Ok(Ok(job)) => {
                        this.revision = Some(job.revision);
                        this.saved = Some(definition);
                        this.notice = Some(Feedback::new(Tone::Success, tr!("Job saved.")));
                        this.changed(cx);
                        this.history.update(cx, |history, cx| history.refresh(cx));
                    }
                    Ok(Err(error)) => {
                        this.notice = Some(Feedback::new(Tone::Error, words::save_error(&error)));
                    }
                    Err(error) => {
                        this.notice = Some(Feedback::new(Tone::Error, error.to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// **Publish**: the stored definition becomes the next version, which
    /// chains name. Unsaved changes are saved first.
    fn publish(&mut self, cx: &mut Context<Self>) {
        if self.unsaved || self.revision.is_none() {
            self.notice = Some(Feedback::new(
                Tone::Warning,
                tr!("Save the job before publishing it: a version freezes what is stored."),
            ));
            cx.notify();
            return;
        }
        let engine = self.engine.clone();
        let id = self.id;
        self.busy = true;
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let published = backend::run(async move { Ok(engine.publish(id).await) }).await;
            let _ = weak.update(cx, |this, cx| {
                this.busy = false;
                match published {
                    Ok(Ok(version)) => {
                        this.notice = Some(Feedback::new(
                            Tone::Success,
                            tr!("Published as version {0}.", version.version),
                        ));
                        this.changed_file = None;
                        this.load_versions(cx);
                    }
                    Ok(Err(error)) => {
                        this.notice =
                            Some(Feedback::new(Tone::Error, words::publish_error(&error)));
                    }
                    Err(error) => {
                        this.notice = Some(Feedback::new(Tone::Error, error.to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The job's published versions, newest first.
    fn load_versions(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.engine.repository() else {
            return;
        };
        let own = (self.kind != JobKind::Chain && self.revision.is_some()).then_some(self.id);
        let chain_jobs: Vec<JobId> = if self.kind == JobKind::Chain {
            self.jobs
                .iter()
                .filter(|job| job.kind != JobKind::Chain)
                .map(|job| job.id)
                .collect()
        } else {
            Vec::new()
        };
        cx.spawn(async move |weak, cx| {
            let read = backend::run(async move {
                let mut own_versions = Vec::new();
                if let Some(id) = own {
                    own_versions = repository.versions(id).await?;
                }
                let mut others = HashMap::new();
                for job in chain_jobs {
                    others.insert(job, repository.versions(job).await?);
                }
                Ok((own_versions, others))
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                if let Ok((mut own, others)) = read {
                    own.sort_by_key(|version| std::cmp::Reverse(version.version));
                    this.versions = own;
                    if let Some(chain) = this.chain.as_mut() {
                        chain.versions = others;
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Deletes a published version after the user confirms; refused while
    /// a chain names it.
    fn delete_version(&mut self, version: i64, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let id = self.id;
        cx.spawn(async move |weak, cx| {
            let deleted =
                backend::run(async move { Ok(engine.delete_version(id, version).await) }).await;
            let _ = weak.update(cx, |this, cx| {
                this.notice = Some(match deleted {
                    Ok(Ok(())) => {
                        Feedback::new(Tone::Success, tr!("Version {0} was deleted.", version))
                    }
                    Ok(Err(error)) => Feedback::new(Tone::Error, words::save_error(&error)),
                    Err(error) => Feedback::new(Tone::Error, error.to_string()),
                });
                this.load_versions(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if self.revision.is_none() {
            cx.emit(JobEvent::Deleted);
            return;
        }
        let engine = self.engine.clone();
        let id = self.id;
        cx.spawn(async move |weak, cx| {
            let deleted = backend::run(async move { Ok(engine.delete_job(id).await) }).await;
            let _ = weak.update(cx, |this, cx| match deleted {
                Ok(Ok(())) => cx.emit(JobEvent::Deleted),
                Ok(Err(error)) => {
                    this.notice = Some(Feedback::new(Tone::Error, words::save_error(&error)));
                    cx.notify();
                }
                Err(error) => {
                    this.notice = Some(Feedback::new(Tone::Error, error.to_string()));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn duplicate(&mut self, cx: &mut Context<Self>) {
        let definition = self.current(cx);
        let copy = definition.duplicate(tr!("{0} (copy)", definition.name));
        cx.emit(JobEvent::Duplicate(Box::new(copy)));
    }

    // -----------------------------------------------------------------------
    // Running.

    fn running(&self) -> bool {
        match &self.active {
            Active::Idle => false,
            Active::Run { status, .. } => {
                matches!(status, RunStatus::Queued { .. } | RunStatus::Active(_))
            }
            Active::Chain { status, .. } => matches!(status, ChainStatus::Running { .. }),
        }
    }

    /// **Run**: the form's values, then the passwords, the confirmation and
    /// the engine. A problem stops it in place, with its reason; nothing
    /// is recorded for a run that does not start.
    pub fn run(&mut self, cx: &mut Context<Self>) {
        if self.running() {
            return;
        }
        self.pending = Some(Pending {
            values: self.typed_values(cx),
            confirmed: false,
            plan: None,
            programs_known: None,
        });
        self.run_lines.clear();
        self.run_error = None;
        self.ran_sql = None;
        self.changed_file = None;
        self.value_errors.clear();
        self.continue_run(cx);
    }

    pub fn resume_pending(&mut self, cx: &mut Context<Self>) {
        self.continue_run(cx);
    }

    pub fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        cx.notify();
    }

    pub fn confirmed(&mut self, what: Ask, window: &mut Window, cx: &mut Context<Self>) {
        match what {
            Ask::Run => {
                if let Some(pending) = self.pending.as_mut() {
                    pending.confirmed = true;
                }
                self.continue_run(cx);
            }
            Ask::DeleteJob => self.delete(cx),
            Ask::DeleteVersion(version) => self.delete_version(version, cx),
            Ask::StoreOutputs => self.set_store_outputs(true, window, cx),
            Ask::ScheduleConsent => self.consented(cx),
            Ask::DeleteSchedule => self.delete_schedule(cx),
        }
    }

    fn refuse(&mut self, text: String, cx: &mut Context<Self>) {
        self.pending = None;
        self.run_lines = vec![Line {
            level: Level::Error,
            text,
        }];
        self.inner = Inner::Inputs;
        cx.notify();
    }

    fn connection_input(
        &self,
        id: ConnectionId,
    ) -> Result<(ConnectionInput, String, bool, Option<SqlDialect>), String> {
        let source = source_of(&self.sources.sources, id).ok_or_else(|| {
            tr!("The job's connection no longer exists. Choose another in the editor.").to_owned()
        })?;
        let name = source.connection.name.clone();
        let input = source
            .input
            .clone()
            .ok_or_else(|| tr!("The connection {0} cannot run now.", name))?;
        let driver = builtin_registry()
            .and_then(|registry| registry.get(input.config.driver))
            .map_err(|error| error.message)?;
        let dialect = source.dialect.or_else(|| Some(driver.descriptor().dialect));
        let production =
            source.environment == Some(jykre::connection_appearance::Environment::Production);
        let revision = self.sources.revisions.get(&id).copied().unwrap_or(0);
        Ok((
            ConnectionInput {
                id,
                driver,
                resolved: input,
                revision,
            },
            name,
            production,
            dialect,
        ))
    }

    /// Takes the run as far as it can go now: each step that needs the user
    /// (a password, the confirmation) or a read (the plan, whether a program
    /// ran before) leaves it waiting, and comes back here.
    fn continue_run(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if !self.writable() {
            self.refuse(words::refusal(&Refusal::Unavailable), cx);
            return;
        }
        if let Some(problem) = self.command_problem() {
            self.refuse(tr!("Not run: {0}", problem), cx);
            return;
        }
        let definition = self.current(cx);
        let problems = definition::problems(&definition);
        if !problems.is_empty() {
            let text = problems
                .iter()
                .map(words::problem)
                .collect::<Vec<_>>()
                .join(" ");
            self.refuse(tr!("Not run: {0}", text), cx);
            return;
        }
        let values = pending.values.clone();
        let stored = (!self.unsaved)
            .then_some(())
            .and(self.revision)
            .map(|revision| (self.id, revision));
        match &definition.body {
            JobBody::Sql(body) => {
                let Some(connection) = body.connection else {
                    self.refuse(words::refusal(&Refusal::NoConnection), cx);
                    return;
                };
                let (input, name, production, dialect) = match self.connection_input(connection) {
                    Ok(found) => found,
                    Err(text) => {
                        self.refuse(text, cx);
                        return;
                    }
                };
                let dialect_now = dialect.unwrap_or(SqlDialect::Postgres);
                let job = SqlJob::from_body(body, connection);
                if let Err(error) = jykre::automation::prepare(&job, dialect_now, &values) {
                    if let jykre::automation::PrepareError::Parameters(errors) = &error {
                        self.value_errors = errors
                            .iter()
                            .map(|error| (error.name.clone(), error.problem))
                            .collect();
                    }
                    self.refuse(words::refusal(&Refusal::Invalid(error)), cx);
                    return;
                }
                if connection_store::needs_password(&input.resolved) {
                    cx.emit(JobEvent::PasswordRequired(connection));
                    return;
                }
                if !pending.confirmed {
                    let risk = jykre::guard::risk(&body.sql, dialect)
                        .map(|risk| crate::app::workspace::risk_description(&[(0, risk)], 1));
                    if let Some((title, description)) = logic::production_confirmation(
                        &name,
                        production,
                        risk,
                        definition.persists_outputs(),
                    ) {
                        cx.emit(JobEvent::Confirm {
                            title,
                            description,
                            ok: tr!("Run"),
                            what: Ask::Run,
                        });
                        return;
                    }
                }
                self.submit(
                    JobRequest {
                        definition,
                        stored,
                        values,
                        connection: Some(input),
                    },
                    dialect,
                    cx,
                );
            }
            JobBody::Process(_) => {
                if let Err(problems) = form_values(definition.inputs(), &values) {
                    self.value_errors = problems
                        .iter()
                        .map(|problem| (problem.input.clone(), problem.problem))
                        .collect();
                    self.refuse(words::refusal(&Refusal::Inputs(problems)), cx);
                    return;
                }
                if !pending.confirmed {
                    match &pending.programs_known {
                        None => {
                            let engine = self.engine.clone();
                            let id = self.id;
                            let revision = stored.map(|(_, revision)| revision);
                            cx.spawn(async move |weak, cx| {
                                let known = match revision {
                                    Some(revision) => backend::run(async move {
                                        Ok(engine.program_has_run(id, Some(revision), None).await)
                                    })
                                    .await
                                    .unwrap_or(false),
                                    None => false,
                                };
                                let _ = weak.update(cx, |this, cx| {
                                    if let Some(pending) = this.pending.as_mut() {
                                        pending.programs_known = Some(vec![known]);
                                        this.continue_run(cx);
                                    }
                                });
                            })
                            .detach();
                            return;
                        }
                        Some(known) if known.iter().any(|known| !known) => {
                            cx.emit(JobEvent::Confirm {
                                title: tr!("Run {0} for the first time?", definition.name),
                                description: words::trusted_warning(&definition.name),
                                ok: tr!("Run the program"),
                                what: Ask::Run,
                            });
                            return;
                        }
                        Some(_) => {}
                    }
                }
                self.submit(
                    JobRequest {
                        definition,
                        stored,
                        values,
                        connection: None,
                    },
                    None,
                    cx,
                );
            }
            JobBody::Chain(chain) => {
                self.continue_chain(&definition, chain.clone(), stored, values, cx)
            }
        }
    }

    fn continue_chain(
        &mut self,
        definition: &JobDefinition,
        chain: definition::ChainBody,
        stored: Option<(JobId, i64)>,
        values: HashMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let Some(plan) = pending.plan.clone() else {
            let engine = self.engine.clone();
            let definition = definition.clone();
            cx.spawn(async move |weak, cx| {
                let plan = backend::run(async move { engine.plan(definition, stored).await }).await;
                let _ = weak.update(cx, |this, cx| match plan {
                    Ok(plan) => {
                        if let Some(pending) = this.pending.as_mut() {
                            pending.plan = Some(plan);
                            this.continue_run(cx);
                        }
                    }
                    Err(error) => this.refuse(error.message, cx),
                });
            })
            .detach();
            return;
        };
        if !plan.problems.is_empty() {
            self.refuse(words::refusal(&Refusal::Chain(plan.problems.clone())), cx);
            return;
        }
        if let Err(problems) = form_values(&chain.inputs, &values) {
            self.value_errors = problems
                .iter()
                .map(|problem| (problem.input.clone(), problem.problem))
                .collect();
            self.refuse(words::refusal(&Refusal::Inputs(problems)), cx);
            return;
        }
        let mut connections = HashMap::new();
        let mut described: HashMap<ConnectionId, (String, bool, Option<SqlDialect>)> =
            HashMap::new();
        for connection in plan.connections() {
            match self.connection_input(connection) {
                Ok((input, name, production, dialect)) => {
                    if connection_store::needs_password(&input.resolved) {
                        cx.emit(JobEvent::PasswordRequired(connection));
                        return;
                    }
                    described.insert(connection, (name, production, dialect));
                    connections.insert(connection, input);
                }
                Err(text) => {
                    self.refuse(text, cx);
                    return;
                }
            }
        }
        if !pending.confirmed {
            let programs: Vec<(JobId, i64)> = plan
                .steps
                .iter()
                .filter(|step| step.definition.kind() == JobKind::Process)
                .map(|step| (step.job, step.version))
                .collect();
            let Some(known) = pending.programs_known.clone() else {
                let engine = self.engine.clone();
                cx.spawn(async move |weak, cx| {
                    let known = backend::run(async move {
                        let mut known = Vec::new();
                        for (job, version) in programs {
                            known.push(engine.program_has_run(job, None, Some(version)).await);
                        }
                        Ok(known)
                    })
                    .await
                    .unwrap_or_default();
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(pending) = this.pending.as_mut() {
                            pending.programs_known = Some(known);
                            this.continue_run(cx);
                        }
                    });
                })
                .detach();
                return;
            };
            let mut known = known.into_iter();
            let lines: Vec<logic::PlanLine> = plan
                .steps
                .iter()
                .map(|step| {
                    let (connection, production, risk) = match &step.definition.body {
                        JobBody::Sql(sql) => match sql.connection.and_then(|id| described.get(&id))
                        {
                            Some((name, production, dialect)) => (
                                Some(name.clone()),
                                *production,
                                production
                                    .then(|| jykre::guard::risk(&sql.sql, *dialect))
                                    .flatten()
                                    .map(|risk| {
                                        crate::app::workspace::risk_description(&[(0, risk)], 1)
                                    }),
                            ),
                            None => (None, false, None),
                        },
                        _ => (None, false, None),
                    };
                    let new_program = step.definition.kind() == JobKind::Process
                        && !known.next().unwrap_or(false);
                    logic::PlanLine {
                        id: step.id.clone(),
                        job: step.job_name.clone(),
                        version: step.version,
                        kind: step.definition.kind(),
                        connection,
                        production,
                        risk,
                        new_program,
                        changed_since: step.changed_since,
                    }
                })
                .collect();
            cx.emit(JobEvent::Confirm {
                title: tr!("Run the chain {0}?", definition.name),
                description: logic::plan_description(&lines),
                ok: tr!("Run the chain"),
                what: Ask::Run,
            });
            return;
        }
        self.pending = None;
        let step_ids: Vec<String> = chain.steps.iter().map(|step| step.id.clone()).collect();
        match self.engine.submit_chain(ChainRequest {
            plan,
            values,
            connections,
        }) {
            Ok(handle) => {
                self.has_result = false;
                self.shown = Shown::Nothing;
                self.inner = Inner::Inputs;
                self.watch_chain(handle, step_ids, cx);
            }
            Err(refusal) => self.refuse(words::refusal(&refusal), cx),
        }
    }

    fn submit(&mut self, request: JobRequest, dialect: Option<SqlDialect>, cx: &mut Context<Self>) {
        self.pending = None;
        self.ran_sql = match &request.definition.body {
            JobBody::Sql(body) if body.parameters.is_empty() => Some(body.sql.clone()),
            _ => None,
        };
        match self.engine.submit(request) {
            Ok(handle) => {
                // The last run's result is not this run's.
                self.has_result = false;
                self.shown = Shown::Nothing;
                self.inner = Inner::Inputs;
                self.watch_run(handle, dialect, cx);
            }
            Err(refusal) => {
                self.changed_file = words::changed_file(&refusal);
                self.refuse(words::refusal(&refusal), cx);
            }
        }
    }

    /// Follows a run's status, read at most once a frame; a slow frame skips
    /// progress, never the end (section 5.6).
    fn watch_run(
        &mut self,
        handle: RunHandle,
        dialect: Option<SqlDialect>,
        cx: &mut Context<Self>,
    ) {
        let status = handle.status();
        self.active = Active::Run {
            handle: handle.clone(),
            status: status.clone(),
        };
        self.show_run_status(status, dialect, cx);
        let mut watched = handle;
        self.watch = Some(cx.spawn(async move |weak, cx| {
            loop {
                let alive = watched.changed().await;
                cx.background_executor().timer(FRAME).await;
                let status = watched.status();
                let ended = matches!(status, RunStatus::Finished(_) | RunStatus::Refused(_));
                if weak
                    .update(cx, |this, cx| {
                        if let Active::Run { status: shown, .. } = &mut this.active {
                            *shown = status.clone();
                        }
                        this.show_run_status(status, dialect, cx)
                    })
                    .is_err()
                    || ended
                    || !alive
                {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// What a status change shows: the outcome's lines and its result, a
    /// refusal, or (while queued) the countdown redrawn each second.
    fn show_run_status(
        &mut self,
        status: RunStatus,
        dialect: Option<SqlDialect>,
        cx: &mut Context<Self>,
    ) {
        let _timer = jykre::logging::UiTimer::start("automation run status");
        match &status {
            RunStatus::Finished(outcome) => {
                self.tick = None;
                self.run_lines = words::outcome_lines(outcome, dialect);
                self.run_error = words::run_error(
                    outcome,
                    self.ran_sql.as_deref(),
                    &self.definition.name,
                    None,
                );
                self.show_result(outcome.clone(), None, cx);
                self.history.update(cx, |history, cx| history.refresh(cx));
            }
            RunStatus::Refused(refusal) => {
                self.tick = None;
                self.changed_file = words::changed_file(refusal);
                self.run_lines = vec![Line {
                    level: Level::Error,
                    text: words::refusal(refusal),
                }];
            }
            RunStatus::Queued { .. } => self.start_tick(cx),
            RunStatus::Active(_) => {
                self.tick = None;
                self.run_lines.clear();
            }
        }
        cx.notify();
    }

    /// What a finished run left to look at, under its account: a SQL run's
    /// rows in the grid, or what a program printed, read from its logs.
    /// `step` names the chain's step it belongs to.
    fn show_result(
        &mut self,
        outcome: Arc<jykre::automation::RunOutcome>,
        step: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let run = outcome.id;
        if outcome.sql.is_some() {
            let grid = self.grid.clone();
            cx.spawn(async move |weak, cx| {
                // A copy of the rows, made off the UI thread: the outcome
                // stays shared with the engine's channel.
                let data = cx
                    .background_executor()
                    .spawn(
                        async move { outcome.sql.as_ref().map(|sql| Arc::new(sql.data.clone())) },
                    )
                    .await;
                let _ = weak.update(cx, |this, cx| {
                    if let Some(data) = data {
                        let _timer = jykre::logging::UiTimer::start("automation result");
                        this.has_result = data.column_count() > 0;
                        this.shown = if this.has_result {
                            Shown::Rows {
                                rows: data.row_count(),
                                columns: data.column_count(),
                                step,
                            }
                        } else {
                            Shown::Nothing
                        };
                        grid.update(cx, |grid, cx| grid.set_page(data, false, 0, None, cx));
                        cx.notify();
                    }
                });
            })
            .detach();
            return;
        }
        let Some(program) = &outcome.program else {
            return;
        };
        let [stdout, stderr, _] = super::program_bits::log_sizes(program);
        if stdout.is_none() && stderr.is_none() {
            // It never started: there is nothing it printed.
            return;
        }
        self.shown = Shown::Output(Box::new(ProgramOutput {
            run,
            step,
            stream: Stream::Stdout,
            stdout: None,
            stderr: None,
            reading: true,
            scroll: UniformListScrollHandle::new(),
        }));
        let failed = outcome.state != jykre::automation::RunState::Succeeded;
        let dir = self.engine.data_dir().to_path_buf();
        cx.spawn(async move |weak, cx| {
            let stdout = read_log_lines(dir.clone(), run, Stream::Stdout).await;
            let stderr = read_log_lines(dir, run, Stream::Stderr).await;
            let _ = weak.update(cx, |this, cx| {
                let Shown::Output(output) = &mut this.shown else {
                    return;
                };
                if output.run != run {
                    return;
                }
                let printed =
                    |log: &Option<LogLines>| log.as_ref().is_some_and(|log| !log.lines.is_empty());
                output.stream = logic::first_stream(failed, printed(&stdout), printed(&stderr));
                output.stdout = stdout;
                output.stderr = stderr;
                output.reading = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// While queued, the time left is redrawn each second.
    fn start_tick(&mut self, cx: &mut Context<Self>) {
        if self.tick.is_some() {
            return;
        }
        self.tick = Some(cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if weak.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
    }

    fn watch_chain(&mut self, handle: ChainHandle, step_ids: Vec<String>, cx: &mut Context<Self>) {
        let status = handle.status();
        self.active = Active::Chain {
            handle: handle.clone(),
            status: status.clone(),
            step: None,
            step_status: None,
            step_ids,
        };
        self.run_lines.clear();
        let mut watched = handle;
        self.watch = Some(cx.spawn(async move |weak, cx| {
            loop {
                let status = watched.status();
                let ended = matches!(status, ChainStatus::Finished(_) | ChainStatus::Refused(_));
                if weak
                    .update(cx, |this, cx| this.show_chain_status(status, cx))
                    .is_err()
                    || ended
                {
                    break;
                }
                if !watched.changed().await {
                    break;
                }
                cx.background_executor().timer(FRAME).await;
            }
        }));
        cx.notify();
    }

    fn show_chain_status(&mut self, status: ChainStatus, cx: &mut Context<Self>) {
        let _timer = jykre::logging::UiTimer::start("automation chain status");
        let mut follow = None;
        let mut last_step = None;
        // Each SQL step's connection by name, to say which one changed.
        let connections: HashMap<String, String> = self
            .chain
            .as_ref()
            .and_then(|chain| chain.plan.as_ref())
            .map(|plan| {
                plan.steps
                    .iter()
                    .filter_map(|step| match &step.definition.body {
                        JobBody::Sql(sql) => {
                            let source = source_of(&self.sources.sources, sql.connection?)?;
                            Some((step.id.clone(), source.connection.name.clone()))
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Active::Chain {
            status: shown,
            step,
            step_status,
            ..
        } = &mut self.active
        {
            *shown = status.clone();
            match &status {
                ChainStatus::Running { run: Some(run), .. } => {
                    if step.as_ref().map(|handle| handle.id) != Some(*run) {
                        *step = self.engine.subscribe(*run);
                        *step_status = step.as_ref().map(RunHandle::status);
                        follow = step.clone();
                    }
                }
                ChainStatus::Running { run: None, .. } => {
                    *step = None;
                    *step_status = None;
                }
                ChainStatus::Finished(outcome) => {
                    let changed = (outcome.cause == Some(jykre::automation::Cause::PlanChanged))
                        .then_some(outcome.failed_step.as_ref())
                        .flatten()
                        .and_then(|step| {
                            Some(logic::plan_changed_text(connections.get(step)?, step))
                        });
                    self.run_lines = words::chain_outcome_lines(outcome, changed);
                    // The step that stopped the chain says what the database
                    // said, as its own run would.
                    self.run_error =
                        outcome
                            .steps
                            .iter()
                            .zip(&outcome.step_ids)
                            .find_map(|(step, id)| {
                                words::run_error(step, None, &self.definition.name, Some(id))
                            });
                    self.step_watch = None;
                    self.tick = None;
                    // The last step that ran left the chain's result.
                    last_step = outcome.steps.last().cloned().zip(
                        outcome
                            .step_ids
                            .get(outcome.steps.len().saturating_sub(1))
                            .cloned(),
                    );
                    self.history.update(cx, |history, cx| history.refresh(cx));
                }
                ChainStatus::Refused(refusal) => {
                    self.run_lines = vec![Line {
                        level: Level::Error,
                        text: words::refusal(refusal),
                    }];
                    self.step_watch = None;
                }
            }
        }
        if let Some((last, id)) = last_step {
            self.show_result(last, Some(id), cx);
        }
        if let Some(mut handle) = follow {
            self.step_watch = Some(cx.spawn(async move |weak, cx| {
                while handle.changed().await {
                    cx.background_executor().timer(FRAME).await;
                    let status = handle.status();
                    let queued = matches!(status, RunStatus::Queued { .. });
                    if weak
                        .update(cx, |this, cx| {
                            if let Active::Chain { step_status, .. } = &mut this.active {
                                *step_status = Some(status);
                            }
                            if queued {
                                this.start_tick(cx);
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }
        cx.notify();
    }

    /// **Stop**: the run or the chain stops at once (a queued run leaves the
    /// queue).
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        match &self.active {
            Active::Run { handle, .. } => {
                self.engine.stop_run(handle.id);
            }
            Active::Chain { handle, .. } => {
                self.engine.stop_chain(handle.id);
            }
            Active::Idle => {}
        }
        cx.notify();
    }

    /// A chain step run again on its own: the form takes the values the
    /// earlier step stored, and says which inputs are left to fill in. It
    /// never runs by itself.
    pub fn prefill(
        &mut self,
        values: HashMap<String, String>,
        missing: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for field in &self.values {
            if let Some(value) = values.get(&field.name) {
                let value = value.clone();
                field
                    .input
                    .update(cx, |input, cx| input.set_value(value, window, cx));
            }
        }
        self.prefill_note = Some(if missing.is_empty() {
            tr!("Filled in from the chain run: its constants and what earlier steps stored. Check the values, then Run.")
                .to_owned()
        } else {
            tr!(
                "Filled in from the chain run: its constants and what earlier steps stored. Fill in {0}, which were not stored or have expired, then Run.",
                missing.join(", ")
            )
        });
        self.set_inner(Inner::Inputs, window, cx);
    }

    // -----------------------------------------------------------------------
    // The chain's plan.

    /// Reads the versions the chain names and checks it against them, off
    /// the UI thread, after a structural change.
    fn refresh_plan(&mut self, cx: &mut Context<Self>) {
        if self.chain.is_none() {
            return;
        }
        let definition = self.current(cx);
        let stored = self.revision.map(|revision| (self.id, revision));
        let engine = self.engine.clone();
        let Some(chain) = self.chain.as_mut() else {
            return;
        };
        chain.plan_generation += 1;
        let generation = chain.plan_generation;
        cx.spawn(async move |weak, cx| {
            let plan = backend::run(async move { engine.plan(definition, stored).await }).await;
            let _ = weak.update(cx, |this, cx| {
                let Some(chain) = this.chain.as_mut() else {
                    return;
                };
                if chain.plan_generation == generation {
                    chain.plan = plan.ok();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A chain step's structure changed: the fields follow and the plan is
    /// read again.
    fn edit_chain(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut definition::ChainBody),
    ) {
        self.store_fields(cx);
        if let JobBody::Chain(body) = &mut self.definition.body {
            edit(body);
        }
        self.rebuild(window, cx);
        self.changed(cx);
        self.refresh_plan(cx);
    }
}

impl Render for JobView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::probe::rendered("JobView");
        column()
            .id("job")
            .key_context("AutomationJob")
            .track_focus(&self.focus)
            .size_full()
            .bg(rgb(colors().bg))
            .child(self.toolbar(cx))
            .child(self.inner_tabs(cx))
            .when_some(self.notice.clone(), |s, notice| {
                s.child(
                    row()
                        .id("job-notice")
                        .role(if notice.tone == Tone::Error {
                            Role::Alert
                        } else {
                            Role::Status
                        })
                        .px_3()
                        .py_2()
                        .child(feedback_line(&notice).flex_1())
                        .child(
                            icon_button(
                                "job-notice-close",
                                gpui_kit::assets::IconName::X,
                                tr!("Close notice"),
                            )
                            .size(px(24.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notice = None;
                                cx.notify();
                            })),
                        ),
                )
            })
            .child(match self.inner {
                Inner::Editor => self.editor_panel(window, cx).into_any_element(),
                Inner::Inputs => self.inputs_panel(cx).into_any_element(),
                Inner::Runs => self.runs_panel(cx).into_any_element(),
                Inner::Schedule => self.schedule_panel(cx).into_any_element(),
            })
    }
}

impl Drop for JobView {
    fn drop(&mut self) {
        // The run belongs to the engine and keeps going; only this view's
        // watchers end with it.
        self.watch = None;
        self.step_watch = None;
        self.tick = None;
    }
}

/// The version of a step, by job and number, among those read.
fn version_of(
    versions: &HashMap<JobId, Vec<StoredVersion>>,
    job: JobId,
    version: i64,
) -> Option<JobDefinition> {
    versions
        .get(&job)?
        .iter()
        .find(|stored| stored.version == version)
        .and_then(|stored| JobDefinition::from_json(&stored.definition_json).ok())
}

/// A new step of the chain, on no job yet.
fn new_step(body: &definition::ChainBody) -> ChainStep {
    ChainStep::new(logic::new_step_id(body), JobId::new(), 0)
}

/// A new output of a job.
fn new_output(kind: JobKind, count: usize) -> (Option<SqlOutput>, Option<OutputSpec>) {
    let name = format!("output_{}", count + 1);
    match kind {
        JobKind::Sql => (
            Some(SqlOutput {
                name,
                kind: ParameterKind::Text,
                column: u32::try_from(count + 1).unwrap_or(1),
            }),
            None,
        ),
        _ => (
            None,
            Some(OutputSpec {
                name,
                kind: ParameterKind::Text,
            }),
        ),
    }
}

/// Whether the program's protocol offers inputs and outputs.
fn takes_inputs(definition: &JobDefinition) -> bool {
    match &definition.body {
        JobBody::Process(process) => process.protocol == Protocol::Jjp1,
        _ => true,
    }
}
