use jykre::tr;
mod connections;
mod demo;
mod layout;
mod probe;
mod result_grid;
mod sql;
mod state;
mod ui;
mod workspace;

use demo::{ORDER_COLUMNS, SALES_SQL, TABLES, table_sql};
use gpui_kit::assets::IconName as I;
use gpui_kit::component::{
    ActiveTheme, Sizable, Theme, ThemeMode, TitleBar, WindowExt as _,
    input::{Input, InputEvent, InputState},
    resizable::{ResizableState, h_resizable, resizable_panel},
    scroll::ScrollableElement,
    tooltip::Tooltip,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::grouped_tabs::{GroupedTabs, GroupedTabsEvent, TabGroupId, TabId};
use jykre::{backend, connection_store, drivers::*, result::DEFAULT_ROW_LIMIT};
use jykre_secrets::SystemSecretStore;
use jykre_storage::{ConfigRepository, RefreshStatus};
use layout::{ASSISTANT_WIDTH, EXPLORER_WIDTH, splitter};
use std::collections::HashMap;
use std::collections::HashSet;
use ui::*;
use workspace::{Workspace, WorkspaceEvent};

/// Asks the user to confirm an action in a dialog drawn with the
/// application's theme; `on_ok` runs only when they confirm.
fn confirm(
    window: &mut Window,
    cx: &mut App,
    title: String,
    description: String,
    ok: &'static str,
    cancel: &'static str,
    on_ok: impl Fn(&mut App) + 'static,
) {
    let on_ok = std::rc::Rc::new(on_ok);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_ok = on_ok.clone();
        alert
            .title(title.clone())
            .description(description.clone())
            .confirm()
            .ok_text(ok)
            .cancel_text(cancel)
            .width(px(440.))
            .on_ok(move |_, _, cx| {
                on_ok(cx);
                true
            })
    });
}

actions!(
    jykre,
    [
        /// Ctrl+Enter: the selection, or the statement at the cursor, in place
        /// of the last result tab created.
        ExecuteCurrentQuery,
        /// Ctrl+Shift+Enter: the same, in a new result tab.
        ExecuteCurrentQueryInNewResult,
        /// Ctrl+Alt+Enter: every statement in the tab, one result tab each.
        ExecuteAllQueries,
        /// Ctrl+W or Ctrl+F4: closes the query tab on screen, asking first
        /// when it is running or has SQL that is not saved.
        CloseActiveTab
    ]
);

#[derive(Clone, Copy, PartialEq)]
enum Section {
    Explorer,
    Saved,
    History,
    Settings,
}

struct Connection {
    name: String,
    profile: Option<ConnectionProfile>,
    metadata_statuses: Vec<RefreshStatus>,
    metadata_version: i64,
    metadata_pages: HashMap<ObjectPath, u32>,
    metadata_more: HashSet<ObjectPath>,
    cache_loading: bool,
    search_results: Vec<MetadataObject>,
    refresh_generation: usize,
    detail: String,
    color: u32,
    group: TabGroupId,
    expanded: bool,
    input: Option<ResolvedConnection>,
    /// Rows kept per statement in this connection's tabs.
    row_limit: u32,
    metadata: Vec<MetadataObject>,
    collapsed: HashSet<ObjectPath>,
    /// Scope being refreshed and how to cancel it.
    refreshing: Option<(ObjectPath, CancellationToken)>,
    status: Feedback,
    notice: Option<String>,
}

#[derive(Clone)]
struct QuerySnapshot {
    title: String,
    connection: String,
    sql: String,
}

struct AppView {
    tabs: Entity<GroupedTabs>,
    panels: HashMap<TabId, Entity<Workspace>>,
    explorer_split: Entity<ResizableState>,
    assistant_split: Entity<ResizableState>,
    saved_queries: Vec<QuerySnapshot>,
    history: Vec<QuerySnapshot>,
    connections: Vec<Connection>,
    section: Section,
    next_query: usize,
    sidebar_open: bool,
    schema_open: bool,
    tables_open: bool,
    orders_open: bool,
    explorer_scroll: ScrollHandle,
    search: Entity<InputState>,
    command: Entity<InputState>,
    command_open: bool,
    connection_open: bool,
    connection_focus: FocusHandle,
    connection_name: Entity<InputState>,
    connection_host: Entity<InputState>,
    connection_database: Entity<InputState>,
    connection_user: Entity<InputState>,
    driver: usize,
    connection_port: Entity<InputState>,
    connection_row_limit: Entity<InputState>,
    connection_password: Entity<InputState>,
    connection_ssl: Entity<InputState>,
    connection_profile: Entity<InputState>,
    connection_warehouse: Entity<InputState>,
    connection_status: Option<Feedback>,
    /// Application-level problem (loading connections, saving preferences,
    /// searching) shown in the explorer until dismissed.
    app_notice: Option<Feedback>,
    command_searching: bool,
    remember_password: bool,
    connection_saving: bool,
    connections_loading: bool,
    search_revision: usize,
    command_revision: usize,
    command_matches: Vec<(usize, String, String)>,
    connection_testing: Option<CancellationToken>,
    connection_revision: usize,
    editing_connection: Option<usize>,
    /// The open tabs, saved queries and history are written to `config.db`
    /// once the last session's were restored (never in the demo).
    persist_state: bool,
    /// Bumped by every change, so only the last of a burst is written.
    state_revision: usize,
    /// Groups for restored tabs whose connection no longer exists, by the
    /// connection's name.
    orphan_groups: Vec<(String, TabGroupId)>,
    _subscriptions: Vec<Subscription>,
}

