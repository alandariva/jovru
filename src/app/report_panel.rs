//! A panel of a report ([ADR-0025](../../docs/adr/0025-report-panels.md)):
//! its SQL, the database of the report's connection it runs in, and the
//! chart, indicator or table it shows. It is its own entity, embedded with
//! `.cached`, so hovering one panel's grid does not lay the others out.
//!
//! The panel holds its definition and what its last run showed; the report
//! runs it (its sessions are shared by the panels of one database) and hands
//! it the outcome. Its SQL editor is created the first time the panel is
//! edited, with the connection's names for completion.
use super::report_chart::ReportChart;
use super::result_grid::ResultGrid;
use super::sql_editor::{NameIndex, SqlCatalogs, SqlEditor, SqlEditorEvent};
use super::ui::*;
use super::workspace::SQL_EDITOR_CONTEXT;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::drivers::*;
use jykre::reports::chart::{self, Visual};
use jykre::reports::export::PanelDocument;
use jykre::reports::run::{PanelOutcome, PanelPhase};
use jykre::reports::{ChartKind, ChartSpec, Direction, Panel, PanelId, Width};
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
const NUMBER_HEIGHT: f32 = 110.;
const TABLE_HEIGHT: f32 = 240.;
const TABLE_ALONE_HEIGHT: f32 = 300.;
const NOTICE_HEIGHT: f32 = 28.;
const STATEMENTS_HEIGHT: f32 = 120.;

