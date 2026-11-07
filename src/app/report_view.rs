//! A report's tab ([ADR-0024](../../docs/adr/0024-reports.md),
//! [ADR-0025](../../docs/adr/0025-report-panels.md)): its definition (title,
//! description and the parameters its panels share), the parameter form,
//! and its panels laid out in two columns.
//!
//! Each panel runs its SQL on its own connection and database. Panels of one
//! connection and database share a session and run in turn; others run at
//! the same time, at most four at once ([`jykre::reports::run`] decides;
//! this view only starts each group's loop). Parameter values are bound by
//! the driver, never written into the SQL (ADR-0023). While the report is
//! edited, a panel dragged by its grip and dropped on another takes its
//! place.
use super::report_panel::{
    PanelDrag, PanelEvent, PanelLanguage, PickSource, ReportPanel, ReportSource, Sources,
    find_source, source_menu,
};
use super::sql_editor::SqlCatalogs;
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::drivers::*;
use jykre::parameters::KINDS;
use jykre::reports::params::{self, ParameterProblem};
use jykre::reports::run::{
    self, PanelOutcome, PanelPhase, Plan, PlanPanel, RunScope, Scheduler, SessionKey, Sessions,
};
use jykre::reports::{
    ConnectionRef, Direction, MAX_CHOICES, MAX_PANELS, Panel, PanelId, Report, Width, drop_index,
    export,
};
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

/// The editable parts of a parameter's specification.
struct SpecRow {
    name: String,
    label: Entity<InputState>,
    default: Entity<InputState>,
    /// A new choice of a list parameter.
    choice: Entity<InputState>,
    /// The list's choices are shown for editing.
    choices_open: bool,
}

/// A parameter's value field in the form. A list parameter keeps its value
/// here too and shows a menu instead.
struct ValueField {
    name: String,
    input: Entity<InputState>,
}

/// A run waiting for the user.
enum Pending {
    /// A connection's password; the panels asked for meanwhile join.
    Password(RunScope),
    /// The confirmation of risky statements on production: exactly this
    /// plan runs once confirmed.
    Risk(Plan),
}

pub enum ReportEvent {
    /// The definition or a run changed.
    Changed,
    /// A run needs this connection's password; the host asks and calls
    /// [`ReportView::resume_pending`].
    PasswordRequired(ConnectionRef),
    /// A run on production connections holds risky statements; the host
    /// asks and calls [`ReportView::confirm_risk`].
    ConfirmRisk {
        /// The production connections, named for the question.
        connection: String,
        description: String,
    },
    /// Store this definition; the host answers with [`ReportView::saved`].
    Save(Box<Report>),
    /// Go to definition asks the explorer to show this object of a panel's
    /// connection.
    Reveal {
        connection: ConnectionRef,
        path: ObjectPath,
    },
}

impl EventEmitter<ReportEvent> for ReportView {}