impl AppView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tabs = cx.new(|cx| {
            let mut tabs = GroupedTabs::new(cx);
            // Closing a busy tab or moving one to another connection asks first.
            tabs.set_confirm_changes(true);
            tabs
        });
        let explorer_split = cx.new(|_| ResizableState::default());
        let assistant_split = cx.new(|_| ResizableState::default());
        let explorer_resize = cx.observe(&explorer_split, |_, _, cx| cx.notify());
        let assistant_resize = cx.observe(&assistant_split, |_, _, cx| cx.notify());
        let tab_subscription =
            cx.subscribe_in(&tabs, window, |this, _, event, window, cx| match event {
                GroupedTabsEvent::NewTabRequested(group) => this.new_query(*group, window, cx),
                GroupedTabsEvent::TabClosed(tab) => {
                    this.panels.remove(tab);
                    this.save_state_soon(cx);
                }
                GroupedTabsEvent::SelectionChanged(_) => this.save_state_soon(cx),
                GroupedTabsEvent::CloseRequested(tab) => this.request_close(*tab, window, cx),
                GroupedTabsEvent::MoveRequested { tab, group, index } => {
                    this.request_move(*tab, *group, *index, window, cx)
                }
                GroupedTabsEvent::TabMoved { tab, from, to, .. } if from != to => {
                    if let (Some(panel), Some(connection)) = (
                        this.panels.get(tab),
                        this.connections
                            .iter()
                            .find(|connection| connection.group == *to),
                    ) {
                        panel.update(cx, |panel, cx| {
                            panel.rebind_connection(
                                connection.name.clone(),
                                connection.input.clone(),
                                connection.row_limit,
                                cx,
                            )
                        });
                    }
                    this.save_state_soon(cx);
                }
                _ => {}
            });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Search objects…")));
        let command = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr!("Search tables in the workspace…"))
        });
        let search_subscription = cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.search_cache(false, cx);
            }
            cx.notify();
        });
        let command_subscription = cx.subscribe_in(
            &command,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.search_cache(true, cx);
                }
                if let InputEvent::PressEnter { .. } = event {
                    let _ = input;
                    if let Some((index, name, sql)) = this.table_matches(cx).into_iter().next() {
                        this.add_query(this.connections[index].group, &name, &sql, window, cx);
                        this.command_open = false;
                    }
                }
                cx.notify();
            },
        );
        let connection_name =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("e.g. PostgreSQL · Staging")));
        let connection_host = cx.new(|cx| InputState::new(window, cx).default_value("localhost"));
        let connection_database =
            cx.new(|cx| InputState::new(window, cx).default_value("commerce"));
        let connection_user = cx.new(|cx| InputState::new(window, cx).default_value("postgres"));
        let mut app = Self {
            tabs,
            explorer_split,
            assistant_split,
            panels: HashMap::new(),
            saved_queries: vec![QuerySnapshot {
                title: tr!("Active customers").into(),
                connection: tr!("PostgreSQL · Local").into(),
                sql: table_sql("customers"),
            }],
            history: vec![QuerySnapshot {
                title: tr!("Revenue by category").into(),
                connection: tr!("PostgreSQL · Local").into(),
                sql: SALES_SQL.into(),
            }],
            connections: Vec::new(),
            section: Section::Explorer,
            next_query: 1,
            sidebar_open: true,
            schema_open: true,
            tables_open: true,
            orders_open: true,
            explorer_scroll: ScrollHandle::new(),
            search,
            command,
            command_open: false,
            connection_open: false,
            connection_focus: cx.focus_handle(),
            connection_name,
            connection_host,
            connection_database,
            connection_user,
            driver: 0,
            connection_port: cx.new(|cx| InputState::new(window, cx).default_value("5432")),
            connection_row_limit: cx
                .new(|cx| InputState::new(window, cx).default_value(DEFAULT_ROW_LIMIT.to_string())),
            connection_password: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(tr!("Enter the password"))
            }),
            connection_ssl: cx.new(|cx| InputState::new(window, cx).default_value("prefer")),
            connection_profile: cx.new(|cx| InputState::new(window, cx).default_value("DEFAULT")),
            connection_warehouse: cx
                .new(|cx| InputState::new(window, cx).placeholder(tr!("Optional for metadata"))),
            connection_status: None,
            app_notice: None,
            command_searching: false,
            remember_password: false,
            connection_saving: false,
            connections_loading: true,
            search_revision: 0,
            command_revision: 0,
            command_matches: Vec::new(),
            connection_testing: None,
            connection_revision: 0,
            editing_connection: None,
            persist_state: false,
            state_revision: 0,
            orphan_groups: Vec::new(),
            _subscriptions: vec![
                // The last edits are written before the application exits.
                cx.on_app_quit(|this, cx| this.save_state_now(cx)),
                tab_subscription,
                search_subscription,
                command_subscription,
                explorer_resize,
                assistant_resize,
            ],
        };
        for field in [
            &app.connection_name,
            &app.connection_host,
            &app.connection_port,
            &app.connection_row_limit,
            &app.connection_database,
            &app.connection_user,
            &app.connection_password,
            &app.connection_ssl,
            &app.connection_profile,
            &app.connection_warehouse,
        ] {
            let subscription =
                cx.subscribe_in(field, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::Change => {
                            this.invalidate_connection_test();
                            cx.notify();
                        }
                        // Enter in any field submits the form, as in other dialogs.
                        InputEvent::PressEnter { .. } if this.connection_open => {
                            this.save_connection(window, cx);
                        }
                        _ => {}
                    }
                });
            app._subscriptions.push(subscription);
        }
        if std::env::var_os("JYKRE_DEMO").is_some() {
            for (name, detail, color) in [
                (tr!("PostgreSQL · Local"), "localhost:5432", MINT),
                ("Databricks · Dev", "analytics / development", BLUE),
                (tr!("MySQL · Finance"), "finance / development", 0xd995a7),
            ] {
                let group = app
                    .tabs
                    .update(cx, |tabs, cx| tabs.add_group(name, rgb(color).into(), cx));
                app.connections.push(Connection {
                    name: name.into(),
                    profile: None,
                    metadata_statuses: Vec::new(),
                    metadata_version: 0,
                    metadata_pages: HashMap::new(),
                    metadata_more: HashSet::new(),
                    cache_loading: false,
                    search_results: Vec::new(),
                    refresh_generation: 0,
                    detail: detail.into(),
                    color,
                    group,
                    expanded: app.connections.is_empty(),
                    input: None,
                    row_limit: DEFAULT_ROW_LIMIT,
                    metadata: Vec::new(),
                    collapsed: HashSet::new(),
                    refreshing: None,
                    status: Feedback::new(Tone::Info, tr!("Demo · simulated data")),
                    notice: None,
                });
            }
            let first = app.connections[0].group;
            app.add_query(first, tr!("Revenue by category"), SALES_SQL, window, cx);
            app.add_query(first, tr!("Customers"), &table_sql("customers"), window, cx);
            let second = app.connections[1].group;
            app.add_query(second, tr!("Daily events"), "-- Events per day\nSELECT DATE(event_time) AS day,\n       COUNT(*) AS events\nFROM analytics.events\nGROUP BY DATE(event_time)\nORDER BY day DESC;", window, cx);
            let third = app.connections[2].group;
            app.add_query(third, tr!("Financial summary"), "-- Monthly financial summary\nSELECT DATE_FORMAT(created_at, '%Y-%m') AS month,\n       SUM(amount) AS total\nFROM transactions\nGROUP BY month\nORDER BY month DESC;", window, cx);
            let first_tab = app.tabs.read(cx).tab_ids().next();
            if let Some(first_tab) = first_tab {
                // The tab was added just above, so selecting it cannot fail.
                let _ = app
                    .tabs
                    .update(cx, |tabs, cx| tabs.select_tab(first_tab, cx));
            }
        } else {
            app.saved_queries.clear();
            app.history.clear();
        }
        app.load_connections(window, cx);
        app
    }

    fn add_query(
        &mut self,
        group: TabGroupId,
        title: &str,
        sql: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<TabId> {
        let group = match self.connections.iter().position(|c| c.group == group) {
            Some(index) => self.ensure_group(index, cx),
            None => group,
        };
        let connection = self
            .connections
            .iter()
            .find(|c| c.group == group)
            .map(|c| c.name.as_str())
            .unwrap_or(tr!("Workspace"));
        let panel = cx.new(|cx| {
            Workspace::new(
                title,
                connection,
                sql,
                self.assistant_split.clone(),
                window,
                cx,
            )
        });
        if let Some((input, row_limit)) = self
            .connections
            .iter()
            .find(|connection| connection.group == group)
            .and_then(|connection| Some((connection.input.clone()?, connection.row_limit)))
        {
            panel.update(cx, |panel, cx| panel.set_connection(input, row_limit, cx));
        }
        let subscription = cx.subscribe_in(
            &panel,
            window,
            |this, panel_entity, event: &WorkspaceEvent, window, cx| {
                match event {
                    WorkspaceEvent::Saved {
                        saved,
                        title,
                        connection,
                        sql,
                    } => {
                        this.saved_queries.retain(|query| {
                            query.title != *title || query.connection != *connection
                        });
                        if *saved {
                            this.saved_queries.push(QuerySnapshot {
                                title: title.clone(),
                                connection: connection.clone(),
                                sql: sql.clone(),
                            });
                        }
                    }
                    WorkspaceEvent::Executed {
                        title,
                        connection,
                        sql,
                    } => {
                        state::record(
                            &mut this.history,
                            QuerySnapshot {
                                title: title.clone(),
                                connection: connection.clone(),
                                sql: sql.clone(),
                            },
                        );
                    }
                    WorkspaceEvent::Changed => {}
                    WorkspaceEvent::ConnectionRequested(name) => {
                        let name = name.clone();
                        if let Some(tab) = this.tab_of(panel_entity)
                            && let Some(index) =
                                this.connections.iter().position(|c| c.name == name)
                        {
                            let group = this.ensure_group(index, cx);
                            this.request_move(tab, group, usize::MAX, window, cx);
                        }
                    }
                }
                this.save_state_soon(cx);
                cx.notify();
            },
        );
        self._subscriptions.push(subscription);
        // The group exists: it was recreated above when it had been closed.
        let Ok(tab) = self.tabs.update(cx, |tabs, cx| {
            tabs.add_tab(group, title.to_string(), panel.clone(), cx)
        }) else {
            tracing::error!("could not add a query tab: its group does not exist");
            return None;
        };
        self.panels.insert(tab, panel);
        self.refresh_connection_choices(cx);
        self.save_state_soon(cx);
        cx.notify();
        Some(tab)
    }

    fn new_query(&mut self, group: TabGroupId, window: &mut Window, cx: &mut Context<Self>) {
        let title = tr!("Query {0}", self.next_query);
        self.next_query += 1;
        let sql = if self
            .connections
            .iter()
            .any(|connection| connection.group == group && connection.input.is_some())
        {
            "SELECT 1;"
        } else {
            "SELECT * FROM orders LIMIT 100;"
        };
        // The new tab takes the keyboard, so its shortcuts (run, close) work
        // at once.
        if let Some(tab) = self.add_query(group, &title, sql, window, cx)
            && let Some(panel) = self.panels.get(&tab)
        {
            panel.update(cx, |panel, cx| panel.focus_editor(window, cx));
        }
    }

    /// The tab group of the connection at `index`. Connections outlive their
    /// tab groups, so the group is recreated when its last tab was closed or
    /// moved away.
    fn ensure_group(&mut self, index: usize, cx: &mut Context<Self>) -> TabGroupId {
        let group = self.connections[index].group;
        if self.tabs.read(cx).group_ids().any(|id| id == group) {
            return group;
        }
        let connection = &mut self.connections[index];
        let group = self.tabs.update(cx, |tabs, cx| {
            tabs.add_group(connection.name.clone(), rgb(connection.color).into(), cx)
        });
        connection.group = group;
        group
    }

    fn tab_of(&self, panel: &Entity<Workspace>) -> Option<TabId> {
        self.panels
            .iter()
            .find(|(_, candidate)| *candidate == panel)
            .map(|(tab, _)| *tab)
    }

    /// Offers every query tab the connections it can switch to.
    pub(super) fn refresh_connection_choices(&self, cx: &mut Context<Self>) {
        let names: Vec<String> = self.connections.iter().map(|c| c.name.clone()).collect();
        for panel in self.panels.values() {
            panel.update(cx, |panel, cx| {
                panel.set_connection_choices(names.clone(), cx)
            });
        }
    }

    /// Closes a tab, asking first when a query is running in it (closing
    /// cancels it) or when its SQL is not saved (closing loses it).
    fn request_close(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let (busy, unsaved) = self.panels.get(&tab).map_or((false, false), |panel| {
            let panel = panel.read(cx);
            (panel.is_busy(), panel.has_unsaved_sql())
        });
        let tabs = self.tabs.downgrade();
        let close = move |window: Option<&mut Window>, cx: &mut App| {
            let _ = tabs.update(cx, |tabs, cx| {
                let _ = tabs.close_tab(tab, cx);
                if let Some(window) = window {
                    tabs.focus_active(window, cx);
                }
            });
        };
        let (title, description, ok) = match (busy, unsaved) {
            (false, false) => {
                close(Some(window), cx);
                return;
            }
            (true, false) => (
                tr!("A query is running in this tab.").to_string(),
                tr!("Closing the tab cancels the query.").to_string(),
                tr!("Close and cancel"),
            ),
            (false, true) => (
                tr!("Close the tab without saving its SQL?").to_string(),
                tr!("The SQL is not in the saved queries, so closing the tab loses it.")
                    .to_string(),
                tr!("Close without saving"),
            ),
            (true, true) => (
                tr!("A query is running in this tab.").to_string(),
                tr!("Closing the tab cancels the query and loses its SQL, which is not in the saved queries.")
                    .to_string(),
                tr!("Close and cancel"),
            ),
        };
        confirm(
            window,
            cx,
            title,
            description,
            ok,
            tr!("Keep the tab"),
            move |cx| close(None, cx),
        );
    }

    /// Moves a tab, asking first when the move changes the connection it
    /// runs on (its results and session belong to the previous one).
    fn request_move(
        &mut self,
        tab: TabId,
        group: TabGroupId,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same_group = self.tabs.read(cx).group_of(tab) == Some(group);
        let Some(panel) = self.panels.get(&tab) else {
            return;
        };
        let (from, busy) = {
            let panel = panel.read(cx);
            (panel.connection_name().to_owned(), panel.is_busy())
        };
        let to = self
            .connections
            .iter()
            .find(|connection| connection.group == group)
            .map(|connection| connection.name.clone());
        if same_group || to.as_deref() == Some(from.as_str()) {
            let _ = self
                .tabs
                .update(cx, |tabs, cx| tabs.move_tab(tab, group, index, cx));
            return;
        }
        let to = to.unwrap_or_else(|| tr!("Without connection").to_owned());
        let message = tr!("Run this tab on {0}?", to);
        let detail = if busy {
            tr!(
                "The tab moves from {0} to {1}. Its SQL stays; the running query is cancelled and the results and session on {0} are closed.",
                from,
                to
            )
        } else {
            tr!(
                "The tab moves from {0} to {1}. Its SQL stays; the results and session on {0} are closed.",
                from,
                to
            )
        };
        let tabs = self.tabs.downgrade();
        confirm(
            window,
            cx,
            message,
            detail,
            tr!("Switch connection"),
            tr!("Cancel"),
            move |cx| {
                let _ = tabs.update(cx, |tabs, cx| tabs.move_tab(tab, group, index, cx));
            },
        );
    }

    /// Runs `action` on the active tab's workspace, unless a dialog or the
    /// command palette is in front of it.
    fn with_active_panel(
        &mut self,
        cx: &mut Context<Self>,
        action: fn(&mut Workspace, &mut Context<Workspace>),
    ) {
        if self.connection_open || self.command_open {
            return;
        }
        if let Some(panel) = self
            .tabs
            .read(cx)
            .active_tab()
            .and_then(|tab| self.panels.get(&tab))
            .cloned()
        {
            panel.update(cx, action);
        }
    }

    fn new_active_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group) = self.active_group(cx) {
            self.new_query(group, window, cx);
        } else {
            self.open_connection(None, window, cx);
        }
    }

    fn active_group(&self, cx: &App) -> Option<TabGroupId> {
        let tabs = self.tabs.read(cx);
        tabs.active_tab()
            .and_then(|tab| tabs.tab_group(tab))
            .or_else(|| self.connections.first().map(|connection| connection.group))
    }

    fn open_table(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.add_query(
            self.connections[0].group,
            name,
            &table_sql(name),
            window,
            cx,
        );
        self.command_open = false;
    }

    fn rail_item(
        &self,
        index: usize,
        section: Section,
        name: I,
        title: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let active = self.section == section && self.sidebar_open;
        div()
            .id(("rail-item", index))
            .role(Role::Button)
            .aria_label(title)
            .aria_selected(active)
            .focusable()
            .tab_index(0)
            .focus_visible(|s| s.bg(rgb(HOVER)))
            .tooltip(move |window, cx| Tooltip::new(title).build(window, cx))
            .size(px(34.))
            .flex_none()
            .rounded(px(7.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(active, |s| s.bg(rgb(0x26342f)))
            .hover(|s| s.bg(rgb(HOVER)))
            .child(icon(name, if active { MINT } else { FAINT }))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.section == section {
                    this.sidebar_open = !this.sidebar_open;
                } else {
                    this.section = section;
                    this.sidebar_open = true;
                }
                cx.notify();
            }))
    }

    fn rail(&self, cx: &mut Context<Self>) -> Div {
        column()
            .w(px(48.))
            .flex_none()
            .h_full()
            .items_center()
            .py_3()
            .gap_2()
            .bg(rgb(0x13161a))
            .border_r_1()
            .border_color(rgb(BORDER))
            .children(
                [
                    (Section::Explorer, I::Database, tr!("Explorer")),
                    (Section::Saved, I::Bookmark, tr!("Saved queries")),
                    (Section::History, I::Clock, tr!("History")),
                ]
                .into_iter()
                .enumerate()
                .map(|(i, (section, name, title))| self.rail_item(i, section, name, title, cx))
                .collect::<Vec<_>>(),
            )
            .child(div().flex_1())
            .child(self.rail_item(3, Section::Settings, I::Settings2, tr!("Preferences"), cx))
            .child(
                div()
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(rgb(0x303c36))
                    .text_color(rgb(MINT))
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("AL"),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> Div {
        let title = match self.section {
            Section::Explorer => tr!("Explorer"),
            Section::Saved => tr!("Saved queries"),
            Section::History => tr!("History"),
            Section::Settings => tr!("Preferences"),
        };
        column()
            .size_full()
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                row()
                    .h(px(48.))
                    .flex_none()
                    .px_4()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        row()
                            .gap_0()
                            .when(self.section == Section::Explorer, |s| {
                                s.child(
                                    icon_button("add-connection", I::Plus, tr!("New connection"))
                                        .when(self.connections_loading, |s| {
                                            s.opacity(0.5).cursor_default()
                                        })
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_connection(None, window, cx)
                                        })),
                                )
                            })
                            .child(
                                icon_button(
                                    "collapse-sidebar",
                                    I::PanelLeftClose,
                                    tr!("Collapse sidebar"),
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.sidebar_open = false;
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
            .when(self.section == Section::Explorer, |s| {
                s.child(
                    div().px_3().pb_3().child(
                        Input::new(&self.search)
                            .small()
                            .prefix(I::Search)
                            .w_full()
                            .aria_label(tr!("Search objects in the explorer")),
                    ),
                )
                .child(
                    row()
                        .px_4()
                        .h(px(26.))
                        .flex_none()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(FAINT))
                                .child(tr!("Connections")),
                        )
                        .child(badge(self.connections.len().to_string(), FAINT)),
                )
                .when_some(self.app_notice.as_ref(), |s, notice| {
                    s.child(
                        row()
                            .id("app-notice")
                            .role(if notice.tone == Tone::Error {
                                Role::Alert
                            } else {
                                Role::Status
                            })
                            .aria_label(notice.text.clone())
                            .mx_3()
                            .mb_2()
                            .p_2()
                            .items_start()
                            .rounded(px(6.))
                            .bg(rgb(notice.tone.background()))
                            .child(feedback_line(notice).flex_1().text_size(px(12.)))
                            .child(
                                icon_button("dismiss-app-notice", I::X, tr!("Close notice"))
                                    .size(px(24.))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.app_notice = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                })
                .child(
                    column()
                        .id("connection-tree")
                        .flex_1()
                        .overflow_y_scroll()
                        .track_scroll(&self.explorer_scroll)
                        .vertical_scrollbar(&self.explorer_scroll)
                        .pb_3()
                        .when(self.connections_loading, |s| {
                            s.child(
                                row()
                                    .px_4()
                                    .py_3()
                                    .child(icon(I::LoaderCircle, FAINT))
                                    .child(label(tr!("Loading connections…"))),
                            )
                        })
                        .when(
                            !self.connections_loading && self.connections.is_empty(),
                            |s| {
                                s.child(
                                    column()
                                        .px_4()
                                        .py_3()
                                        .gap_2()
                                        .child(label(tr!(
                                            "No connections yet. Add one to explore its metadata and run SQL."
                                        )))
                                        .child(
                                            button(
                                                "explorer-add-connection",
                                                I::Plus,
                                                tr!("New connection"),
                                            )
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.open_connection(None, window, cx)
                                            })),
                                        ),
                                )
                            },
                        )
                        .children(self.connections.iter().enumerate().map(
                            |(index, connection)| self.connection_tree(index, connection, cx),
                        )),
                )
            })
            .when(self.section != Section::Explorer, |s| {
                s.child(self.sidebar_content(cx))
            })
    }

    fn connection_tree(
        &self,
        index: usize,
        connection: &Connection,
        cx: &mut Context<Self>,
    ) -> Div {
        if connection.input.is_some() {
            return self.live_connection_tree(index, connection, cx);
        }
        let query = self.search.read(cx).value().to_lowercase();
        let expanded = connection.expanded || !query.is_empty();
        column().flex_none()
            .child(row().id(("connection", index)).role(Role::Button).aria_label(connection.name.clone()).aria_expanded(expanded).focusable().tab_index(0).focus_visible(|s| s.bg(rgb(HOVER))).h(px(38.)).px_3().gap_2().cursor_pointer().hover(|s| s.bg(rgb(SURFACE)))
                .on_click(cx.listener(move |this, _, _, cx| { this.connections[index].expanded = !this.connections[index].expanded; cx.notify(); }))
                .child(icon(if expanded { I::ChevronDown } else { I::ChevronRight }, FAINT))
                .child(icon(I::Database, connection.color))
                .child(div().text_size(px(14.)).flex_1().truncate().child(connection.name.clone()))
                .child(dot(connection.color)))
            .when(expanded, |s| s
                .child(div().pl(px(43.)).pb_2().child(div().text_size(px(12.)).text_color(rgb(FAINT)).child(connection.detail.clone())))
                .when(index == 0, |s| s
                    .child(row().px_3().pl(px(29.)).h(px(30.)).child(icon(I::ChevronDown, FAINT)).child(icon(I::Database, MUTED)).child(label("commerce")))
                    .child(row().id("schema-public").role(Role::Button).aria_label(tr!("Schema public")).aria_expanded(self.schema_open).focusable().tab_index(0).focus_visible(|s| s.bg(rgb(HOVER))).pl(px(43.)).h(px(30.)).cursor_pointer()
                        .child(icon(if self.schema_open { I::ChevronDown } else { I::ChevronRight }, FAINT)).child(icon(I::Layers, MUTED)).child(label("public"))
                        .on_click(cx.listener(|this, _, _, cx| { this.schema_open = !this.schema_open; cx.notify(); })))
                    .when(self.schema_open || !query.is_empty(), |s| s
                        .child(row().id("tables-folder").role(Role::Button).aria_label(tr!("Tables")).aria_expanded(self.tables_open).focusable().tab_index(0).focus_visible(|s| s.bg(rgb(HOVER))).pl(px(57.)).pr_3().h(px(30.)).cursor_pointer()
                            .child(icon(if self.tables_open { I::ChevronDown } else { I::ChevronRight }, FAINT)).child(icon(I::Table2, MUTED)).child(label(tr!("Tables")))
                            .child(div().flex_1()).child(label("7"))
                            .on_click(cx.listener(|this, _, _, cx| { this.tables_open = !this.tables_open; cx.notify(); })))
                        .when(self.tables_open || !query.is_empty(), |s| s.children(TABLES.into_iter().enumerate().filter(|(_, table)| query.is_empty() || table.contains(&query)).map(|(table_index, table)| {
                            column().flex_none()
                                .child(row().id(("schema-table", table_index)).role(Role::Button).aria_label(tr!("Open table {0}", table)).focusable().tab_index(0).focus_visible(|s| s.bg(rgb(HOVER))).h(px(32.)).pl(px(71.)).pr_3().gap_2().cursor_pointer()
                                    .when(table == "orders", |s| s.bg(rgb(0x25352f)))
                                    .hover(|s| s.bg(rgb(HOVER)))
                                    .child(icon(if table == "orders" && self.orders_open { I::ChevronDown } else { I::ChevronRight }, FAINT))
                                    .child(icon(I::Table2, if table == "orders" { MINT } else { FAINT }))
                                    .child(div().text_size(px(14.)).text_color(rgb(if table == "orders" { MINT } else { MUTED })).child(table))
                                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                        if table == "orders" && event.click_count() == 1 { this.orders_open = !this.orders_open; cx.notify(); }
                                        else { this.open_table(table, window, cx); }
                                    })))
                                .when(table == "orders" && self.orders_open, |s| s.children(ORDER_COLUMNS.into_iter().map(|(name, kind, key)| {
                                    row().h(px(30.)).pl(px(99.)).pr_3().gap_2().child(icon(if key { I::KeyRound } else { I::Columns3 }, if key { 0xd8be85 } else { FAINT }))
                                        .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child(name)).child(div().flex_1())
                                        .child(div().text_size(px(12.)).text_color(rgb(FAINT)).child(kind))
                                })))
                        })))
                        .child(row().pl(px(57.)).h(px(32.)).child(icon(I::ChevronRight, FAINT)).child(icon(I::Eye, FAINT)).child(label(tr!("Views"))))
                        .child(row().pl(px(57.)).h(px(32.)).child(icon(I::ChevronRight, FAINT)).child(icon(I::Braces, FAINT)).child(label(tr!("Functions"))))))
                .when(index != 0, |s| s.child(column().pl_5().pr_3().pb_3().gap_2()
                    .child(label(if index == 1 { tr!("analytics · 12 tables") } else { tr!("finance · 8 tables") }))
                    .child(button(("open-connection-query", index), I::SquareTerminal, tr!("New query"))
                        .on_click(cx.listener(move |this, _, window, cx| { let group = this.connections[index].group; this.new_query(group, window, cx); }))))))
    }

    fn sidebar_content(&self, cx: &mut Context<Self>) -> Div {
        column()
            .flex_1()
            .p_3()
            .gap_2()
            .when(
                self.section == Section::Saved || self.section == Section::History,
                |s| {
                    let queries = if self.section == Section::Saved {
                        &self.saved_queries
                    } else {
                        &self.history
                    };
                    s.child(label(if self.section == Section::Saved {
                        tr!("Your library")
                    } else {
                        tr!("Recent runs")
                    }))
                    .when(queries.is_empty(), |s| {
                        s.child(div().p_3().text_size(px(14.)).text_color(rgb(MUTED)).child(
                            if self.section == Section::Saved {
                                tr!("Save a query with the bookmark icon in the editor.")
                            } else {
                                tr!("Queries you run appear here.")
                            },
                        ))
                    })
                    .children(queries.iter().enumerate().map(|(index, query)| {
                        let query = query.clone();
                        column()
                            .id(("query-snapshot", index))
                            .role(Role::Button)
                            .aria_label(tr!("Open {0}", query.title))
                            .focusable()
                            .tab_index(0)
                            .focus_visible(|s| s.bg(rgb(HOVER)))
                            .p_3()
                            .gap_1()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(HOVER)))
                            .child(
                                row()
                                    .child(icon(
                                        if self.section == Section::Saved {
                                            I::Bookmark
                                        } else {
                                            I::Clock
                                        },
                                        MINT,
                                    ))
                                    .child(
                                        div()
                                            .text_size(px(14.))
                                            .truncate()
                                            .child(query.title.clone()),
                                    ),
                            )
                            .child(div().pl_6().child(label(query.connection.clone())))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let Some(group) = this
                                    .connections
                                    .iter()
                                    .find(|c| c.name == query.connection)
                                    .or(this.connections.first())
                                    .map(|c| c.group)
                                else {
                                    return;
                                };
                                this.add_query(group, &query.title, &query.sql, window, cx);
                            }))
                    }))
                },
            )
            .when(self.section == Section::Settings, |s| {
                s.child(self.preferences(cx))
            })
    }

    fn preferences(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        column().id("appearance-preferences").overflow_y_scroll().gap_3()
            .child(label(tr!("Appearance")))
            .child(row().justify_between().child(label(tr!("Theme"))).child(badge("Graphite", MINT)))
            .child(row().justify_between().child(label(tr!("SQL font"))).child(label("Cascadia Code")))
            .child(row().justify_between().child(label(tr!("SQL size"))).child(badge(format!("{} px", f32::from(cx.theme().mono_font_size)), MINT)))
            .child(row().gap_1().children([12., 14., 16., 18., 20.].into_iter().enumerate().map(|(index, size)| {
                div().id(("sql-font-size", index)).role(Role::Button).aria_label(tr!("SQL font at {0} pixels", size))
                    .focusable().tab_index(0).focus_visible(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT))).w(px(37.)).h(px(32.)).flex().items_center().justify_center()
                    .rounded(px(4.)).border_1().border_color(rgb(BORDER)).cursor_pointer()
                    .when(f32::from(cx.theme().mono_font_size) == size, |s| s.bg(rgb(0x263b37)).border_color(rgb(MINT)))
                    .focus_visible(|s| s.border_color(rgb(MINT)))
                    .hover(|s| s.bg(rgb(HOVER))).child(format!("{size}"))
                    .on_click(cx.listener(move |this, _, _, cx| { Theme::update(cx, |theme| theme.mono_font_size = px(size)); this.persist_font_size(size, cx); }))
            })))
            .child(div().mt_3().child(label(tr!("Layout"))))
            .child(label(tr!("Drag the dividers. With the keyboard, use Tab to focus a divider and the arrows to adjust it.")))
            .child(button("reset-layout", I::RotateCcw, tr!("Reset widths"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.explorer_split.update(cx, |state, cx| state.resize_panel(0, px(EXPLORER_WIDTH), window, cx));
                    this.assistant_split.update(cx, |state, cx| state.resize_panel(1, px(ASSISTANT_WIDTH), window, cx));
                    cx.notify();
                })))
            .child(div().mt_3().child(label(tr!("Shortcuts"))))
            .children([
                (tr!("Search tables"), "Ctrl Shift P"), (tr!("New query"), "Ctrl N"),
                (tr!("Run SQL"), "Ctrl ↵"), (tr!("Explorer"), "Ctrl B"),
                (tr!("Assistant"), "Ctrl Shift B"), (tr!("SQL font"), "Ctrl + / −"), (tr!("Reset font"), "Ctrl 0"),
            ].into_iter().map(|(title, key)| row().justify_between().child(label(title)).child(shortcut(key))))
            .child(div().mt_3().child(label(tr!("Preferences saved on this computer."))))
    }

    fn titlebar_content(&self, cx: &mut Context<Self>) -> Div {
        row()
            .w_full()
            .h_full()
            .pr_3()
            .gap_3()
            .child(
                row()
                    .flex_none()
                    .gap_2()
                    .child(
                        div()
                            .size(px(21.))
                            .rounded(px(5.))
                            .bg(rgb(MINT))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(I::Database, 0x172820)),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Jykre Studio"),
                    ),
            )
            .child(divider())
            .child(
                row()
                    .flex_none()
                    .gap_2()
                    .child(icon(I::PanelsTopLeft, MUTED))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(tr!("Workspace")),
                    )
                    .child(icon(I::ChevronRight, FAINT))
                    .child(label(tr!("Personal"))),
            )
            .child(div().flex_1())
            .child(
                row()
                    .id("global-search")
                    .role(Role::Button)
                    .aria_label(tr!("Search the workspace"))
                    .focusable()
                    .tab_index(0)
                    .focus_visible(|s| s.border_color(rgb(MINT)))
                    .w(px(400.))
                    .min_w(px(180.))
                    .flex_shrink_1()
                    .h(px(32.))
                    .px_3()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(BG))
                    .cursor_pointer()
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .hover(|s| s.border_color(rgb(0x4a5753)))
                    .child(icon(I::Search, FAINT))
                    .child(label(tr!("Search the workspace…")))
                    .child(div().flex_1())
                    .child(shortcut("Ctrl Shift P"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.command_open = true;
                        this.command.update(cx, |input, cx| input.focus(window, cx));
                        this.search_cache(true, cx);
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(divider())
            .child(
                button("new-query", I::Plus, tr!("New query"))
                    .flex_none()
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .border_1()
                    .border_color(rgb(0x3b4943))
                    .text_color(rgb(MINT))
                    .on_click(cx.listener(|this, _, window, cx| this.new_active_query(window, cx))),
            )
    }

    /// The tabs, or a welcome state with the next step when there is nothing
    /// to show yet.
    fn main_area(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.connections_loading || !self.connections.is_empty() {
            return self.tabs.clone().into_any_element();
        }
        column()
            .id("welcome")
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .p_6()
            .child(icon(I::Database, MINT))
            .child(div().text_size(px(16.)).child(tr!("Connect to a database")))
            .child(label(tr!(
                "Add a PostgreSQL, MySQL or Databricks connection to explore its metadata and run SQL."
            )))
            .child(
                primary_button("welcome-add-connection", I::Plus, tr!("New connection")).on_click(
                    cx.listener(|this, _, window, cx| this.open_connection(None, window, cx)),
                ),
            )
            .into_any_element()
    }

    fn table_matches(&self, _: &App) -> Vec<(usize, String, String)> {
        self.command_matches.clone()
    }

    fn command_palette(&self, cx: &mut Context<Self>) -> AnyElement {
        let matches = self.table_matches(cx);
        self.overlay("command-backdrop", cx)
            .child(
                column()
                    .w(px(530.))
                    .max_h(px(580.))
                    .mt(px(100.))
                    .rounded(px(10.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(0x424c57))
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        row().p_4().border_b_1().border_color(rgb(BORDER)).child(
                            Input::new(&self.command)
                                .prefix(I::Search)
                                .appearance(false)
                                .bordered(false)
                                .w_full(),
                        ),
                    )
                    .child(
                        div()
                            .px_4()
                            .pt_4()
                            .pb_2()
                            .child(label(tr!("Tables and views · connections"))),
                    )
                    .child(
                        column()
                            .id("command-results")
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .px_2()
                            .pb_3()
                            .when(matches.is_empty(), |s| {
                                s.child(div().p_5().child(label(if self.command_searching {
                                    tr!("Searching…")
                                } else if self.connections.is_empty() {
                                    tr!("No connections yet. Add one to search its tables.")
                                } else {
                                    tr!("No objects found. Refresh the connection metadata.")
                                })))
                            })
                            .children(matches.into_iter().enumerate().map(
                                |(index, (connection_index, name, sql))| {
                                    row()
                                        .id(("command-table", index))
                                        .role(Role::Button)
                                        .aria_label(tr!("Open table {0}", name))
                                        .focusable()
                                        .tab_index(0)
                                        .focus_visible(|s| s.bg(rgb(HOVER)))
                                        .h(px(43.))
                                        .px_3()
                                        .rounded(px(5.))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgb(HOVER)))
                                        .child(icon(I::Table2, MINT))
                                        .child(
                                            div().flex_1().text_size(px(14.)).child(name.clone()),
                                        )
                                        .child(label(
                                            self.connections[connection_index].name.clone(),
                                        ))
                                        .child(icon(I::CornerDownLeft, FAINT))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.add_query(
                                                this.connections[connection_index].group,
                                                &name,
                                                &sql,
                                                window,
                                                cx,
                                            );
                                            this.command_open = false;
                                        }))
                                },
                            )),
                    )
                    .child(
                        row()
                            .h(px(40.))
                            .px_4()
                            .justify_between()
                            .bg(rgb(BG))
                            .border_t_1()
                            .border_color(rgb(BORDER))
                            .child(label(tr!("Click an object to open the query.")))
                            .child(row().child(shortcut("Esc")).child(label(tr!("close")))),
                    ),
            )
            .into_any_element()
    }

    fn overlay(&self, id: &'static str, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id(id)
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .bg(black().opacity(0.65))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    // Only the palette closes on an outside click. The connection
                    // form keeps what was typed until it is closed explicitly.
                    if this.command_open {
                        this.command_open = false;
                        cx.notify();
                    }
                }),
            )
    }
}

