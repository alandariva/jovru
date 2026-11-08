//! A panel of a report ([ADR-0025](../../docs/adr/0025-report-panels.md)):
//! its SQL, the connection and database it runs in, and the chart,
//! numbers or table it shows. It is its own entity, embedded with
//! `.cached`, so hovering one panel's grid does not lay the others out.
//!
//! The panel holds its definition and what its last run showed; the report
//! runs it (its sessions are shared by the panels of one connection and
//! database) and hands it the outcome. Its SQL editor is created the first
//! time the panel is edited, with its connection's names for completion.
//! While the report is edited, the panel's header chooses where it runs and
//! its grip drags it to another place.
use super::copying::{self, CopyJob, CopyWhat};
use super::report_chart::ReportChart;
use super::result_grid::ResultGrid;
use super::sql_editor::{GoToLine, NameIndex, SqlCatalogs, SqlEditor, SqlEditorEvent};
use super::ui::*;
use super::workspace::SQL_EDITOR_CONTEXT;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::copy_as::{CopyFormat, CopyOptions, CopyRequest, CopySource};
use jykre::drivers::*;
use jykre::reports::chart::{self, ChartProblem, Visual};
use jykre::reports::export::PanelDocument;
use jykre::reports::run::{PanelOutcome, PanelPhase};
use jykre::reports::{
    ChartKind, ChartSpec, Comparison, Condition, ConnectionRef, Direction, Panel, PanelId, Signal,
    Width,
};
use jykre::result::ResultData;
use jykre::{backend, tr};
use std::rc::Rc;
use std::sync::Arc;

/// The most columns the chart's value chips offer.
const MAX_CHIPS: usize = 40;
const HEADER_HEIGHT: f32 = 44.;
const FOOTER_HEIGHT: f32 = 30.;
const EDITOR_HEIGHT: f32 = 180.;
/// The title, database, editor and visual rows while editing.
const DEFINITION_HEIGHT: f32 = EDITOR_HEIGHT + 136.;
const CHART_HEIGHT: f32 = 240.;
/// One line of numbers.
const NUMBER_HEIGHT: f32 = 110.;
/// The colours' explanation, then one line per number while editing.
const COLORS_HINT_HEIGHT: f32 = 24.;
const COLOR_ROW_HEIGHT: f32 = 34.;
const TABLE_HEIGHT: f32 = 240.;
const TABLE_ALONE_HEIGHT: f32 = 300.;
const NOTICE_HEIGHT: f32 = 28.;
const STATEMENTS_HEIGHT: f32 = 120.;

/// A connection a report's panels may run on, as the host knows it.
#[derive(Clone)]
pub struct ReportSource {
    pub connection: ConnectionRef,
    /// `None` while the connection cannot run (no profile, demo).
    pub input: Option<ResolvedConnection>,
    pub row_limit: u32,
    pub environment: Option<jykre::connection_appearance::Environment>,
    pub dialect: Option<SqlDialect>,
    /// The databases (namespaces) a panel may run in.
    pub choices: Vec<SessionContext>,
    /// The connection's cache names, once the explorer read them.
    pub names: Option<(ConnectionId, Arc<NameIndex>)>,
}

/// The connections a report's panels may run on.
pub type Sources = Rc<Vec<ReportSource>>;

/// The source `connection` names: by its identity, then by its name.
pub fn find_source<'a>(
    sources: &'a [ReportSource],
    connection: &ConnectionRef,
) -> Option<&'a ReportSource> {
    connection
        .id
        .and_then(|id| {
            sources
                .iter()
                .find(|source| source.connection.id == Some(id))
        })
        .or_else(|| {
            sources
                .iter()
                .find(|source| source.connection.name == connection.name)
        })
}

/// What a source menu does with the connection and database chosen.
pub type PickSource = Rc<dyn Fn(ConnectionRef, SessionContext, &mut Window, &mut App)>;

/// Fills `menu` with one submenu per connection, each offering the session
/// default and the connection's databases; `current` is checked.
pub fn source_menu(
    mut menu: gpui_kit::component::menu::PopupMenu,
    window: &mut Window,
    cx: &mut Context<gpui_kit::component::menu::PopupMenu>,
    sources: &Sources,
    current: Option<(ConnectionRef, SessionContext)>,
    pick: PickSource,
) -> gpui_kit::component::menu::PopupMenu {
    if sources.is_empty() {
        return menu.label(tr!("No connections yet. Add one in the explorer."));
    }
    for (index, source) in sources.iter().enumerate() {
        let sources = sources.clone();
        let current = current.clone();
        let pick = pick.clone();
        let here = current
            .as_ref()
            .is_some_and(|(connection, _)| source.connection.same(connection));
        let label = if here {
            format!("✓ {}", source.connection.name)
        } else {
            source.connection.name.clone()
        };
        menu = menu.submenu(label, window, cx, move |mut menu, _, _| {
            let Some(source) = sources.get(index) else {
                return menu;
            };
            let item = |label: String, context: SessionContext| {
                let pick = pick.clone();
                let connection = source.connection.clone();
                let checked = current.as_ref().is_some_and(|(chosen, chosen_context)| {
                    chosen.same(&connection) && *chosen_context == context
                });
                PopupMenuItem::new(label)
                    .checked(checked)
                    .on_click(move |_, window, cx| {
                        pick(connection.clone(), context.clone(), window, cx)
                    })
            };
            menu = menu.item(item(
                tr!("Session default").to_owned(),
                SessionContext::default(),
            ));
            if source.choices.is_empty() {
                menu = menu.label(tr!("Refresh the metadata to list namespaces"));
            }
            for choice in &source.choices {
                menu = menu.item(item(jykre::ddl::context_label(choice), choice.clone()));
            }
            menu
        });
    }
    menu
}

/// What the panels' editors need from the report. The report keeps it, so
/// a panel opened for editing later gets it too.
#[derive(Clone, Default)]
pub struct PanelLanguage {
    pub catalogs: Arc<SqlCatalogs>,
    pub sources: Sources,
}

/// A panel being dragged to another place of its report.
#[derive(Clone)]
pub struct PanelDrag {
    /// The report the panel belongs to; another report refuses it.
    pub owner: EntityId,
    pub panel: PanelId,
    pub title: SharedString,
}

/// What follows the pointer while a panel is dragged.
struct PanelDragPreview {
    title: SharedString,
}

impl Render for PanelDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        row()
            .w(px(220.))
            .h(px(36.))
            .px_3()
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(colors().mint))
            .bg(rgb(colors().surface))
            .shadow_md()
            .text_size(px(13.))
            .text_color(rgb(colors().text))
            .child(icon(I::GripVertical, colors().mint))
            .child(div().min_w_0().truncate().child(self.title.clone()))
    }
}

