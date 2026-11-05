use jykre::tr;
mod connections;
mod copying;
mod databricks_connection;
mod demo;
mod layout;
mod password;
pub mod performance;
mod probe;
mod report_chart;
mod report_panel;
mod report_picker;
mod report_view;
mod reports;
mod result_grid;
mod saved_query_dialog;
mod scope_picker;
mod sql_editor;
mod state;
mod ui;
mod workspace;

use connections::ExplorerEntry;
use demo::{ORDER_COLUMNS, SALES_SQL, TABLES, table_sql};
use gpui_kit::assets::IconName as I;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::{
    ActiveTheme, Sizable, Theme, ThemeMode, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogAction, DialogClose, DialogFooter},
    input::{Input, InputEvent, InputState, ShowCompletions},
    resizable::{ResizableState, h_resizable, resizable_panel},
    scroll::ScrollableElement,
    tooltip::Tooltip,
};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::explorer::{Tree, TreeRow};
use jykre::grouped_tabs::{GroupedTabs, GroupedTabsEvent, TabGroupId, TabId};
use jykre::query::ShortcutAction;
use jykre::refresh_queue::RefreshQueue;
use jykre::workspace_state::QueryEntry;
use jykre::{backend, connection_store, drivers::*, result::DEFAULT_ROW_LIMIT};
use jykre_secrets::SystemSecretStore;
use jykre_storage::{ConfigRepository, RefreshStatus};
use layout::{ASSISTANT_WIDTH, EXPLORER_WIDTH, splitter};
use std::collections::HashMap;
use std::ops::Range;

/// Height of an entry in the saved queries and the history.
const LIBRARY_ROW_HEIGHT: f32 = 56.;
use ui::*;
use workspace::{Workspace, WorkspaceEvent};

/// Where the keyboard is when a confirmation opens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfirmFocus {
    /// On the dialog: Enter confirms, but no button looks focused.
    Dialog,
    /// On the OK button, visibly, so Enter confirms what the user sees.
    Ok,
}

/// What a confirmation asks and how it opens.
struct Confirmation {
    title: String,
    description: String,
    ok: &'static str,
    cancel: &'static str,
    focus: ConfirmFocus,
}

/// Asks the user to confirm an action in a dialog drawn with the
/// application's theme; `on_ok` runs only when they confirm.
fn confirm(
    window: &mut Window,
    cx: &mut App,
    confirmation: Confirmation,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
) {
    let Confirmation {
        title,
        description,
        ok,
        cancel,
        focus,
    } = confirmation;
    let on_ok = std::rc::Rc::new(on_ok);
    let ok_focus = cx.focus_handle();
    // The OK button takes focus once, when the dialog opens; later rebuilds
    // must not take it back from where the user moved it.
    let opening = std::cell::Cell::new(true);
    window.open_alert_dialog(cx, move |alert, window, cx| {
        let on_ok = on_ok.clone();
        let alert = alert
            .title(title.clone())
            .description(description.clone())
            .confirm()
            .ok_text(ok)
            .cancel_text(cancel)
            .width(px(440.))
            .on_ok(move |_, window, cx| {
                on_ok(window, cx);
                true
            });
        if focus == ConfirmFocus::Dialog {
            return alert;
        }
        if opening.replace(false) {
            ok_focus.focus(window, cx);
        }
        alert.footer(
            DialogFooter::new()
                .justify_center()
                .child(DialogClose::new().trigger(|button| button.label(cancel)))
                .child(
                    DialogAction::new().child(
                        Button::new("confirm-ok")
                            .primary()
                            .label(ok)
                            .track_focus(&ok_focus),
                    ),
                ),
        )
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
        CloseActiveTab,
        /// Ctrl+Tab or Ctrl+Page Down: the next query tab, from anywhere.
        NextTab,
        /// Ctrl+Shift+Tab or Ctrl+Page Up: the previous query tab.
        PreviousTab,
        /// Ctrl+Shift+E: opens or closes the export bar of the result on
        /// screen.
        ExportResult
    ]
);

/// The part of the connection dialog on screen. The header, the driver and
/// the footer with Test and Save stay visible in every one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnectionSection {
    Connection,
    Metadata,
    Options,
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    Explorer,
    Saved,
    Reports,
    History,
    Settings,
}

struct Connection {
    /// Identifies the connection for as long as the application runs, so
    /// work that comes back later finds it even after another connection
    /// was deleted and the list shifted ([`AppView::connection_index`]).
    key: u64,
    name: String,
    profile: Option<ConnectionProfile>,
    metadata_statuses: Vec<RefreshStatus>,
    /// The explorer tree read from the cache, page by page.
    tree: Tree,
    /// When each chosen namespace was last read in full.
    ages: HashMap<ObjectPath, std::time::SystemTime>,
    /// Nodes the last refresh could not read for lack of permission; their
    /// last good read stays.
    no_access: std::collections::HashSet<ObjectPath>,
    /// The last-level namespaces in the cache, which tabs offer as their
    /// context.
    namespaces: Vec<ObjectPath>,
    /// The editor's index of the cached relations, built off the UI thread
    /// after each read of the cache.
    names: Option<std::sync::Arc<sql_editor::NameIndex>>,
    /// The rows of the current explorer search.
    search_rows: Vec<TreeRow>,
    /// Bumped whenever `search_rows` changes.
    search_revision: u64,
    /// Bumped by every expand or collapse, so only the last of a burst is
    /// saved.
    expanded_revision: usize,
    refresh_generation: usize,
    detail: String,
    /// The colour its explorer entry and tab group share: the one the user
    /// chose, or the driver's.
    color: u32,
    /// The colour and environment the user chose.
    appearance: jykre::connection_appearance::Appearance,
    group: TabGroupId,
    expanded: bool,
    input: Option<ResolvedConnection>,
    /// Rows kept per statement in this connection's tabs.
    row_limit: u32,
    /// Hours after which opening the connection reads its chosen namespaces
    /// again; 0 never does.
    revalidate_hours: u32,
    /// Scope being refreshed and how to cancel it.
    refreshing: Option<(ObjectPath, CancellationToken)>,
    /// Refreshes asked for while another runs, started in order.
    refresh_queue: RefreshQueue,
    /// What the user chose to introspect; pending until they choose.
    scope: jykre::metadata_scope::MetadataScope,
    /// How far reading the chosen namespaces got, while it runs.
    progress: Option<jykre::metadata_scope::RefreshProgress>,
    status: Feedback,
    notice: Option<String>,
}

/// A table or view the command palette found.
#[derive(Clone, PartialEq)]
struct PaletteMatch {
    /// Index of its connection.
    connection: usize,
    /// Where it is in the explorer, which identifies it.
    path: ObjectPath,
    /// Where it is read (a partition kept in another schema).
    stored_at: ObjectPath,
}

/// A row of the command palette: a saved query, listed first, or a table
/// or view.
#[derive(Clone, PartialEq)]
enum PaletteItem {
    /// The saved query with this identity.
    Saved(u64),
    Relation(PaletteMatch),
}

/// Saved queries the palette lists at most, above the tables.
const PALETTE_SAVED_LIMIT: usize = 20;