/// What the panels' editors need from the report. The report keeps it, so
/// a panel opened for editing later gets it too.
#[derive(Clone, Default)]
pub struct PanelLanguage {
    pub dialect: Option<SqlDialect>,
    pub connection: Option<ConnectionConfig>,
    pub catalogs: Arc<SqlCatalogs>,
    /// The connection's cache names, once the explorer read them.
    pub names: Option<(ConnectionId, Arc<NameIndex>)>,
    /// The databases (namespaces) of the connection a panel may run in.
    pub choices: Rc<Vec<SessionContext>>,
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

/// The SQL editor, made the first time the panel is edited.
struct PanelEditor {
    editor: Entity<EditorState>,
    sql_editor: Entity<SqlEditor>,
    /// The catalogs and names last handed to the editor, so they are not
    /// installed again for nothing.
    catalogs: Option<Arc<SqlCatalogs>>,
    names: Option<(ConnectionId, i64)>,
    _subscriptions: Vec<Subscription>,
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
    _subscriptions: Vec<Subscription>,
}

impl ReportPanel {
    pub fn new(
        panel: Panel,
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
        }
        if self.notice.is_some() {
            height += NOTICE_HEIGHT;
        }
        if self.show_ran && !self.statements.is_empty() {
            height += STATEMENTS_HEIGHT;
        }
        height += match &self.panel.chart {
            None => TABLE_ALONE_HEIGHT,
            Some(spec) => {
                let shown = if spec.kind == ChartKind::Number {
                    NUMBER_HEIGHT
                } else {
                    CHART_HEIGHT
                };
                shown + if self.panel.table { TABLE_HEIGHT } else { 0. }
            }
        };
        height
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
            self.focus(window, cx);
        }
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
        self.editor = Some(PanelEditor {
            editor,
            sql_editor,
            catalogs: None,
            names: None,
            _subscriptions: subscriptions,
        });
        self.apply_language(cx);
    }

    /// What the editor needs from the report: the connection, its dialect,
    /// the catalogs, the cache's names and where the panel's names resolve.
    pub fn set_language(&mut self, language: PanelLanguage, cx: &mut Context<Self>) {
        self.language = language;
        self.apply_language(cx);
        cx.notify();
    }

    fn apply_language(&mut self, cx: &mut Context<Self>) {
        let language = self.language.clone();
        let context = self.panel.context.clone();
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let catalogs = (!editor
            .catalogs
            .as_ref()
            .is_some_and(|known| Arc::ptr_eq(known, &language.catalogs)))
        .then(|| language.catalogs.clone());
        let names = language
            .names
            .as_ref()
            .map(|(id, names)| (*id, names.version));
        let new_names = (editor.names != names).then(|| language.names.clone());
        editor.sql_editor.update(cx, |sql_editor, cx| {
            sql_editor.set_connection(language.connection.as_ref());
            sql_editor.set_dialect(language.dialect, cx);
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
        self.notice = outcome.reopened.then(|| {
            Feedback::new(
                Tone::Warning,
                tr!("The connection was lost and opened again; session settings started over."),
            )
        });
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

    fn set_chart(&mut self, chart: Option<ChartSpec>, cx: &mut Context<Self>) {
        self.fresh = false;
        self.panel.chart = chart;
        if self.panel.chart.is_none() {
            self.panel.table = true;
        }
        self.derive_visual(cx);
        cx.emit(PanelEvent::Changed);
        cx.notify();
    }

    fn set_context(&mut self, context: SessionContext, cx: &mut Context<Self>) {
        self.panel.context = context;
        self.apply_language(cx);
        cx.emit(PanelEvent::Changed);
        cx.notify();
    }

    /// The panel as the print document shows it.
    pub fn print_parts(&self) -> PanelDocument<'_> {
        PanelDocument {
            title: self.panel.shown_title(self.index),
            context: if self.panel.context == SessionContext::default() {
                String::new()
            } else {
                jykre::ddl::context_label(&self.panel.context)
            },
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

    fn copy(&mut self, tabs: bool, confirmed: bool, window: &mut Window, cx: &mut Context<Self>) {
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
        cx.emit(PanelEvent::Changed);
        cx.notify();
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
            .border_color(rgb(BORDER))
            .child(icon(self.symbol(), MINT))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .when(self.panel.context != SessionContext::default(), |s| {
                s.child(badge(jykre::ddl::context_label(&self.panel.context), BLUE))
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
                .child(spinning_icon("panel-spinner", I::LoaderCircle, AMBER))
                .into_any_element(),
            PanelPhase::Queued => div()
                .id("panel-queued")
                .role(Role::Status)
                .child(badge(tr!("Waiting"), FAINT))
                .into_any_element(),
            PanelPhase::Failed => div()
                .id("panel-failed")
                .role(Role::Status)
                .child(badge(tr!("Failed"), RED))
                .into_any_element(),
            PanelPhase::Cancelled => div()
                .id("panel-cancelled")
                .role(Role::Status)
                .child(badge(tr!("Stopped"), AMBER))
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
                        this.copy(false, false, window, cx)
                    }))
                    .item(item(tr!("Copy for a spreadsheet"), |this, window, cx| {
                        this.copy(true, false, window, cx)
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
            .border_color(rgb(BORDER))
            .child(
                row()
                    .child(
                        div().w(px(260.)).child(
                            Input::new(&self.title)
                                .small()
                                .aria_label(tr!("Panel title")),
                        ),
                    )
                    .child(self.context_menu(cx)),
            )
            .child(
                div()
                    .h(px(EDITOR_HEIGHT))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .key_context(SQL_EDITOR_CONTEXT)
                    .child(Editor::new(&editor.editor).h_full()),
            )
            .child(self.visual_controls(cx))
    }

    /// Where the panel's unqualified names resolve: the session default or
    /// one of the connection's databases (schemas, catalogs).
    fn context_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let current = self.panel.context.clone();
        let choices = self.language.choices.clone();
        let known = current == SessionContext::default() || choices.contains(&current);
        let shown = if current == SessionContext::default() {
            tr!("Session default").to_owned()
        } else {
            jykre::ddl::context_label(&current)
        };
        Button::new("panel-context")
            .ghost()
            .small()
            .icon(if known { I::Database } else { I::TriangleAlert })
            .label(shown.clone())
            .dropdown_caret(true)
            .accessibility_label(tr!("Database: {0}", shown))
            .tooltip(if known {
                tr!("Where the panel's unqualified names resolve")
            } else {
                tr!("The connection does not list this database. Refresh its metadata or choose another.")
            })
            .dropdown_menu(move |menu, _, _| {
                let pick = |context: SessionContext| {
                    let view = view.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        let _ = view.update(cx, |this, cx| this.set_context(context.clone(), cx));
                    }
                };
                let mut menu = menu.item(
                    PopupMenuItem::new(tr!("Session default"))
                        .checked(current == SessionContext::default())
                        .on_click(pick(SessionContext::default())),
                );
                if choices.is_empty() {
                    menu = menu.label(tr!("Refresh the metadata to list namespaces"));
                }
                for choice in choices.iter() {
                    menu = menu.item(
                        PopupMenuItem::new(jykre::ddl::context_label(choice))
                            .checked(*choice == current)
                            .on_click(pick(choice.clone())),
                    );
                }
                menu
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
                                .on_click(move |_, _, cx| {
                                    let _ =
                                        table_view.update(cx, |this, cx| this.set_chart(None, cx));
                                }),
                        );
                        for kind in ChartKind::ALL {
                            let view = kind_view.clone();
                            let spec =
                                switch_kind(kind_spec.clone(), kind, &kind_columns, &kind_numeric);
                            menu = menu.item(
                                PopupMenuItem::new(chart_name(kind))
                                    .checked(
                                        kind_spec.as_ref().is_some_and(|spec| spec.kind == kind),
                                    )
                                    .on_click(move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            this.set_chart(Some(spec.clone()), cx)
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
                                            .on_click(move |_, _, cx| {
                                                let _ = view.update(cx, |this, cx| {
                                                    this.set_chart(Some(spec.clone()), cx)
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
                            let current = current.clone();
                            div()
                                .id(("panel-value", index))
                                .role(Role::CheckBox)
                                .aria_label(tr!("Show the column {0}", shown))
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
                                    } else if spec.kind.single_value() {
                                        spec.values = vec![name.clone()];
                                    } else {
                                        spec.values.push(name.clone());
                                    }
                                    this.set_chart(Some(spec), cx);
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
            Some(ChartKind::Number) => Some(NUMBER_HEIGHT),
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
                RED
            } else {
                MUTED
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
                        .text_color(rgb(AMBER))
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
            .border_color(rgb(BORDER))
            .child(label(parts.join(" · ")))
            .when(
                self.shown.as_ref().is_some_and(|shown| shown.limited),
                |s| s.child(badge(tr!("Stopped at the row limit"), AMBER)),
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
        column()
            .id(("report-panel", self.panel.id.0))
            .role(Role::Group)
            .aria_label(shown_title)
            .size_full()
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(if self.editing { MINT } else { BORDER }))
            .bg(rgb(PANEL))
            .overflow_hidden()
            .child(self.header(cx))
            .when(self.editing, |s| s.child(self.definition_rows(cx)))
            .when_some(self.notice.clone(), |s, notice| {
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
                        .font_family("Cascadia Code")
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .child(self.statements.join("\n"))
                        .child(div().pt_1().text_color(rgb(FAINT)).child(tr!(
                            "Parameter values are sent apart from the SQL and bound by the database."
                        ))),
                )
            })
            .child(self.body())
            .child(self.footer(cx))
    }
}

/// The chart a change to `kind` gives: the current columns where they fit,
/// otherwise the first ones the result offers.
fn switch_kind(
    current: Option<ChartSpec>,
    kind: ChartKind,
    columns: &[String],
    numeric: &[String],
) -> ChartSpec {
    let mut spec = current.unwrap_or(ChartSpec {
        kind,
        label: String::new(),
        values: Vec::new(),
    });
    spec.kind = kind;
    if kind.labelled() && spec.label.is_empty() {
        spec.label = columns
            .iter()
            .find(|name| !numeric.contains(name))
            .or_else(|| columns.first())
            .cloned()
            .unwrap_or_default();
    }
    if spec.values.is_empty() {
        spec.values = numeric
            .iter()
            .filter(|name| **name != spec.label)
            .take(if kind.single_value() { 1 } else { 3 })
            .cloned()
            .collect();
    }
    if kind.single_value() {
        spec.values.truncate(1);
    }
    spec
}

pub fn chart_name(kind: ChartKind) -> &'static str {
    match kind {
        ChartKind::Bar => tr!("Bars"),
        ChartKind::Line => tr!("Lines"),
        ChartKind::Pie => tr!("Pie"),
        ChartKind::Number => tr!("Number"),
    }
}