impl Render for AppView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        probe::frame_started();
        probe::rendered("AppView");
        column()
            .id("jykre")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Segoe UI")
            .text_size(px(14.))
            .on_action(cx.listener(|this, _: &ExecuteCurrentQuery, _, cx| {
                this.with_active_panel(cx, Workspace::execute_current);
            }))
            .on_action(
                cx.listener(|this, _: &ExecuteCurrentQueryInNewResult, _, cx| {
                    this.with_active_panel(cx, Workspace::execute_current_in_new_result);
                }),
            )
            .on_action(cx.listener(|this, _: &ExecuteAllQueries, _, cx| {
                this.with_active_panel(cx, Workspace::execute_all);
            }))
            .on_action(cx.listener(|this, _: &CloseActiveTab, window, cx| {
                if this.connection_open || this.command_open {
                    return;
                }
                if let Some(tab) = this.tabs.read(cx).active_tab() {
                    this.request_close(tab, window, cx);
                }
            }))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    if this.command_open {
                        this.command_open = false;
                        cx.notify();
                    } else {
                        this.close_connection_dialog(cx);
                    }
                } else if this.connection_open {
                    // Workspace shortcuts would act behind the modal form.
                } else if event.keystroke.modifiers.control
                    && event.keystroke.modifiers.shift
                    && event.keystroke.key.eq_ignore_ascii_case("p")
                {
                    this.command_open = !this.command_open;
                    if this.command_open {
                        this.command.update(cx, |input, cx| input.focus(window, cx));
                        this.search_cache(true, cx);
                    }
                    cx.notify();
                    window.prevent_default();
                    cx.stop_propagation();
                } else if event.keystroke.modifiers.control && event.keystroke.key == "n" {
                    this.new_active_query(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                } else if event.keystroke.modifiers.control && event.keystroke.key == "b" {
                    if event.keystroke.modifiers.shift {
                        if let Some(panel) = this
                            .tabs
                            .read(cx)
                            .active_tab()
                            .and_then(|tab| this.panels.get(&tab))
                            .cloned()
                        {
                            panel.update(cx, |panel, cx| panel.toggle_assistant(cx));
                        }
                    } else {
                        this.sidebar_open = !this.sidebar_open;
                        cx.notify();
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                } else if event.keystroke.modifiers.control
                    && matches!(event.keystroke.key.as_str(), "+" | "=" | "-" | "0")
                {
                    let current = f32::from(cx.theme().mono_font_size);
                    let size = match event.keystroke.key.as_str() {
                        "-" => (current - 1.).max(12.),
                        "0" => 14.,
                        _ => (current + 1.).min(24.),
                    };
                    Theme::update(cx, |theme| theme.mono_font_size = px(size));
                    this.persist_font_size(size, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }))
            .child(
                TitleBar::new()
                    .h(px(46.))
                    .bg(rgb(0x13161a))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(self.titlebar_content(cx)),
            )
            .child(
                row()
                    .flex_1()
                    .min_h_0()
                    .gap_0()
                    .items_stretch()
                    .child(self.rail(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(if self.sidebar_open {
                                h_resizable("explorer-layout")
                                    .with_state(&self.explorer_split)
                                    .with_handle_appearance(splitter(
                                        &self.explorer_split,
                                        0,
                                        tr!("Explorer width. Use the arrows to adjust."),
                                        240.0..480.0,
                                    ))
                                    .child(
                                        resizable_panel()
                                            .size(px(EXPLORER_WIDTH))
                                            .size_range(px(240.)..px(480.))
                                            .fixed()
                                            .child(self.sidebar(cx)),
                                    )
                                    .child(
                                        resizable_panel()
                                            .size_range(px(760.)..Pixels::MAX)
                                            .child(self.main_area(cx)),
                                    )
                                    .into_any_element()
                            } else {
                                self.main_area(cx)
                            }),
                    ),
            )
            .child(
                row()
                    .h(px(30.))
                    .flex_none()
                    .px_3()
                    .gap_3()
                    .bg(rgb(0x13161a))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .justify_between()
                    .child(
                        row()
                            .gap_3()
                            .child(
                                row()
                                    .gap_1()
                                    .child(icon(I::Folder, FAINT))
                                    .child(label(tr!("Personal workspace"))),
                            )
                            .child(divider())
                            .child(label(match self.connections.len() {
                                1 => tr!("1 connection").to_string(),
                                count => tr!("{0} connections", count),
                            })),
                    )
                    .child(
                        row()
                            .gap_3()
                            .child(label(tr!("Local data · optional vault")))
                            .child(divider())
                            .child(
                                row()
                                    .gap_1()
                                    .child(dot(MINT))
                                    .child(label("Jykre Studio · v0.1")),
                            ),
                    ),
            )
            .when(self.command_open, |s| s.child(self.command_palette(cx)))
            .when(self.connection_open, |s| {
                s.child(self.connection_dialog(cx))
            })
            // Painted last, so it closes the frame for the probe.
            .child(probe::frame_end())
    }
}