struct AppView {
    tabs: Entity<GroupedTabs>,
    panels: HashMap<TabId, Entity<Workspace>>,
    /// The report tabs, apart from the query tabs ([`reports`]).
    report_tabs: HashMap<TabId, reports::ReportTab>,
    /// The saved reports.
    reports: jykre::reports::ReportLibrary,
    /// The reports were read, so writing them loses nothing.
    persist_reports: bool,
    /// Bumped by every change, so only the last of a burst is written.
    reports_revision: usize,
    /// What each SQL dialect has built in, read at start from the catalog
    /// directory and handed to every tab; the built-in words until then.
    sql_catalogs: std::sync::Arc<sql_editor::SqlCatalogs>,
    explorer_split: Entity<ResizableState>,
    assistant_split: Entity<ResizableState>,
    saved_queries: Vec<QueryEntry>,
    history: Vec<QueryEntry>,
    /// The identity the next saved query gets.
    next_saved_id: u64,
    /// The search over the saved queries or the history, and the positions
    /// in the list on screen that match it, kept as the list or the search
    /// changes rather than filtered while drawing.
    library_search: Entity<InputState>,
    library_shown: Vec<usize>,
    library_scroll: UniformListScrollHandle,
    connections: Vec<Connection>,
    /// The key of the last connection added.
    next_connection_key: u64,
    section: Section,
    next_query: usize,
    /// Bumped by every font size change, so only the last of a burst is
    /// written.
    font_revision: usize,
    sidebar_open: bool,
    schema_open: bool,
    tables_open: bool,
    orders_open: bool,
    /// The explorer's virtualized list: connections and the rows of their
    /// trees, built only while on screen.
    explorer_list: ListState,
    explorer_entries: Vec<ExplorerEntry>,
    /// What the entries were built from, so they are rebuilt only when a
    /// tree, an expansion or a search changes.
    explorer_signature: Vec<(bool, bool, u64)>,
    /// A node go to definition asked to show, selected once its row is read.
    pending_reveal: Option<(usize, ObjectPath)>,
    /// The inspection panel, while open.
    inspection: Option<connections::InspectionPanel>,
    /// Bumped by every inspection, so a late answer for an earlier one is
    /// dropped.
    inspection_revision: usize,
    search: Entity<InputState>,
    command: Entity<InputState>,
    command_open: bool,
    connection_open: bool,
    connection_section: ConnectionSection,
    connection_focus: FocusHandle,
    connection_name: Entity<InputState>,
    connection_host: Entity<InputState>,
    connection_database: Entity<InputState>,
    connection_user: Entity<InputState>,
    driver: usize,
    connection_port: Entity<InputState>,
    connection_row_limit: Entity<InputState>,
    /// The colour and environment chosen in the connection dialog.
    connection_appearance: jykre::connection_appearance::Appearance,
    connection_password: Entity<InputState>,
    connection_ssl: Entity<InputState>,
    /// PostgreSQL: the search_path sessions start with.
    connection_search_path: Entity<InputState>,
    /// PostgreSQL: offer the server's other databases to introspect.
    all_databases: bool,
    /// Hours after which opening the connection reads its chosen namespaces
    /// again; empty or 0 never does.
    connection_revalidate: Entity<InputState>,
    connection_databricks: Entity<databricks_connection::DatabricksForm>,
    connection_scope: Entity<scope_picker::ScopePicker>,
    connection_status: Option<Feedback>,
    /// Application-level problem (loading connections, saving preferences,
    /// searching) shown in the explorer until dismissed.
    app_notice: Option<Feedback>,
    /// The newest copy of `config.db` and when it was taken, offered when
    /// the configuration could not be opened at start.
    config_backup: Option<(std::path::PathBuf, std::time::SystemTime)>,
    command_searching: bool,
    remember_password: bool,
    connection_saving: bool,
    connections_loading: bool,
    search_revision: usize,
    command_revision: usize,
    command_matches: Vec<PaletteItem>,
    /// The match Enter opens; the arrow keys move it.
    command_selected: usize,
    command_scroll: ScrollHandle,
    /// Keeps Tab inside the palette while it is open.
    command_focus: FocusHandle,
    connection_testing: Option<CancellationToken>,
    /// When the connection test on screen started.
    connection_test_started: Option<std::time::Instant>,
    connection_revision: usize,
    editing_connection: Option<usize>,
    /// The password prompt and what waits for it, while it is open.
    password_prompt: Option<password::PasswordPrompt>,
    password_input: Entity<InputState>,
    password_remember: bool,
    /// Keeps Tab inside the password prompt.
    password_focus: FocusHandle,
    /// The prompt just opened: its field takes the keyboard.
    password_needs_focus: bool,
    /// The dialog that names a saved query and gives it tags, while open.
    query_details: Option<saved_query_dialog::QueryDetails>,
    details_name: Entity<InputState>,
    details_tags: Entity<InputState>,
    /// Why the dialog did not save (a blank name).
    details_error: Option<String>,
    /// Keeps Tab inside the dialog.
    details_focus: FocusHandle,
    /// The dialog just opened: its name field takes the keyboard.
    details_needs_focus: bool,
    /// The dialog that adds a query tab's SQL to a saved report, while open.
    report_picker: Option<report_picker::ReportPicker>,
    /// The tag chosen under the library search; only saved queries with it
    /// are listed.
    library_tag: Option<String>,
    /// The open tabs, saved queries and history are written to `config.db`
    /// once the last session's were restored (never in the demo).
    persist_state: bool,
    /// Bumped by every change, so only the last of a burst is written.
    state_revision: usize,
    /// Groups for restored tabs whose connection no longer exists, by the
    /// connection's name.
    orphan_groups: Vec<(String, TabGroupId)>,
    /// The explorer node selected by a click or the arrow keys.
    explorer_selected: Option<(usize, ObjectPath)>,
    /// Focus of each explorer node shown so far, so the arrow keys can move
    /// it. Rendering takes `&self`, hence the cell.
    node_focus: std::cell::RefCell<HashMap<(usize, ObjectPath), FocusHandle>>,
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
        // The SQL catalogs are read off the UI thread; tabs use the built-in
        // words until they arrive.
        cx.spawn(async move |weak, cx| {
            let catalogs = sql_editor::load_catalogs().await;
            let _ = weak.update(cx, |this: &mut Self, cx| {
                this.set_sql_catalogs(catalogs, cx)
            });
        })
        .detach();
        let tab_subscription =
            cx.subscribe_in(&tabs, window, |this, _, event, window, cx| match event {
                GroupedTabsEvent::NewTabRequested(group) => this.new_query(*group, window, cx),
                GroupedTabsEvent::TabClosed(tab) => {
                    this.panels.remove(tab);
                    this.report_tabs.remove(tab);
                    this.save_state_soon(cx);
                }
                GroupedTabsEvent::TabMoved { tab, from, to, .. }
                    if from != to && this.report_tabs.contains_key(tab) =>
                {
                    this.report_moved(*tab, *to, cx);
                }
                GroupedTabsEvent::SelectionChanged(_) => this.save_state_soon(cx),
                GroupedTabsEvent::CloseRequested(tab) => this.request_close(*tab, window, cx),
                GroupedTabsEvent::MoveRequested { tab, group, index } => {
                    this.request_move(*tab, *group, *index, window, cx)
                }
                GroupedTabsEvent::TabMoved { tab, from, to, .. } if from != to => {
                    let appearance = this
                        .connections
                        .iter()
                        .find(|connection| connection.group == *to)
                        .map_or((FAINT, None), |connection| {
                            (connection.color, connection.appearance.environment)
                        });
                    let target = this
                        .connections
                        .iter()
                        .find(|connection| connection.group == *to)
                        .map(|connection| {
                            (
                                connection.name.clone(),
                                connection.input.clone(),
                                connection.row_limit,
                            )
                        })
                        // A group without a connection: the tab no longer
                        // runs anywhere until one is chosen.
                        .or_else(|| {
                            this.orphan_groups
                                .iter()
                                .find(|(_, group)| group == to)
                                .map(|(name, _)| {
                                    let name = if name.is_empty() {
                                        tr!("Without connection").to_owned()
                                    } else {
                                        name.clone()
                                    };
                                    (name, None, DEFAULT_ROW_LIMIT)
                                })
                        });
                    if let (Some(panel), Some((name, input, row_limit))) =
                        (this.panels.get(tab), target)
                    {
                        panel.update(cx, |panel, cx| {
                            panel.rebind_connection(name, input, row_limit, cx);
                            panel.set_appearance(appearance.0, appearance.1, cx);
                        });
                    }
                    this.save_state_soon(cx);
                }
                _ => {}
            });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Search objects…")));
        let command = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr!("Search saved queries and tables…"))
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
                    // Enter opens the highlighted match, not always the first.
                    if let Some(item) = this.command_matches.get(this.command_selected).cloned() {
                        this.open_palette_item(item, window, cx);
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
            report_tabs: HashMap::new(),
            reports: jykre::reports::ReportLibrary::default(),
            persist_reports: false,
            reports_revision: 0,
            sql_catalogs: std::sync::Arc::default(),
            saved_queries: vec![QueryEntry {
                id: 1,
                title: tr!("Active customers").into(),
                connection: tr!("PostgreSQL · Local").into(),
                sql: table_sql("customers"),
                ..QueryEntry::default()
            }],
            history: vec![QueryEntry::ran(
                tr!("Revenue by category"),
                tr!("PostgreSQL · Local"),
                SALES_SQL,
                jykre::workspace_state::RunStatus::Succeeded,
                None,
                None,
            )],
            next_saved_id: 2,
            library_search: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!("Search SQL, titles and connections…"))
            }),
            library_shown: Vec::new(),
            library_scroll: UniformListScrollHandle::new(),
            font_revision: 0,
            connections: Vec::new(),
            next_connection_key: 0,
            section: Section::Explorer,
            next_query: 1,
            sidebar_open: true,
            schema_open: true,
            tables_open: true,
            orders_open: true,
            explorer_list: ListState::new(0, ListAlignment::Top, px(200.)),
            explorer_entries: Vec::new(),
            explorer_signature: Vec::new(),
            pending_reveal: None,
            inspection: None,
            inspection_revision: 0,
            search,
            command,
            command_open: false,
            connection_open: false,
            connection_section: ConnectionSection::Connection,
            connection_focus: cx.focus_handle(),
            connection_name,
            connection_host,
            connection_database,
            connection_user,
            driver: 0,
            connection_port: cx.new(|cx| InputState::new(window, cx).default_value("5432")),
            connection_row_limit: cx
                .new(|cx| InputState::new(window, cx).default_value(DEFAULT_ROW_LIMIT.to_string())),
            connection_appearance: Default::default(),
            connection_password: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(tr!("Enter the password"))
            }),
            connection_ssl: cx.new(|cx| InputState::new(window, cx).default_value("prefer")),
            connection_search_path: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!("Server default, e.g. sales, public"))
            }),
            all_databases: false,
            connection_revalidate: cx
                .new(|cx| InputState::new(window, cx).placeholder(tr!("Never"))),
            connection_databricks: cx
                .new(|cx| databricks_connection::DatabricksForm::new(window, cx)),
            connection_scope: cx.new(|cx| scope_picker::ScopePicker::new(window, cx)),
            connection_status: None,
            app_notice: None,
            config_backup: None,
            command_searching: false,
            remember_password: false,
            connection_saving: false,
            connections_loading: true,
            search_revision: 0,
            command_revision: 0,
            command_matches: Vec::new(),
            command_selected: 0,
            command_scroll: ScrollHandle::new(),
            command_focus: cx.focus_handle(),
            connection_testing: None,
            connection_test_started: None,
            connection_revision: 0,
            editing_connection: None,
            password_prompt: None,
            password_input: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(tr!("Enter the password"))
            }),
            password_remember: false,
            password_focus: cx.focus_handle(),
            password_needs_focus: false,
            query_details: None,
            details_name: cx
                .new(|cx| InputState::new(window, cx).placeholder(tr!("e.g. Revenue by month"))),
            details_tags: cx
                .new(|cx| InputState::new(window, cx).placeholder(tr!("e.g. finance, monthly"))),
            details_error: None,
            details_focus: cx.focus_handle(),
            details_needs_focus: false,
            report_picker: None,
            library_tag: None,
            persist_state: false,
            state_revision: 0,
            orphan_groups: Vec::new(),
            explorer_selected: None,
            node_focus: std::cell::RefCell::new(HashMap::new()),
            _subscriptions: vec![
                // The last edits are written before the application exits.
                cx.on_app_quit(|this, cx| {
                    let state = this.save_state_now(cx);
                    let reports = this.save_reports_now();
                    async move {
                        state.await;
                        reports.await;
                    }
                }),
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
        app._subscriptions.push(cx.subscribe_in(
            &app.password_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit_password(window, cx);
                }
            },
        ));
        // Enter in either field of the saved query dialog saves it.
        for input in [app.details_name.clone(), app.details_tags.clone()] {
            app._subscriptions.push(cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.submit_details(window, cx),
                    InputEvent::Change if this.details_error.is_some() => {
                        this.details_error = None;
                        cx.notify();
                    }
                    _ => {}
                },
            ));
        }
        app._subscriptions.push(cx.subscribe_in(
            &app.connection_databricks,
            window,
            |this, _, event: &databricks_connection::FormEvent, window, cx| match event {
                databricks_connection::FormEvent::Changed => {
                    this.invalidate_connection_test();
                    cx.notify();
                }
                databricks_connection::FormEvent::Submit if this.connection_open => {
                    this.save_connection(window, cx)
                }
                _ => {}
            },
        ));
        app._subscriptions.push(cx.subscribe(
            &app.connection_scope,
            |this, _, event: &scope_picker::PickerEvent, cx| match event {
                scope_picker::PickerEvent::LoadRequested => this.load_namespaces(cx),
            },
        ));
        if demo::enabled() {
            for (name, detail, color) in [
                (tr!("PostgreSQL · Local"), "localhost:5432", MINT),
                ("Databricks · Dev", "analytics / development", BLUE),
                (tr!("MySQL · Finance"), "finance / development", 0xd995a7),
            ] {
                let group = app
                    .tabs
                    .update(cx, |tabs, cx| tabs.add_group(name, rgb(color).into(), cx));
                app.next_connection_key += 1;
                app.connections.push(Connection {
                    key: app.next_connection_key,
                    name: name.into(),
                    profile: None,
                    metadata_statuses: Vec::new(),
                    tree: Tree::default(),
                    ages: HashMap::new(),
                    no_access: Default::default(),
                    namespaces: Vec::new(),
                    names: None,
                    search_rows: Vec::new(),
                    search_revision: 0,
                    expanded_revision: 0,
                    refresh_generation: 0,
                    detail: detail.into(),
                    color,
                    appearance: Default::default(),
                    group,
                    expanded: app.connections.is_empty(),
                    input: None,
                    row_limit: DEFAULT_ROW_LIMIT,
                    revalidate_hours: 0,
                    refreshing: None,
                    refresh_queue: RefreshQueue::default(),
                    scope: jykre::metadata_scope::MetadataScope::default(),
                    progress: None,
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
        app._subscriptions.push(cx.subscribe(
            &app.library_search,
            |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.filter_library(cx);
                    cx.notify();
                }
            },
        ));
        app.filter_library(cx);
        app.load_connections(window, cx);
        app
    }

    /// The saved queries or the history entries the library search matches,
    /// for the list on screen. Called when the list, the search or the
    /// section changes, never while drawing.
    fn filter_library(&mut self, cx: &App) {
        let query = self.library_search.read(cx).value().to_lowercase();
        // A chosen tag that no saved query has any more stops filtering.
        if let Some(tag) = &self.library_tag
            && !self
                .saved_queries
                .iter()
                .any(|entry| entry.tags.iter().any(|kept| kept.eq_ignore_ascii_case(tag)))
        {
            self.library_tag = None;
        }
        let (entries, tag) = match self.section {
            Section::History => (&self.history, None),
            Section::Saved => (&self.saved_queries, self.library_tag.as_deref()),
            Section::Reports => {
                self.library_shown = self
                    .reports
                    .reports
                    .iter()
                    .enumerate()
                    .filter(|(_, report)| report.matches(&query))
                    .map(|(index, _)| index)
                    .collect();
                return;
            }
            Section::Explorer | Section::Settings => {
                self.library_shown.clear();
                return;
            }
        };
        self.library_shown = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.matches(&query)
                    && tag.is_none_or(|tag| {
                        entry.tags.iter().any(|kept| kept.eq_ignore_ascii_case(tag))
                    })
            })
            .map(|(index, _)| index)
            .collect();
    }

    /// A tag under the library search: choosing it lists only the saved
    /// queries that have it, choosing it again lists them all.
    fn library_tag_chip(
        &self,
        position: usize,
        tag: String,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let chosen = self
            .library_tag
            .as_ref()
            .is_some_and(|current| current.eq_ignore_ascii_case(&tag));
        div()
            .id(("library-tag", position))
            .role(Role::CheckBox)
            .aria_label(tr!("Show only queries tagged {0}", tag))
            .aria_toggled(if chosen {
                Toggled::True
            } else {
                Toggled::False
            })
            .focusable()
            .tab_index(0)
            .flex_none()
            .px(px(7.))
            .py(px(2.))
            .rounded(px(4.))
            .border_1()
            .border_color(if chosen {
                rgb(MINT)
            } else {
                transparent_black().into()
            })
            .focus_visible(|s| s.border_color(rgb(TEXT)))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(rgb(if chosen { MINT } else { MUTED }))
            .bg(Hsla::from(rgb(if chosen { MINT } else { FAINT })).opacity(0.12))
            .hover(|s| s.text_color(rgb(TEXT)))
            .child(format!("#{tag}"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.library_tag = if chosen { None } else { Some(tag.clone()) };
                this.filter_library(cx);
                cx.notify();
            }))
    }

    /// The saved queries the palette's text matches, listed above the tables
    /// found so far (kept until their search answers). Called as the text
    /// changes, so they show at once.
    pub(super) fn match_saved_in_palette(&mut self, cx: &App) {
        let query = self.command.read(cx).value().to_lowercase();
        let chosen = self.command_matches.get(self.command_selected).cloned();
        let relations = self
            .command_matches
            .drain(..)
            .filter(|item| matches!(item, PaletteItem::Relation(_)));
        let saved = self
            .saved_queries
            .iter()
            .filter(|entry| entry.matches(&query))
            .take(PALETTE_SAVED_LIMIT)
            .map(|entry| PaletteItem::Saved(entry.id));
        self.command_matches = saved.chain(relations).collect::<Vec<_>>();
        self.command_selected = jykre::palette::keep(chosen.as_ref(), &self.command_matches);
        self.command_scroll.scroll_to_item(self.command_selected);
    }

    /// Opens what a palette row holds and closes the palette.
    fn open_palette_item(
        &mut self,
        item: PaletteItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command_open = false;
        match item {
            PaletteItem::Saved(id) => {
                if let Some(entry) = self.saved_queries.iter().find(|entry| entry.id == id) {
                    self.open_saved_query(entry.clone(), window, cx);
                }
            }
            PaletteItem::Relation(found) => self.open_relation(
                found.connection,
                &found.path,
                &found.stored_at,
                false,
                window,
                cx,
            ),
        }
    }

    /// Opens a saved query: the tab that edits it, when one is open,
    /// otherwise a new tab linked to it, so the tab shows whether it is
    /// saved and the save button updates it.
    fn open_saved_query(&mut self, entry: QueryEntry, window: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .panels
            .iter()
            .find(|(_, panel)| panel.read(cx).saved_id() == Some(entry.id))
            .map(|(tab, _)| *tab);
        if let Some(tab) = open {
            self.show_tab(tab, window, cx);
            return;
        }
        if let Some(tab) = self.open_entry(&entry, window, cx)
            && let Some(panel) = self.panels.get(&tab)
        {
            panel.update(cx, |panel, _| panel.open_saved(entry.id, &entry.sql));
        }
    }

    /// Opens a saved query or history entry in a new tab of its connection.
    /// One whose connection is gone opens without one, never on another
    /// connection, and says so.
    fn open_entry(
        &mut self,
        entry: &QueryEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<TabId> {
        let missing = !self.connections.iter().any(|c| c.name == entry.connection);
        let group = self.group_for(&entry.connection, cx);
        let tab = self.add_query(group, &entry.title, &entry.sql, window, cx)?;
        if missing && let Some(panel) = self.panels.get(&tab) {
            panel.update(cx, |panel, cx| panel.warn_missing_connection(cx));
        }
        if let Some(panel) = self.panels.get(&tab) {
            panel.update(cx, |panel, cx| panel.focus_editor(window, cx));
        }
        Some(tab)
    }

    /// Removes a saved query after the user confirms. A tab that edits it
    /// becomes a draft again, so closing it asks before losing its SQL.
    fn remove_saved_query(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.saved_queries.iter().find(|entry| entry.id == id) else {
            return;
        };
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            Confirmation {
                title: tr!("Remove the saved query {0}?", entry.title),
                description: tr!(
                    "It leaves the library. A tab that has it open keeps its SQL, which is then not saved."
                )
                .to_string(),
                ok: tr!("Remove"),
                cancel: tr!("Keep it"),
                focus: ConfirmFocus::Dialog,
            },
            move |_, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.saved_queries.retain(|entry| entry.id != id);
                    for panel in this.panels.values() {
                        panel.update(cx, |panel, cx| panel.forget_saved(id, cx));
                    }
                    this.filter_library(cx);
                    this.save_state_soon(cx);
                    cx.notify();
                });
            },
        );
    }

    /// Selects a tab and puts the keyboard in its editor.
    fn show_tab(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.tabs.update(cx, |tabs, cx| tabs.select_tab(tab, cx));
        if let Some(panel) = self.panels.get(&tab) {
            panel.update(cx, |panel, cx| panel.focus_editor(window, cx));
        } else if let Some(report) = self.report_tabs.get(&tab) {
            report.view.update(cx, |view, cx| view.focus(window, cx));
        }
    }

    /// Ctrl+Tab and Ctrl+Shift+Tab (or Ctrl+Page Down and Ctrl+Page Up):
    /// the next or previous query tab, from wherever the keyboard is.
    fn step_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection_open
            || self.command_open
            || self.password_prompt.is_some()
            || self.dialog_open()
        {
            return;
        }
        let next = {
            let tabs = self.tabs.read(cx);
            let ids: Vec<TabId> = tabs.tab_ids().collect();
            let current = tabs
                .active_tab()
                .and_then(|active| ids.iter().position(|id| *id == active));
            current.map(|current| ids[jykre::palette::step(current, ids.len(), delta)])
        };
        if let Some(tab) = next {
            self.show_tab(tab, window, cx);
        }
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
        // A tab whose connection is gone keeps that name, so it is saved and
        // shown as such.
        let connection = self
            .connections
            .iter()
            .find(|c| c.group == group)
            .map(|c| c.name.clone())
            .or_else(|| {
                self.orphan_groups
                    .iter()
                    .find(|(name, orphan)| *orphan == group && !name.is_empty())
                    .map(|(name, _)| name.clone())
            })
            .unwrap_or_else(|| tr!("Without connection").to_owned());
        let panel = cx.new(|cx| {
            Workspace::new(
                title,
                &connection,
                sql,
                self.assistant_split.clone(),
                window,
                cx,
            )
        });
        let catalogs = self.sql_catalogs.clone();
        panel.update(cx, |panel, cx| panel.set_catalogs(catalogs, cx));
        if let Some((input, row_limit)) = self
            .connections
            .iter()
            .find(|connection| connection.group == group)
            .and_then(|connection| Some((connection.input.clone()?, connection.row_limit)))
        {
            panel.update(cx, |panel, cx| panel.set_connection(input, row_limit, cx));
        }
        if let Some(connection) = self
            .connections
            .iter()
            .find(|connection| connection.group == group && connection.input.is_some())
        {
            let (color, environment) = (connection.color, connection.appearance.environment);
            panel.update(cx, |panel, cx| panel.set_appearance(color, environment, cx));
        }
        let subscription = cx.subscribe_in(
            &panel,
            window,
            |this, panel_entity, event: &WorkspaceEvent, window, cx| {
                match event {
                    // A new query is named and tagged first, in a dialog
                    // that starts from the tab's title.
                    WorkspaceEvent::Saved {
                        id: None,
                        title,
                        connection,
                        sql,
                    } => {
                        this.open_details(
                            saved_query_dialog::QueryDetails::New {
                                panel: panel_entity.downgrade(),
                                connection: connection.clone(),
                                sql: sql.clone(),
                            },
                            title,
                            &[],
                            window,
                            cx,
                        );
                    }
                    // A saved query is found by its identity, never by its
                    // title, so two tabs with the same title never replace
                    // each other's query.
                    WorkspaceEvent::Saved {
                        id: Some(id),
                        title,
                        connection,
                        sql,
                    } => {
                        let id = jykre::workspace_state::save_query(
                            &mut this.saved_queries,
                            &mut this.next_saved_id,
                            Some(*id),
                            title,
                            &[],
                            connection,
                            sql,
                        );
                        let sql = sql.clone();
                        panel_entity.update(cx, |panel, cx| panel.saved(id, sql, cx));
                        this.filter_library(cx);
                    }
                    WorkspaceEvent::Executed(entry) => {
                        jykre::workspace_state::WorkspaceState::record(
                            &mut this.history,
                            entry.clone(),
                        );
                        if this.section == Section::History {
                            this.filter_library(cx);
                        }
                    }
                    WorkspaceEvent::Changed => {}
                    WorkspaceEvent::SaveAsReport {
                        title,
                        connection,
                        sql,
                        context,
                    } => {
                        let (title, connection, sql) =
                            (title.clone(), connection.clone(), sql.clone());
                        this.report_from_query(
                            &title,
                            &connection,
                            &sql,
                            context.clone(),
                            window,
                            cx,
                        );
                    }
                    WorkspaceEvent::AddToReport {
                        connection,
                        sql,
                        context,
                    } => {
                        let connection = connection.clone();
                        this.open_report_picker(
                            &connection,
                            sql.clone(),
                            context.clone(),
                            window,
                            cx,
                        );
                    }
                    WorkspaceEvent::ConfirmRisk {
                        connection,
                        description,
                    } => {
                        let panel = panel_entity.downgrade();
                        confirm(
                            window,
                            cx,
                            Confirmation {
                                title: tr!("Run on {0}?", connection),
                                description: description.clone(),
                                ok: tr!("Run"),
                                cancel: tr!("Cancel"),
                                focus: ConfirmFocus::Dialog,
                            },
                            move |_, cx| {
                                let _ = panel.update(cx, |panel, cx| panel.confirm_risk(cx));
                            },
                        );
                    }
                    WorkspaceEvent::PasswordRequired => {
                        let name = panel_entity.read(cx).connection_name().to_owned();
                        if let Some(index) = this.connections.iter().position(|c| c.name == name) {
                            let resume = panel_entity.clone();
                            let cancel = panel_entity.clone();
                            this.ask_password(
                                index,
                                Box::new(move |_, _, cx| {
                                    resume.update(cx, |panel, cx| panel.resume_pending(cx))
                                }),
                                Box::new(move |_, cx| {
                                    cancel.update(cx, |panel, _| panel.cancel_pending())
                                }),
                                cx,
                            );
                        }
                    }
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
                    WorkspaceEvent::StructureChanged {
                        connection,
                        namespaces,
                    } => {
                        if let Some(index) =
                            this.connections.iter().position(|c| c.name == *connection)
                        {
                            this.refresh_after_ddl(index, namespaces.clone(), cx);
                        }
                    }
                    WorkspaceEvent::Reveal { connection, path } => {
                        if let Some(index) =
                            this.connections.iter().position(|c| c.name == *connection)
                        {
                            this.reveal_object(index, path.clone(), cx);
                        }
                    }
                }
                this.save_state_soon(cx);
                cx.notify();
            },
        );
        self._subscriptions.push(subscription);
        // The tab shows the run in progress and how the last one ended. The
        // tab bar redraws only when that changes, not on every keystroke.
        let status = cx.observe(&panel, |this, panel, cx| {
            let status = panel.read(cx).tab_status();
            if let Some(tab) = this.tab_of(&panel) {
                let _ = this
                    .tabs
                    .update(cx, |tabs, cx| tabs.set_tab_status(tab, status, cx));
            }
        });
        self._subscriptions.push(status);
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
        // A new query starts empty, with a placeholder: nothing to select
        // and delete before typing.
        // The new tab takes the keyboard, so its shortcuts (run, close) work
        // at once.
        if let Some(tab) = self.add_query(group, &title, "", window, cx)
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
        // A new or moved tab offers its connection's namespaces too.
        for index in 0..self.connections.len() {
            self.push_context_choices(index, cx);
        }
        self.sync_reports(cx);
    }

    /// Closes a tab, asking first when a query is running in it (closing
    /// cancels it), a transaction is open (closing rolls it back) or its SQL
    /// is not saved (closing loses it).
    fn request_close(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(report) = self.report_tabs.get(&tab) {
            let view = report.view.read(cx);
            let (busy, unsaved) = (view.is_busy(), view.has_unsaved_changes(cx));
            self.request_report_close(tab, busy, unsaved, window, cx);
            return;
        }
        let (busy, unsaved, transaction, exporting) =
            self.panels
                .get(&tab)
                .map_or((false, false, false, false), |panel| {
                    let panel = panel.read(cx);
                    (
                        panel.is_busy(),
                        panel.has_unsaved_sql(),
                        panel.has_open_transaction(),
                        panel.is_exporting(),
                    )
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
        if exporting {
            let description = if unsaved {
                tr!(
                    "Closing the tab cancels the export, which leaves no file, and loses its SQL, which is not in the saved queries."
                )
            } else {
                tr!("Closing the tab cancels the export, which leaves no file.")
            };
            let description = if transaction {
                format!(
                    "{description} {}",
                    tr!("The open transaction is rolled back.")
                )
            } else {
                description.to_owned()
            };
            confirm(
                window,
                cx,
                Confirmation {
                    title: tr!("An export is running in this tab.").to_string(),
                    description: description.to_string(),
                    ok: tr!("Close and cancel"),
                    cancel: tr!("Keep the tab"),
                    focus: ConfirmFocus::Dialog,
                },
                move |window, cx| close(Some(window), cx),
            );
            return;
        }
        if transaction && !busy {
            let description = if unsaved {
                tr!(
                    "Closing the tab rolls back the open transaction and loses its SQL, which is not in the saved queries."
                )
            } else {
                tr!("Closing the tab rolls back the open transaction.")
            };
            confirm(
                window,
                cx,
                Confirmation {
                    title: tr!("A transaction is open in this tab.").to_string(),
                    description: description.to_string(),
                    ok: tr!("Close and roll back"),
                    cancel: tr!("Keep the tab"),
                    focus: ConfirmFocus::Dialog,
                },
                move |window, cx| close(Some(window), cx),
            );
            return;
        }
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
            Confirmation {
                title,
                description,
                ok,
                cancel: tr!("Keep the tab"),
                focus: ConfirmFocus::Dialog,
            },
            move |window, cx| close(Some(window), cx),
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
        if self.report_tabs.contains_key(&tab) {
            // A report keeps its definition on any connection; the move
            // itself switches it ([`AppView::report_moved`]).
            let _ = self
                .tabs
                .update(cx, |tabs, cx| tabs.move_tab(tab, group, index, cx));
            return;
        }
        let Some(panel) = self.panels.get(&tab) else {
            return;
        };
        let (from, busy, transaction) = {
            let panel = panel.read(cx);
            (
                panel.connection_name().to_owned(),
                panel.is_busy(),
                panel.has_open_transaction(),
            )
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
        let detail = if transaction {
            format!("{detail} {}", tr!("The open transaction is rolled back."))
        } else {
            detail
        };
        let tabs = self.tabs.downgrade();
        confirm(
            window,
            cx,
            Confirmation {
                title: message,
                description: detail,
                ok: tr!("Switch connection"),
                cancel: tr!("Cancel"),
                focus: ConfirmFocus::Dialog,
            },
            move |_, cx| {
                let _ = tabs.update(cx, |tabs, cx| tabs.move_tab(tab, group, index, cx));
            },
        );
    }

    /// A run shortcut on the active tab's workspace, unless a dialog or the
    /// command palette is in front of it. It never cancels by itself: while
    /// a run is active it asks first, with Yes focused so Enter confirms.
    /// `all` is Ctrl+Alt+Enter, whose confirmation stops the whole run.
    fn run_shortcut(
        &mut self,
        run: fn(&mut Workspace, &mut Context<Workspace>),
        all: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connection_open
            || self.command_open
            || self.password_prompt.is_some()
            || self.dialog_open()
        {
            return;
        }
        let active = self.tabs.read(cx).active_tab();
        if let Some(report) = active.and_then(|tab| self.report_tabs.get(&tab)) {
            // A report runs the panel whose SQL has the keyboard, or all of
            // them; Ctrl+Alt+Enter always runs all of them.
            report.view.update(cx, |view, cx| {
                if all {
                    view.run(jykre::reports::run::RunScope::All, cx);
                } else {
                    view.run_shortcut(window, cx);
                }
            });
            return;
        }
        let Some(panel) = active.and_then(|tab| self.panels.get(&tab)).cloned() else {
            return;
        };
        if panel.read(cx).shortcut() != ShortcutAction::ConfirmCancel {
            // Running, or telling that the previous run is still stopping.
            panel.update(cx, run);
            return;
        }
        let (title, description) = if all {
            (
                tr!("Stop the whole run?"),
                tr!(
                    "Every statement of this tab is running in order. Stopping interrupts the one on the server and skips the ones still waiting; statements that finished keep their results."
                ),
            )
        } else {
            (
                tr!("Cancel the running query?"),
                tr!(
                    "A query is still running in this tab. Cancelling interrupts it on the server; statements that finished keep their results."
                ),
            )
        };
        let panel = panel.downgrade();
        confirm(
            window,
            cx,
            Confirmation {
                title: title.to_string(),
                description: description.to_string(),
                ok: tr!("Yes, cancel"),
                cancel: tr!("No, keep running"),
                focus: ConfirmFocus::Ok,
            },
            move |_, cx| {
                let _ = panel.update(cx, |panel, cx| panel.stop(cx));
            },
        );
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
                    this.filter_library(cx);
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
                    (Section::Reports, I::FileChartColumn, tr!("Reports")),
                    (Section::History, I::Clock, tr!("History")),
                ]
                .into_iter()
                .enumerate()
                .map(|(i, (section, name, title))| self.rail_item(i, section, name, title, cx))
                .collect::<Vec<_>>(),
            )
            .child(div().flex_1())
            .child(self.rail_item(4, Section::Settings, I::Settings2, tr!("Preferences"), cx))
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
            Section::Reports => tr!("Reports"),
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
                            .when(self.section == Section::Reports, |s| {
                                s.child(
                                    icon_button("add-report", I::Plus, tr!("New report"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.new_report(window, cx)
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
                .when_some(self.config_backup.as_ref(), |s, (_, taken)| {
                    let age = jykre::explorer::format_age(
                        std::time::SystemTime::now()
                            .duration_since(*taken)
                            .unwrap_or_default(),
                    );
                    s.child(
                        column()
                            .id("config-backup")
                            .role(Role::Status)
                            .mx_3()
                            .mb_2()
                            .p_2()
                            .gap_2()
                            .rounded(px(6.))
                            .bg(rgb(SURFACE))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(MUTED))
                                    .child(tr!(
                                        "A copy of the configuration, taken {0}, is available. Restoring it keeps the damaged file next to it.",
                                        age
                                    )),
                            )
                            .child(
                                button(
                                    "restore-config-backup",
                                    I::ArchiveRestore,
                                    tr!("Restore the backup"),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.restore_config_backup(window, cx)
                                })),
                            ),
                    )
                })
                .child(
                    column()
                        .id("connection-tree")
                        .flex_1()
                        .min_h_0()
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
                        .child({
                            let view = cx.entity();
                            list(self.explorer_list.clone(), move |entry, _, cx| {
                                view.update(cx, |this, cx| this.explorer_entry(entry, cx))
                            })
                            .flex_1()
                            .pb_3()
                        })
                        .vertical_scrollbar(&self.explorer_list),
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
                .child(icon(I::Server, connection.color))
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
                                .child(row().id(("schema-table", table_index)).role(Role::Button).aria_label(tr!("New query for table {0}", table)).focusable().tab_index(0).focus_visible(|s| s.bg(rgb(HOVER))).h(px(32.)).pl(px(71.)).pr_3().gap_2().cursor_pointer()
                                    .when(table == "orders", |s| s.bg(rgb(0x25352f)))
                                    .hover(|s| s.bg(rgb(HOVER)))
                                    .child(icon(if table == "orders" && self.orders_open { I::ChevronDown } else { I::ChevronRight }, FAINT))
                                    .child(icon(I::Table2, if table == "orders" { MINT } else { FAINT }))
                                    .child(div().text_size(px(14.)).text_color(rgb(if table == "orders" { MINT } else { MUTED })).child(table))
                                    // As in the real explorer: a click selects (orders also
                                    // shows its columns); a double click or Enter opens.
                                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                        if event.is_keyboard() || event.click_count() >= 2 { this.open_table(table, window, cx); }
                                        else if table == "orders" { this.orders_open = !this.orders_open; cx.notify(); }
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

    /// One entry of the library list: a saved query (its title, connection
    /// and the start of its SQL, with a button to remove it) or a run of the
    /// history (the start of its SQL, how it ended, when and how long it
    /// took).
    fn library_entry(
        &self,
        position: usize,
        saved: bool,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        let index = *self.library_shown.get(position)?;
        let entry = if saved {
            self.saved_queries.get(index)?
        } else {
            self.history.get(index)?
        }
        .clone();
        let preview = entry.preview(80);
        let (symbol, color, headline, detail) = if saved {
            let tags: String = entry.tags.iter().map(|tag| format!("#{tag} ")).collect();
            (
                I::Bookmark,
                MINT,
                entry.title.clone(),
                format!("{tags}{} · {preview}", entry.connection),
            )
        } else {
            use jykre::workspace_state::RunStatus;
            let (symbol, color) = match entry.status {
                Some(RunStatus::Failed) => (I::CircleX, RED),
                Some(RunStatus::Cancelled) => (I::Ban, AMBER),
                Some(RunStatus::Succeeded) => (I::CircleCheck, MINT),
                None => (I::Clock, FAINT),
            };
            let mut detail = vec![entry.connection.clone()];
            if let Some(at) = entry.time() {
                detail.push(jykre::explorer::format_age(
                    std::time::SystemTime::now()
                        .duration_since(at)
                        .unwrap_or_default(),
                ));
            }
            if let Some(elapsed) = entry.elapsed_ms {
                detail.push(jykre::cell_format::duration(elapsed));
            }
            if let Some(outcome) = &entry.detail {
                detail.push(outcome.clone());
            }
            (symbol, color, preview, detail.join(" · "))
        };
        let id = entry.id;
        let label_text = if saved {
            tr!("Open the saved query {0}", entry.title)
        } else {
            tr!("Open {0}", entry.preview(200))
        };
        Some(
            row()
                .id(("library-entry", position))
                // The list's width, so the text truncates before the
                // buttons instead of pushing them out of sight.
                .w_full()
                .h(px(LIBRARY_ROW_HEIGHT))
                .gap_1()
                .child(
                    column()
                        .id("open")
                        .role(Role::Button)
                        .aria_label(label_text)
                        .focusable()
                        .tab_index(0)
                        .focus_visible(|s| s.bg(rgb(HOVER)))
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .px_2()
                        .justify_center()
                        .gap_1()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(HOVER)))
                        .child(
                            row().min_w_0().child(icon(symbol, color)).child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(14.))
                                    .when(!saved, |s| {
                                        s.font_family("Cascadia Code").text_size(px(13.))
                                    })
                                    .child(headline),
                            ),
                        )
                        .child(
                            div()
                                .pl_6()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(FAINT))
                                .child(detail),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if saved {
                                this.open_saved_query(entry.clone(), window, cx);
                            } else {
                                this.open_entry(&entry, window, cx);
                            }
                        })),
                )
                // Removing is its own control, with a confirmation; the
                // save button in the editor never removes.
                .when(saved, |s| {
                    s.child(
                        icon_button("edit", I::Pencil, tr!("Rename or edit tags")).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.edit_saved_query(id, window, cx)
                            }),
                        ),
                    )
                    .child(
                        icon_button("remove", I::Trash, tr!("Remove from saved queries")).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.remove_saved_query(id, window, cx)
                            }),
                        ),
                    )
                }),
        )
    }

    fn sidebar_content(&self, cx: &mut Context<Self>) -> Div {
        column()
            .flex_1()
            .min_h_0()
            .p_3()
            .gap_2()
            .when(
                self.section == Section::Saved || self.section == Section::History,
                |s| {
                    let saved = self.section == Section::Saved;
                    let empty = if saved {
                        self.saved_queries.is_empty()
                    } else {
                        self.history.is_empty()
                    };
                    s.child(label(if saved {
                        tr!("Your library")
                    } else {
                        tr!("Recent runs")
                    }))
                    .when(!empty, |s| {
                        s.child(
                            Input::new(&self.library_search)
                                .small()
                                .prefix(I::Search)
                                .w_full()
                                .aria_label(if saved {
                                    tr!("Search the saved queries")
                                } else {
                                    tr!("Search the history")
                                }),
                        )
                    })
                    .when(saved, |s| {
                        let tags = jykre::workspace_state::all_tags(&self.saved_queries);
                        s.when(!tags.is_empty(), |s| {
                            s.child(
                                div()
                                    .id("library-tags")
                                    .role(Role::Group)
                                    .aria_label(tr!("Tags"))
                                    .flex()
                                    .flex_wrap()
                                    .gap_1()
                                    .max_h(px(64.))
                                    .overflow_y_scroll()
                                    .children(tags.into_iter().enumerate().map(
                                        |(position, tag)| self.library_tag_chip(position, tag, cx),
                                    )),
                            )
                        })
                    })
                    .map(|s| {
                        if empty {
                            s.child(div().p_3().text_size(px(14.)).text_color(rgb(MUTED)).child(
                                if saved {
                                    tr!("Save a query with the bookmark icon in the editor.")
                                } else {
                                    tr!("Queries you run appear here.")
                                },
                            ))
                        } else if self.library_shown.is_empty() {
                            s.child(
                                div()
                                    .p_3()
                                    .text_size(px(14.))
                                    .text_color(rgb(MUTED))
                                    .child(tr!("Nothing matches the search.")),
                            )
                        } else {
                            // Virtualized: only the entries on screen are built.
                            s.child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .min_h_0()
                                    .child(
                                        uniform_list(
                                            "library-entries",
                                            self.library_shown.len(),
                                            cx.processor(
                                                move |this, range: Range<usize>, _, cx| {
                                                    range
                                                        .filter_map(|position| {
                                                            this.library_entry(position, saved, cx)
                                                        })
                                                        .collect::<Vec<_>>()
                                                },
                                            ),
                                        )
                                        .size_full()
                                        .track_scroll(&self.library_scroll),
                                    )
                                    .vertical_scrollbar(&self.library_scroll),
                            )
                        }
                    })
                },
            )
            .when(self.section == Section::Reports, |s| {
                self.reports_list(s, cx)
            })
            .when(self.section == Section::Settings, |s| {
                s.child(self.preferences(cx))
            })
    }

    /// Hands the SQL catalogs to every tab, and to the ones opened later.
    fn set_sql_catalogs(
        &mut self,
        catalogs: std::sync::Arc<sql_editor::SqlCatalogs>,
        cx: &mut Context<Self>,
    ) {
        for panel in self.panels.values() {
            let catalogs = catalogs.clone();
            panel.update(cx, |panel, cx| panel.set_catalogs(catalogs, cx));
        }
        for report in self.report_tabs.values() {
            let catalogs = catalogs.clone();
            report
                .view
                .update(cx, |view, cx| view.set_catalogs(catalogs, cx));
        }
        self.sql_catalogs = catalogs;
        cx.notify();
    }

    /// Which SQL catalogs the editor uses, for Settings.
    fn sql_catalog_status(&self) -> Div {
        let loaded: Vec<String> = self
            .sql_catalogs
            .loaded()
            .map(|(_, catalog)| match &catalog.version {
                Some(version) => format!("{} {version}", catalog.name),
                None => catalog.name.clone(),
            })
            .collect();
        let text = if loaded.is_empty() {
            tr!("Not found: built-in keywords only").to_owned()
        } else {
            loaded.join(" · ")
        };
        row()
            .justify_between()
            .child(label(tr!("SQL catalogs")))
            .child(label(text))
    }

    fn preferences(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        column().id("appearance-preferences").overflow_y_scroll().gap_3()
            .child(label(tr!("Appearance")))
            .child(row().justify_between().child(label(tr!("Theme"))).child(badge("Graphite", MINT)))
            .child(row().justify_between().child(label(tr!("SQL font"))).child(label("Cascadia Code")))
            .child(row().justify_between().child(label(tr!("SQL size"))).child(badge(format!("{} px", f32::from(cx.theme().mono_font_size)), MINT)))
            .child(self.sql_catalog_status())
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
                (tr!("Assistant"), "Ctrl Shift B"), (tr!("Next or previous tab"), "Ctrl Tab / Ctrl Shift Tab"),
                (tr!("SQL font"), "Ctrl + / −"), (tr!("Reset font"), "Ctrl 0"),
            ].into_iter().map(|(title, key)| row().justify_between().child(label(title)).child(shortcut(key))))
            .child(div().mt_3().child(label(tr!("Diagnostics"))))
            .child(label(tr!("The log keeps the last 7 days, without passwords, SQL or values.")))
            .child(button("open-log-folder", I::FolderOpen, tr!("Open log folder"))
                .on_click(cx.listener(|this, _, _, cx| this.open_log_folder(cx))))
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

    fn command_palette(&self, cx: &mut Context<Self>) -> AnyElement {
        let matches = &self.command_matches;
        // A search in progress says so even while the previous matches are
        // still listed, and they are dimmed: they may not match the text.
        let pending = self.command_searching && !matches.is_empty();
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
                        row()
                            .px_4()
                            .pt_4()
                            .pb_2()
                            .justify_between()
                            .child(label(tr!("Saved queries · tables and views")))
                            .when(pending, |s| {
                                s.child(
                                    row()
                                        .id("command-pending")
                                        .role(Role::Status)
                                        .aria_label(tr!("Searching…"))
                                        .gap_1()
                                        .child(spinning_icon(
                                            "command-pending-spinner",
                                            I::LoaderCircle,
                                            AMBER,
                                        ))
                                        .child(label(tr!("Searching…"))),
                                )
                            }),
                    )
                    .child(
                        column()
                            .id("command-results")
                            .role(Role::ListBox)
                            .aria_label(tr!("Saved queries · tables and views"))
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .track_scroll(&self.command_scroll)
                            .px_2()
                            .pb_3()
                            .when(pending, |s| s.opacity(0.6))
                            .when(matches.is_empty(), |s| {
                                s.child(div().p_5().child(label(if self.command_searching {
                                    tr!("Searching…")
                                } else if self.connections.is_empty() && self.saved_queries.is_empty() {
                                    tr!("No connections yet. Add one to search its tables.")
                                } else {
                                    tr!("No objects found. Refresh the connection metadata.")
                                })))
                            })
                            .children(matches.iter().enumerate().map(|(index, item)| {
                                self.palette_row(index, item, cx)
                            })),
                    )
                    .child(
                        row()
                            .h(px(40.))
                            .px_4()
                            .justify_between()
                            .bg(rgb(BG))
                            .border_t_1()
                            .border_color(rgb(BORDER))
                            .child(label(tr!(
                                "↑ ↓ choose · Enter opens the saved query or a new one for the table · #tag filters"
                            )))
                            .child(row().child(shortcut("Esc")).child(label(tr!("close")))),
                    )
                    // Tab cycles through the palette while it is open.
                    .focus_trap("command-focus-trap", &self.command_focus),
            )
            .into_any_element()
    }

    /// One row of the palette. The first row of each kind carries its
    /// heading, so the rows stay the list's children one for one and the
    /// arrows scroll to the right one.
    fn palette_row(&self, index: usize, item: &PaletteItem, cx: &mut Context<Self>) -> Div {
        let selected = index == self.command_selected;
        let previous = index
            .checked_sub(1)
            .and_then(|at| self.command_matches.get(at));
        let first_of_kind = matches!(
            (previous, item),
            (None, _) | (Some(PaletteItem::Saved(_)), PaletteItem::Relation(_))
        );
        let heading = first_of_kind.then(|| match item {
            PaletteItem::Saved(_) => tr!("Saved queries"),
            PaletteItem::Relation(_) => tr!("Tables and views · connections"),
        });
        let option = |id: (&'static str, usize)| {
            row()
                .id(id)
                .role(Role::ListBoxOption)
                .aria_selected(selected)
                .focusable()
                .tab_index(0)
                .when(selected, |s| s.bg(rgb(SURFACE)))
                .focus_visible(|s| s.bg(rgb(HOVER)))
                .h(px(43.))
                .px_3()
                .rounded(px(5.))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(HOVER)))
        };
        let text = |name: String, detail: String| {
            row()
                .flex_1()
                .min_w_0()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .max_w(px(260.))
                        .truncate()
                        .text_size(px(14.))
                        .child(name),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(rgb(FAINT))
                        .child(detail),
                )
        };
        let open = {
            let item = item.clone();
            cx.listener(move |this, _, window, cx| this.open_palette_item(item.clone(), window, cx))
        };
        let row_element = match item {
            PaletteItem::Saved(id) => {
                let Some(entry) = self.saved_queries.iter().find(|entry| entry.id == *id) else {
                    return div();
                };
                let tags = entry
                    .tags
                    .iter()
                    .map(|tag| format!("#{tag}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                option(("command-saved", index))
                    .aria_label(tr!("Open the saved query {0}", entry.title))
                    .child(icon(I::Bookmark, MINT))
                    .child(text(entry.title.clone(), tags))
                    .child(label(entry.connection.clone()))
                    .child(icon(I::CornerDownLeft, FAINT))
                    .on_click(open)
            }
            PaletteItem::Relation(found) => {
                let name = found
                    .path
                    .0
                    .last()
                    .map(|part| part.name.clone())
                    .unwrap_or_default();
                // Where it is, so objects of the same name in other schemas
                // tell apart.
                let location = found.path.0[..found.path.0.len().saturating_sub(1)]
                    .iter()
                    .map(|part| part.name.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                let connection = self
                    .connections
                    .get(found.connection)
                    .map(|connection| connection.name.clone())
                    .unwrap_or_default();
                option(("command-table", index))
                    .aria_label(tr!(
                        "New query for table {0} in {1}, {2}",
                        name,
                        location,
                        connection
                    ))
                    .child(icon(I::Table2, MINT))
                    .child(text(name, location))
                    .child(label(connection))
                    .child(icon(I::CornerDownLeft, FAINT))
                    .on_click(open)
            }
        };
        column()
            .when_some(heading, |s, heading| {
                s.child(div().px_3().pt_2().pb_1().child(label(heading)))
            })
            .child(row_element)
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
                    if this.inspection.take().is_some() {
                        cx.notify();
                    }
                }),
            )
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        probe::frame_started();
        probe::rendered("AppView");
        self.sync_explorer(cx);
        if std::mem::take(&mut self.details_needs_focus) {
            self.details_name
                .update(cx, |input, cx| input.focus(window, cx));
        }
        if let Some(picker) = self.report_picker.as_mut()
            && std::mem::take(&mut picker.needs_focus)
        {
            let search = picker.search().clone();
            search.update(cx, |input, cx| input.focus(window, cx));
        }
        if std::mem::take(&mut self.password_needs_focus) {
            self.password_input
                .update(cx, |input, cx| input.focus(window, cx));
        }
        column()
            .id("jykre")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Segoe UI")
            .text_size(px(14.))
            .on_action(cx.listener(|this, _: &ExecuteCurrentQuery, window, cx| {
                this.run_shortcut(Workspace::execute_current, false, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &ExecuteCurrentQueryInNewResult, window, cx| {
                    this.run_shortcut(Workspace::execute_current_in_new_result, false, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &ExecuteAllQueries, window, cx| {
                this.run_shortcut(Workspace::execute_all, true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.step_tab(1, window, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousTab, window, cx| this.step_tab(-1, window, cx)),
            )
            .on_action(cx.listener(|this, _: &CloseActiveTab, window, cx| {
                if this.connection_open
                    || this.command_open
                    || this.password_prompt.is_some()
                    || this.dialog_open()
                {
                    return;
                }
                if let Some(tab) = this.tabs.read(cx).active_tab() {
                    this.request_close(tab, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ExportResult, _, cx| {
                if this.connection_open
                    || this.command_open
                    || this.password_prompt.is_some()
                    || this.dialog_open()
                {
                    return;
                }
                if let Some(panel) = this
                    .tabs
                    .read(cx)
                    .active_tab()
                    .and_then(|tab| this.panels.get(&tab))
                    .cloned()
                {
                    panel.update(cx, Workspace::toggle_export);
                }
            }))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    if this.password_prompt.is_some() {
                        this.cancel_password(window, cx);
                    } else if this.details_open() {
                        this.cancel_details(window, cx);
                    } else if this.report_picker.is_some() {
                        this.close_report_picker(window, cx);
                    } else if this.inspection.take().is_some() {
                        cx.notify();
                    } else if this.command_open {
                        this.command_open = false;
                        cx.notify();
                    } else {
                        this.close_connection_dialog(cx);
                    }
                } else if this.command_open
                    && matches!(event.keystroke.key.as_str(), "up" | "down")
                    && !event.keystroke.modifiers.modified()
                {
                    // The arrows move the highlighted match; the text field
                    // keeps every other key.
                    let delta = if event.keystroke.key == "up" { -1 } else { 1 };
                    this.command_selected = jykre::palette::step(
                        this.command_selected,
                        this.command_matches.len(),
                        delta,
                    );
                    this.command_scroll.scroll_to_item(this.command_selected);
                    cx.notify();
                    window.prevent_default();
                    cx.stop_propagation();
                } else if this.connection_open
                    || this.password_prompt.is_some()
                    || this.dialog_open()
                {
                    // Workspace shortcuts would act behind the modal form.
                } else if event.keystroke.modifiers.control
                    && event.keystroke.modifiers.shift
                    && event.keystroke.key.eq_ignore_ascii_case("p")
                {
                    this.command_open = !this.command_open;
                    if this.command_open {
                        this.command_selected = 0;
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
            .when(self.inspection.is_some(), |s| {
                s.child(self.inspection_panel(cx))
            })
            .when(self.connection_open, |s| {
                s.child(self.connection_dialog(cx))
            })
            .when(self.password_prompt.is_some(), |s| {
                s.child(self.password_dialog(cx))
            })
            .when(self.details_open(), |s| s.child(self.details_dialog(cx)))
            .when(self.report_picker.is_some(), |s| {
                s.child(self.report_picker_dialog(cx))
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
            // Suggestions on demand, also with nothing typed yet.
            KeyBinding::new("ctrl-space", ShowCompletions, Some(&editor)),
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
            KeyBinding::new("ctrl-tab", NextTab, None),
            KeyBinding::new("ctrl-pagedown", NextTab, None),
            KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
            KeyBinding::new("ctrl-pageup", PreviousTab, None),
            KeyBinding::new("ctrl-tab", NextTab, Some(&editor)),
            KeyBinding::new("ctrl-pagedown", NextTab, Some(&editor)),
            KeyBinding::new("ctrl-shift-tab", PreviousTab, Some(&editor)),
            KeyBinding::new("ctrl-pageup", PreviousTab, Some(&editor)),
            KeyBinding::new("ctrl-shift-e", ExportResult, None),
            KeyBinding::new("ctrl-shift-e", ExportResult, Some(&editor)),
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