pub struct ReportView {
    /// The working definition's title, description and parameters; the
    /// panels hold their own.
    report: Report,
    /// The definition as stored; `None` for a draft.
    saved: Option<Report>,
    /// The definition differs from the stored one, worked out when it
    /// changes rather than on every frame.
    unsaved: bool,
    mode: ReportMode,
    title: Entity<InputState>,
    description: Entity<InputState>,
    panels: Vec<Entity<ReportPanel>>,
    /// The panel whose definition is open.
    editing: Option<PanelId>,
    spec_rows: Vec<SpecRow>,
    values: Vec<ValueField>,
    errors: HashMap<String, ParameterProblem>,
    /// The connections the panels may run on and what their editors need,
    /// kept for the panels made later.
    language: PanelLanguage,
    scheduler: Scheduler,
    sessions: Sessions,
    pending: Option<Pending>,
    next_run: u64,
    scroll: ScrollHandle,
    notice: Option<Feedback>,
    /// What listens to the parameter fields, replaced with them.
    field_subscriptions: Vec<Subscription>,
    panel_subscriptions: Vec<Subscription>,
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
        let mut subscriptions = Vec::new();
        for field in [&title, &description] {
            subscriptions.push(cx.subscribe(field, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.changed(cx);
                }
            }));
        }
        let draft = saved.is_none();
        let mut header = report.clone();
        header.panels.clear();
        let mut view = Self {
            report: header,
            saved,
            unsaved: false,
            mode,
            title,
            description,
            panels: Vec::new(),
            editing: None,
            spec_rows: Vec::new(),
            values: Vec::new(),
            errors: HashMap::new(),
            language: PanelLanguage::default(),
            scheduler: Scheduler::default(),
            sessions: Sessions::default(),
            pending: None,
            next_run: 0,
            scroll: ScrollHandle::new(),
            notice: None,
            field_subscriptions: Vec::new(),
            panel_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        };
        for panel in report.panels {
            // A draft's panel without a chart takes the one its first
            // result suggests; a saved one chose to show a table.
            let fresh = draft && panel.chart.is_none();
            let entity = view.make_panel(panel, fresh, window, cx);
            view.panels.push(entity);
        }
        view.positions(cx);
        view.rebuild_fields(window, cx);
        if mode == ReportMode::Edit
            && let Some(first) = view.panels.first().map(|panel| panel.read(cx).id())
        {
            view.edit_panel(Some(first), window, cx);
        }
        view.refresh_unsaved(cx);
        view
    }

    fn make_panel(
        &mut self,
        panel: Panel,
        fresh: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ReportPanel> {
        let language = self.language.clone();
        let arranging = self.mode == ReportMode::Edit;
        let owner = cx.entity_id();
        let entity = cx.new(|cx| {
            let mut view = ReportPanel::new(panel, owner, fresh, language, window, cx);
            view.set_arranging(arranging, cx);
            view
        });
        self.panel_subscriptions.push(cx.subscribe_in(
            &entity,
            window,
            |this, panel, event: &PanelEvent, window, cx| {
                this.panel_event(panel, event, window, cx)
            },
        ));
        entity
    }

    fn panel_event(
        &mut self,
        panel: &Entity<ReportPanel>,
        event: &PanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = panel.read(cx).id();
        match event {
            PanelEvent::SqlChanged => {
                self.sync_parameters(window, cx);
                self.changed(cx);
            }
            PanelEvent::Changed => {
                self.close_unused_sessions(cx);
                self.changed(cx);
            }
            PanelEvent::Run => self.run(RunScope::panel(id), cx),
            PanelEvent::Stop => {
                self.scheduler.cancel_panel(id);
                self.sync_phases(cx);
            }
            PanelEvent::Edit => self.edit_panel(Some(id), window, cx),
            PanelEvent::DoneEditing => self.edit_panel(None, window, cx),
            PanelEvent::Move(direction) => self.move_panel(id, *direction, cx),
            PanelEvent::Duplicate => self.duplicate_panel(id, window, cx),
            PanelEvent::Remove => self.remove_panel(id, window, cx),
            PanelEvent::Reveal(path) => {
                let connection = panel.read(cx).definition().connection.clone();
                cx.emit(ReportEvent::Reveal {
                    connection,
                    path: path.clone(),
                });
            }
        }
    }

    /// Something of the definition changed.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.refresh_unsaved(cx);
        cx.emit(ReportEvent::Changed);
        cx.notify();
    }

    fn refresh_unsaved(&mut self, cx: &App) {
        let current = self.definition(cx);
        self.unsaved = match &self.saved {
            None => current
                .panels
                .iter()
                .any(|panel| !panel.sql.trim().is_empty()),
            Some(saved) => !current.same_definition(saved),
        };
    }

    fn panel_entity(&self, id: PanelId, cx: &App) -> Option<Entity<ReportPanel>> {
        self.panels
            .iter()
            .find(|panel| panel.read(cx).id() == id)
            .cloned()
    }

    fn panel_definitions(&self, cx: &App) -> Vec<Panel> {
        self.panels
            .iter()
            .map(|panel| panel.read(cx).definition().clone())
            .collect()
    }

    /// Tells each panel its place, for its default title and moving it.
    fn positions(&mut self, cx: &mut Context<Self>) {
        let count = self.panels.len();
        for (index, panel) in self.panels.iter().enumerate() {
            panel.update(cx, |panel, cx| panel.set_position(index, count, cx));
        }
    }

    /// Opens the definition of panel `id` (closing any other), or closes it.
    fn edit_panel(&mut self, id: Option<PanelId>, window: &mut Window, cx: &mut Context<Self>) {
        if id.is_some() && self.mode != ReportMode::Edit {
            self.set_mode(ReportMode::Edit, window, cx);
        }
        self.editing = id;
        for panel in self.panels.clone() {
            let editing = Some(panel.read(cx).id()) == id;
            panel.update(cx, |panel, cx| panel.set_editing(editing, window, cx));
        }
        // The panel opened, often just added at the end, comes into view
        // once the frame that lays it out for editing is drawn.
        if let Some(index) = id.and_then(|id| {
            self.panels
                .iter()
                .position(|panel| panel.read(cx).id() == id)
        }) {
            cx.on_next_frame(window, move |this, _, cx| {
                this.scroll.scroll_to_top_of_item(index);
                cx.notify();
            });
        }
        cx.notify();
    }

    fn next_panel_id(&self, cx: &App) -> PanelId {
        PanelId(
            self.panels
                .iter()
                .map(|panel| panel.read(cx).id().0)
                .max()
                .unwrap_or(0)
                + 1,
        )
    }

    /// Adds a panel of `sql` on `connection` in `context` and opens its
    /// definition. Refused when the report holds [`MAX_PANELS`] already.
    pub fn add_panel(
        &mut self,
        sql: String,
        connection: ConnectionRef,
        context: SessionContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), jykre::reports::Full> {
        if self.panels.len() >= MAX_PANELS {
            self.notice = Some(Feedback::new(
                Tone::Warning,
                tr!("A report holds at most {0} panels.", MAX_PANELS),
            ));
            cx.notify();
            return Err(jykre::reports::Full);
        }
        let panel = Panel::new(self.next_panel_id(cx), connection, sql, context);
        let id = panel.id;
        let entity = self.make_panel(panel, true, window, cx);
        self.panels.push(entity);
        self.positions(cx);
        self.sync_parameters(window, cx);
        self.edit_panel(Some(id), window, cx);
        self.changed(cx);
        Ok(())
    }

    /// Moves the panel `id` dropped on the panel at `target` (or after the
    /// last one) to take that place.
    fn drop_panel(&mut self, id: PanelId, target: usize, cx: &mut Context<Self>) {
        let Some(from) = self
            .panels
            .iter()
            .position(|panel| panel.read(cx).id() == id)
        else {
            return;
        };
        let Some(index) = drop_index(from, target, self.panels.len()) else {
            return;
        };
        let panel = self.panels.remove(from);
        self.panels.insert(index, panel);
        self.positions(cx);
        self.changed(cx);
    }

    fn move_panel(&mut self, id: PanelId, direction: Direction, cx: &mut Context<Self>) {
        let Some(index) = self
            .panels
            .iter()
            .position(|panel| panel.read(cx).id() == id)
        else {
            return;
        };
        let other = match direction {
            Direction::Up if index > 0 => index - 1,
            Direction::Down if index + 1 < self.panels.len() => index + 1,
            _ => return,
        };
        self.panels.swap(index, other);
        self.positions(cx);
        self.changed(cx);
    }

    fn duplicate_panel(&mut self, id: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        if self.panels.len() >= MAX_PANELS {
            self.notice = Some(Feedback::new(
                Tone::Warning,
                tr!("A report holds at most {0} panels.", MAX_PANELS),
            ));
            cx.notify();
            return;
        }
        let Some(index) = self
            .panels
            .iter()
            .position(|panel| panel.read(cx).id() == id)
        else {
            return;
        };
        let mut copy = self.panels[index].read(cx).definition().clone();
        copy.id = self.next_panel_id(cx);
        let entity = self.make_panel(copy, false, window, cx);
        self.panels.insert(index + 1, entity);
        self.positions(cx);
        self.changed(cx);
    }

    fn remove_panel(&mut self, id: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        self.scheduler.remove_panel(id);
        self.panels.retain(|panel| panel.read(cx).id() != id);
        if self.editing == Some(id) {
            self.editing = None;
        }
        self.positions(cx);
        self.sync_parameters(window, cx);
        self.close_unused_sessions(cx);
        self.sync_phases(cx);
        self.changed(cx);
    }

    /// Where a panel runs, when its connection can run.
    fn key_of(&self, panel: &Panel) -> Option<SessionKey> {
        let source = find_source(&self.language.sources, &panel.connection)?;
        Some(SessionKey {
            connection: source.connection.id?,
            context: panel.context.clone(),
        })
    }

    /// Closes the idle sessions of connections and databases no panel runs
    /// in any more.
    fn close_unused_sessions(&mut self, cx: &App) {
        let used: Vec<SessionKey> = self
            .panels
            .iter()
            .filter_map(|panel| self.key_of(panel.read(cx).definition()))
            .collect();
        for session in self.sessions.retain_used(&used) {
            backend::close_in_background(session);
        }
    }

    /// The parameters follow the markers of every panel's SQL: a new marker
    /// gets a field, a removed one loses its own, and the others keep what
    /// they hold.
    fn sync_parameters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.store_spec_fields(cx);
        let sqls: Vec<(String, Option<SqlDialect>)> = self
            .panels
            .iter()
            .map(|panel| {
                let panel = panel.read(cx);
                (
                    panel.definition().sql.clone(),
                    panel.source().and_then(|source| source.dialect),
                )
            })
            .collect();
        let names = params::names_all(sqls.iter().map(|(sql, dialect)| (sql.as_str(), *dialect)));
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

    /// Fields for every parameter: its label, default and choices while
    /// editing, its value in the form. A value already typed stays.
    fn rebuild_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.typed_values(cx);
        let open: Vec<String> = self
            .spec_rows
            .iter()
            .filter(|row| row.choices_open)
            .map(|row| row.name.clone())
            .collect();
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
                choice: cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Add a choice"))),
                choices_open: open.contains(&spec.name),
            })
            .collect();
        for row in &self.spec_rows {
            for field in [&row.label, &row.default] {
                self.field_subscriptions.push(cx.subscribe(
                    field,
                    |this, _, event: &InputEvent, cx| {
                        if matches!(event, InputEvent::Change) {
                            this.changed(cx);
                        }
                    },
                ));
            }
            let name = row.name.clone();
            self.field_subscriptions.push(cx.subscribe_in(
                &row.choice,
                window,
                move |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.add_choice(&name, window, cx);
                    }
                },
            ));
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
                    InputEvent::PressEnter { .. } => this.run(RunScope::All, cx),
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

    /// Adds what the choice field holds to the list of parameter `name`.
    fn add_choice(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.spec_rows.iter().find(|row| row.name == name) else {
            return;
        };
        let choice = row.choice.read(cx).value().to_string();
        let field = row.choice.clone();
        if choice.trim().is_empty() {
            return;
        }
        if let Some(spec) = self
            .report
            .parameters
            .iter_mut()
            .find(|spec| spec.name == name)
        {
            if spec.choices.len() >= MAX_CHOICES {
                self.notice = Some(Feedback::new(
                    Tone::Warning,
                    tr!("A list offers at most {0} choices.", MAX_CHOICES),
                ));
            } else if !spec.choices.contains(&choice) {
                spec.choices.push(choice);
            }
        }
        field.update(cx, |input, cx| input.set_value("", window, cx));
        self.changed(cx);
    }

    fn remove_choice(&mut self, name: &str, index: usize, cx: &mut Context<Self>) {
        if let Some(spec) = self
            .report
            .parameters
            .iter_mut()
            .find(|spec| spec.name == name)
            && index < spec.choices.len()
        {
            spec.choices.remove(index);
        }
        self.changed(cx);
    }

    fn typed_values(&self, cx: &App) -> HashMap<String, String> {
        self.values
            .iter()
            .map(|field| (field.name.clone(), field.input.read(cx).value().to_string()))
            .collect()
    }

    /// The definition as the fields and panels hold it.
    pub fn definition(&self, cx: &App) -> Report {
        let mut report = self.report.clone();
        report.title = self.title.read(cx).value().trim().to_owned();
        report.description = self.description.read(cx).value().to_string();
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
        report.panels = self.panel_definitions(cx);
        // A connection renamed since the panel chose it is saved by its
        // current name.
        for panel in &mut report.panels {
            if let Some(source) = find_source(&self.language.sources, &panel.connection) {
                panel.connection = source.connection.clone();
            }
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

    /// The connections the panels run on, once each, in their order.
    pub fn connections(&self, cx: &App) -> Vec<ConnectionRef> {
        self.definition(cx).connections()
    }

    /// The definition differs from the stored one (a draft always does once
    /// a panel holds SQL).
    pub fn has_unsaved_changes(&self, _: &App) -> bool {
        self.unsaved
    }

    pub fn is_busy(&self) -> bool {
        self.scheduler.is_busy()
    }

    pub fn tab_status(&self) -> jykre::grouped_tabs::TabStatus {
        self.scheduler.status()
    }

    /// Puts the keyboard where the tab is worked: the SQL of the panel being
    /// edited, the title while editing, the first parameter otherwise.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.editing.and_then(|id| self.panel_entity(id, cx)) {
            panel.update(cx, |panel, cx| panel.focus(window, cx));
            return;
        }
        match (self.mode, self.values.first()) {
            (ReportMode::View, Some(field)) => {
                field.input.update(cx, |input, cx| input.focus(window, cx));
            }
            _ => self.title.update(cx, |input, cx| input.focus(window, cx)),
        }
    }

    /// What the panels' editors need changed: hands it to every panel.
    fn push_language(&mut self, cx: &mut Context<Self>) {
        for panel in &self.panels {
            let language = self.language.clone();
            panel.update(cx, |panel, cx| panel.set_language(language, cx));
        }
    }

    pub fn set_catalogs(&mut self, catalogs: Arc<SqlCatalogs>, cx: &mut Context<Self>) {
        self.language.catalogs = catalogs;
        self.push_language(cx);
    }

    /// The connections the panels may run on, as the host knows them now.
    /// A connection whose settings changed, or that is gone, stops its
    /// panels and closes its sessions; a new password keeps them.
    pub fn set_sources(&mut self, sources: Sources, cx: &mut Context<Self>) {
        let old = self.language.sources.clone();
        for before in old.iter() {
            let Some(id) = before.connection.id else {
                continue;
            };
            let config =
                |source: &ReportSource| source.input.as_ref().map(|input| input.config.clone());
            let same = sources
                .iter()
                .find(|source| source.connection.id == Some(id))
                .is_some_and(|after| config(after) == config(before));
            if !same {
                self.scheduler.cancel_connection(id);
                for session in self.sessions.close_connection(id) {
                    backend::close_in_background(session);
                }
            }
        }
        self.language.sources = sources;
        self.push_language(cx);
        self.sync_phases(cx);
        self.refresh_unsaved(cx);
        cx.notify();
    }

    /// The host stored the definition as `report`.
    pub fn saved(&mut self, report: Report, cx: &mut Context<Self>) {
        self.report.id = report.id;
        self.report.created_at = report.created_at;
        self.report.updated_at = report.updated_at;
        self.saved = Some(report);
        self.notice = Some(Feedback::new(Tone::Success, tr!("Report saved.")));
        self.refresh_unsaved(cx);
        cx.notify();
    }

    /// The stored report was removed: the tab is a draft again.
    pub fn forget_saved(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.saved_id() == Some(id) {
            self.saved = None;
            self.report.id = 0;
            self.refresh_unsaved(cx);
            cx.notify();
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        self.store_spec_fields(cx);
        let report = self.definition(cx);
        let problem = if report.title.is_empty() {
            Some(tr!("Give the report a title before saving it."))
        } else if report
            .panels
            .iter()
            .all(|panel| panel.sql.trim().is_empty())
        {
            Some(tr!("Write the SQL of a panel before saving the report."))
        } else {
            None
        };
        if let Some(problem) = problem {
            self.notice = Some(Feedback::new(Tone::Warning, problem));
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
        let arranging = mode == ReportMode::Edit;
        if !arranging {
            self.editing = None;
        }
        for panel in self.panels.clone() {
            panel.update(cx, |panel, cx| {
                panel.set_arranging(arranging, cx);
                if !arranging {
                    panel.set_editing(false, window, cx);
                }
            });
        }
        self.focus(window, cx);
        cx.notify();
    }

    /// Runs the panel whose SQL editor has the keyboard, or else every
    /// panel (`Ctrl Enter`).
    pub fn run_shortcut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self
            .editing
            .and_then(|id| self.panel_entity(id, cx))
            .filter(|panel| panel.read(cx).editor_focused(window, cx))
            .map(|panel| panel.read(cx).id());
        match focused {
            Some(id) => self.run(RunScope::panel(id), cx),
            None => self.run(RunScope::All, cx),
        }
    }

    /// Runs the panels of `scope` with the values in the form, each on its
    /// own connection. A panel whose connection cannot run says so and the
    /// others run; a connection without its password asks for it first.
    pub fn run(&mut self, scope: RunScope, cx: &mut Context<Self>) {
        // While a password is asked for, the panels asked for meanwhile
        // join the waiting run; a waiting confirmation is asked again.
        if let Some(Pending::Password(waiting)) = self.pending.take() {
            self.pending = Some(Pending::Password(waiting.merge(scope)));
            return;
        }
        self.store_spec_fields(cx);
        let panels = self.panel_definitions(cx);
        let sources = self.language.sources.clone();
        let mut planned: Vec<PlanPanel> = Vec::new();
        let mut unavailable: Vec<(PanelId, String)> = Vec::new();
        let mut asked = false;
        for panel in panels
            .iter()
            .filter(|panel| scope.includes(panel.id) && !panel.sql.trim().is_empty())
        {
            let source = find_source(&sources, &panel.connection);
            let ready = source
                .and_then(|source| Some((source, source.connection.id?, source.input.as_ref()?)));
            match ready {
                Some((source, connection, input)) => {
                    if connection_store::needs_password(input) {
                        self.pending = Some(Pending::Password(scope.clone()));
                        cx.emit(ReportEvent::PasswordRequired(source.connection.clone()));
                        asked = true;
                        break;
                    }
                    planned.push(PlanPanel {
                        panel,
                        connection,
                        dialect: source.dialect,
                        max_rows: source.row_limit,
                    });
                }
                None => {
                    let name = source.map_or(panel.connection.name.clone(), |source| {
                        source.connection.name.clone()
                    });
                    unavailable.push((panel.id, name));
                }
            }
        }
        if asked {
            return;
        }
        for (id, name) in unavailable {
            let message = if name.is_empty() {
                tr!("Choose a connection for this panel.").to_owned()
            } else {
                tr!(
                    "The connection {0} is not available. Choose another one.",
                    name
                )
            };
            self.scheduler.mark_failed(id);
            if let Some(panel) = self.panel_entity(id, cx) {
                panel.update(cx, |panel, cx| panel.fail(message, cx));
            }
        }
        self.sync_phases(cx);
        let typed = self.typed_values(cx);
        let mut plan = match run::plan(
            &planned,
            panels.len(),
            &scope,
            &self.report.parameters,
            &typed,
            &mut self.next_run,
        ) {
            Ok(plan) => plan,
            Err(errors) => {
                self.errors = errors
                    .into_iter()
                    .map(|error| (error.name, error.problem))
                    .collect();
                self.notice = Some(Feedback::new(
                    Tone::Warning,
                    tr!("Fill in the parameters marked below."),
                ));
                cx.notify();
                return;
            }
        };
        self.errors.clear();
        if plan.is_empty() {
            let any_sql = panels
                .iter()
                .any(|panel| scope.includes(panel.id) && !panel.sql.trim().is_empty());
            if !any_sql {
                self.notice = Some(Feedback::new(
                    Tone::Info,
                    tr!("There is no SQL statement to execute."),
                ));
            }
            cx.emit(ReportEvent::Changed);
            cx.notify();
            return;
        }
        for (_, jobs) in &mut plan.groups {
            for job in jobs {
                job.info.adopt = self
                    .panel_entity(job.panel, cx)
                    .is_some_and(|panel| panel.read(cx).is_fresh());
            }
        }
        // Each production connection's statements are checked; one
        // confirmation names them all.
        let mut names = Vec::new();
        let mut descriptions = Vec::new();
        for connection in plan.connections() {
            let Some(source) = sources
                .iter()
                .find(|source| source.connection.id == Some(connection))
            else {
                continue;
            };
            if source.environment != Some(jykre::connection_appearance::Environment::Production) {
                continue;
            }
            let statements = plan.statements_on(connection);
            let risks = jykre::guard::risks(&statements, source.dialect);
            if !risks.is_empty() {
                names.push(source.connection.name.clone());
                descriptions.push(super::workspace::risk_description(&risks, statements.len()));
            }
        }
        if !names.is_empty() {
            self.pending = Some(Pending::Risk(plan));
            let description = if names.len() == 1 {
                descriptions.concat()
            } else {
                names
                    .iter()
                    .zip(&descriptions)
                    .map(|(name, description)| format!("{name}\n{description}"))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            cx.emit(ReportEvent::ConfirmRisk {
                connection: names.join(", "),
                description,
            });
            return;
        }
        self.start_plan(plan, cx);
    }

    fn start_plan(&mut self, plan: Plan, cx: &mut Context<Self>) {
        self.scheduler.enqueue(plan);
        self.notice = None;
        self.sync_phases(cx);
        self.start_groups(cx);
        cx.emit(ReportEvent::Changed);
        cx.notify();
    }

    /// Starts the loop of each group the scheduler lets run: it lends the
    /// group its connection and database's session, runs the group's panels
    /// in turn on it and gives it back. Each panel takes the connection as
    /// it is then, with a password given meanwhile.
    fn start_groups(&mut self, cx: &mut Context<Self>) {
        for context in self.scheduler.start() {
            let (session, lease) = self.sessions.take(&context);
            cx.spawn(async move |weak, cx| {
                let mut session = session;
                loop {
                    let Ok(Some((job, input))) =
                        weak.update(cx, |this, cx| this.next_job(&context, lease, cx))
                    else {
                        break;
                    };
                    let run::PanelJob {
                        panel,
                        run,
                        requests,
                        memory_ceiling,
                        cancel,
                        info,
                    } = job;
                    let (place, lent) = (context.context.clone(), session.take());
                    let id = context.connection;
                    // Reading the result for the chart and dropping the
                    // other statements' results happen on the runtime.
                    let settled = match input {
                        Some(connection) => {
                            backend::run(async move {
                                let result = backend::query_in_context(
                                    Some(id),
                                    connection,
                                    place,
                                    lent,
                                    requests,
                                    memory_ceiling,
                                    cancel,
                                )
                                .await;
                                Ok(run::settle(result, info))
                            })
                            .await
                        }
                        // The connection went away meanwhile; the session
                        // goes back as it was.
                        None => {
                            let (_, outcome) = run::settle(
                                Err(DriverError::new(
                                    ErrorCode::Connection,
                                    tr!("The connection is not available."),
                                )),
                                info,
                            );
                            Ok((lent, outcome))
                        }
                    };
                    let outcome = match settled {
                        Ok((kept, outcome)) => {
                            session = kept;
                            outcome
                        }
                        Err(error) => PanelOutcome {
                            phase: PanelPhase::Failed,
                            error: Some(error),
                            ..PanelOutcome::default()
                        },
                    };
                    let mut outcome = Some(outcome);
                    let shown = weak.update(cx, |this, cx| {
                        if let Some(outcome) = outcome.take() {
                            this.publish(panel, run, outcome, cx);
                        }
                    });
                    if shown.is_err() {
                        if let Some(outcome) = outcome {
                            backend::release(outcome);
                        }
                        break;
                    }
                }
                // The tab closed meanwhile: the session closes with it.
                let mut kept = session;
                if weak
                    .update(cx, |this, cx| {
                        this.group_done(&context, lease, kept.take(), cx)
                    })
                    .is_err()
                    && let Some(session) = kept
                {
                    backend::close_in_background(session);
                }
            })
            .detach();
        }
    }

    /// The next panel of the group running at `context`, with its
    /// connection as it is now (`None` when it cannot run), unless the
    /// group's lease was voided (the connection changed).
    fn next_job(
        &mut self,
        context: &SessionKey,
        lease: u64,
        cx: &mut Context<Self>,
    ) -> Option<(run::PanelJob, Option<ResolvedConnection>)> {
        if !self.sessions.is_current(context, lease) {
            return None;
        }
        let job = self.scheduler.next(context);
        self.sync_phases(cx);
        cx.notify();
        let input = self
            .language
            .sources
            .iter()
            .find(|source| source.connection.id == Some(context.connection))
            .and_then(|source| source.input.clone());
        job.map(|job| (job, input))
    }

    fn publish(&mut self, id: PanelId, run: u64, outcome: PanelOutcome, cx: &mut Context<Self>) {
        let panel = self.panel_entity(id, cx);
        match panel {
            Some(panel) if self.scheduler.finish(id, run, outcome.phase) => {
                panel.update(cx, |panel, cx| panel.publish(outcome, cx));
            }
            // A run stopped, removed or voided meanwhile.
            _ => backend::release(outcome),
        }
        self.sync_phases(cx);
        cx.emit(ReportEvent::Changed);
        cx.notify();
    }

    fn group_done(
        &mut self,
        context: &SessionKey,
        lease: u64,
        session: Option<Box<dyn Session>>,
        cx: &mut Context<Self>,
    ) {
        for session in self.sessions.give_back(context, lease, session) {
            backend::close_in_background(session);
        }
        self.close_unused_sessions(cx);
        self.start_groups(cx);
        self.sync_phases(cx);
        cx.emit(ReportEvent::Changed);
        cx.notify();
    }

    /// Hands each panel its phase; a panel redraws only when it changed.
    fn sync_phases(&mut self, cx: &mut Context<Self>) {
        for panel in &self.panels {
            let phase = self.scheduler.phase(panel.read(cx).id());
            panel.update(cx, |panel, cx| panel.set_phase(phase, cx));
        }
    }

    /// Stops every panel.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.scheduler.is_busy() {
            self.scheduler.cancel_all();
            self.sync_phases(cx);
            cx.notify();
        }
    }

    /// The password is in memory: the waiting run starts.
    pub fn resume_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(Pending::Password(scope)) = self.pending.take() {
            self.run(scope, cx);
        }
    }

    pub fn cancel_pending(&mut self) {
        if matches!(self.pending, Some(Pending::Password(_))) {
            self.pending = None;
        }
    }

    /// The user confirmed the risky statements: exactly the plan they saw
    /// runs.
    pub fn confirm_risk(&mut self, cx: &mut Context<Self>) {
        match self.pending.take() {
            Some(Pending::Risk(plan)) => self.start_plan(plan, cx),
            other => self.pending = other,
        }
    }

    /// Writes the print document where the user chooses and opens it in the
    /// browser, which prints it or saves it as PDF.
    fn export(&mut self, cx: &mut Context<Self>) {
        if !self.panels.iter().any(|panel| panel.read(cx).has_result()) {
            self.notice = Some(Feedback::new(
                Tone::Info,
                tr!("Run the report before exporting it."),
            ));
            cx.notify();
            return;
        }
        let title = self.title(cx);
        let description = self.description.read(cx).value().to_string();
        let html = {
            let app: &App = cx;
            let connections = self
                .connections(app)
                .into_iter()
                .map(|connection| {
                    find_source(&self.language.sources, &connection)
                        .map_or(connection.name, |source| source.connection.name.clone())
                })
                .collect();
            export::html(&export::Document {
                title: &title,
                description: &description,
                connections,
                panels: self
                    .panels
                    .iter()
                    .map(|panel| panel.read(app).print_parts())
                    .collect(),
                generated: export::now_utc(),
            })
        };
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

    /// **Add a panel**: a menu of the connections, each with its
    /// databases; the new panel runs where it is chosen.
    fn add_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let sources = self.language.sources.clone();
        let pick: PickSource = Rc::new(move |connection, context, window, cx| {
            let _ = view.update(cx, |this, cx| {
                // A full report says so in its notice bar.
                let _ = this.add_panel(String::new(), connection, context, window, cx);
            });
        });
        Button::new("report-add-panel")
            .ghost()
            .small()
            .icon(I::Plus)
            .dropdown_caret(true)
            .accessibility_label(tr!("Add a panel"))
            .tooltip(tr!("Add a panel"))
            .dropdown_menu(move |menu, window, cx| {
                let menu = menu.label(tr!("Where the new panel runs"));
                source_menu(menu, window, cx, &sources, None, pick.clone())
            })
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        let editing = self.mode == ReportMode::Edit;
        let running = self.scheduler.is_busy();
        row()
            .h(px(48.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(rgb(colors().border))
            .child(icon(I::FileChartColumn, colors().mint))
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
            .child(self.add_menu(cx))
            .child(
                icon_button(
                    "report-save",
                    if self.unsaved {
                        I::Save
                    } else {
                        I::BookmarkCheck
                    },
                    if self.unsaved {
                        tr!("Save report")
                    } else {
                        tr!("Saved in the reports")
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
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
                    .on_click(cx.listener(|this, _, _, cx| this.run(RunScope::All, cx)))
            })
    }

    fn definition_panel(&self, cx: &mut Context<Self>) -> Div {
        column()
            .flex_none()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(rgb(colors().border))
            .child(
                Input::new(&self.description)
                    .small()
                    .aria_label(tr!("Report description")),
            )
            .when(!self.spec_rows.is_empty(), |s| {
                s.child(label(tr!("Parameters shared by the panels")))
                    .children(
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
        let invalid = params::invalid_choices(&spec);
        column()
            .gap_1()
            .child(
                row()
                    .gap_2()
                    .child(
                        div()
                            .w(px(120.))
                            .font_family(fonts().code.clone())
                            .text_size(px(13.))
                            .text_color(rgb(colors().sql_parameter))
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
                            if let Some(spec) =
                                this.report.parameters.iter_mut().find(|s| s.name == name)
                            {
                                spec.required = !spec.required;
                            }
                            this.changed(cx);
                        }))
                    })
                    .child({
                        let name = spec.name.clone();
                        button(("parameter-list", index), I::List, tr!("List of choices"))
                            .aria_selected(row_fields.choices_open || !spec.choices.is_empty())
                            .when(!spec.choices.is_empty(), |s| {
                                s.child(badge(spec.choices.len().to_string(), colors().mint))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(row) =
                                    this.spec_rows.iter_mut().find(|row| row.name == name)
                                {
                                    row.choices_open = !row.choices_open;
                                }
                                cx.notify();
                            }))
                    }),
            )
            .when(row_fields.choices_open, |s| {
                let name = spec.name.clone();
                s.child(
                    row()
                        .id(("parameter-choices", index))
                        .pl(px(128.))
                        .flex_wrap()
                        .gap_1()
                        .children(spec.choices.iter().enumerate().map(|(position, choice)| {
                            let name = name.clone();
                            let bad = invalid.contains(&position);
                            row()
                                .id(("choice", position))
                                .gap_1()
                                .pl_2()
                                .rounded(px(4.))
                                .border_1()
                                .border_color(rgb(if bad { colors().red } else { colors().border }))
                                .text_size(px(12.))
                                .child(choice.clone())
                                .child(
                                    icon_button("remove-choice", I::X, tr!("Remove the choice"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.remove_choice(&name, position, cx)
                                        })),
                                )
                        }))
                        .child(
                            div().w(px(180.)).child(
                                Input::new(&row_fields.choice)
                                    .small()
                                    .aria_label(tr!("A new choice of {0}", spec.name)),
                            ),
                        )
                        .child({
                            let name = spec.name.clone();
                            button(("add-choice", index), I::Plus, tr!("Add")).on_click(
                                cx.listener(move |this, _, window, cx| {
                                    this.add_choice(&name, window, cx)
                                }),
                            )
                        })
                        .when(!invalid.is_empty(), |s| {
                            s.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors().red))
                                    .child(invalid_hint(spec.kind)),
                            )
                        }),
                )
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
        self.changed(cx);
    }

    fn parameter_form(&self, cx: &mut Context<Self>) -> Div {
        row()
            .flex_none()
            .flex_wrap()
            .items_end()
            .gap_3()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(colors().border))
            .children(self.values.iter().enumerate().filter_map(|(index, field)| {
                let spec = self
                    .report
                    .parameters
                    .iter()
                    .find(|spec| spec.name == field.name)?;
                let problem = self.errors.get(&field.name);
                let control = if spec.choices.is_empty() {
                    Input::new(&field.input)
                        .small()
                        .aria_label(spec.shown_label().to_owned())
                        .into_any_element()
                } else {
                    self.choice_menu(index, spec, field, cx).into_any_element()
                };
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
                                .border_color(rgb(if problem.is_some() {
                                    colors().red
                                } else {
                                    colors().border
                                }))
                                .child(control),
                        )
                        .when_some(problem, |s, problem| {
                            s.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors().red))
                                    .child(match problem {
                                        ParameterProblem::Missing => tr!("Required"),
                                        ParameterProblem::Invalid => invalid_hint(spec.kind),
                                        ParameterProblem::NotAChoice => {
                                            tr!("Choose one of the list")
                                        }
                                    }),
                            )
                        }),
                )
            }))
    }

    /// A list parameter's value: one of its choices, or none when optional.
    fn choice_menu(
        &self,
        index: usize,
        spec: &jykre::reports::ParameterSpec,
        field: &ValueField,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current = field.input.read(cx).value().to_string();
        let shown = if current.is_empty() {
            tr!("Choose").to_owned()
        } else {
            current.clone()
        };
        let view = cx.entity().downgrade();
        let input = field.input.clone();
        let choices = spec.choices.clone();
        let required = spec.required;
        Button::new(("parameter-value", index))
            .ghost()
            .small()
            .w_full()
            .label(shown)
            .dropdown_caret(true)
            .accessibility_label(spec.shown_label().to_owned())
            .dropdown_menu(move |mut menu, _, _| {
                let pick = |value: String| {
                    let view = view.clone();
                    let input = input.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        input.update(cx, |input, cx| input.set_value(value.clone(), window, cx));
                        let _ = view.update(cx, |this, cx| {
                            this.errors.clear();
                            cx.notify();
                        });
                    }
                };
                if !required {
                    menu = menu.item(
                        PopupMenuItem::new(tr!("No value"))
                            .checked(current.is_empty())
                            .on_click(pick(String::new())),
                    );
                }
                for choice in &choices {
                    menu = menu.item(
                        PopupMenuItem::new(choice.clone())
                            .checked(*choice == current)
                            .on_click(pick(choice.clone())),
                    );
                }
                menu
            })
    }

    /// The panels in two columns: a half-width panel shares its row.
    fn panels_grid(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.panels.is_empty() {
            return div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(14.))
                .text_color(rgb(colors().muted))
                .child(tr!(
                    "Add a panel with the + button: each has its SQL, its database and its chart."
                ))
                .into_any_element();
        }
        // The panels are the scrolled element's own children, so the one
        // opened for editing can be scrolled into view by its position. A
        // panel dragged by its grip is dropped on another to take its place,
        // or after the last one.
        let owner = cx.entity_id();
        let accepts = move |value: &dyn std::any::Any, _: &mut Window, _: &mut App| {
            value
                .downcast_ref::<PanelDrag>()
                .is_some_and(|drag| drag.owner == owner)
        };
        let count = self.panels.len();
        div()
            .id("report-panels")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_wrap()
            .content_start()
            .items_start()
            .p_2()
            .children(self.panels.iter().enumerate().map(|(index, panel)| {
                let (width, height) = {
                    let panel = panel.read(cx);
                    (panel.width(), panel.height())
                };
                div()
                    .id(("panel-slot", index))
                    .w(match width {
                        Width::Half => relative(0.5),
                        Width::Full => relative(1.),
                    })
                    .h(px(height))
                    .p_1()
                    .rounded(px(10.))
                    .border_2()
                    .border_color(transparent_black())
                    .drag_over::<PanelDrag>(|style, _, _, _| style.border_color(rgb(colors().mint)))
                    .can_drop(accepts)
                    .on_drop(cx.listener(move |this, drag: &PanelDrag, _, cx| {
                        this.drop_panel(drag.panel, index, cx)
                    }))
                    .child(panel.clone().cached(StyleRefinement::default().size_full()))
            }))
            .when(cx.has_active_drag(), |s| {
                s.child(
                    div()
                        .id("panel-slot-end")
                        .w_full()
                        .h(px(56.))
                        .m_1()
                        .rounded(px(8.))
                        .border_2()
                        .border_dashed()
                        .border_color(rgb(colors().border))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.))
                        .text_color(rgb(colors().faint))
                        .child(tr!("Drop here to move the panel to the end"))
                        .drag_over::<PanelDrag>(|style, _, _, _| {
                            style.border_color(rgb(colors().mint))
                        })
                        .can_drop(accepts)
                        .on_drop(cx.listener(move |this, drag: &PanelDrag, _, cx| {
                            this.drop_panel(drag.panel, count, cx)
                        })),
                )
            })
            .into_any_element()
    }

    fn footer(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let mut running = 0;
        let mut failed = 0;
        for panel in &self.panels {
            match panel.read(cx).phase() {
                PanelPhase::Queued | PanelPhase::Running => running += 1,
                PanelPhase::Failed => failed += 1,
                _ => {}
            }
        }
        let mut parts = vec![if self.panels.len() == 1 {
            tr!("1 panel").to_owned()
        } else {
            tr!("{0} panels", self.panels.len())
        }];
        if running > 0 {
            parts.push(tr!("{0} running", running));
        }
        if failed > 0 {
            parts.push(tr!("{0} failed", failed));
        }
        row()
            .id("report-footer")
            .role(Role::Status)
            .h(px(30.))
            .flex_none()
            .px_3()
            .border_t_1()
            .border_color(rgb(colors().border))
            .child(label(parts.join(" · ")))
            .when(self.mode == ReportMode::Edit, |s| {
                s.child(div().flex_1()).child(label(tr!(
                    "Ctrl Enter in a panel's SQL runs that panel; elsewhere, the whole report."
                )))
            })
    }
}