pub fn run() {
    application().with_assets(assets::AllAssets).run(|cx| {
        init(cx);
        probe::start(cx);
        // The editor binds Ctrl+Enter itself; a binding in its own, deeper
        // context added later wins there, and the global ones apply anywhere
        // else in the window.
        let editor = format!("{} > Input", workspace::SQL_EDITOR_CONTEXT);
        cx.bind_keys([
            KeyBinding::new("ctrl-enter", ExecuteCurrentQuery, None),
            KeyBinding::new("ctrl-shift-enter", ExecuteCurrentQueryInNewResult, None),
            KeyBinding::new("ctrl-alt-enter", ExecuteAllQueries, None),
            KeyBinding::new("ctrl-enter", ExecuteCurrentQuery, Some(&editor)),
            KeyBinding::new(
                "ctrl-shift-enter",
                ExecuteCurrentQueryInNewResult,
                Some(&editor),
            ),
            KeyBinding::new("ctrl-alt-enter", ExecuteAllQueries, Some(&editor)),
            KeyBinding::new("ctrl-w", CloseActiveTab, None),
            KeyBinding::new("ctrl-f4", CloseActiveTab, None),
            KeyBinding::new("ctrl-w", CloseActiveTab, Some(&editor)),
            KeyBinding::new("ctrl-f4", CloseActiveTab, Some(&editor)),
        ]);
        Theme::change(ThemeMode::Dark, None, cx);
        Theme::update(cx, |theme| {
            theme.font_family = "Segoe UI".into();
            theme.font_size = px(14.);
            theme.mono_font_family = "Cascadia Code".into();
            theme.mono_font_size = px(14.);
            theme.background = rgb(BG).into();
            theme.foreground = rgb(TEXT).into();
            theme.border = rgb(BORDER).into();
            theme.input = rgb(BG).into();
            theme.accent = rgb(HOVER).into();
            theme.accent_foreground = rgb(TEXT).into();
            theme.muted = rgb(SURFACE).into();
            theme.muted_foreground = rgb(MUTED).into();
            theme.primary = rgb(MINT).into();
            theme.primary_foreground = rgb(0x172820).into();
            theme.tab_bar = rgb(PANEL).into();
            theme.tab_active = rgb(BG).into();
            theme.tab_foreground = rgb(MUTED).into();
            theme.tab_active_foreground = rgb(TEXT).into();
            theme.title_bar = rgb(0x13161a).into();
            theme.title_bar_border = rgb(BORDER).into();
            theme.radius = px(5.);
        });
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1480.), px(920.)),
                cx,
            ))),
            window_min_size: Some(size(px(1100.), px(700.))),
            ..TitleBar::window_options()
        };
        let opened = open_window(options, cx, |window, cx| {
            window.set_window_title("Jykre Studio · Workspace");
            cx.new(|cx| AppView::new(window, cx))
        });
        if opened.is_err() {
            tracing::error!("could not open the main window");
            cx.quit();
        }
    });
}