pub enum PanelEvent {
    /// The SQL changed: the report's parameters may follow.
    SqlChanged,
    /// Another part of the definition changed.
    Changed,
    Run,
    Stop,
    /// The panel asks to be the one edited.
    Edit,
    DoneEditing,
    Move(Direction),
    Duplicate,
    Remove,
    /// Go to definition asks the explorer to show this object.
    Reveal(ObjectPath),
}

impl EventEmitter<PanelEvent> for ReportPanel {}

impl copying::CopyHost for ReportPanel {
    fn copy_job(&mut self) -> &mut Option<CopyJob> {
        &mut self.copy_job
    }

    fn copy_epoch(&self) -> u64 {
        self.copy_epoch
    }

    fn copy_notice(&mut self, feedback: Feedback, cx: &mut Context<Self>) {
        self.notice = Some(feedback);
        cx.emit(PanelEvent::Changed);
        cx.notify();
    }

    /// The status takes the notice line, so the report lays the panel out
    /// again.
    fn copy_status_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(PanelEvent::Changed);
        cx.notify();
    }

    /// The panel's menu leaves the keyboard nowhere; the panel takes it so
    /// `Esc` cancels.
    fn copy_focus(&self) -> Option<FocusHandle> {
        Some(self.focus.clone())
    }
}

/// The SQL editor, made the first time the panel is edited.
struct PanelEditor {
    editor: Entity<EditorState>,
    sql_editor: Entity<SqlEditor>,
    /// The editor's **Go to line** prompt (`Ctrl G`).
    go_to_line: Entity<GoToLine>,
    /// The catalogs and names last handed to the editor, so they are not
    /// installed again for nothing.
    catalogs: Option<Arc<SqlCatalogs>>,
    names: Option<(ConnectionId, i64)>,
    _subscriptions: Vec<Subscription>,
}

/// The target of one colour of one number, typed while editing.
struct TargetField {
    column: String,
    signal: Signal,
    input: Entity<InputState>,
}

/// What the last run left to show.
struct Shown {
    data: Arc<ResultData>,
    limited: bool,
    elapsed_ms: Option<u128>,
    numeric: Vec<String>,
}

