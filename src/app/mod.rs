use jykre::tr;
mod assistant_panel;
mod assistant_settings;
mod automation;
mod connections;
mod copying;
mod databricks_connection;
mod demo;
mod exit_writes;
mod favorites;
mod fonts;
mod import;
mod layout;
mod library_files;
mod password;
pub mod performance;
mod probe;
mod quick_search;
mod report_chart;
mod report_panel;
mod report_picker;
mod report_view;
mod reports;
mod result_grid;
mod saved_query_dialog;
mod scope_picker;
mod settings;
mod shortcut_editor;
mod shortcuts;
mod sql_editor;
mod state;
mod telemetry_settings;
mod theme;
mod ui;
mod updates;
mod workspace;

use connections::ExplorerEntry;
use demo::{ORDER_COLUMNS, SALES_SQL, TABLES, table_sql};
use gpui_kit::assets::IconName as I;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::{
    ActiveTheme, Sizable, Theme, TitleBar, WindowExt as _,
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
        /// Ctrl+F2: stops the run of the tab on screen, as Stop does: a query
        /// tab's statements or a report's panels. It never asks first.
        CancelRun,
        /// Ctrl+Shift+T: reopens the query tab closed last in this session,
        /// with its SQL, title, connection, context and `:name` fields; it
        /// runs nothing.
        ReopenClosedTab,
        /// Ctrl+O: opens `.sql` files chosen in the system dialog.
        OpenFile,
        /// Ctrl+S: saves the query tab on screen to its file, asking where
        /// first when it has none.
        SaveFile,
        /// Ctrl+Shift+S: saves the query tab on screen to a file chosen now.
        SaveFileAs,
        /// Ctrl+W or Ctrl+F4: closes the query tab on screen, asking first
        /// when it is running or has SQL that is not saved.
        CloseActiveTab,
        /// Ctrl+Tab or Ctrl+Page Down: the next query tab, from anywhere.
        NextTab,
        /// Ctrl+Shift+Tab or Ctrl+Page Up: the previous query tab.
        PreviousTab,
        /// Ctrl+Shift+E: opens or closes the export bar of the result on
        /// screen.
        ExportResult,
        /// Ctrl+/ in the SQL editor: comments the selected lines (or the
        /// caret's line) with `--`, or uncomments them.
        ToggleComment,
        /// Ctrl+G in the SQL editor: asks for a line, and optionally a
        /// column, and moves the cursor there.
        GoToLine,
        /// Shift+F10 in the explorer: the selected node's context menu.
        ExplorerContextMenu,
        /// Ctrl+F in the explorer: the quick search marks its matches or
        /// shows only them.
        ToggleExplorerFilter,
        /// Ctrl+T: a new query tab on the connection of the tab on screen;
        /// without one, a tab that asks for a connection.
        NewTab,
        /// Ctrl+,: opens Preferences.
        ShowPreferences,
        /// Shift+F10 in a result grid: its menu on the focused header's
        /// column, or the focused cell's, to hide, freeze or move it.
        ResultGridMenu,
        /// Ctrl+Shift+F: the field that finds a column by name in the
        /// result grid that has the keyboard, else in the query tab's result
        /// on screen.
        FindColumn
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
    Automation,
    History,
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
    color: jykre::connection_appearance::ConnectionColor,
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
    /// The automation engine, its jobs and tabs ([`automation`]).
    automation: automation::Host,
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
    /// Query tabs closed in this session, for **Reopen closed query tab**.
    closed_tabs: jykre::closed_tabs::ClosedTabs,
    /// What is local about the saved-query files: identities, tags,
    /// connections, the directory (ADR-0031).
    query_library: jykre::query_files::library::QueryLibrary,
    /// The local library state was read, so it may be written.
    persist_library: bool,
    library_revision: u64,
    /// The scan of the library's directory in progress.
    library_scan: Option<CancellationToken>,
    library_scanned_at: Option<std::time::Instant>,
    /// The files the last scan found.
    library_found: Vec<jykre::query_files::scan::Found>,
    /// The last scan stopped at a bound.
    library_stopped: bool,
    /// Library entries whose file the last scan did not find.
    library_missing: std::collections::HashSet<u64>,
    /// The subfolder of the library's folder that holds a file, by entry,
    /// so two files with the same name stay apart in the list.
    library_folders: HashMap<u64, String>,
    /// Saved queries of an earlier version still in `workspace.state`.
    unmigrated: Vec<QueryEntry>,
    /// The digest of each recovery file's text as last written.
    recovery_written: HashMap<String, u64>,
    /// Recovery files no tab needs any more, removed by the next state
    /// write: after any earlier write of the same file, never before it.
    recovery_removes: Vec<String>,
    /// State writes take turns, in the order they were taken.
    state_writes: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// File tabs shown as unsaved in the tab strip.
    unsaved_marked: std::collections::HashSet<TabId>,
    /// Quitting without saving the file tabs: their edits are dropped.
    discard_file_edits: bool,
    /// The question about unsaved files was answered for this close.
    files_settled: bool,
    /// The explorer's favorites, read at start with the connections.
    favorites: jykre::favorites::Favorites,
    /// Favorites are written only once they were read, so a value that
    /// could not be read is never written over.
    persist_favorites: bool,
    /// Tells the last change to the favorites from earlier ones still
    /// waiting to be written.
    favorites_revision: u64,
    favorites_scroll: UniformListScrollHandle,
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
    /// The Preferences dialog is open, on this category ([`settings`]).
    settings_open: bool,
    settings_page: settings::SettingsPage,
    /// Keeps Tab inside Preferences.
    settings_focus: FocusHandle,
    /// Preferences > Assistant (ADR-0028).
    assistant_settings: Entity<assistant_settings::AssistantSettingsView>,
    telemetry_settings: Entity<telemetry_settings::TelemetrySettingsView>,
    /// Preferences › Shortcuts (0.3.0 A12).
    shortcut_editor: Entity<shortcut_editor::ShortcutEditor>,
    /// Updates (ADR-0033): Preferences › About › Updates and the status
    /// bar's badge, which share the `updates::Updater`.
    update_settings: Entity<updates::UpdateSettingsView>,
    update_badge: Entity<updates::UpdateBadge>,
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
    /// The palette found more rows than it lists.
    command_more: bool,
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
    /// SQLite: the full path of the database file.
    connection_file: Entity<InputState>,
    /// SQLite: open the file read-only.
    sqlite_read_only: bool,
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
    /// The explorer's search is reading the caches; its field shows it.
    explorer_searching: bool,
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
    /// **Import from file…**, while open ([`import`]).
    import_dialog: Option<import::ImportHost>,
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
    /// The explorer's quick search, while open ([`quick_search`]).
    explorer_quick: Option<quick_search::QuickSearch>,
    /// Whether the quick search marks its matches or shows only them; kept
    /// from one search to the next.
    explorer_quick_mode: jykre::explorer::QuickMode,
    /// The quick search the explorer entries were last built for.
    explorer_quick_key: Option<(String, jykre::explorer::QuickMode)>,
    /// The explorer tree's own focus: it keeps the keyboard while a filter
    /// hides the selected node, so typing goes on.
    explorer_focus: FocusHandle,
    /// Where the selected explorer node was drawn this frame, for the
    /// context menu `Shift F10` opens.
    selected_bounds: std::rc::Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    /// The selection before the explorer search started, given back when
    /// the search is cleared and the node it chose is not shown.
    selection_before_search: Option<Option<(usize, ObjectPath)>>,
    /// Focus of each explorer node shown so far, so the arrow keys can move
    /// it. Rendering takes `&self`, hence the cell.
    node_focus: std::cell::RefCell<HashMap<(usize, ObjectPath), FocusHandle>>,
    /// The user confirmed closing the window although transactions were
    /// open; it closes without asking again.
    closing: bool,
    /// The question about open transactions was answered for this close;
    /// the one about active automation runs comes next.
    transactions_settled: bool,
    /// **Restart now** of an installed update: when the window closes after
    /// its questions, this executable starts. Any other close clears it.
    restart_to: Option<std::path::PathBuf>,
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
        let updater = cx.new(updates::Updater::new);
        let update_subscription =
            cx.subscribe_in(&updater, window, |this, _, event, window, cx| match event {
                updates::UpdaterEvent::OpenUpdates => {
                    this.open_settings(settings::SettingsPage::About, window, cx);
                }
                updates::UpdaterEvent::RestartRequested(path) => {
                    this.restart_to = Some(path.clone());
                    this.files_settled = false;
                    this.discard_file_edits = false;
                    this.transactions_settled = false;
                    this.close_window(window, cx);
                }
            });
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
                // A report's own group offers no new tab.
                GroupedTabsEvent::NewTabRequested(group)
                    if this.is_report_group(*group)
                        || this.automation.tab_groups().any(|own| own == *group) => {}
                GroupedTabsEvent::NewTabRequested(group) => this.new_query(*group, window, cx),
                GroupedTabsEvent::TabClosed(tab) => {
                    // What the tab keeps (never its results or session) can
                    // be reopened in this session.
                    if let Some(panel) = this.panels.remove(tab) {
                        let mut state = panel.read(cx).state();
                        // Closed with the user's agreement: its recovery file
                        // goes (reopening it brings the text from memory).
                        if let Some(name) = state.recovery.take() {
                            this.recovery_written.remove(&name);
                            this.recovery_removes.push(name);
                        }
                        this.closed_tabs.push(state);
                    }
                    this.unsaved_marked.remove(tab);
                    this.report_tabs.remove(tab);
                    this.automation.forget(*tab);
                    this.save_state_soon(cx);
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
                        .map_or((None, None), |connection| {
                            (Some(connection.color), connection.appearance.environment)
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
                this.search_changed(cx);
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
            automation: automation::Host::default(),
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
            closed_tabs: jykre::closed_tabs::ClosedTabs::default(),
            query_library: jykre::query_files::library::QueryLibrary::default(),
            persist_library: false,
            library_revision: 0,
            library_scan: None,
            library_scanned_at: None,
            library_found: Vec::new(),
            library_stopped: false,
            library_missing: std::collections::HashSet::new(),
            library_folders: HashMap::new(),
            unmigrated: Vec::new(),
            recovery_written: HashMap::new(),
            recovery_removes: Vec::new(),
            state_writes: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            unsaved_marked: std::collections::HashSet::new(),
            discard_file_edits: false,
            files_settled: false,
            favorites: jykre::favorites::Favorites::default(),
            persist_favorites: false,
            favorites_revision: 0,
            favorites_scroll: UniformListScrollHandle::new(),
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
            settings_open: false,
            settings_page: settings::SettingsPage::default(),
            settings_focus: cx.focus_handle(),
            assistant_settings: cx
                .new(|cx| assistant_settings::AssistantSettingsView::new(window, cx)),
            telemetry_settings: cx
                .new(|cx| telemetry_settings::TelemetrySettingsView::new(window, cx)),
            shortcut_editor: cx.new(shortcut_editor::ShortcutEditor::new),
            update_settings: cx.new(|cx| updates::UpdateSettingsView::new(updater.clone(), cx)),
            update_badge: cx.new(|cx| updates::UpdateBadge::new(updater, cx)),
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
            command_more: false,
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
            connection_file: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(tr!("Full path of an existing database file"))
            }),
            sqlite_read_only: false,
            connection_revalidate: cx
                .new(|cx| InputState::new(window, cx).placeholder(tr!("Never"))),
            connection_databricks: cx
                .new(|cx| databricks_connection::DatabricksForm::new(window, cx)),
            connection_scope: cx.new(|cx| scope_picker::ScopePicker::new(window, cx)),
            connection_status: None,
            app_notice: None,
            config_backup: None,
            command_searching: false,
            explorer_searching: false,
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
            import_dialog: None,
            library_tag: None,
            persist_state: false,
            state_revision: 0,
            orphan_groups: Vec::new(),
            explorer_selected: None,
            explorer_quick: None,
            explorer_quick_mode: jykre::explorer::QuickMode::default(),
            explorer_quick_key: None,
            explorer_focus: cx.focus_handle(),
            selected_bounds: Default::default(),
            selection_before_search: None,
            node_focus: std::cell::RefCell::new(HashMap::new()),
            closing: false,
            transactions_settled: false,
            restart_to: None,
            _subscriptions: vec![
                // The last edits are written before the application exits.
                // Closing the window releases this view before GPUI runs the
                // quit handlers, so the writes are queued on release, for
                // the handler of the whole application (`exit_writes`).
                cx.on_release(|this, cx| {
                    let state = this.save_state_now(cx);
                    let reports = this.save_reports_now();
                    exit_writes::queue(cx, async move {
                        state.await;
                        reports.await;
                    });
                    // After the writes are queued: restarting quits, which
                    // awaits them before the new version starts.
                    if let Some(path) = this.restart_to.take() {
                        tracing::info!("restarting into the installed update");
                        cx.set_restart_path(path);
                        cx.restart();
                    }
                }),
                update_subscription,
                tab_subscription,
                search_subscription,
                command_subscription,
                explorer_resize,
                assistant_resize,
            ],
        };
        // The system's close (the title bar's X on Windows, Alt F4, the
        // window manager) asks first when transactions are open.
        let view = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            view.update(cx, |this, cx| {
                // Each close asks about unsaved files again: an answer to
                // a close that was then cancelled does not carry over.
                this.restart_to = None;
                this.files_settled = false;
                this.discard_file_edits = false;
                this.transactions_settled = false;
                this.may_close(window, cx)
            })
            .unwrap_or(true)
        });
        // The library is a view of its folder: coming back to the window
        // reads it again, as another program may have changed it.
        app._subscriptions
            .push(cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.window_activated(cx);
                }
            }));
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
        // The file name a new title gives follows the typing.
        app._subscriptions.push(cx.subscribe_in(
            &app.details_name.clone(),
            window,
            |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) && this.details_open() {
                    cx.notify();
                }
            },
        ));
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
        // The system's theme is followed while the application runs.
        app._subscriptions
            .push(cx.observe_window_appearance(window, |this, window, cx| {
                let choice = theme::choice(cx);
                if choice == jykre::theme::ThemeChoice::System {
                    this.set_theme(choice, window, cx);
                }
            }));
        app._subscriptions.push(cx.subscribe(
            &app.connection_scope,
            |this, _, event: &scope_picker::PickerEvent, cx| match event {
                scope_picker::PickerEvent::LoadRequested => this.load_namespaces(cx),
            },
        ));
        if demo::enabled() {
            for (name, detail, color) in [
                (
                    tr!("PostgreSQL · Local"),
                    "localhost:5432",
                    jykre::connection_appearance::ConnectionColor::Mint,
                ),
                (
                    "Databricks · Dev",
                    "analytics / development",
                    jykre::connection_appearance::ConnectionColor::Blue,
                ),
                (
                    tr!("MySQL · Finance"),
                    "finance / development",
                    jykre::connection_appearance::ConnectionColor::Pink,
                ),
            ] {
                let group = app.tabs.update(cx, |tabs, cx| {
                    tabs.add_group(name, rgb(connection_color(color)).into(), cx)
                });
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
        app.load_shortcuts(cx);
        app.start_automation(window, cx);
        assistant_settings::load(cx);
        telemetry_settings::load(cx);
        jykre::telemetry::startup_finished();
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
            Section::Explorer | Section::Automation => {
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
                rgb(colors().mint)
            } else {
                transparent_black().into()
            })
            .focus_visible(|s| s.border_color(rgb(colors().text)))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(rgb(if chosen {
                colors().mint
            } else {
                colors().muted
            }))
            .bg(Hsla::from(rgb(if chosen {
                colors().mint
            } else {
                colors().faint
            }))
            .opacity(0.12))
            .hover(|s| s.text_color(rgb(colors().text)))
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
        self.command_more = jykre::palette::truncate(&mut self.command_matches, &query);
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
                connections::OpenTable::Data,
                window,
                cx,
            ),
        }
    }

    /// Opens a saved query: the tab that edits it, when one is open,
    /// otherwise a new tab linked to it, so the tab shows whether it is
    /// saved and the save button updates it.
    fn open_saved_query(&mut self, entry: QueryEntry, window: &mut Window, cx: &mut Context<Self>) {
        // A file of the library opens from disk (ADR-0031).
        if self.is_file_entry(entry.id) {
            self.open_library_file(entry.id, window, cx);
            return;
        }
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
                    this.unmigrated.retain(|entry| entry.id != id);
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
        } else {
            self.focus_automation_tab(tab, window, cx);
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
            let (color, environment) = (Some(connection.color), connection.appearance.environment);
            panel.update(cx, |panel, cx| panel.set_appearance(color, environment, cx));
        }
        let subscription = cx.subscribe_in(
            &panel,
            window,
            |this, panel_entity, event: &WorkspaceEvent, window, cx| {
                match event {
                    WorkspaceEvent::Save { save_as } => {
                        let tab = this
                            .panels
                            .iter()
                            .find(|(_, panel)| *panel == panel_entity)
                            .map(|(tab, _)| *tab);
                        if let Some(tab) = tab {
                            this.save_tab(tab, *save_as, None, window, cx);
                        }
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
                    WorkspaceEvent::Changed => {
                        let tab = this
                            .panels
                            .iter()
                            .find(|(_, panel)| *panel == panel_entity)
                            .map(|(tab, _)| *tab);
                        if let Some(tab) = tab {
                            this.mark_unsaved(tab, cx);
                        }
                    }
                    WorkspaceEvent::OpenPreferences => {
                        this.open_settings(settings::SettingsPage::Assistant, window, cx);
                    }
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
                    WorkspaceEvent::ConfirmPendingEdits { pending } => {
                        let panel = panel_entity.downgrade();
                        confirm(
                            window,
                            cx,
                            Confirmation {
                                title: tr!(
                                    "Run and lose {0} pending changes?",
                                    jykre::cell_format::count(*pending)
                                ),
                                description: tr!(
                                    "Edits in this tab's results are not written. The result tabs this run replaces lose theirs; write them first to keep them."
                                )
                                .to_owned(),
                                ok: tr!("Run"),
                                cancel: tr!("Cancel"),
                                focus: ConfirmFocus::Dialog,
                            },
                            move |_, cx| {
                                let _ =
                                    panel.update(cx, |panel, cx| panel.confirm_pending_edits(cx));
                            },
                        );
                    }
                    WorkspaceEvent::OpenGenerated {
                        title,
                        connection,
                        sql,
                        parameters,
                        context,
                    } => {
                        let group = this.group_for(connection, cx);
                        if let Some(tab) = this.add_query(group, title, sql, window, cx)
                            && let Some(panel) = this.panels.get(&tab).cloned()
                        {
                            panel.update(cx, |panel, cx| {
                                panel.open_generated(parameters, context.clone(), cx);
                                panel.focus_editor(window, cx);
                            });
                        }
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
                                    cancel.update(cx, |panel, cx| panel.cancel_pending(cx))
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
            tabs.add_group(
                connection.name.clone(),
                rgb(connection_color(connection.color)).into(),
                cx,
            )
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

    /// Whether the window may close now. While query tabs hold open
    /// transactions it asks first, since closing rolls them back, and closes
    /// the window once the user confirms. Reports keep no transactions.
    fn may_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.closing {
            return true;
        }
        // First the files with unsaved edits: save them all, drop the
        // edits, or keep the window. Drafts reopen at the next start.
        let unsaved = self.unsaved_file_tabs(cx);
        if !self.files_settled && !unsaved.is_empty() {
            let names: Vec<String> = unsaved
                .iter()
                .filter_map(|tab| self.panels.get(tab))
                .map(|panel| panel.read(cx).title().to_string())
                .collect();
            let title = if names.len() == 1 {
                tr!("Save the changes to {0}?", names[0])
            } else {
                tr!("Save the changes to {0} files?", names.len())
            };
            let view = cx.entity().downgrade();
            let (save, discard) = (view.clone(), view);
            library_files::choose(
                window,
                cx,
                library_files::Choice {
                    title,
                    description: tr!(
                        "Unsaved edits to: {0}. Tabs without a file reopen at the next start.",
                        names.join(", ")
                    ),
                    actions: vec![
                        (
                            tr!("Save all"),
                            std::rc::Rc::new(move |window: &mut Window, cx: &mut App| {
                                let _ = save.update(cx, |this, cx| {
                                    let tabs = this.unsaved_file_tabs(cx);
                                    this.save_all(
                                        tabs,
                                        std::rc::Rc::new(|this, window, cx| {
                                            this.files_settled = true;
                                            this.close_window(window, cx);
                                        }),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                        ),
                        (
                            tr!("Don't save"),
                            std::rc::Rc::new(move |window: &mut Window, cx: &mut App| {
                                let _ = discard.update(cx, |this, cx| {
                                    this.discard_file_edits = true;
                                    this.files_settled = true;
                                    this.close_window(window, cx);
                                });
                            }),
                        ),
                    ],
                    cancel: tr!("Keep open"),
                },
            );
            return false;
        }
        let open = self
            .panels
            .values()
            .filter(|panel| panel.read(cx).has_open_transaction())
            .count();
        // Last, runs still active are named and stopped (ADR-0034).
        if open == 0 || self.transactions_settled {
            return self.automation_may_close(window, cx);
        }
        let (title, description) = if open == 1 {
            (
                tr!("A transaction is open in a tab.").to_string(),
                tr!("Closing Jykre Studio rolls it back.").to_string(),
            )
        } else {
            (
                tr!("Transactions are open in {0} tabs.", open),
                tr!("Closing Jykre Studio rolls them back.").to_string(),
            )
        };
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            Confirmation {
                title,
                description,
                ok: tr!("Close and roll back"),
                cancel: tr!("Keep open"),
                focus: ConfirmFocus::Dialog,
            },
            move |window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.transactions_settled = true;
                    this.close_window(window, cx);
                });
            },
        );
        false
    }

    /// Closes the window once nothing else needs asking (the transactions'
    /// question may still come).
    fn close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.may_close(window, cx) {
            self.closing = true;
            window.remove_window();
        }
    }

    /// Closes a tab, asking first when a query is running in it (closing
    /// cancels it), a transaction is open (closing rolls it back) or an
    /// export runs (closing cancels it); then, when its SQL is not saved,
    /// it offers to save it, close without saving or keep the tab.
    fn request_close(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(report) = self.report_tabs.get(&tab) {
            let view = report.view.read(cx);
            let (busy, unsaved) = (view.is_busy(), view.has_unsaved_changes(cx));
            self.request_report_close(tab, busy, unsaved, window, cx);
            return;
        }
        if self.automation.owns(tab) {
            self.request_automation_close(tab, window, cx);
            return;
        }
        let (busy, unsaved, transaction, exporting) =
            self.panels
                .get(&tab)
                .map_or((false, false, false, false), |panel| {
                    let panel = panel.read(cx);
                    (
                        panel.is_busy(),
                        panel.has_unsaved_changes(),
                        panel.has_open_transaction(),
                        panel.is_exporting(),
                    )
                });
        let view = cx.entity().downgrade();
        // After the question about running work: save, discard or keep.
        let then = move |window: &mut Window, cx: &mut App| {
            let _ = view.update(cx, |this, cx| {
                if unsaved {
                    this.offer_save_and_close(tab, window, cx);
                } else {
                    this.close_tab_now(tab, window, cx);
                }
            });
        };
        let question = if exporting {
            let description = tr!("Closing the tab cancels the export, which leaves no file.");
            let description = if transaction {
                format!(
                    "{description} {}",
                    tr!("The open transaction is rolled back.")
                )
            } else {
                description.to_owned()
            };
            Some((
                tr!("An export is running in this tab.").to_string(),
                description,
                tr!("Close and cancel"),
            ))
        } else if transaction && !busy {
            Some((
                tr!("A transaction is open in this tab.").to_string(),
                tr!("Closing the tab rolls back the open transaction.").to_string(),
                tr!("Close and roll back"),
            ))
        } else if busy {
            Some((
                tr!("A query is running in this tab.").to_string(),
                tr!("Closing the tab cancels the query.").to_string(),
                tr!("Close and cancel"),
            ))
        } else {
            None
        };
        // Edits not written are lost with the tab (ADR-0039).
        let pending = self
            .panels
            .get(&tab)
            .map_or(0, |panel| panel.read(cx).pending_edits(cx));
        let question = match question {
            _ if pending == 0 => question,
            Some((title, description, ok)) => Some((
                title,
                format!(
                    "{description} {}",
                    tr!(
                        "The {0} pending changes to its results are lost.",
                        jykre::cell_format::count(pending)
                    )
                ),
                ok,
            )),
            None => Some((
                tr!("Edits in this tab are not written.").to_string(),
                tr!(
                    "Closing the tab loses {0} pending changes to its results.",
                    jykre::cell_format::count(pending)
                ),
                tr!("Close and discard"),
            )),
        };
        match question {
            Some((title, description, ok)) => confirm(
                window,
                cx,
                Confirmation {
                    title,
                    description,
                    ok,
                    cancel: tr!("Keep the tab"),
                    focus: ConfirmFocus::Dialog,
                },
                then,
            ),
            // `then` updates this view, which is being updated now: go on
            // directly instead.
            None if unsaved => self.offer_save_and_close(tab, window, cx),
            None => self.close_tab_now(tab, window, cx),
        }
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
        // A report's or a job's tab stays in its own group, and no other tab
        // joins one.
        if self.report_tabs.contains_key(&tab)
            || self.is_report_group(group)
            || self.automation.owns(tab)
            || self.automation.tab_groups().any(|own| own == group)
        {
            if same_group {
                let _ = self
                    .tabs
                    .update(cx, |tabs, cx| tabs.move_tab(tab, group, index, cx));
            }
            return;
        }
        let Some(panel) = self.panels.get(&tab) else {
            return;
        };
        let (from, busy, transaction, connected) = {
            let panel = panel.read(cx);
            (
                panel.connection_name().to_owned(),
                panel.is_busy(),
                panel.has_open_transaction(),
                panel.has_connection(),
            )
        };
        let to = self
            .connections
            .iter()
            .find(|connection| connection.group == group)
            .map(|connection| connection.name.clone());
        // A tab without a connection has no session or results to lose.
        if same_group || to.as_deref() == Some(from.as_str()) || (!connected && !busy) {
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
    /// Ctrl+F2: stops what the tab on screen runs, as its Stop button does.
    /// Stopping loses nothing that finished, so it does not ask first.
    fn cancel_run(&mut self, cx: &mut Context<Self>) {
        let active = self.tabs.read(cx).active_tab();
        if let Some(report) = active.and_then(|tab| self.report_tabs.get(&tab)) {
            report.view.update(cx, |view, cx| view.stop(cx));
        } else if let Some(panel) = active.and_then(|tab| self.panels.get(&tab)) {
            panel.update(cx, |panel, cx| panel.stop(cx));
        } else if let Some(tab) = active {
            self.stop_automation_tab(tab, cx);
        }
    }

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

    /// `Ctrl S` / `Ctrl Shift S` on the query tab on screen.
    fn save_active(&mut self, save_as: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection_open
            || self.command_open
            || self.settings_open
            || self.password_prompt.is_some()
            || self.dialog_open()
        {
            return;
        }
        if let Some(tab) = self
            .tabs
            .read(cx)
            .active_tab()
            .filter(|tab| self.panels.contains_key(tab))
        {
            self.save_tab(tab, save_as, None, window, cx);
        }
    }

    /// Shows a file tab's unsaved edits in the tab strip (`●` after its
    /// title) and takes the mark away once they are saved.
    fn mark_unsaved(&mut self, tab: TabId, cx: &mut Context<Self>) {
        let Some(panel) = self.panels.get(&tab) else {
            return;
        };
        let (unsaved, title) = {
            let panel = panel.read(cx);
            (
                panel.file().is_some() && panel.has_unsaved_changes(),
                panel.title(),
            )
        };
        if unsaved == self.unsaved_marked.contains(&tab) {
            return;
        }
        let shown = if unsaved {
            self.unsaved_marked.insert(tab);
            format!("{title} ●")
        } else {
            self.unsaved_marked.remove(&tab);
            title.to_string()
        };
        let _ = self
            .tabs
            .update(cx, |tabs, cx| tabs.set_tab_title(tab, shown, cx));
    }

    /// **Reopen closed query tab** (`Ctrl Shift T`): the query tab closed
    /// last in this session comes back with its SQL, title, connection,
    /// context, timeout and `:name` fields, and runs nothing. Its results,
    /// session and any transaction (rolled back when it closed) are not
    /// restored, and the tab says so. One whose connection is gone opens
    /// without a connection and says what to choose.
    fn reopen_closed_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut state) = self.closed_tabs.pop() else {
            self.app_notice = Some(Feedback::new(
                Tone::Info,
                tr!("No query tab was closed in this session."),
            ));
            cx.notify();
            return;
        };
        // A link to a saved query removed meanwhile is dropped: the tab is
        // a draft again.
        if state
            .saved_id
            .is_some_and(|id| !self.saved_queries.iter().any(|entry| entry.id == id))
        {
            state.saved_id = None;
            state.saved_sql = None;
        }
        let missing = !state.connection.is_empty()
            && !self.connections.iter().any(|c| c.name == state.connection);
        let group = self.group_for(&state.connection, cx);
        let Some(tab) = self.add_query(group, &state.title, &state.sql, window, cx) else {
            return;
        };
        if let Some(panel) = self.panels.get(&tab) {
            panel.update(cx, |panel, cx| {
                panel.restore(&state, cx);
                if missing {
                    panel.warn_missing_connection(cx);
                } else {
                    panel.reopened(cx);
                }
                panel.focus_editor(window, cx);
            });
        }
        self.save_state_soon(cx);
        cx.notify();
    }

    fn new_active_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group) = self.active_group(cx) {
            self.new_query(group, window, cx);
        } else {
            self.open_connection(None, window, cx);
        }
    }

    /// `Ctrl T`: a new query tab on the connection of the query tab on
    /// screen. Without one (no tab, a report's or a tab without a
    /// connection), the new tab has no connection and asks for one.
    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let group = {
            let tabs = self.tabs.read(cx);
            tabs.active_tab()
                .filter(|tab| self.panels.contains_key(tab))
                .and_then(|tab| tabs.tab_group(tab))
                .filter(|group| self.connections.iter().any(|c| c.group == *group))
        };
        match group {
            Some(group) => self.new_query(group, window, cx),
            None => {
                let group = self.group_for("", cx);
                let title = tr!("Query {0}", self.next_query);
                self.next_query += 1;
                if let Some(tab) = self.add_query(group, &title, "", window, cx)
                    && let Some(panel) = self.panels.get(&tab)
                {
                    panel.update(cx, |panel, cx| {
                        panel.ask_for_connection(cx);
                        panel.focus_editor(window, cx);
                    });
                }
            }
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
            .focus_visible(|s| s.bg(rgb(colors().hover)))
            .tooltip(move |window, cx| Tooltip::new(title).build(window, cx))
            .size(px(34.))
            .flex_none()
            .rounded(px(7.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(active, |s| s.bg(rgb(colors().accent_bg)))
            .hover(|s| s.bg(rgb(colors().hover)))
            .child(icon(
                name,
                if active {
                    colors().mint
                } else {
                    colors().faint
                },
            ))
            // Scheduled runs that ended badly since the user last looked.
            .when(section == Section::Automation, |s| {
                s.relative().children(self.automation_rail_badge())
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.section == section {
                    this.sidebar_open = !this.sidebar_open;
                } else {
                    this.section = section;
                    this.filter_library(cx);
                    this.sidebar_open = true;
                    // The library is a view of its folder: read it again.
                    if section == Section::Saved {
                        this.scan_library(cx);
                    }
                    // Automation held by another window: try its lock again.
                    if section == Section::Automation {
                        this.automation_shown(cx);
                    }
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
            .bg(rgb(colors().chrome))
            .border_r_1()
            .border_color(rgb(colors().border))
            .children(
                [
                    (Section::Explorer, I::Database, tr!("Explorer")),
                    (Section::Saved, I::Bookmark, tr!("Saved queries")),
                    (Section::Reports, I::FileChartColumn, tr!("Reports")),
                    (Section::Automation, I::Zap, tr!("Automation")),
                    (Section::History, I::Clock, tr!("History")),
                ]
                .into_iter()
                .enumerate()
                .map(|(i, (section, name, title))| self.rail_item(i, section, name, title, cx))
                .collect::<Vec<_>>(),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("rail-preferences")
                    .role(Role::Button)
                    .aria_label(tr!("Preferences"))
                    .aria_selected(self.settings_open)
                    .focusable()
                    .tab_index(0)
                    .focus_visible(|s| s.bg(rgb(colors().hover)))
                    .tooltip(|window, cx| {
                        Tooltip::new(tr!(
                            "Preferences · {0}",
                            shortcuts::label::<ShowPreferences>(cx)
                        ))
                        .build(window, cx)
                    })
                    .size(px(34.))
                    .flex_none()
                    .rounded(px(7.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .when(self.settings_open, |s| s.bg(rgb(colors().accent_bg)))
                    .hover(|s| s.bg(rgb(colors().hover)))
                    .child(icon(
                        I::Settings2,
                        if self.settings_open {
                            colors().mint
                        } else {
                            colors().faint
                        },
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let page = this.settings_page;
                        this.open_settings(page, window, cx);
                    })),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> Div {
        let title = match self.section {
            Section::Explorer => tr!("Explorer"),
            Section::Saved => tr!("Saved queries"),
            Section::Reports => tr!("Reports"),
            Section::Automation => tr!("Automation"),
            Section::History => tr!("History"),
        };
        column()
            .size_full()
            .bg(rgb(colors().panel))
            .border_r_1()
            .border_color(rgb(colors().border))
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
                            .when(self.section == Section::Automation, |s| {
                                s.child(self.automation_add_menu(cx))
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
                    div()
                        .px_3()
                        .pb_3()
                        // Esc clears the text, then gives the keyboard to the
                        // tree; Down goes to the tree's first node.
                        .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.modifiers.modified() {
                                return;
                            }
                            match event.keystroke.key.as_str() {
                                "escape" if !this.search.read(cx).value().is_empty() => {
                                    this.search
                                        .update(cx, |input, cx| input.set_value("", window, cx));
                                    this.search_changed(cx);
                                }
                                "escape" | "down" => this.focus_explorer_tree(window, cx),
                                _ => return,
                            }
                            window.prevent_default();
                            cx.stop_propagation();
                        }))
                        .child({
                            let input = Input::new(&self.search)
                                .small()
                                .prefix(I::Search)
                                .w_full()
                                .aria_label(tr!("Search objects in the explorer"));
                            // A short search scans the whole cache, off the
                            // UI thread; the field says it is running.
                            if self.explorer_searching {
                                input.suffix(
                                    div()
                                        .id("explorer-search-pending")
                                        .role(Role::Status)
                                        .aria_label(tr!("Searching…"))
                                        .child(spinning_icon(
                                            "explorer-search-spinner",
                                            I::LoaderCircle,
                                            colors().amber,
                                        )),
                                )
                            } else {
                                input
                            }
                        }),
                )
                .when(!self.favorites.items.is_empty(), |s| {
                    s.child(self.favorites_section(cx))
                })
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
                                .text_color(rgb(colors().faint))
                                .child(tr!("Connections")),
                        )
                        .child(badge(self.connections.len().to_string(), colors().faint)),
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
                            .bg(rgb(colors().surface))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors().muted))
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
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .when(self.connections_loading, |s| {
                            s.child(
                                row()
                                    .px_4()
                                    .py_3()
                                    .child(icon(I::LoaderCircle, colors().faint))
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
                        // Above the list, not over it, so the first rows
                        // (often the matches) stay visible.
                        .when_some(self.quick_search_bar(cx), |s, bar| s.child(bar))
                        .child({
                            let view = cx.entity();
                            // Typing on the tree searches it (`quick_search`);
                            // its keys are seen here before the focused node.
                            div()
                                .id("explorer-keys")
                                .role(Role::Tree)
                                .aria_label(tr!("Explorer"))
                                .key_context(quick_search::EXPLORER_CONTEXT)
                                .track_focus(&self.explorer_focus)
                                // With the keyboard on the tree but no node,
                                // Up and Down still step through the quick
                                // search's matches.
                                .on_action(cx.listener(
                                    |this, _: &shortcuts::ExplorerUp, window, cx| {
                                        this.quick_step_or_pass(false, window, cx)
                                    },
                                ))
                                .on_action(cx.listener(
                                    |this, _: &shortcuts::ExplorerDown, window, cx| {
                                        this.quick_step_or_pass(true, window, cx)
                                    },
                                ))
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_h_0()
                                .capture_key_down(cx.listener(
                                    |this, event: &KeyDownEvent, window, cx| {
                                        if this.explorer_capture_key(event, window, cx) {
                                            window.prevent_default();
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
                                .on_action(cx.listener(
                                    |this, _: &ExplorerContextMenu, window, cx| {
                                        this.open_node_menu(window, cx)
                                    },
                                ))
                                .on_action(cx.listener(
                                    |this, _: &ToggleExplorerFilter, window, cx| {
                                        let mode = this.explorer_quick_mode.toggled();
                                        this.set_quick_mode(mode, window, cx)
                                    },
                                ))
                                .child(
                                    list(self.explorer_list.clone(), move |entry, _, cx| {
                                        view.update(cx, |this, cx| {
                                            this.explorer_entry(entry, cx)
                                        })
                                    })
                                    .flex_1()
                                    .pb_3(),
                                )
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
            .child(row().id(("connection", index)).role(Role::Button).aria_label(connection.name.clone()).aria_description(connection.detail.clone()).aria_expanded(expanded).focusable().tab_index(0).focus_visible(|s| s.bg(rgb(colors().hover))).h(px(38.)).px_3().gap_2().cursor_pointer().hover(|s| s.bg(rgb(colors().surface)))
                .on_click(cx.listener(move |this, _, _, cx| { this.connections[index].expanded = !this.connections[index].expanded; cx.notify(); }))
                // Where it connects, built only when shown.
                .tooltip({ let detail = connection.detail.clone(); move |window, cx| gpui_kit::component::tooltip::Tooltip::new(detail.clone()).build(window, cx) })
                .child(icon(if expanded { I::ChevronDown } else { I::ChevronRight }, colors().faint))
                .child(icon(I::Server, connection_color(connection.color)))
                .child(div().text_size(px(14.)).flex_1().truncate().child(connection.name.clone()))
                .child(dot(connection_color(connection.color))))
            .when(expanded, |s| s
                .when(index == 0, |s| s
                    .child(row().px_3().pl(px(29.)).h(px(30.)).child(icon(I::ChevronDown, colors().faint)).child(icon(I::Database, colors().muted)).child(label("commerce")))
                    .child(row().id("schema-public").role(Role::Button).aria_label(tr!("Schema public")).aria_expanded(self.schema_open).focusable().tab_index(0).focus_visible(|s| s.bg(rgb(colors().hover))).pl(px(43.)).h(px(30.)).cursor_pointer()
                        .child(icon(if self.schema_open { I::ChevronDown } else { I::ChevronRight }, colors().faint)).child(icon(I::Layers, colors().muted)).child(label("public"))
                        .on_click(cx.listener(|this, _, _, cx| { this.schema_open = !this.schema_open; cx.notify(); })))
                    .when(self.schema_open || !query.is_empty(), |s| s
                        .child(row().id("tables-folder").role(Role::Button).aria_label(tr!("Tables")).aria_expanded(self.tables_open).focusable().tab_index(0).focus_visible(|s| s.bg(rgb(colors().hover))).pl(px(57.)).pr_3().h(px(30.)).cursor_pointer()
                            .child(icon(if self.tables_open { I::ChevronDown } else { I::ChevronRight }, colors().faint)).child(icon(I::Table2, colors().muted)).child(label(tr!("Tables")))
                            .child(div().flex_1()).child(label("7"))
                            .on_click(cx.listener(|this, _, _, cx| { this.tables_open = !this.tables_open; cx.notify(); })))
                        .when(self.tables_open || !query.is_empty(), |s| s.children(TABLES.into_iter().enumerate().filter(|(_, table)| query.is_empty() || table.contains(&query)).map(|(table_index, table)| {
                            column().flex_none()
                                .child(row().id(("schema-table", table_index)).role(Role::Button).aria_label(tr!("New query for table {0}", table)).focusable().tab_index(0).focus_visible(|s| s.bg(rgb(colors().hover))).h(px(32.)).pl(px(71.)).pr_3().gap_2().cursor_pointer()
                                    .when(table == "orders", |s| s.bg(rgb(colors().accent_bg)))
                                    .hover(|s| s.bg(rgb(colors().hover)))
                                    .child(icon(if table == "orders" && self.orders_open { I::ChevronDown } else { I::ChevronRight }, colors().faint))
                                    .child(icon(I::Table2, if table == "orders" { colors().mint } else { colors().faint }))
                                    .child(div().text_size(px(14.)).text_color(rgb(if table == "orders" { colors().mint } else { colors().muted })).child(table))
                                    // As in the real explorer: a click selects (orders also
                                    // shows its columns); a double click or Enter opens.
                                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                        if event.is_keyboard() || event.click_count() >= 2 { this.open_table(table, window, cx); }
                                        else if table == "orders" { this.orders_open = !this.orders_open; cx.notify(); }
                                    })))
                                .when(table == "orders" && self.orders_open, |s| s.children(ORDER_COLUMNS.into_iter().map(|(name, kind, key)| {
                                    row().h(px(30.)).pl(px(99.)).pr_3().gap_2().child(icon(if key { I::KeyRound } else { I::Columns3 }, if key { colors().amber } else { colors().faint }))
                                        .child(div().text_size(px(12.)).text_color(rgb(colors().muted)).child(name)).child(div().flex_1())
                                        .child(div().text_size(px(12.)).text_color(rgb(colors().faint)).child(kind))
                                })))
                        })))
                        .child(row().pl(px(57.)).h(px(32.)).child(icon(I::ChevronRight, colors().faint)).child(icon(I::Eye, colors().faint)).child(label(tr!("Views"))))
                        .child(row().pl(px(57.)).h(px(32.)).child(icon(I::ChevronRight, colors().faint)).child(icon(I::Braces, colors().faint)).child(label(tr!("Functions"))))))
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
        let file = saved && self.is_file_entry(entry.id);
        let missing = file && self.library_missing.contains(&entry.id);
        let (symbol, color, headline, detail) = if saved {
            let tags: String = entry.tags.iter().map(|tag| format!("#{tag} ")).collect();
            let (symbol, color) = if missing {
                (I::FileQuestionMark, colors().amber)
            } else if file {
                (I::FileText, colors().mint)
            } else {
                (I::Bookmark, colors().faint)
            };
            let preview = if missing {
                tr!("The file is not there: locate it or forget it").to_owned()
            } else if !file {
                tr!("Not moved to a file yet").to_owned()
            } else {
                preview
            };
            // Folder (in a subfolder), connection and SQL, those it has.
            let folder = self.library_folders.get(&entry.id).cloned();
            let context: Vec<String> = folder
                .into_iter()
                .chain(Some(entry.connection.clone()))
                .chain(Some(preview))
                .filter(|part| !part.is_empty())
                .collect();
            (
                symbol,
                color,
                entry.title.clone(),
                format!("{tags}{}", context.join(" · ")),
            )
        } else {
            use jykre::workspace_state::RunStatus;
            let (symbol, color) = match entry.status {
                Some(RunStatus::Failed) => (I::CircleX, colors().red),
                Some(RunStatus::Cancelled) => (I::Ban, colors().amber),
                Some(RunStatus::Succeeded) => (I::CircleCheck, colors().mint),
                None => (I::Clock, colors().faint),
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
                        .focus_visible(|s| s.bg(rgb(colors().hover)))
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .px_2()
                        .justify_center()
                        .gap_1()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(colors().hover)))
                        .child(
                            row().min_w_0().child(icon(symbol, color)).child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(14.))
                                    .when(!saved, |s| {
                                        s.font_family(fonts().code.clone()).text_size(px(13.))
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
                                .text_color(rgb(colors().faint))
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
                    .map(|s| {
                        if missing {
                            s.child(
                                icon_button(
                                    "missing",
                                    I::FolderSearch,
                                    tr!("Locate or forget the missing file"),
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.open_library_file(id, window, cx)
                                    },
                                )),
                            )
                        } else if file {
                            s.child(
                                icon_button("reveal", I::FolderOpen, tr!("Show in folder"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(entry) = this.query_library.entry(id) {
                                            cx.reveal_path(&entry.path);
                                        }
                                    })),
                            )
                        } else {
                            s.child(
                                icon_button("remove", I::Trash, tr!("Remove from saved queries"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.remove_saved_query(id, window, cx)
                                    })),
                            )
                        }
                    })
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
                    s.child(
                        row()
                            .gap_1()
                            .child(div().flex_1().min_w_0().child(label(if saved {
                                tr!("Your library")
                            } else {
                                tr!("Recent runs")
                            })))
                            .when(saved && self.library_scan.is_some(), |s| {
                                s.child(
                                    div()
                                        .id("library-scanning")
                                        .role(Role::Status)
                                        .aria_label(tr!("Reading the saved queries folder…"))
                                        .child(spinning_icon(
                                            "library-scan-spinner",
                                            I::LoaderCircle,
                                            colors().amber,
                                        )),
                                )
                            })
                            .when(saved && !self.unmigrated.is_empty() && self.persist_library, |s| {
                                s.child(
                                    icon_button(
                                        "migrate-saved",
                                        I::FolderInput,
                                        tr!("Move the saved queries of an earlier version to files"),
                                    )
                                    .size(px(26.))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.migrate_saved_queries(cx)
                                    })),
                                )
                            })
                            .when(saved, |s| {
                                s.child(
                                    icon_button(
                                        "library-open",
                                        I::FolderOpen,
                                        tr!("Open a .sql file… · {0}", shortcuts::label::<OpenFile>(cx)),
                                    )
                                        .size(px(26.))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_files_dialog(window, cx)
                                        })),
                                )
                                .child(
                                    icon_button("library-refresh", I::RefreshCw, tr!("Read the folder again"))
                                        .size(px(26.))
                                        .on_click(cx.listener(|this, _, _, cx| this.scan_library(cx))),
                                )
                            }),
                    )
                    .when(saved && !demo::enabled(), |s| {
                        s.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(rgb(colors().faint))
                                .child(self.library_dir().display().to_string()),
                        )
                        .when(self.library_stopped, |s| {
                            s.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(colors().amber))
                                    .child(tr!(
                                        "The folder holds more than the library lists (10,000 files, 8 folders deep)."
                                    )),
                            )
                        })
                    })
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
                            s.child(
                                div()
                                    .p_3()
                                    .text_size(px(14.))
                                    .text_color(rgb(colors().muted))
                                    .child(if saved {
                                        tr!(
                                            "Save a query to a file with {0}, or open one with {1}.",
                                            shortcuts::label::<SaveFile>(cx),
                                            shortcuts::label::<OpenFile>(cx)
                                        )
                                    } else {
                                        tr!("Queries you run appear here.").to_owned()
                                    }),
                            )
                        } else if self.library_shown.is_empty() {
                            s.child(
                                div()
                                    .p_3()
                                    .text_size(px(14.))
                                    .text_color(rgb(colors().muted))
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
            .when(self.section == Section::Automation, |s| {
                self.automation_list(s, cx)
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

    fn titlebar_content(&self, cx: &mut Context<Self>) -> Div {
        row()
            .w_full()
            .h_full()
            .pr_3()
            .gap_3()
            .child(
                row().flex_none().gap_2().child(app_mark(px(21.))).child(
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
                    .child(icon(I::PanelsTopLeft, colors().muted))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(tr!("Workspace")),
                    )
                    .child(icon(I::ChevronRight, colors().faint))
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
                    .focus_visible(|s| s.border_color(rgb(colors().mint)))
                    .w(px(400.))
                    .min_w(px(180.))
                    .flex_shrink_1()
                    .h(px(32.))
                    .px_3()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(colors().border))
                    .bg(rgb(colors().bg))
                    .cursor_pointer()
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .hover(|s| s.border_color(rgb(colors().border_strong)))
                    .child(icon(I::Search, colors().faint))
                    .child(label(tr!("Search the workspace…")))
                    .child(div().flex_1())
                    .child(shortcut(shortcuts::label::<shortcuts::SearchCache>(cx)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.command_open = true;
                        this.command.update(cx, |input, cx| input.focus(window, cx));
                        this.search_cache(true, cx);
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .when(!self.closed_tabs.is_empty(), |s| {
                s.child(
                    icon_button(
                        "reopen-closed-tab",
                        I::Undo2,
                        tr!(
                            "Reopen closed query tab · {0}",
                            shortcuts::label::<ReopenClosedTab>(cx)
                        ),
                    )
                    .flex_none()
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.reopen_closed_tab(window, cx)),
                    ),
                )
            })
            .child(divider())
            .child(
                button("new-query", I::Plus, tr!("New query"))
                    .flex_none()
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .border_1()
                    .border_color(rgb(colors().border_strong))
                    .text_color(rgb(colors().mint))
                    .on_click(cx.listener(|this, _, window, cx| this.new_active_query(window, cx))),
            )
    }

    /// The tabs, or a welcome state with the next step when there is nothing
    /// to show yet.
    fn main_area(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.connections_loading || !self.connections.is_empty() || self.automation.has_tabs() {
            return self.tabs.clone().into_any_element();
        }
        column()
            .id("welcome")
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .p_6()
            .child(icon(I::Database, colors().mint))
            .child(div().text_size(px(16.)).child(tr!("Connect to a database")))
            .child(label(tr!(
                "Add a PostgreSQL, MySQL, Databricks or SQLite connection to explore its metadata and run SQL."
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
                    .bg(rgb(colors().panel))
                    .border_1()
                    .border_color(rgb(colors().border_strong))
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        row().p_4().border_b_1().border_color(rgb(colors().border)).child(
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
                                            colors().amber,
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
                    .when(self.command_more, |s| {
                        s.child(div().px_4().pb_2().child(label(
                            if self.command.read(cx).value().trim().is_empty() {
                                tr!("Type to search every table, view and saved query.").to_string()
                            } else {
                                tr!(
                                    "Showing the first {0}. Type more to narrow the search.",
                                    matches.len()
                                )
                            },
                        )))
                    })
                    .child(
                        row()
                            .h(px(40.))
                            .px_4()
                            .justify_between()
                            .bg(rgb(colors().bg))
                            .border_t_1()
                            .border_color(rgb(colors().border))
                            .child(label(tr!(
                                "↑ ↓ choose · Enter opens the saved query or the table's data · #tag filters"
                            )))
                            .child(
                                row()
                                    .child(shortcut(shortcuts::label::<shortcuts::Dismiss>(cx)))
                                    .child(label(tr!("close"))),
                            ),
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
                .when(selected, |s| s.bg(rgb(colors().surface)))
                .focus_visible(|s| s.bg(rgb(colors().hover)))
                .h(px(43.))
                .px_3()
                .rounded(px(5.))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(colors().hover)))
        };
        // What the text asked for is marked where the row shows it: the
        // words without `#tag` filters, and those filters in the tags.
        let typed = self.command.read(cx).value().to_string();
        let words = jykre::workspace_state::without_tag_filters(&typed);
        let words = words.trim();
        let tag_filters: Vec<&str> = typed
            .split_whitespace()
            .filter(|word| word.len() > 1 && word.starts_with('#'))
            .collect();
        let mark = |value: &str, tags: bool| {
            let mut needles = vec![words];
            if tags {
                needles.extend(tag_filters.iter().copied());
            }
            marked(
                value.to_owned(),
                jykre::palette::highlights(value, &needles),
            )
        };
        let text = |name: String, detail: String, tags: bool| {
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
                        .child(mark(&name, false)),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(rgb(colors().faint))
                        .child(mark(&detail, tags)),
                )
        };
        let connection_label = |name: &str| {
            div()
                .text_size(px(12.))
                .text_color(rgb(colors().muted))
                .child(mark(name, false))
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
                    .child(icon(I::Bookmark, colors().mint))
                    .child(text(entry.title.clone(), tags, true))
                    .child(connection_label(&entry.connection))
                    .child(icon(I::CornerDownLeft, colors().faint))
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
                        "View data of table {0} in {1}, {2}",
                        name,
                        location,
                        connection
                    ))
                    .child(icon(I::Table2, colors().mint))
                    .child(text(name, location, false))
                    .child(label(connection))
                    .child(icon(I::CornerDownLeft, colors().faint))
                    .on_click(open)
            }
        };
        // Rows keep their height and the list scrolls; shrinking them to fit
        // piles them on top of each other.
        column()
            .flex_none()
            .when_some(heading, |s, heading| {
                s.child(div().px_3().pt_2().pb_1().child(label(heading)))
            })
            .child(row_element.flex_none())
    }

    fn overlay(&self, id: &'static str, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id(id)
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .bg(rgba(colors().scrim))
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
        // Set again by the selected node while it is drawn.
        self.selected_bounds.set(None);
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
            .bg(rgb(colors().bg))
            .text_color(rgb(colors().text))
            .font_family(fonts().ui.clone())
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
            .on_action(cx.listener(|this, _: &CancelRun, _, cx| this.cancel_run(cx)))
            .on_action(cx.listener(|this, _: &OpenFile, window, cx| {
                if this.connection_open
                    || this.command_open
                    || this.settings_open
                    || this.password_prompt.is_some()
                    || this.dialog_open()
                {
                    return;
                }
                this.open_files_dialog(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &SaveFile, window, cx| this.save_active(false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SaveFileAs, window, cx| this.save_active(true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &ReopenClosedTab, window, cx| {
                if this.connection_open
                    || this.command_open
                    || this.settings_open
                    || this.password_prompt.is_some()
                    || this.dialog_open()
                {
                    return;
                }
                this.reopen_closed_tab(window, cx);
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
            .on_action(cx.listener(|this, _: &NewTab, window, cx| {
                if this.connection_open
                    || this.command_open
                    || this.settings_open
                    || this.password_prompt.is_some()
                    || this.dialog_open()
                {
                    return;
                }
                this.new_tab(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShowPreferences, window, cx| {
                if this.settings_open {
                    this.close_settings(window, cx);
                } else {
                    let page = this.settings_page;
                    this.open_settings(page, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FindColumn, window, cx| {
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
                    panel.update(cx, |panel, cx| panel.find_column(window, cx));
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
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::search_cache_shortcut))
            .on_action(cx.listener(Self::new_query_shortcut))
            .on_action(cx.listener(Self::toggle_explorer))
            .on_action(cx.listener(Self::toggle_assistant_shortcut))
            .on_action(cx.listener(Self::larger_font))
            .on_action(cx.listener(Self::smaller_font))
            .on_action(cx.listener(Self::reset_font))
            // The search's list: the arrows move the highlighted match and
            // the text field keeps every other key.
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.command_open
                    && matches!(event.keystroke.key.as_str(), "up" | "down")
                    && !event.keystroke.modifiers.modified()
                {
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
                }
            }))
            .child(
                TitleBar::new()
                    // On Linux the title bar draws its own X, which would
                    // remove the window without asking.
                    .on_close_window({
                        let view = cx.entity().downgrade();
                        move |_, window, cx| {
                            let close = view
                                .update(cx, |this, cx| {
                                    this.restart_to = None;
                                    this.transactions_settled = false;
                                    this.may_close(window, cx)
                                })
                                .unwrap_or(true);
                            if close {
                                window.remove_window();
                            }
                        }
                    })
                    .h(px(46.))
                    .bg(rgb(colors().chrome))
                    .border_b_1()
                    .border_color(rgb(colors().border))
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
                    .bg(rgb(colors().chrome))
                    .border_t_1()
                    .border_color(rgb(colors().border))
                    .justify_between()
                    .child(label(match self.connections.len() {
                        1 => tr!("1 connection").to_string(),
                        count => tr!("{0} connections", count),
                    }))
                    .when_some(self.automation.stopping(), |s, count| {
                        s.child(
                            row()
                                .id("automation-stopping")
                                .role(Role::Status)
                                .gap_1()
                                .child(spinning_icon(
                                    "automation-stopping-spinner",
                                    I::LoaderCircle,
                                    colors().amber,
                                ))
                                .child(label(tr!(
                                    "Stopping {0} automation runs before closing (up to about 11 s)…",
                                    count
                                ))),
                        )
                    })
                    .children(self.automation_news_line(cx))
                    .child(
                        row()
                            .gap_3()
                            .items_center()
                            .child(self.update_badge.clone())
                            .child(label(format!(
                                "Jykre Studio · v{}",
                                env!("CARGO_PKG_VERSION")
                            ))),
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
            .when_some(self.import_view(), |s, dialog| s.child(dialog))
            .when(self.settings_open, |s| {
                s.child(self.settings_dialog(window, cx))
            })
            // Painted last, so it closes the frame for the probe.
            .child(probe::frame_end())
    }
}

/// The application's default keys, bound at start. The editor binds
/// Ctrl+Enter itself; a binding in its own, deeper context added later wins
/// there, and the global ones apply anywhere else in the window.
pub(crate) fn application_bindings() -> Vec<KeyBinding> {
    let editor = format!("{} > Input", workspace::SQL_EDITOR_CONTEXT);
    let editor = editor.as_str();
    vec![
        KeyBinding::new("ctrl-enter", ExecuteCurrentQuery, None),
        KeyBinding::new("ctrl-shift-enter", ExecuteCurrentQueryInNewResult, None),
        KeyBinding::new("ctrl-alt-enter", ExecuteAllQueries, None),
        KeyBinding::new("ctrl-enter", ExecuteCurrentQuery, Some(editor)),
        // Suggestions on demand, also with nothing typed yet.
        KeyBinding::new("ctrl-space", ShowCompletions, Some(editor)),
        KeyBinding::new(
            "ctrl-shift-enter",
            ExecuteCurrentQueryInNewResult,
            Some(editor),
        ),
        KeyBinding::new("ctrl-alt-enter", ExecuteAllQueries, Some(editor)),
        KeyBinding::new("ctrl-f2", CancelRun, None),
        KeyBinding::new("ctrl-f2", CancelRun, Some(editor)),
        KeyBinding::new("ctrl-shift-t", ReopenClosedTab, None),
        KeyBinding::new("ctrl-shift-t", ReopenClosedTab, Some(editor)),
        KeyBinding::new("ctrl-o", OpenFile, None),
        KeyBinding::new("ctrl-o", OpenFile, Some(editor)),
        KeyBinding::new("ctrl-s", SaveFile, None),
        KeyBinding::new("ctrl-s", SaveFile, Some(editor)),
        KeyBinding::new("ctrl-shift-s", SaveFileAs, None),
        KeyBinding::new("ctrl-shift-s", SaveFileAs, Some(editor)),
        KeyBinding::new("ctrl-w", CloseActiveTab, None),
        KeyBinding::new("ctrl-f4", CloseActiveTab, None),
        KeyBinding::new("ctrl-w", CloseActiveTab, Some(editor)),
        KeyBinding::new("ctrl-f4", CloseActiveTab, Some(editor)),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-pagedown", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
        KeyBinding::new("ctrl-pageup", PreviousTab, None),
        KeyBinding::new("ctrl-tab", NextTab, Some(editor)),
        KeyBinding::new("ctrl-pagedown", NextTab, Some(editor)),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, Some(editor)),
        KeyBinding::new("ctrl-pageup", PreviousTab, Some(editor)),
        KeyBinding::new("ctrl-shift-e", ExportResult, None),
        KeyBinding::new("ctrl-shift-e", ExportResult, Some(editor)),
        KeyBinding::new("ctrl-/", ToggleComment, Some(editor)),
        // Bound by none of the editor component's keys (gpui-base 0.9.0).
        KeyBinding::new("ctrl-g", GoToLine, Some(editor)),
        KeyBinding::new("ctrl-t", NewTab, None),
        KeyBinding::new("ctrl-t", NewTab, Some(editor)),
        KeyBinding::new("ctrl-,", ShowPreferences, None),
        KeyBinding::new("ctrl-,", ShowPreferences, Some(editor)),
        KeyBinding::new(
            "shift-f10",
            ExplorerContextMenu,
            Some(quick_search::EXPLORER_CONTEXT),
        ),
        KeyBinding::new(
            "ctrl-f",
            ToggleExplorerFilter,
            Some(quick_search::EXPLORER_CONTEXT),
        ),
        KeyBinding::new("shift-f10", ResultGridMenu, Some(result_grid::GRID_CONTEXT)),
        KeyBinding::new("ctrl-shift-f", FindColumn, None),
        KeyBinding::new("ctrl-shift-f", FindColumn, Some(editor)),
    ]
}

/// The application's identity on Linux: the Wayland app ID and the X11
/// `WM_CLASS`. The desktop finds the window's name and icon in
/// `<APP_ID>.desktop`, which `assets/linux/install.sh` installs.
const APP_ID: &str = "io.github.alandariva.JykreStudio";

pub fn run() {
    application().with_assets(assets::AllAssets).run(|cx| {
        init(cx);
        fonts::resolve(cx);
        probe::start(cx);
        exit_writes::install(cx);
        cx.bind_keys(application_bindings());
        cx.bind_keys(shortcuts::default_bindings());
        // Everything bound so far is the default keymap; the keys the user
        // chose are applied over it once they are read.
        shortcuts::install(cx);
        // The system's theme until the preference is read.
        theme::apply(jykre::theme::ThemeChoice::System, None, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1480.), px(920.)),
                cx,
            ))),
            window_min_size: Some(size(px(1100.), px(700.))),
            app_id: Some(APP_ID.to_owned()),
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

#[cfg(test)]
mod tests {
    use super::APP_ID;

    const DESKTOP_ENTRY: &str =
        include_str!("../../assets/linux/io.github.alandariva.JykreStudio.desktop");

    /// The desktop matches the window to its entry by the app ID, and the
    /// entry names the icon `install.sh` installs under the same ID.
    #[test]
    fn the_desktop_entry_names_the_app_id() {
        let keys: Vec<&str> = DESKTOP_ENTRY.lines().collect();
        assert!(keys.contains(&format!("Icon={APP_ID}").as_str()));
        assert!(keys.contains(&format!("StartupWMClass={APP_ID}").as_str()));
        let install = include_str!("../../assets/linux/install.sh");
        assert!(install.contains(&format!(
            "APP_ID={APP_ID}
"
        )));
    }
}