impl Render for ReportView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        column()
            .id("report")
            .size_full()
            .bg(rgb(colors().bg))
            .child(self.toolbar(cx))
            .when_some(self.notice.clone(), |s, notice| {
                s.child(div().px_3().py_2().child(feedback_line(&notice)))
            })
            .when(self.mode == ReportMode::Edit, |s| {
                s.child(self.definition_panel(cx))
            })
            .when(!self.values.is_empty(), |s| {
                s.child(self.parameter_form(cx))
            })
            .child(self.panels_grid(cx))
            .child(self.footer(cx))
    }
}

impl Drop for ReportView {
    fn drop(&mut self) {
        self.scheduler.reset();
        for session in self.sessions.clear() {
            backend::close_in_background(session);
        }
    }
}

pub(super) fn kind_name(kind: ParameterKind) -> &'static str {
    match kind {
        ParameterKind::Text => tr!("Text"),
        ParameterKind::Number => tr!("Number"),
        ParameterKind::Boolean => tr!("True or false"),
        ParameterKind::Date => tr!("Date"),
        ParameterKind::Timestamp => tr!("Date and time"),
    }
}

pub(super) fn placeholder(kind: ParameterKind) -> &'static str {
    match kind {
        ParameterKind::Text => tr!("Text"),
        ParameterKind::Number => tr!("e.g. 1250.50"),
        ParameterKind::Boolean => tr!("true or false"),
        ParameterKind::Date => tr!("YYYY-MM-DD"),
        ParameterKind::Timestamp => tr!("YYYY-MM-DD HH:MM:SS"),
    }
}

pub(super) fn invalid_hint(kind: ParameterKind) -> &'static str {
    match kind {
        ParameterKind::Text => tr!("Invalid text"),
        ParameterKind::Number => tr!("A number, with a dot for decimals"),
        ParameterKind::Boolean => tr!("true or false"),
        ParameterKind::Date => tr!("A date as YYYY-MM-DD"),
        ParameterKind::Timestamp => tr!("A date and time as YYYY-MM-DD HH:MM"),
    }
}