pub struct ReportPanel {
    panel: Panel,
    /// The report the panel belongs to, for dragging it.
    owner: EntityId,
    index: usize,
    count: usize,
    /// The report is being edited: the panel can be moved, resized and
    /// removed.
    arranging: bool,
    /// The panel's definition is open.
    editing: bool,
    /// No chart was ever chosen: the first result's suggestion is taken.
    fresh: bool,
    title: Entity<InputState>,
    editor: Option<PanelEditor>,
    language: PanelLanguage,
    phase: PanelPhase,
    shown: Option<Shown>,
    error: Option<String>,
    /// The statements and values of the last run.
    statements: Vec<String>,
    bound: Vec<(String, Option<String>)>,
    show_ran: bool,
    /// What the panel draws, for the print document.
    visual: Option<Visual>,
    chart_problem: Option<String>,
    /// Tells a chart computed off the UI thread from an older one.
    visual_revision: u64,
    grid: Entity<ResultGrid>,
    chart: Entity<ReportChart>,
    notice: Option<Feedback>,
    /// One per number and colour of a number panel, while it is edited.
    targets: Vec<TargetField>,
    target_subscriptions: Vec<Subscription>,
    /// The copy of the result running off the UI thread, if any.
    copy_job: Option<CopyJob>,
    /// Bumped whenever the result on screen is replaced, so a copy confirmed
    /// afterwards is dropped.
    copy_epoch: u64,
    /// Taken while a copy runs, so `Esc` reaches the panel.
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ReportPanel {
    pub fn new(
        panel: Panel,
        owner: EntityId,
        fresh: bool,
        language: PanelLanguage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("Panel title"))
                .default_value(panel.title.clone())
        });
        let subscriptions = vec![cx.subscribe(&title, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.panel.title = this.title.read(cx).value().trim().to_owned();
                cx.emit(PanelEvent::Changed);
                cx.notify();
            }
        })];
        Self {
            panel,
            owner,
            index: 0,
            count: 1,
            arranging: false,
            editing: false,
            fresh,
            title,
            editor: None,
            language,
            phase: PanelPhase::Idle,
            shown: None,
            error: None,
            statements: Vec::new(),
            bound: Vec::new(),
            show_ran: false,
            visual: None,
            chart_problem: None,
            visual_revision: 0,
            grid: cx.new(|cx| ResultGrid::new(Arc::default(), false, cx)),
            chart: cx.new(|_| ReportChart::new()),
            notice: None,
            targets: Vec::new(),
            target_subscriptions: Vec::new(),
            copy_job: None,
            copy_epoch: 0,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    pub fn id(&self) -> PanelId {
        self.panel.id
    }

    /// The definition as the panel holds it.
    pub fn definition(&self) -> &Panel {
        &self.panel
    }

    pub fn is_fresh(&self) -> bool {
        self.fresh
    }

    pub fn width(&self) -> Width {
        if self.editing {
            Width::Full
        } else {
            self.panel.width
        }
    }

    /// The height the report gives the panel: what it shows decides it.
    pub fn height(&self) -> f32 {
        let mut height = HEADER_HEIGHT + FOOTER_HEIGHT + 12.;
        if self.editing {
            height += DEFINITION_HEIGHT;
            if let Some(spec) = self
                .panel
                .chart
                .as_ref()
                .filter(|spec| spec.kind == ChartKind::Number && !spec.values.is_empty())
            {
                height += COLORS_HINT_HEIGHT + COLOR_ROW_HEIGHT * spec.values.len() as f32;
            }
        }
        if self.notice.is_some() || self.copy_status().is_some() {
            height += NOTICE_HEIGHT;
        }
        if self.show_ran && !self.statements.is_empty() {
            height += STATEMENTS_HEIGHT;
        }
        height += match &self.panel.chart {
            None => TABLE_ALONE_HEIGHT,
            Some(spec) => {
                let shown = if spec.kind == ChartKind::Number {
                    self.number_height()
                } else {
                    CHART_HEIGHT
                };
                shown + if self.panel.table { TABLE_HEIGHT } else { 0. }
            }
        };
        height
    }

    /// The most numbers on one line: two in half a row, four in a whole one.
    fn numbers_per_row(&self) -> usize {
        match self.width() {
            Width::Half => 2,
            Width::Full => 4,
        }
    }

    /// The height of a number panel's numbers, a line per
    /// [`Self::numbers_per_row`].
    fn number_height(&self) -> f32 {
        let count = self
            .panel
            .chart
            .as_ref()
            .map_or(1, |spec| spec.values.len().max(1));
        NUMBER_HEIGHT * count.div_ceil(self.numbers_per_row()) as f32
    }

    /// Hands the chart the numbers' layout of the panel's width.
    fn sync_number_layout(&mut self, cx: &mut Context<Self>) {
        let per_row = self.numbers_per_row();
        self.chart
            .update(cx, |chart, cx| chart.set_per_row(per_row, cx));
    }

    /// Its place among the report's panels, for its default title and for
    /// moving it.
    pub fn set_position(&mut self, index: usize, count: usize, cx: &mut Context<Self>) {
        if (self.index, self.count) != (index, count) {
            self.index = index;
            self.count = count;
            cx.notify();
        }
    }

    pub fn set_arranging(&mut self, arranging: bool, cx: &mut Context<Self>) {
        if self.arranging != arranging {
            self.arranging = arranging;
            cx.notify();
        }
    }

    /// Opens or closes the panel's definition. The SQL editor is made the
    /// first time, and takes the keyboard.
    pub fn set_editing(&mut self, editing: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing == editing {
            return;
        }
        self.editing = editing;
        if editing {
            self.ensure_editor(window, cx);
            self.sync_targets(window, cx);
            self.focus(window, cx);
        }
        self.sync_number_layout(cx);
        cx.notify();
    }

    fn ensure_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_some() {
            return;
        }
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .placeholder(tr!(
                    "SELECT … WHERE day >= :start. Each :name becomes a field of the report."
                ))
                .default_value(self.panel.sql.clone())
        });
        let sql_editor = cx.new(|cx| SqlEditor::new(&editor, cx));
        let subscriptions = vec![
            cx.subscribe(
                &sql_editor,
                |this, _, event: &SqlEditorEvent, cx| match event {
                    SqlEditorEvent::Notice(notice) => {
                        this.notice = Some(notice.clone());
                        cx.emit(PanelEvent::Changed);
                        cx.notify();
                    }
                    SqlEditorEvent::Reveal(path) => cx.emit(PanelEvent::Reveal(path.clone())),
                    SqlEditorEvent::Formatting => cx.notify(),
                },
            ),
            cx.observe(&editor, |this, editor, cx| {
                let sql = editor.read(cx).text().to_string();
                if sql != this.panel.sql {
                    this.panel.sql = sql;
                    cx.emit(PanelEvent::SqlChanged);
                }
            }),
        ];
        let go_to_line = cx.new(|_| GoToLine::new(&sql_editor));
        self.editor = Some(PanelEditor {
            editor,
            sql_editor,
            go_to_line,
            catalogs: None,
            names: None,
            _subscriptions: subscriptions,
        });
        self.apply_language(cx);
    }

    /// What the editor needs from the report: the connections, with their
    /// dialect and cache names, and the catalogs.
    pub fn set_language(&mut self, language: PanelLanguage, cx: &mut Context<Self>) {
        self.language = language;
        self.apply_language(cx);
        cx.notify();
    }

    /// The connection the panel runs on, as the host knows it now.
    pub fn source(&self) -> Option<&ReportSource> {
        find_source(&self.language.sources, &self.panel.connection)
    }

    /// Where the panel runs, with the connection's current name.
    fn source_label(&self) -> String {
        let name = self
            .source()
            .map_or(self.panel.connection.name.as_str(), |source| {
                source.connection.name.as_str()
            });
        let name = if name.is_empty() {
            tr!("Choose a connection")
        } else {
            name
        };
        jykre::reports::source_label(name, &self.panel.context)
    }

    /// Whether the connection exists and lists the panel's database.
    fn source_known(&self) -> bool {
        self.source().is_some_and(|source| {
            self.panel.context == SessionContext::default()
                || source.choices.contains(&self.panel.context)
        })
    }

    /// Runs on `connection` in `context` from now on. Another connection
    /// brings its dialect and names to the editor, and the report reads the
    /// parameters again by that dialect.
    pub fn set_source(
        &mut self,
        connection: ConnectionRef,
        context: SessionContext,
        cx: &mut Context<Self>,
    ) {
        let moved = !self.panel.connection.same(&connection);
        if !moved && self.panel.context == context {
            return;
        }
        self.panel.connection = connection;
        self.panel.context = context;
        self.apply_language(cx);
        if moved {
            cx.emit(PanelEvent::SqlChanged);
        }
        cx.emit(PanelEvent::Changed);
        cx.notify();
    }

    /// The run could not start: the panel says why.
    pub fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.error = Some(message.clone());
        if self.has_result() {
            self.notice = Some(Feedback::new(Tone::Error, message));
        }
        cx.notify();
    }

    fn apply_language(&mut self, cx: &mut Context<Self>) {
        let source = self.source().cloned();
        let catalogs_now = self.language.catalogs.clone();
        let context = self.panel.context.clone();
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let catalogs = (!editor
            .catalogs
            .as_ref()
            .is_some_and(|known| Arc::ptr_eq(known, &catalogs_now)))
        .then(|| catalogs_now.clone());
        let source_names = source.as_ref().and_then(|source| source.names.clone());
        let names = source_names
            .as_ref()
            .map(|(id, names)| (*id, names.version));
        let new_names = (editor.names != names).then(|| source_names.clone());
        let config = source
            .as_ref()
            .and_then(|source| source.input.as_ref())
            .map(|input| input.config.clone());
        let dialect = source.as_ref().and_then(|source| source.dialect);
        editor.sql_editor.update(cx, |sql_editor, cx| {
            sql_editor.set_connection(config.as_ref());
            sql_editor.set_dialect(dialect, cx);
            if let Some(catalogs) = catalogs.clone() {
                sql_editor.set_catalogs(catalogs, cx);
            }
            match new_names {
                Some(Some((id, names))) => sql_editor.set_names(id, names),
                Some(None) => sql_editor.forget_server(),
                None => {}
            }
            sql_editor.set_context(context);
        });
        if let Some(catalogs) = catalogs {
            editor.catalogs = Some(catalogs);
        }
        editor.names = names;
    }

    pub fn phase(&self) -> PanelPhase {
        self.phase
    }

    pub fn set_phase(&mut self, phase: PanelPhase, cx: &mut Context<Self>) {
        if self.phase != phase {
            self.phase = phase;
            cx.notify();
        }
    }

    /// Whether the keyboard is in the panel's SQL editor.
    pub fn editor_focused(&self, window: &Window, cx: &App) -> bool {
        self.editing
            && self.editor.as_ref().is_some_and(|editor| {
                editor
                    .editor
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            })
    }

    /// Puts the keyboard in the SQL editor while editing.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.as_ref().filter(|_| self.editing) {
            editor
                .editor
                .update(cx, |editor, cx| editor.focus(window, cx));
        }
    }

    /// Shows how a run ended. A fresh panel takes the chart its result
    /// suggests.
    pub fn publish(&mut self, outcome: PanelOutcome, cx: &mut Context<Self>) {
        self.statements = outcome.info.statements;
        self.bound = outcome.info.bound;
        self.error = outcome.error.as_ref().map(|error| match outcome.phase {
            PanelPhase::Cancelled => tr!("The panel was stopped.").to_owned(),
            _ => tr!("The panel failed: {0}", error),
        });
        self.notice = if outcome.transaction_closed {
            Some(Feedback::new(
                Tone::Warning,
                tr!(
                    "This panel's SQL left a transaction open. Reports keep no transactions, so its session was closed and the transaction rolled back."
                ),
            ))
        } else {
            outcome.reopened.then(|| {
                Feedback::new(
                    Tone::Warning,
                    tr!("The connection was lost and opened again; session settings started over."),
                )
            })
        };
        if outcome.info.adopt
            && self.fresh
            && self.panel.chart.is_none()
            && let Some(suggestion) = outcome.suggestion
        {
            self.panel.chart = Some(suggestion);
            self.panel.table = false;
            self.fresh = false;
            cx.emit(PanelEvent::Changed);
        }
        if let Some(data) = outcome.data {
            // A copy of the result being replaced never reaches the
            // clipboard.
            self.copy_job = None;
            self.copy_epoch += 1;
            self.grid.update(cx, |grid, cx| {
                grid.set_page(data.clone(), false, 0, None, cx)
            });
            if let Some(previous) = self.shown.replace(Shown {
                data,
                limited: outcome.limited,
                elapsed_ms: outcome.elapsed_ms,
                numeric: outcome.numeric,
            }) && let Ok(previous) = Arc::try_unwrap(previous.data)
            {
                backend::release(previous);
            }
            self.visual_revision += 1;
            let visual = match outcome.visual {
                Some(Ok(visual)) => Ok(Some(visual)),
                Some(Err(problem)) => Err(problem.message()),
                None => Ok(None),
            };
            self.show_visual(visual, cx);
        }
        // A run that failed after an earlier one succeeded keeps that
        // result on screen, and says above it why this one did not replace
        // it; without a result the body says it.
        if let Some(error) = self.error.clone().filter(|_| self.has_result()) {
            let tone = if outcome.phase == PanelPhase::Cancelled {
                Tone::Warning
            } else {
                Tone::Error
            };
            self.notice = Some(Feedback::new(tone, error));
        }
        cx.notify();
    }

    fn show_visual(&mut self, visual: Result<Option<Visual>, String>, cx: &mut Context<Self>) {
        let (visual, problem) = match visual {
            Ok(visual) => (visual, None),
            Err(problem) => (None, Some(problem)),
        };
        self.chart_problem = problem;
        self.visual = visual.clone();
        self.sync_number_layout(cx);
        self.chart
            .update(cx, |chart, cx| chart.set_visual(visual, cx));
    }

    /// Computes what the chart shows again, off the UI thread (it reads the
    /// whole result), after the chart changed. A newer change wins.
    fn derive_visual(&mut self, cx: &mut Context<Self>) {
        self.visual_revision += 1;
        let revision = self.visual_revision;
        let (Some(shown), Some(spec)) = (&self.shown, self.panel.chart.clone()) else {
            self.show_visual(Ok(None), cx);
            return;
        };
        let data = shown.data.clone();
        let task = cx.background_spawn(async move { chart::visual(&data, &spec) });
        cx.spawn(async move |weak, cx| {
            let visual = task.await;
            let _ = weak.update(cx, |this, cx| {
                if this.visual_revision == revision {
                    this.show_visual(visual.map(Some).map_err(|problem| problem.message()), cx);
                    cx.emit(PanelEvent::Changed);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn set_chart(&mut self, chart: Option<ChartSpec>, window: &mut Window, cx: &mut Context<Self>) {
        self.fresh = false;
        self.panel.chart = chart;
        if self.panel.chart.is_none() {
            self.panel.table = true;
        }
        self.sync_targets(window, cx);
        self.derive_visual(cx);
        cx.emit(PanelEvent::Changed);
        cx.notify();
    }

    /// Shows column `name` of the chart, or stops showing it; a number
    /// panel that already shows all it can says so.
    fn toggle_value(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut spec) = self.panel.chart.clone() else {
            return;
        };
        if spec.toggle_value(name) {
            self.set_chart(Some(spec), window, cx);
        } else {
            self.notice = Some(Feedback::new(
                Tone::Warning,
                ChartProblem::TooManyNumbers.message(),
            ));
            cx.emit(PanelEvent::Changed);
            cx.notify();
        }
    }

    /// Sets when number `column` shows in `signal`'s colour, keeping the
    /// target already typed for it.
    fn set_comparison(
        &mut self,
        column: &str,
        signal: Signal,
        comparison: Option<Comparison>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut spec) = self.panel.chart.clone() else {
            return;
        };
        let target = self
            .target_field(column, signal)
            .map(|field| field.input.read(cx).value().trim().to_owned())
            .unwrap_or_default();
        spec.set_condition(
            column,
            signal,
            comparison.map(|comparison| Condition::new(comparison, target)),
        );
        self.set_chart(Some(spec), window, cx);
    }

    /// The target typed for number `column` changed.
    fn set_target(&mut self, column: &str, signal: Signal, cx: &mut Context<Self>) {
        let Some(target) = self
            .target_field(column, signal)
            .map(|field| field.input.read(cx).value().to_string())
        else {
            return;
        };
        if self
            .panel
            .chart
            .as_mut()
            .is_some_and(|spec| spec.set_target(column, signal, &target))
        {
            self.derive_visual(cx);
            cx.emit(PanelEvent::Changed);
            cx.notify();
        }
    }

    fn target_field(&self, column: &str, signal: Signal) -> Option<&TargetField> {
        self.targets
            .iter()
            .find(|field| field.column == column && field.signal == signal)
    }

    /// One target field per number and colour of a number panel being
    /// edited; a field kept keeps what it holds.
    fn sync_targets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted: Vec<(String, Signal)> = self
            .panel
            .chart
            .as_ref()
            .filter(|spec| spec.kind == ChartKind::Number && self.editing)
            .map(|spec| {
                spec.values
                    .iter()
                    .flat_map(|column| Signal::ALL.map(|signal| (column.clone(), signal)))
                    .collect()
            })
            .unwrap_or_default();
        let current: Vec<(String, Signal)> = self
            .targets
            .iter()
            .map(|field| (field.column.clone(), field.signal))
            .collect();
        if wanted == current {
            return;
        }
        let mut old = std::mem::take(&mut self.targets);
        self.targets = wanted
            .into_iter()
            .map(|(column, signal)| {
                if let Some(index) = old
                    .iter()
                    .position(|field| field.column == column && field.signal == signal)
                {
                    return old.swap_remove(index);
                }
                let target = self
                    .panel
                    .chart
                    .as_ref()
                    .and_then(|spec| spec.colors_of(&column).get(signal).cloned())
                    .map(|condition| condition.target)
                    .unwrap_or_default();
                TargetField {
                    column,
                    signal,
                    input: cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder(tr!("e.g. 1250.50"))
                            .default_value(target)
                    }),
                }
            })
            .collect();
        self.target_subscriptions = self
            .targets
            .iter()
            .map(|field| {
                let column = field.column.clone();
                let signal = field.signal;
                cx.subscribe(&field.input, move |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.set_target(&column, signal, cx);
                    }
                })
            })
            .collect();
    }

    /// The panel as the print document shows it.
    pub fn print_parts(&self) -> PanelDocument<'_> {
        PanelDocument {
            title: self.panel.shown_title(self.index),
            source: self.source_label(),
            width: self.panel.width,
            result: self.shown.as_ref().map(|shown| &*shown.data),
            limited: self.shown.as_ref().is_some_and(|shown| shown.limited),
            visual: self.visual.as_ref(),
            table: self.panel.shows_table(),
            note: self.error.clone().or_else(|| self.chart_problem.clone()),
            bound: self.bound.clone(),
        }
    }

    pub fn has_result(&self) -> bool {
        self.shown
            .as_ref()
            .is_some_and(|shown| shown.data.column_count() > 0)
    }

    /// Copies the whole result as CSV, or as tab-separated text for a
    /// spreadsheet (`sheet`), with the column names, off the UI thread and
    /// asking first above 50 MiB. The copy menu's choices do not apply.
    fn copy(&mut self, sheet: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self
            .shown
            .as_ref()
            .map(|shown| shown.data.clone())
            .filter(|result| result.column_count() > 0)
        else {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("Run the panel before copying its result."),
            ));
            cx.emit(PanelEvent::Changed);
            cx.notify();
            return;
        };
        let (format, what) = if sheet {
            (CopyFormat::Text, CopyWhat::ResultSheet)
        } else {
            (CopyFormat::Csv, CopyWhat::ResultCsv)
        };
        let request = CopyRequest {
            result,
            source: CopySource::Page,
            options: CopyOptions {
                format,
                header: true,
                ..CopyOptions::default()
            },
            dialect: None,
            table: String::new(),
            quoted_columns: Vec::new(),
        };
        // Its status, then its outcome, take the notice line.
        copying::start(self, request, what, window, cx);
    }

    /// What a copy running past the copy budget shows in the notice line.
    fn copy_status(&self) -> Option<String> {
        self.copy_job.as_ref().and_then(copying::status)
    }

    fn copy_status_line(&self, status: String, cx: &mut Context<Self>) -> Stateful<Div> {
        row()
            .id("panel-copy")
            .role(Role::Status)
            .h(px(NOTICE_HEIGHT))
            .flex_none()
            .px_2()
            .pt_1()
            .gap_2()
            .child(div().flex_1().min_w_0().truncate().child(label(status)))
            .child(
                Button::new("panel-cancel-copy")
                    .ghost()
                    .xsmall()
                    .label(tr!("Cancel copy"))
                    .on_click(cx.listener(|this, _, _, cx| copying::cancel(this, cx))),
            )
    }

    fn header(&self, cx: &mut Context<Self>) -> Div {
        let active = self.phase.is_active();
        let title = self.panel.shown_title(self.index);
        row()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(rgb(colors().border))
            .when(self.arranging, |s| s.child(self.grip(&title)))
            .child(icon(self.symbol(), colors().mint))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .map(|s| {
                if self.arranging {
                    s.child(self.source_button(cx))
                } else {
                    s.child(badge(
                        self.source_label(),
                        if self.source_known() {
                            colors().blue
                        } else {
                            colors().amber
                        },
                    ))
                }
            })
            .child(self.phase_mark())
            .child(if active {
                icon_button("panel-stop", I::Square, tr!("Stop this panel"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::Stop)))
            } else {
                icon_button("panel-run", I::Play, tr!("Run this panel"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::Run)))
            })
            .when(self.arranging, |s| {
                s.child(if self.editing {
                    icon_button("panel-done", I::Check, tr!("Close the panel's definition"))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::DoneEditing)))
                } else {
                    icon_button("panel-edit", I::Pencil, tr!("Edit the panel"))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::Edit)))
                })
            })
            .child(self.menu(cx))
    }

    fn symbol(&self) -> I {
        match self.panel.chart.as_ref().map(|chart| chart.kind) {
            None => I::Table,
            Some(ChartKind::Bar) => I::ChartColumn,
            Some(ChartKind::Line) => I::ChartLine,
            Some(ChartKind::Pie) => I::ChartPie,
            Some(ChartKind::Number) => I::Hash,
        }
    }

    fn phase_mark(&self) -> AnyElement {
        match self.phase {
            PanelPhase::Running => row()
                .id("panel-running")
                .role(Role::Status)
                .aria_label(tr!("Running"))
                .gap_1()
                .child(spinning_icon(
                    "panel-spinner",
                    I::LoaderCircle,
                    colors().amber,
                ))
                .into_any_element(),
            PanelPhase::Queued => div()
                .id("panel-queued")
                .role(Role::Status)
                .child(badge(tr!("Waiting"), colors().faint))
                .into_any_element(),
            PanelPhase::Failed => div()
                .id("panel-failed")
                .role(Role::Status)
                .child(badge(tr!("Failed"), colors().red))
                .into_any_element(),
            PanelPhase::Cancelled => div()
                .id("panel-cancelled")
                .role(Role::Status)
                .child(badge(tr!("Stopped"), colors().amber))
                .into_any_element(),
            PanelPhase::Idle | PanelPhase::Succeeded => div().into_any_element(),
        }
    }

    fn menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let arranging = self.arranging;
        let first = self.index == 0;
        let last = self.index + 1 >= self.count;
        let half = self.panel.width == Width::Half;
        Button::new("panel-menu")
            .ghost()
            .small()
            .icon(I::EllipsisVertical)
            .accessibility_label(tr!("Panel actions"))
            .tooltip(tr!("Panel actions"))
            .dropdown_menu(move |menu, _, _| {
                let item = |label: &'static str,
                            action: fn(
                    &mut ReportPanel,
                    &mut Window,
                    &mut Context<ReportPanel>,
                )| {
                    let view = view.clone();
                    PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| action(this, window, cx));
                    })
                };
                let mut menu = menu
                    .item(item(tr!("Copy as CSV"), |this, window, cx| {
                        this.copy(false, window, cx)
                    }))
                    .item(item(tr!("Copy for a spreadsheet"), |this, window, cx| {
                        this.copy(true, window, cx)
                    }));
                if arranging {
                    menu = menu.separator();
                    if !first {
                        menu = menu.item(item(tr!("Move up"), |_, _, cx| {
                            cx.emit(PanelEvent::Move(Direction::Up))
                        }));
                    }
                    if !last {
                        menu = menu.item(item(tr!("Move down"), |_, _, cx| {
                            cx.emit(PanelEvent::Move(Direction::Down))
                        }));
                    }
                    menu = menu
                        .item(
                            item(tr!("Half width"), |this, _, cx| {
                                this.panel.width = match this.panel.width {
                                    Width::Half => Width::Full,
                                    Width::Full => Width::Half,
                                };
                                this.sync_number_layout(cx);
                                cx.emit(PanelEvent::Changed);
                                cx.notify();
                            })
                            .checked(half),
                        )
                        .item(item(tr!("Duplicate the panel"), |_, _, cx| {
                            cx.emit(PanelEvent::Duplicate)
                        }))
                        .item(item(tr!("Remove the panel"), |_, _, cx| {
                            cx.emit(PanelEvent::Remove)
                        }));
                }
                menu
            })
    }

    /// The panel's definition: title, database, SQL and how it is shown.
    fn definition_rows(&self, cx: &mut Context<Self>) -> Div {
        let Some(editor) = &self.editor else {
            return div();
        };
        column()
            .flex_none()
            .gap_2()
            .p_2()
            .border_b_1()
            .border_color(rgb(colors().border))
            .child(
                row().child(
                    div().w(px(260.)).child(
                        Input::new(&self.title)
                            .small()
                            .aria_label(tr!("Panel title")),
                    ),
                ),
            )
            .child(
                div()
                    .relative()
                    .child(
                        div()
                            .h(px(EDITOR_HEIGHT))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(rgb(colors().border))
                            .key_context(SQL_EDITOR_CONTEXT)
                            .on_action(cx.listener(|this, _: &super::ToggleComment, window, cx| {
                                if let Some(editor) = &this.editor {
                                    editor
                                        .sql_editor
                                        .update(cx, |editor, cx| editor.toggle_comment(window, cx));
                                }
                            }))
                            .on_action(cx.listener(|this, _: &super::GoToLine, window, cx| {
                                if let Some(editor) = &this.editor {
                                    editor
                                        .go_to_line
                                        .update(cx, |prompt, cx| prompt.open(window, cx));
                                }
                            }))
                            .child(Editor::new(&editor.editor).h_full()),
                    )
                    // Outside the editor's key context: the run shortcuts do
                    // not apply in the prompt's field.
                    .child(editor.go_to_line.clone()),
            )
            .child(self.visual_controls(cx))
            .when_some(
                self.panel
                    .chart
                    .clone()
                    .filter(|spec| spec.kind == ChartKind::Number && !spec.values.is_empty()),
                |s, spec| s.child(self.color_rows(&spec, cx)),
            )
    }

    /// When each number turns green or red: a comparison and a target per
    /// colour, both optional.
    fn color_rows(&self, spec: &ChartSpec, cx: &mut Context<Self>) -> Div {
        column()
            .gap_1()
            .child(div().h(px(COLORS_HINT_HEIGHT - 4.)).child(label(tr!(
                "Colour a number green or red when it meets a condition; when both hold, red wins."
            ))))
            .children(spec.values.iter().enumerate().map(|(index, column)| {
                let conditions = spec.colors_of(column);
                row()
                    .h(px(COLOR_ROW_HEIGHT - 4.))
                    .gap_2()
                    .child(
                        div()
                            .w(px(160.))
                            .flex_none()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(rgb(colors().text))
                            .child(column.clone()),
                    )
                    .children(Signal::ALL.into_iter().map(|signal| {
                        self.condition_controls(
                            index,
                            column,
                            signal,
                            conditions.get(signal).cloned(),
                            cx,
                        )
                    }))
            }))
    }

    /// "Green when ≥ [target]": the comparison's menu and, once one is
    /// chosen, the target's field, outlined when it is not a number.
    fn condition_controls(
        &self,
        index: usize,
        column: &str,
        signal: Signal,
        condition: Option<Condition>,
        cx: &mut Context<Self>,
    ) -> Div {
        let view = cx.entity().downgrade();
        let (color, caption, menu_label) = match signal {
            Signal::Green => (
                colors().mint,
                tr!("Green when"),
                tr!("When {0} turns green", column),
            ),
            Signal::Red => (
                colors().red,
                tr!("Red when"),
                tr!("When {0} turns red", column),
            ),
        };
        let id = index * 2 + usize::from(signal == Signal::Red);
        let current = condition.as_ref().map(|condition| condition.comparison);
        let menu_column = column.to_owned();
        let invalid = condition
            .as_ref()
            .is_some_and(|condition| !condition.target.is_empty() && !condition.target_is_valid());
        row()
            .gap_1()
            .child(dot(color))
            .child(label(caption))
            .child(
                Button::new(("number-comparison", id))
                    .ghost()
                    .small()
                    .label(current.map_or(tr!("Never"), comparison_symbol))
                    .dropdown_caret(true)
                    .accessibility_label(menu_label)
                    .dropdown_menu(move |menu, _, _| {
                        let never_view = view.clone();
                        let never_column = menu_column.clone();
                        let mut menu = menu.item(
                            PopupMenuItem::new(tr!("Never"))
                                .checked(current.is_none())
                                .on_click(move |_, window, cx| {
                                    let _ = never_view.update(cx, |this, cx| {
                                        this.set_comparison(&never_column, signal, None, window, cx)
                                    });
                                }),
                        );
                        for comparison in Comparison::ALL {
                            let view = view.clone();
                            let column = menu_column.clone();
                            menu = menu.item(
                                PopupMenuItem::new(comparison_name(comparison))
                                    .checked(current == Some(comparison))
                                    .on_click(move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.set_comparison(
                                                &column,
                                                signal,
                                                Some(comparison),
                                                window,
                                                cx,
                                            )
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .when_some(
                self.target_field(column, signal)
                    .filter(|_| condition.is_some()),
                |s, field| {
                    s.child(
                        div()
                            .w(px(110.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(rgb(if invalid {
                                colors().red
                            } else {
                                colors().panel
                            }))
                            .child(
                                Input::new(&field.input)
                                    .small()
                                    .aria_label(tr!("Target of {0}", column)),
                            ),
                    )
                    .when(invalid, |s| {
                        s.child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(colors().red))
                                .child(tr!("A number, with a dot for decimals")),
                        )
                    })
                },
            )
            // The red condition starts at the same place on every line.
            .when(signal == Signal::Green, |s| {
                s.flex_none().min_w(px(280.)).mr_3()
            })
    }

    /// Where the panel runs: its connection and the database its
    /// unqualified names resolve in, chosen in a menu of the connections,
    /// each with its databases.
    fn source_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let known = self.source_known();
        let shown = self.source_label();
        let sources = self.language.sources.clone();
        let current = Some((self.panel.connection.clone(), self.panel.context.clone()));
        let pick: PickSource = Rc::new(move |connection, context, _, cx| {
            let _ = view.update(cx, |this, cx| this.set_source(connection, context, cx));
        });
        Button::new("panel-source")
            .ghost()
            .small()
            .icon(if known { I::Database } else { I::TriangleAlert })
            .label(shown.clone())
            .dropdown_caret(true)
            .accessibility_label(tr!("Runs on: {0}", shown))
            .tooltip(if known {
                tr!("The connection and database the panel runs in")
            } else {
                tr!("The connection or its database is no longer listed. Choose another one or refresh its metadata.")
            })
            .dropdown_menu(move |menu, window, cx| {
                source_menu(menu, window, cx, &sources, current.clone(), pick.clone())
            })
    }

    /// The grip that drags the panel to another place of the report.
    fn grip(&self, title: &str) -> impl IntoElement {
        let drag = PanelDrag {
            owner: self.owner,
            panel: self.panel.id,
            title: title.to_owned().into(),
        };
        div()
            .id("panel-grip")
            .flex_none()
            .size(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .cursor_grab()
            .hover(|s| s.bg(rgb(colors().hover)))
            .tooltip(move |window, cx| {
                Tooltip::new(tr!("Drag to move the panel")).build(window, cx)
            })
            .child(icon(I::GripVertical, colors().faint))
            .on_drag(drag, |drag, _, _, cx| {
                cx.new(|_| PanelDragPreview {
                    title: drag.title.clone(),
                })
            })
    }

    /// How the result shows: as a table, a chart or an indicator, which
    /// columns it uses, whether the table shows too, and the width.
    fn visual_controls(&self, cx: &mut Context<Self>) -> Div {
        let view = cx.entity().downgrade();
        let spec = self.panel.chart.clone();
        let columns: Vec<String> = self
            .shown
            .as_ref()
            .map(|shown| {
                shown
                    .data
                    .columns()
                    .iter()
                    .map(|column| column.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let numeric: Vec<String> = self
            .shown
            .as_ref()
            .map(|shown| shown.numeric.clone())
            .unwrap_or_default();
        let kind_view = view.clone();
        let kind_spec = spec.clone();
        let kind_columns = columns.clone();
        let kind_numeric = numeric.clone();
        row()
            .flex_wrap()
            .gap_2()
            .child(label(tr!("Show as")))
            .child(
                Button::new("panel-visual")
                    .ghost()
                    .small()
                    .label(
                        spec.as_ref()
                            .map_or(tr!("Table"), |spec| chart_name(spec.kind)),
                    )
                    .dropdown_caret(true)
                    .accessibility_label(tr!("How the panel shows its result"))
                    .dropdown_menu(move |menu, _, _| {
                        let table_view = kind_view.clone();
                        let mut menu = menu.item(
                            PopupMenuItem::new(tr!("Table"))
                                .checked(kind_spec.is_none())
                                .on_click(move |_, window, cx| {
                                    let _ = table_view
                                        .update(cx, |this, cx| this.set_chart(None, window, cx));
                                }),
                        );
                        for kind in ChartKind::ALL {
                            let view = kind_view.clone();
                            let spec = ChartSpec::switched(
                                kind_spec.clone(),
                                kind,
                                &kind_columns,
                                &kind_numeric,
                            );
                            menu = menu.item(
                                PopupMenuItem::new(chart_name(kind))
                                    .checked(
                                        kind_spec.as_ref().is_some_and(|spec| spec.kind == kind),
                                    )
                                    .on_click(move |_, window, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.set_chart(Some(spec.clone()), window, cx)
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            )
            .when_some(
                spec.clone().filter(|spec| spec.kind.labelled()),
                |s, current| {
                    let label_view = view.clone();
                    let label_columns = if columns.is_empty() && !current.label.is_empty() {
                        vec![current.label.clone()]
                    } else {
                        columns.clone()
                    };
                    let label_current = current.clone();
                    s.child(label(tr!("Labels"))).child(
                        Button::new("panel-label")
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
                                for name in &label_columns {
                                    let view = label_view.clone();
                                    let mut spec = label_current.clone();
                                    spec.label = name.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(name.clone())
                                            .checked(label_current.label == *name)
                                            .on_click(move |_, window, cx| {
                                                let _ = view.update(cx, |this, cx| {
                                                    this.set_chart(Some(spec.clone()), window, cx)
                                                });
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                },
            )
            .when_some(spec.clone(), |s, current| {
                // An indicator shows any column; a chart, the number columns.
                let offered: Vec<String> = if current.kind == ChartKind::Number {
                    columns.clone()
                } else {
                    numeric.clone()
                };
                let offered: Vec<String> = if offered.is_empty() {
                    current.values.clone()
                } else {
                    offered
                };
                let left_out = offered.len().saturating_sub(MAX_CHIPS);
                s.child(label(if current.kind.single_value() {
                    tr!("Value")
                } else {
                    tr!("Values")
                }))
                .children(
                    offered
                        .into_iter()
                        .take(MAX_CHIPS)
                        .enumerate()
                        .map(|(index, name)| {
                            let on = current.values.contains(&name);
                            let shown = name.clone();
                            div()
                                .id(("panel-value", index))
                                .role(Role::CheckBox)
                                .aria_label(tr!("Show the column {0}", shown))
                                .aria_selected(on)
                                .focusable()
                                .tab_index(0)
                                .focus_visible(|s| s.border_color(rgb(colors().mint)))
                                .border_1()
                                .border_color(rgb(if on { colors().mint } else { colors().border }))
                                .rounded(px(4.))
                                .px_2()
                                .h(px(24.))
                                .flex()
                                .items_center()
                                .cursor_pointer()
                                .text_size(px(12.))
                                .text_color(rgb(if on { colors().mint } else { colors().muted }))
                                .child(shown)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.toggle_value(&name, window, cx);
                                }))
                        }),
                )
                .when(left_out > 0, |s| {
                    s.child(label(tr!("and {0} more columns", left_out)))
                })
                .child(
                    button(
                        "panel-table",
                        if self.panel.table {
                            I::SquareCheck
                        } else {
                            I::Square
                        },
                        tr!("Table too"),
                    )
                    .aria_selected(self.panel.table)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.panel.table = !this.panel.table;
                        cx.emit(PanelEvent::Changed);
                        cx.notify();
                    })),
                )
            })
            .when(columns.is_empty(), |s| {
                s.child(label(tr!("Run the panel to choose its columns.")))
            })
    }

    fn body(&self) -> AnyElement {
        let chart_height = match self.panel.chart.as_ref().map(|chart| chart.kind) {
            None => None,
            Some(ChartKind::Number) => Some(self.number_height()),
            Some(_) => Some(CHART_HEIGHT),
        };
        if !self.has_result() {
            let text = match (self.phase, &self.error, &self.shown) {
                (PanelPhase::Running, _, _) => tr!("Running the panel…").to_owned(),
                (PanelPhase::Queued, _, _) => tr!("Waiting for its database's session…").to_owned(),
                (_, Some(error), _) => error.clone(),
                (_, None, Some(_)) => {
                    tr!("The last statement returned no rows to show.").to_owned()
                }
                _ if self.panel.sql.trim().is_empty() => {
                    tr!("Edit the panel to write its SQL.").to_owned()
                }
                _ => tr!("Run the report to see this panel.").to_owned(),
            };
            let tone = if self.error.is_some() && !self.phase.is_active() {
                colors().red
            } else {
                colors().muted
            };
            return div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .px_3()
                .text_size(px(13.))
                .text_color(rgb(tone))
                .child(text)
                .into_any_element();
        }
        column()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .when_some(
                chart_height.filter(|_| self.visual.is_some()),
                |s, height| {
                    s.child(
                        div().h(px(height)).flex_none().px_2().py_2().child(
                            self.chart
                                .clone()
                                .cached(StyleRefinement::default().size_full()),
                        ),
                    )
                },
            )
            .when_some(self.chart_problem.clone(), |s, problem| {
                s.child(
                    div()
                        .h(px(NOTICE_HEIGHT))
                        .flex_none()
                        .px_2()
                        .text_size(px(12.))
                        .text_color(rgb(colors().amber))
                        .child(problem),
                )
            })
            .when(self.panel.shows_table(), |s| {
                s.child(
                    div().flex_1().min_h_0().child(
                        self.grid
                            .clone()
                            .cached(StyleRefinement::default().size_full()),
                    ),
                )
            })
            .into_any_element()
    }

    fn footer(&self, cx: &mut Context<Self>) -> Div {
        let mut parts = Vec::new();
        if let Some(shown) = &self.shown {
            parts.push(tr!(
                "{0} rows",
                jykre::cell_format::count(shown.data.row_count())
            ));
            if let Some(elapsed) = shown.elapsed_ms {
                parts.push(jykre::cell_format::duration(elapsed as u64));
            }
        }
        row()
            .h(px(FOOTER_HEIGHT))
            .flex_none()
            .px_2()
            .border_t_1()
            .border_color(rgb(colors().border))
            .child(label(parts.join(" · ")))
            .when(
                self.shown.as_ref().is_some_and(|shown| shown.limited),
                |s| s.child(badge(tr!("Stopped at the row limit"), colors().amber)),
            )
            .child(div().flex_1())
            .when(!self.statements.is_empty(), |s| {
                s.child(
                    button(
                        "panel-ran",
                        I::Code,
                        if self.show_ran {
                            tr!("Hide the SQL that ran")
                        } else {
                            tr!("Show the SQL that ran")
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_ran = !this.show_ran;
                        cx.emit(PanelEvent::Changed);
                        cx.notify();
                    })),
                )
            })
    }
}

impl Render for ReportPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let shown_title = self.panel.shown_title(self.index);
        let copy_status = self.copy_status();
        column()
            .id(("report-panel", self.panel.id.0))
            .role(Role::Group)
            .aria_label(shown_title)
            .size_full()
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(if self.editing { colors().mint } else { colors().border }))
            .bg(rgb(colors().panel))
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                // Esc cancels a copy of the result, when nothing inside the
                // panel took it first.
                if event.keystroke.key == "escape" && this.copy_job.is_some() {
                    copying::cancel(this, cx);
                    cx.stop_propagation();
                }
            }))
            .child(self.header(cx))
            .when(self.editing, |s| s.child(self.definition_rows(cx)))
            .when_some(copy_status.clone(), |s, status| {
                s.child(self.copy_status_line(status, cx))
            })
            // A copy running past the copy budget takes the notice line.
            .when_some(self.notice.clone().filter(|_| copy_status.is_none()), |s, notice| {
                // One line: a long error is cut here, and the whole text
                // shows on hover.
                let text = notice.text.clone();
                s.child(
                    div()
                        .id("panel-notice")
                        .role(Role::Status)
                        .h(px(NOTICE_HEIGHT))
                        .flex_none()
                        .px_2()
                        .pt_1()
                        .overflow_hidden()
                        .tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
                        .child(feedback_line(&notice)),
                )
            })
            .when(self.show_ran && !self.statements.is_empty(), |s| {
                s.child(
                    div()
                        .id("panel-statements")
                        .h(px(STATEMENTS_HEIGHT))
                        .flex_none()
                        .overflow_y_scroll()
                        .px_2()
                        .py_1()
                        .font_family(fonts().code.clone())
                        .text_size(px(12.))
                        .text_color(rgb(colors().muted))
                        .child(self.statements.join("\n"))
                        .child(div().pt_1().text_color(rgb(colors().faint)).child(tr!(
                            "Parameter values are sent apart from the SQL and bound by the database."
                        ))),
                )
            })
            .child(self.body())
            .child(self.footer(cx))
    }
}

pub fn chart_name(kind: ChartKind) -> &'static str {
    match kind {
        ChartKind::Bar => tr!("Bars"),
        ChartKind::Line => tr!("Lines"),
        ChartKind::Pie => tr!("Pie"),
        ChartKind::Number => tr!("Number"),
    }
}

/// A comparison as its button shows it.
fn comparison_symbol(comparison: Comparison) -> &'static str {
    match comparison {
        Comparison::Greater => ">",
        Comparison::GreaterOrEqual => "≥",
        Comparison::Less => "<",
        Comparison::LessOrEqual => "≤",
        Comparison::Equal => "=",
        Comparison::NotEqual => "≠",
    }
}

/// A comparison as its menu names it.
fn comparison_name(comparison: Comparison) -> &'static str {
    match comparison {
        Comparison::Greater => tr!("Greater than (>)"),
        Comparison::GreaterOrEqual => tr!("At least (≥)"),
        Comparison::Less => tr!("Less than (<)"),
        Comparison::LessOrEqual => tr!("At most (≤)"),
        Comparison::Equal => tr!("Equal to (=)"),
        Comparison::NotEqual => tr!("Not equal to (≠)"),
    }
}
