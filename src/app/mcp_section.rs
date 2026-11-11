//! The MCP section of the rail (plan 5.2 and 5.5, [ADR-0042](../../docs/adr/0042-mcp-server-for-the-assistant-tools.md)).
//!
//! The sidebar, the token tabs and the Requests tab present
//! [`jykre::mcp::McpEndpoint`] and send it intents. Token records, review,
//! deadlines and whether an answer may leave stay in `jykre::mcp`. A new
//! card updates the rail and the status bar and does not take the keyboard.

use super::*;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::connection_appearance::Environment;
use jykre::drivers::{ConnectionId, DriverKind};
use jykre::mcp::{
    ActivityEntry, CONNECTION_CHANGED, CardContent, CardId, CardPlace, CardSnapshot, DEFAULT_PORT,
    EnabledTools, ListenState, McpEndpoint, McpTokenId, Outcome, ReviewPolicy, TokenRecord,
    environment_label, split_wire,
};
use jykre::tr;
use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often at most the section reads the endpoint, once a frame.
const FRAME: Duration = Duration::from_millis(16);

const TOKEN_ROW: f32 = 64.;
const NAME_LIMIT: usize = 60;

/// Control characters and bidirectional marks as visible escapes.
///
/// Newlines and tabs stay, so a statement keeps its lines. Every other
/// control, and the bidirectional overrides, is `\u{....}` so a purpose or
/// a statement cannot redraw the card around it.
pub(super) fn show_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\n' || ch == '\t' {
            out.push(ch);
        } else if ch.is_control() || is_bidi_mark(ch) {
            out.push_str(&format!("\\u{{{:04x}}}", u32::from(ch)));
        } else {
            out.push(ch);
        }
    }
    out
}

/// Bidirectional format characters (Unicode bidi controls and marks).
fn is_bidi_mark(ch: char) -> bool {
    matches!(
        ch,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// What the listener is, as the sidebar and the status bar read it.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Listener {
    #[default]
    Off,
    Listening {
        port: u16,
    },
    PortInUse {
        port: u16,
    },
    Demo,
}

fn listener_of(state: ListenState) -> Listener {
    match state {
        ListenState::Off => Listener::Off,
        ListenState::Listening { addr } => Listener::Listening { port: addr.port() },
        ListenState::PortInUse { port } => Listener::PortInUse { port },
        ListenState::Demo => Listener::Demo,
    }
}

/// How long a new token lasts. Thirty days is the form's default.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ExpiryChoice {
    Days7,
    #[default]
    Days30,
    Days90,
    Year,
    Never,
}

impl ExpiryChoice {
    const ALL: [Self; 5] = [
        Self::Days7,
        Self::Days30,
        Self::Days90,
        Self::Year,
        Self::Never,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Days7 => tr!("7 days"),
            Self::Days30 => tr!("30 days"),
            Self::Days90 => tr!("90 days"),
            Self::Year => tr!("1 year"),
            Self::Never => tr!("Never"),
        }
    }
}

/// Unix milliseconds when a choice expires, counted from `now_ms`.
fn expires_at(choice: ExpiryChoice, now_ms: u64) -> Option<u64> {
    let day = 24 * 60 * 60 * 1000;
    let add = match choice {
        ExpiryChoice::Days7 => 7 * day,
        ExpiryChoice::Days30 => 30 * day,
        ExpiryChoice::Days90 => 90 * day,
        ExpiryChoice::Year => 365 * day,
        ExpiryChoice::Never => return None,
    };
    Some(now_ms.saturating_add(add))
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// UTC `YYYY-MM-DD` for a Unix millisecond timestamp. Display only.
fn utc_date(unix_ms: u64) -> String {
    let z = (unix_ms / 86_400_000) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{y:04}-{m:02}-{d:02}")
}

fn prefix(id: &McpTokenId) -> String {
    format!("jykmcp_{}…", id.as_str())
}

fn review_label(policy: ReviewPolicy) -> &'static str {
    match policy {
        ReviewPolicy::EveryCall => tr!("every call"),
        ReviewPolicy::FirstSession => tr!("first session"),
        ReviewPolicy::Never => tr!("never"),
    }
}

fn driver_short(driver: DriverKind) -> &'static str {
    match driver {
        DriverKind::Postgres => "PG",
        DriverKind::MySql => "MySQL",
        DriverKind::Databricks => "Databricks",
        DriverKind::Sqlite => "SQLite",
        _ => "DB",
    }
}

fn tool_marks(tools: EnabledTools) -> String {
    format!(
        "search {}  describe {}  run_query {}",
        if tools.search_objects { "✓" } else { "✗" },
        if tools.describe_relation {
            "✓"
        } else {
            "✗"
        },
        if tools.run_query { "✓" } else { "✗" },
    )
}

fn enabled_tools(tools: EnabledTools) -> String {
    let mut names = Vec::new();
    if tools.search_objects {
        names.push("search");
    }
    if tools.describe_relation {
        names.push("describe");
    }
    if tools.run_query {
        names.push("run_query");
    }
    if names.is_empty() {
        tr!("none").to_owned()
    } else {
        names.join(", ")
    }
}

fn outcome_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Sent => tr!("sent"),
        Outcome::NotSent => tr!("not sent"),
        Outcome::Declined => tr!("declined"),
        Outcome::Withheld => tr!("withheld"),
        Outcome::TimedOut => tr!("not answered"),
        Outcome::Cancelled => tr!("cancelled"),
        Outcome::Busy => tr!("busy"),
        Outcome::Paused => tr!("paused"),
    }
}

fn kib(bytes: usize) -> String {
    if bytes < 1024 {
        tr!("{0} B", bytes)
    } else {
        let value = bytes as f64 / 1024.0;
        tr!("{0} KiB", format!("{value:.1}"))
    }
}

fn clock(deadline: Instant) -> String {
    let secs = deadline.saturating_duration_since(Instant::now()).as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn name_error(name: &str, existing: &[String], except: Option<&str>) -> Option<String> {
    let name = name.trim();
    let length = name.chars().count();
    if length == 0 {
        return Some(tr!("Enter a name for the token.").to_owned());
    }
    if length > NAME_LIMIT {
        return Some(tr!("Enter a name of at most 60 characters.").to_owned());
    }
    if existing
        .iter()
        .any(|kept| kept == name && except != Some(kept.as_str()))
    {
        return Some(tr!("A token already has this name.").to_owned());
    }
    None
}

fn expired(record: &TokenRecord, now_ms: u64) -> bool {
    record.expires_unix_ms.is_some_and(|at| at <= now_ms)
}

fn suspension(reason: &str) -> String {
    if reason == CONNECTION_CHANGED {
        tr!("Suspended: the connection changed").to_owned()
    } else {
        tr!("Suspended: {0}", show_controls(reason))
    }
}

/// A saved connection the form can bind a token to.
#[derive(Clone)]
struct SavedConnection {
    id: ConnectionId,
    name: String,
    revision: i64,
    environment: Option<Environment>,
    environment_key: String,
    color: jykre::connection_appearance::ConnectionColor,
    driver: DriverKind,
}

/// What Requests asks the window to do. Card buttons that do not need the
/// password dialog call the endpoint themselves.
enum McpEvent {
    /// Escape: leave the Requests tab without a decision.
    Leave,
    /// Run, after the password dialog when the endpoint asks for one.
    Run(CardId),
}

enum Phase {
    Form,
    Secret,
    Record,
}

enum TabKind {
    Requests(Entity<RequestsView>),
    Token(Entity<TokenView>),
}

struct McpTab {
    kind: TabKind,
    group: TabGroupId,
    _subscriptions: Vec<Subscription>,
}

/// The endpoint and the snapshots the section is showing.
#[derive(Default)]
pub(super) struct Host {
    endpoint: Option<McpEndpoint>,
    watch: Option<Task<()>>,
    demo: bool,
    listener: Listener,
    waiting: usize,
    clients: usize,
    paused: bool,
    port: u16,
    /// Filled only from [`AppView::pull_snapshots`].
    tokens: Vec<TokenRecord>,
    cards: Vec<CardSnapshot>,
    activity: Vec<ActivityEntry>,
    client_labels: Vec<(McpTokenId, String)>,
    scroll: UniformListScrollHandle,
    tabs: HashMap<TabId, McpTab>,
    port_input: Option<Entity<InputState>>,
    _port_sub: Option<Subscription>,
    notice: Option<String>,
    switching: bool,
    shutdown_started: bool,
    quit_settled: bool,
}

impl Host {
    pub(super) fn tab_groups(&self) -> impl Iterator<Item = TabGroupId> + '_ {
        self.tabs.values().map(|tab| tab.group)
    }

    pub(super) fn has_tabs(&self) -> bool {
        !self.tabs.is_empty()
    }

    pub(super) fn owns(&self, tab: TabId) -> bool {
        self.tabs.contains_key(&tab)
    }

    pub(super) fn forget(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
    }

    pub(super) fn demo(&self) -> bool {
        self.demo
    }
}

impl AppView {
    /// Starts the endpoint. In the demo, `listen` is told so and nothing
    /// binds. The switch stays off until this returns.
    pub(super) fn start_mcp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let demo = super::demo::enabled();
        self.mcp.demo = demo;
        self.mcp.endpoint = Some(McpEndpoint::off());
        self.mcp.switching = true;
        self.mcp.port_input =
            Some(cx.new(|cx| InputState::new(window, cx).default_value(DEFAULT_PORT.to_string())));
        if let Some(input) = self.mcp.port_input.clone() {
            self.mcp._port_sub =
                Some(
                    cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                        if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                            this.commit_port(window, cx);
                        }
                    }),
                );
        }
        let dir = jykre::connection_store::data_dir();
        cx.spawn_in(window, async move |weak, cx| {
            let listened =
                jykre::backend::run(async move { Ok(McpEndpoint::listen(dir, demo).await) }).await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.mcp.switching = false;
                match listened {
                    Ok(endpoint) => this.install_endpoint(endpoint, window, cx),
                    Err(error) => {
                        this.mcp.notice = Some(error.to_string());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn install_endpoint(
        &mut self,
        endpoint: McpEndpoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut changes = endpoint.changes();
        let port = endpoint.port().to_string();
        if let Some(input) = &self.mcp.port_input {
            input.update(cx, |input, cx| input.set_value(port, window, cx));
        }
        self.mcp.endpoint = Some(endpoint);
        self.mcp.watch = Some(cx.spawn(async move |weak, cx| {
            loop {
                if changes.changed().await.is_err() {
                    break;
                }
                cx.background_executor().timer(FRAME).await;
                let _ = changes.borrow_and_update();
                if weak.update(cx, |this, cx| this.mcp_changed(cx)).is_err() {
                    break;
                }
            }
        }));
        self.mcp_changed(cx);
    }

    /// Reads the endpoint into the fields the sidebar and the tabs render.
    fn pull_snapshots(&mut self) {
        let Some(endpoint) = &self.mcp.endpoint else {
            return;
        };
        let state = endpoint.state();
        // endpoint snapshots
        self.mcp.listener = listener_of(state);
        self.mcp.waiting = endpoint.waiting();
        self.mcp.paused = endpoint.paused();
        self.mcp.port = endpoint.port();
        self.mcp.clients = endpoint.clients();
        self.mcp.tokens = endpoint.tokens();
        self.mcp.cards = endpoint.cards();
        self.mcp.activity = endpoint.activity();
        self.mcp.client_labels = endpoint.client_labels();
    }

    fn mcp_changed(&mut self, cx: &mut Context<Self>) {
        self.pull_snapshots();
        let endpoint = self.mcp.endpoint.clone();
        let tokens = self.mcp.tokens.clone();
        let cards = self.mcp.cards.clone();
        let activity = self.mcp.activity.clone();
        let labels = self.mcp.client_labels.clone();
        let names: Vec<String> = tokens.iter().map(|token| token.name.clone()).collect();
        let connections = self.saved_connections();
        for tab in self.mcp.tabs.values() {
            match &tab.kind {
                TabKind::Requests(view) => {
                    let (endpoint, cards, activity, tokens, labels) = (
                        endpoint.clone(),
                        cards.clone(),
                        activity.clone(),
                        tokens.clone(),
                        labels.clone(),
                    );
                    view.update(cx, |view, cx| {
                        view.apply(endpoint, cards, activity, tokens, labels, cx)
                    });
                }
                TabKind::Token(view) => {
                    let (endpoint, names, connections, tokens) = (
                        endpoint.clone(),
                        names.clone(),
                        connections.clone(),
                        tokens.clone(),
                    );
                    view.update(cx, |view, cx| {
                        view.apply(endpoint, names, connections, tokens, cx)
                    });
                }
            }
        }
        self.rename_mcp_tabs(cx);
        cx.notify();
    }

    fn rename_mcp_tabs(&mut self, cx: &mut Context<Self>) {
        let titles: Vec<(TabId, TabGroupId, String)> = self
            .mcp
            .tabs
            .iter()
            .filter_map(|(id, tab)| match &tab.kind {
                TabKind::Token(view) => Some((*id, tab.group, view.read(cx).title())),
                TabKind::Requests(_) => None,
            })
            .collect();
        for (tab, group, title) in titles {
            let _ = self.tabs.update(cx, |tabs, cx| {
                let _ = tabs.rename_group(group, title.clone(), cx);
                tabs.set_tab_title(tab, title, cx)
            });
        }
    }

    /// Connections a token may bind to, with the colour and environment the
    /// form shows. Unsaved drafts are left out: a token needs an id.
    fn saved_connections(&self) -> Vec<SavedConnection> {
        self.connections
            .iter()
            .filter_map(|connection| {
                let profile = connection.profile.as_ref()?;
                Some(SavedConnection {
                    id: profile.id,
                    name: connection.name.clone(),
                    revision: profile.revision,
                    environment: connection.appearance.environment,
                    environment_key: environment_label(connection.appearance.environment)
                        .to_owned(),
                    color: connection.color,
                    driver: profile.config.driver,
                })
            })
            .collect()
    }

    pub(super) fn sync_mcp(&self, cx: &mut Context<Self>) {
        let connections = self.saved_connections();
        for tab in self.mcp.tabs.values() {
            if let TabKind::Token(view) = &tab.kind {
                let connections = connections.clone();
                view.update(cx, |view, cx| view.set_connections(connections, cx));
            }
        }
    }

    fn endpoint(&self) -> Option<McpEndpoint> {
        self.mcp.endpoint.clone()
    }

    fn commit_port(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mcp.demo {
            return;
        }
        let Some(input) = self.mcp.port_input.clone() else {
            return;
        };
        let text = input.read(cx).value().to_string();
        match text.parse::<u16>() {
            Ok(port) if port > 0 => {
                if let Some(endpoint) = &self.mcp.endpoint {
                    endpoint.set_port(port);
                }
                self.mcp.port = port;
                cx.notify();
            }
            _ => {
                let restore = if self.mcp.port == 0 {
                    DEFAULT_PORT
                } else {
                    self.mcp.port
                };
                input.update(cx, |input, cx| {
                    input.set_value(restore.to_string(), window, cx)
                });
            }
        }
    }

    fn set_server(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.mcp.demo || self.mcp.switching {
            return;
        }
        let endpoint = self.endpoint();
        self.mcp.switching = true;
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let result = jykre::backend::run(async move {
                if let Some(endpoint) = endpoint {
                    endpoint.set_enabled(on).await
                } else {
                    Ok(McpEndpoint::off())
                }
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                this.mcp.switching = false;
                match result {
                    Ok(endpoint) => {
                        // The window is not here; the port field is updated
                        // on the next blur. Install still watches `changes`.
                        let mut changes = endpoint.changes();
                        this.mcp.port = endpoint.port();
                        this.mcp.endpoint = Some(endpoint);
                        this.mcp.watch = Some(cx.spawn(async move |weak, cx| {
                            loop {
                                if changes.changed().await.is_err() {
                                    break;
                                }
                                cx.background_executor().timer(FRAME).await;
                                let _ = changes.borrow_and_update();
                                if weak.update(cx, |this, cx| this.mcp_changed(cx)).is_err() {
                                    break;
                                }
                            }
                        }));
                        this.mcp_changed(cx);
                    }
                    Err(error) => {
                        this.mcp.notice = Some(error.to_string());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if self.mcp.demo || !matches!(self.mcp.listener, Listener::Listening { .. }) {
            return;
        }
        let paused = !self.mcp.paused;
        if let Some(endpoint) = &self.mcp.endpoint {
            endpoint.set_paused(paused);
        }
        self.mcp_changed(cx);
    }

    /// The rail's count, in amber, while cards wait. It is not focusable
    /// on its own: a new card never takes the keyboard.
    pub(super) fn mcp_rail_badge(&self) -> Option<Div> {
        let waiting = self.mcp.waiting;
        (waiting > 0).then(|| {
            div()
                .absolute()
                .top(px(-2.))
                .right(px(-2.))
                .min_w(px(16.))
                .h(px(16.))
                .px_1()
                .rounded(px(8.))
                .bg(rgb(colors().amber))
                .text_color(rgb(colors().bg))
                .text_size(px(10.))
                .flex()
                .items_center()
                .justify_center()
                .child(if waiting > 99 {
                    "99+".to_owned()
                } else {
                    waiting.to_string()
                })
        })
    }

    /// The status bar, only while the server is on (listening, paused, or
    /// off because the port is in use). Clicking it opens Requests.
    pub(super) fn mcp_status_line(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let text = if self.mcp.waiting > 0
            && matches!(
                self.mcp.listener,
                Listener::Listening { .. } | Listener::PortInUse { .. }
            ) {
            tr!("MCP · {0} waiting", self.mcp.waiting)
        } else {
            match self.mcp.listener {
                Listener::Listening { .. } if self.mcp.paused => tr!("MCP · Paused").to_owned(),
                Listener::Listening { .. } => tr!("MCP · Listening").to_owned(),
                Listener::PortInUse { .. } => {
                    tr!("MCP · Off because the port is in use").to_owned()
                }
                Listener::Off | Listener::Demo => return None,
            }
        };
        // Waiting is named while the listener is up. A port that is in use
        // is not waiting; the arm above already returned the port sentence
        // when nothing is waiting. A waiting count with the port in use
        // still uses the waiting sentence, which is the pending state.
        let status = text.clone();
        Some(
            row()
                .id("mcp-status")
                .role(Role::Button)
                .aria_label(status)
                .focusable()
                .tab_index(0)
                .focus_visible(|s| s.bg(rgb(colors().hover)))
                .gap_1()
                .px_1()
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(colors().hover)))
                .child(icon(
                    I::Cable,
                    if self.mcp.waiting > 0 {
                        colors().amber
                    } else {
                        colors().mint
                    },
                ))
                .child(label(text))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.show_mcp(window, cx);
                })),
        )
    }

    /// The shortcut and the status bar: the MCP section, its sidebar, and
    /// the Requests tab. Choosing Requests again shows the open one.
    pub(super) fn show_mcp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.section = Section::Mcp;
        self.sidebar_open = true;
        self.filter_library(cx);
        self.open_requests(window, cx);
    }

    pub(super) fn mcp_list(&self, s: Div, cx: &mut Context<Self>) -> Div {
        let demo = self.mcp.demo;
        s.child(self.mcp_server(cx))
            .when_some(self.mcp.notice.as_ref(), |s, notice| {
                let notice = notice.clone();
                s.child(
                    row()
                        .id("mcp-notice")
                        .role(Role::Alert)
                        .aria_label(notice.clone())
                        .p_2()
                        .items_start()
                        .rounded(px(6.))
                        .bg(rgb(Tone::Error.background()))
                        .child(div().flex_1().text_size(px(12.)).child(notice))
                        .child(
                            icon_button("dismiss-mcp-notice", I::X, tr!("Close notice"))
                                .size(px(24.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.mcp.notice = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(self.mcp_requests_row(cx))
            .map(|s| {
                if self.mcp.tokens.is_empty() {
                    s.child(
                        column()
                            .p_3()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .text_color(rgb(colors().muted))
                                    .child(tr!("No tokens yet. Create one for each MCP client.")),
                            )
                            .child(self.mcp_new_token_button("mcp-empty-new-token", demo, cx)),
                    )
                } else {
                    s.child(
                        div()
                            .id("mcp-tokens")
                            .role(Role::List)
                            .aria_label(tr!("Tokens"))
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(
                                uniform_list(
                                    "mcp-token-entries",
                                    self.mcp.tokens.len(),
                                    cx.processor(move |this, range: Range<usize>, _, cx| {
                                        range
                                            .filter_map(|index| this.mcp_token_row(index, cx))
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .size_full()
                                .track_scroll(&self.mcp.scroll),
                            )
                            .vertical_scrollbar(&self.mcp.scroll),
                    )
                }
            })
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(colors().faint))
                    .child(tr!(
                        "Tokens are kept in the system vault. A token is shown once."
                    )),
            )
    }

    fn mcp_server(&self, cx: &mut Context<Self>) -> Div {
        let demo = self.mcp.demo || matches!(self.mcp.listener, Listener::Demo);
        let on = matches!(self.mcp.listener, Listener::Listening { .. });
        let address = self.mcp_address();
        column()
            .gap_2()
            .child(
                row()
                    .gap_2()
                    .items_center()
                    .child(label(tr!("Server")))
                    .child(self.mcp_switch("mcp-server-off", tr!("Off"), !on, demo, cx, false))
                    .child(self.mcp_switch("mcp-server-on", tr!("On"), on, demo, cx, true))
                    .child(
                        button(
                            "mcp-pause",
                            I::Pause,
                            if self.mcp.paused {
                                tr!("Resume")
                            } else {
                                tr!("Pause")
                            },
                        )
                        .when(demo || !on, |s| s.opacity(0.5))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_pause(cx))),
                    ),
            )
            .child(
                row()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(fonts().mono.clone())
                            .text_size(px(12.))
                            .child(address.clone()),
                    )
                    .child(
                        button("mcp-copy-address", I::Copy, tr!("Copy")).on_click(cx.listener(
                            move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(address.clone()));
                            },
                        )),
                    ),
            )
            .child(
                row()
                    .gap_2()
                    .items_center()
                    .child(label(tr!("Port")))
                    .when_some(self.mcp.port_input.clone(), |s, input| {
                        s.child(
                            div().w(px(88.)).child(
                                Input::new(&input)
                                    .small()
                                    .disabled(demo)
                                    .aria_label(tr!("Port")),
                            ),
                        )
                    })
                    .child(
                        div()
                            .id("mcp-listener")
                            .role(if matches!(self.mcp.listener, Listener::PortInUse { .. }) {
                                Role::Alert
                            } else {
                                Role::Status
                            })
                            .text_size(px(12.))
                            .text_color(rgb(
                                if demo || matches!(self.mcp.listener, Listener::PortInUse { .. }) {
                                    colors().amber
                                } else {
                                    colors().muted
                                },
                            ))
                            .child(self.listener_text()),
                    ),
            )
    }

    fn mcp_switch(
        &self,
        id: &'static str,
        title: &'static str,
        selected: bool,
        demo: bool,
        cx: &mut Context<Self>,
        on: bool,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .role(Role::Button)
            .aria_label(title)
            .aria_toggled(if selected {
                Toggled::True
            } else {
                Toggled::False
            })
            .focusable()
            .tab_index(0)
            .focus_visible(|s| s.border_color(rgb(colors().text)))
            .h(px(28.))
            .px_3()
            .rounded(px(5.))
            .border_1()
            .border_color(rgb(if selected {
                colors().mint
            } else {
                colors().border
            }))
            .when(selected, |s| s.bg(rgb(colors().accent_bg)))
            .when(demo, |s| s.opacity(0.5))
            .cursor_pointer()
            .text_size(px(12.))
            .child(title)
            .on_click(cx.listener(move |this, _, _, cx| {
                if !demo {
                    this.set_server(on, cx);
                }
            }))
    }

    fn mcp_address(&self) -> String {
        let port = match self.mcp.listener {
            Listener::Listening { port } | Listener::PortInUse { port } => port,
            _ => {
                if self.mcp.port == 0 {
                    DEFAULT_PORT
                } else {
                    self.mcp.port
                }
            }
        };
        format!("http://127.0.0.1:{port}/mcp")
    }

    fn listener_text(&self) -> String {
        if self.mcp.demo || matches!(self.mcp.listener, Listener::Demo) {
            return tr!("Not available in the demo").to_owned();
        }
        match self.mcp.listener {
            Listener::Off => tr!("Off").to_owned(),
            Listener::Listening { .. } if self.mcp.paused => tr!("Paused").to_owned(),
            Listener::Listening { .. } => match self.mcp.clients {
                0 => tr!("Listening").to_owned(),
                1 => tr!("Listening · 1 client").to_owned(),
                n => tr!("Listening · {0} clients", n),
            },
            Listener::PortInUse { port } => tr!("Port {0} is in use; choose another", port),
            Listener::Demo => tr!("Not available in the demo").to_owned(),
        }
    }

    fn mcp_requests_row(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let waiting = self.mcp.waiting;
        row()
            .id("mcp-requests")
            .role(Role::Button)
            .aria_label(if waiting == 0 {
                tr!("Requests").to_owned()
            } else {
                tr!("Requests, {0} waiting", waiting)
            })
            .focusable()
            .tab_index(0)
            .focus_visible(|s| s.bg(rgb(colors().hover)))
            .h(px(36.))
            .px_2()
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(colors().hover)))
            .child(icon(I::ListChecks, colors().mint))
            .child(div().flex_1().child(tr!("Requests")))
            .when(waiting > 0, |s| {
                s.child(badge(tr!("{0} waiting", waiting), colors().amber))
            })
            .on_click(cx.listener(|this, _, window, cx| this.open_requests(window, cx)))
    }

    pub(super) fn mcp_new_token_button(
        &self,
        id: &'static str,
        demo: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        button(id, I::Plus, tr!("New token…"))
            .when(demo, |s| s.opacity(0.5))
            .on_click(cx.listener(move |this, _, window, cx| {
                if !demo {
                    this.open_new_token(window, cx);
                }
            }))
    }

    fn mcp_token_row(&self, index: usize, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let record = self.mcp.tokens.get(index)?;
        let id = record.id;
        let saved = self
            .saved_connections()
            .into_iter()
            .find(|connection| connection.id == record.connection_id);
        let now_ms = unix_now_ms();
        let state = if expired(record, now_ms) {
            Some(tr!("Expired").to_owned())
        } else {
            record.suspended.as_deref().map(suspension)
        };
        let name = show_controls(&record.name);
        let prefix = prefix(&id);
        Some(
            column()
                .id(("mcp-token", index))
                .role(Role::ListItem)
                .aria_label(tr!("Open token {0}", name))
                .focusable()
                .tab_index(0)
                .focus_visible(|s| s.bg(rgb(colors().hover)))
                .w_full()
                .h(px(TOKEN_ROW))
                .px_2()
                .py_1()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(colors().hover)))
                .justify_center()
                .gap(px(2.))
                .child(
                    row()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(14.))
                                .child(name),
                        )
                        .child(
                            div()
                                .font_family(fonts().mono.clone())
                                .text_size(px(11.))
                                .text_color(rgb(colors().faint))
                                .child(prefix),
                        ),
                )
                .child(
                    row()
                        .gap_2()
                        .items_center()
                        .child(match &saved {
                            Some(connection) => div()
                                .flex_none()
                                .child(dot(connection_color(connection.color))),
                            None => div(),
                        })
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(rgb(colors().muted))
                                .child(match &saved {
                                    Some(connection) => match connection.environment {
                                        Some(environment) => tr!(
                                            "{0} · {1} ({2})",
                                            driver_short(connection.driver),
                                            connection.name.clone(),
                                            environment_name(environment)
                                        ),
                                        None => tr!(
                                            "{0} · {1}",
                                            driver_short(connection.driver),
                                            connection.name.clone()
                                        ),
                                    },
                                    None => tr!("Connection missing").to_owned(),
                                }),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(rgb(colors().faint))
                                .child(tool_marks(record.tools)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(rgb(colors().faint))
                                .child(review_label(record.review)),
                        )
                        .when_some(state, |s, state| {
                            s.child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.))
                                    .text_color(rgb(colors().amber))
                                    .child(state),
                            )
                        }),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_token(id, window, cx);
                })),
        )
    }

    fn open_requests(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .mcp
            .tabs
            .iter()
            .find_map(|(tab, open)| match &open.kind {
                TabKind::Requests(_) => Some(*tab),
                TabKind::Token(_) => None,
            });
        if let Some(tab) = open {
            self.show_tab(tab, window, cx);
            return;
        }
        let title = tr!("Requests").to_owned();
        let group = self.tabs.update(cx, |tabs, cx| {
            let group = tabs.add_group(title.clone(), rgb(colors().neutral).into(), cx);
            let _ = tabs.set_group_new_tab(group, false, cx);
            group
        });
        let view = cx.new(|cx| RequestsView::new(self.endpoint(), window, cx));
        let events =
            cx.subscribe_in(
                &view,
                window,
                |this, _, event: &McpEvent, window, cx| match event {
                    McpEvent::Leave => {
                        if let Some(tab) = this.mcp_tab_of_requests() {
                            this.close_tab_now(tab, window, cx);
                        }
                    }
                    McpEvent::Run(card) => this.mcp_run(*card, window, cx),
                },
            );
        let Ok(tab) = self
            .tabs
            .update(cx, |tabs, cx| tabs.add_tab(group, title, view.clone(), cx))
        else {
            tracing::error!("could not add the Requests tab");
            return;
        };
        self.mcp.tabs.insert(
            tab,
            McpTab {
                kind: TabKind::Requests(view.clone()),
                group,
                _subscriptions: vec![events],
            },
        );
        self.mcp_changed(cx);
        view.update(cx, |view, cx| view.focus(window, cx));
    }

    fn mcp_tab_of_requests(&self) -> Option<TabId> {
        self.mcp
            .tabs
            .iter()
            .find_map(|(tab, open)| match &open.kind {
                TabKind::Requests(_) => Some(*tab),
                TabKind::Token(_) => None,
            })
    }

    fn open_new_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mcp.demo {
            return;
        }
        self.open_token_view(None, window, cx);
    }

    fn open_token(&mut self, id: McpTokenId, window: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .mcp
            .tabs
            .iter()
            .find_map(|(tab, open)| match &open.kind {
                TabKind::Token(view) if view.read(cx).token_id() == Some(id) => Some(*tab),
                _ => None,
            });
        if let Some(tab) = open {
            self.show_tab(tab, window, cx);
            return;
        }
        self.open_token_view(Some(id), window, cx);
    }

    fn open_token_view(
        &mut self,
        id: Option<McpTokenId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let record = id.and_then(|id| self.mcp.tokens.iter().find(|token| token.id == id).cloned());
        let title = record
            .as_ref()
            .map(|record| show_controls(&record.name))
            .unwrap_or_else(|| tr!("New token").to_owned());
        let group = self.tabs.update(cx, |tabs, cx| {
            let group = tabs.add_group(title.clone(), rgb(colors().neutral).into(), cx);
            let _ = tabs.set_group_new_tab(group, false, cx);
            group
        });
        let names: Vec<String> = self
            .mcp
            .tokens
            .iter()
            .map(|token| token.name.clone())
            .collect();
        let view = cx.new(|cx| {
            TokenView::new(
                record,
                names,
                self.saved_connections(),
                self.endpoint(),
                window,
                cx,
            )
        });
        let watched = view.clone();
        let renamed = cx.observe(&watched, |this, view, cx| {
            let title = view.read(cx).title();
            let found = this.mcp.tabs.iter().find_map(|(tab, open)| {
                let group = open.group;
                match &open.kind {
                    TabKind::Token(token) if token.entity_id() == view.entity_id() => {
                        Some((*tab, group))
                    }
                    _ => None,
                }
            });
            // `found` borrowed tabs; the group is copied above.
            if let Some((tab, group)) = found {
                let _ = this.tabs.update(cx, |tabs, cx| {
                    let _ = tabs.rename_group(group, title.clone(), cx);
                    tabs.set_tab_title(tab, title, cx)
                });
            }
        });
        let Ok(tab) = self
            .tabs
            .update(cx, |tabs, cx| tabs.add_tab(group, title, view.clone(), cx))
        else {
            tracing::error!("could not add a token tab");
            return;
        };
        self.mcp.tabs.insert(
            tab,
            McpTab {
                kind: TabKind::Token(view.clone()),
                group,
                _subscriptions: vec![renamed],
            },
        );
        view.update(cx, |view, cx| view.focus(window, cx));
        cx.notify();
    }

    /// Puts the keyboard on an MCP tab's frame, not on a card button.
    pub(super) fn focus_mcp_tab(
        &self,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.mcp.tabs.get(&tab).map(|tab| &tab.kind) {
            Some(TabKind::Requests(view)) => {
                view.update(cx, |view, cx| view.focus(window, cx));
                true
            }
            Some(TabKind::Token(view)) => {
                view.update(cx, |view, cx| view.focus(window, cx));
                true
            }
            None => false,
        }
    }

    /// Closing drops the one-time secret from the field before the tab goes.
    pub(super) fn request_mcp_close(
        &mut self,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_mcp_secret(tab, window, cx);
        self.close_tab_now(tab, window, cx);
    }

    pub(super) fn clear_mcp_secret(&self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(TabKind::Token(view)) = self.mcp.tabs.get(&tab).map(|tab| &tab.kind) {
            view.update(cx, |view, cx| view.clear_secret(window, cx));
        }
    }

    /// Run. When the endpoint says the connection still needs a password,
    /// the existing dialog asks; cancelling it is a decline.
    fn mcp_run(&mut self, card: CardId, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(endpoint) = self.endpoint() else {
            return;
        };
        let Some(connection) = endpoint.password_needed(card) else {
            endpoint.approve_statement(card);
            return;
        };
        let index = self.connections.iter().position(|candidate| {
            candidate
                .profile
                .as_ref()
                .is_some_and(|profile| profile.id == connection)
        });
        let Some(index) = index else {
            endpoint.password_cancelled(card);
            return;
        };
        let approve = endpoint.clone();
        let cancel = endpoint;
        self.ask_password(
            index,
            Box::new(move |_, _, _| {
                approve.approve_statement(card);
            }),
            Box::new(move |_, _| {
                cancel.password_cancelled(card);
            }),
            cx,
        );
    }

    /// The listener, so a close that already waits on Automation can stop
    /// it in the same task.
    pub(super) fn mcp_endpoint(&self) -> Option<McpEndpoint> {
        self.mcp.endpoint.clone()
    }

    /// The listener is stopping because the window is closing.
    pub(super) fn mcp_mark_shutdown(&mut self) {
        self.mcp.shutdown_started = true;
    }

    /// The listener has stopped and the window may go.
    pub(super) fn mcp_mark_closed(&mut self) {
        self.mcp.shutdown_started = true;
        self.mcp.quit_settled = true;
    }

    /// `true` when nothing is listening, or the listener has stopped.
    /// Otherwise it shuts the listener down and closes the window after.
    pub(super) fn mcp_may_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.mcp.quit_settled {
            return true;
        }
        let Some(endpoint) = self.endpoint() else {
            return true;
        };
        if matches!(endpoint.state(), ListenState::Off | ListenState::Demo) {
            return true;
        }
        if self.mcp.shutdown_started {
            return false;
        }
        self.mcp.shutdown_started = true;
        cx.spawn_in(window, async move |weak, cx| {
            let _ = jykre::backend::run(async move {
                endpoint.shutdown().await;
                Ok(())
            })
            .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.mcp.quit_settled = true;
                this.closing = true;
                window.remove_window();
                cx.notify();
            });
        })
        .detach();
        false
    }
}

/// One card plus what the endpoint said about the statement.
struct ShownCard {
    snap: CardSnapshot,
    changes_data: bool,
}

struct ShownGroup {
    name: String,
    connection: String,
    production: bool,
    tools: String,
    review: String,
    client: Option<String>,
    cards: Vec<ShownCard>,
}

struct RequestsView {
    endpoint: Option<McpEndpoint>,
    groups: Vec<ShownGroup>,
    activity: Vec<ActivityEntry>,
    running_since: HashMap<CardId, Instant>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    tick: Option<Task<()>>,
}

impl EventEmitter<McpEvent> for RequestsView {}

impl RequestsView {
    fn new(endpoint: Option<McpEndpoint>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            endpoint,
            groups: Vec::new(),
            activity: Vec::new(),
            running_since: HashMap::new(),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            tick: None,
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    fn apply(
        &mut self,
        endpoint: Option<McpEndpoint>,
        cards: Vec<CardSnapshot>,
        activity: Vec<ActivityEntry>,
        tokens: Vec<TokenRecord>,
        labels: Vec<(McpTokenId, String)>,
        cx: &mut Context<Self>,
    ) {
        self.endpoint = endpoint;
        let mut order: Vec<McpTokenId> = tokens.iter().map(|token| token.id).collect();
        for card in &cards {
            if !order.contains(&card.token) {
                order.push(card.token);
            }
        }
        let mut groups = Vec::new();
        for token in order {
            let own: Vec<CardSnapshot> = cards
                .iter()
                .filter(|card| card.token == token)
                .cloned()
                .collect();
            if own.is_empty() {
                continue;
            }
            let record = tokens.iter().find(|record| record.id == token);
            let name = record
                .map(|record| show_controls(&record.name))
                .unwrap_or_else(|| prefix(&token));
            let (connection, production) = record
                .map(|record| {
                    let production = record.environment_label == Environment::Production.key();
                    let connection = if record.environment_label.is_empty() {
                        record.connection_id.to_string()
                    } else {
                        format!(
                            "{} ({})",
                            record.connection_id,
                            show_controls(&record.environment_label)
                        )
                    };
                    (connection, production)
                })
                .unwrap_or_else(|| (tr!("Connection missing").to_owned(), false));
            let tools = record
                .map(|record| enabled_tools(record.tools))
                .unwrap_or_else(|| tr!("none").to_owned());
            let review = record
                .map(|record| review_label(record.review).to_owned())
                .unwrap_or_default();
            let client = labels
                .iter()
                .find(|(id, _)| *id == token)
                .map(|(_, name)| show_controls(name));
            let cards = own
                .into_iter()
                .map(|snap| {
                    let changes_data = self
                        .endpoint
                        .as_ref()
                        .is_some_and(|endpoint| endpoint.changes_data(snap.id));
                    if matches!(snap.place, CardPlace::Running) {
                        self.running_since
                            .entry(snap.id)
                            .or_insert_with(Instant::now);
                    }
                    ShownCard { snap, changes_data }
                })
                .collect();
            groups.push(ShownGroup {
                name,
                connection,
                production,
                tools,
                review,
                client,
                cards,
            });
        }
        self.groups = groups;
        self.activity = activity;
        self.follow(cx);
        cx.notify();
    }

    fn follow(&mut self, cx: &mut Context<Self>) {
        let live = self.groups.iter().any(|group| {
            group.cards.iter().any(|card| {
                matches!(
                    card.snap.place,
                    CardPlace::Front { .. } | CardPlace::Running
                )
            })
        });
        if !live {
            self.tick = None;
            return;
        }
        if self.tick.is_some() {
            return;
        }
        self.tick = Some(cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = weak
                    .update(cx, |this, cx| {
                        let live = this.groups.iter().any(|group| {
                            group.cards.iter().any(|card| {
                                matches!(
                                    card.snap.place,
                                    CardPlace::Front { .. } | CardPlace::Running
                                )
                            })
                        });
                        if !live {
                            this.tick = None;
                        }
                        cx.notify();
                        live
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        }));
    }

    fn decide_send(&self, card: CardId) {
        if let Some(endpoint) = &self.endpoint {
            endpoint.send(card);
        }
    }

    fn decide_dont_send(&self, card: CardId) {
        if let Some(endpoint) = &self.endpoint {
            endpoint.dont_send(card);
        }
    }

    fn decide_decline(&self, card: CardId) {
        if let Some(endpoint) = &self.endpoint {
            endpoint.decline(card);
        }
    }

    fn decide_stop(&self, card: CardId) {
        if let Some(endpoint) = &self.endpoint {
            endpoint.stop(card);
        }
    }

    fn decide_send_result(&self, card: CardId) {
        if let Some(endpoint) = &self.endpoint {
            endpoint.send_result(card);
        }
    }

    fn decide_withhold(&self, card: CardId) {
        if let Some(endpoint) = &self.endpoint {
            endpoint.withhold(card);
        }
    }

    fn card_frame(&self, group_at: usize, card_at: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(group) = self.groups.get(group_at) else {
            return div().into_any_element();
        };
        let Some(card) = group.cards.get(card_at) else {
            return div().into_any_element();
        };
        let id = card.snap.id;
        let place = card.snap.place;
        let heading = match place {
            CardPlace::Front { deadline } => {
                tr!("{0}  {1}", card.snap.tool.as_str(), clock(deadline))
            }
            CardPlace::WaitingInLine => tr!("{0}  waiting in line", card.snap.tool.as_str()),
            CardPlace::Running => {
                let elapsed = self
                    .running_since
                    .get(&id)
                    .map(|started| started.elapsed().as_secs())
                    .unwrap_or(0);
                tr!("{0}  {1} s", card.snap.tool.as_str(), elapsed)
            }
        };
        let body = self.card_body(group, card, cx);
        let actions =
            matches!(place, CardPlace::Front { .. }).then(|| self.card_actions(group, card, cx));
        let stop = matches!(place, CardPlace::Running).then(|| {
            row().justify_end().child(
                button(("mcp-stop", id.get()), I::Square, tr!("Stop"))
                    .on_click(cx.listener(move |this, _, _, _| this.decide_stop(id))),
            )
        });
        column()
            .id(("mcp-card", id.get()))
            .role(Role::Status)
            .aria_label(heading.clone())
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(colors().border))
            .bg(rgb(colors().bg))
            .px_3()
            .py_2()
            .gap_2()
            .child(
                div()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(heading),
            )
            .child(body)
            .children(actions)
            .children(stop)
            .into_any_element()
    }

    fn card_body(&self, group: &ShownGroup, card: &ShownCard, cx: &mut Context<Self>) -> Div {
        let id = card.snap.id;
        match &card.snap.content {
            CardContent::Cache { arguments, answer } => column()
                .gap_1()
                .child(
                    div()
                        .font_family(fonts().mono.clone())
                        .text_size(px(12.))
                        .child(show_controls(arguments)),
                )
                .child(label(tr!("Answer to send ({0}):", kib(answer.len()))))
                .child(
                    div()
                        .font_family(fonts().mono.clone())
                        .text_size(px(12.))
                        .whitespace_normal()
                        .child(show_controls(answer)),
                ),
            CardContent::Statement { sql, purpose } => column().gap_2().child(self.purpose(purpose)).child(self.sql(sql)).when(card.changes_data, |s| {
                s.child(badge(tr!("Changes or deletes data"), colors().amber))
            }).when(group.production, |s| {
                s.child(label(tr!("This is a production connection.")))
            }).child(
                div()
                    .id(("mcp-statement-status", id.get()))
                    .role(Role::Status)
                    .child(label(tr!(
                        "It runs in a session of the MCP server's own on {0}. Nothing of the result is sent until you approve it.",
                        group.name.clone()
                    ))),
            ),
            CardContent::Result {
                sql,
                purpose,
                preview_text,
                row_count,
                hidden_column_names,
            } => {
                let hidden: Vec<String> = hidden_column_names.iter().cloned().collect();
                column()
                    .gap_2()
                    .child(self.purpose(purpose))
                    .child(self.sql(sql))
                    .child(label(tr!("{0} rows", *row_count)))
                    .when(!hidden.is_empty(), |s| {
                        s.child(label(tr!("Hidden columns"))).child(
                            row().flex_wrap().gap_1().children(hidden.iter().enumerate().map(|(at, name)| {
                                let name = name.clone();
                                let label_name = show_controls(&name);
                                button(("mcp-unhide", id.get() as usize * 64 + at), I::EyeOff, label_name)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_hidden(id, &name, cx);
                                    }))
                            })),
                        )
                    })
                    .child(label(tr!("What is sent if you send it")))
                    .child(
                        div()
                            .font_family(fonts().mono.clone())
                            .text_size(px(12.))
                            .whitespace_normal()
                            .child(show_controls(preview_text)),
                    )
            }
        }
    }

    fn purpose(&self, purpose: &str) -> Div {
        column()
            .gap_1()
            .p_2()
            .rounded(px(4.))
            .border_1()
            .border_color(rgb(colors().border))
            .child(label(tr!("The client says:")))
            .child(
                div()
                    .font_family(fonts().mono.clone())
                    .text_size(px(12.))
                    .whitespace_normal()
                    .child(show_controls(purpose)),
            )
    }

    fn sql(&self, sql: &str) -> Div {
        div()
            .px_2()
            .py_1()
            .rounded(px(4.))
            .bg(rgb(colors().surface))
            .font_family(fonts().mono.clone())
            .text_size(px(12.))
            .text_color(rgb(colors().blue))
            .whitespace_normal()
            .child(show_controls(sql))
    }

    fn card_actions(&self, group: &ShownGroup, card: &ShownCard, cx: &mut Context<Self>) -> Div {
        let id = card.snap.id;
        let run = if group.production && card.changes_data {
            tr!("Run on production")
        } else {
            tr!("Run")
        };
        match &card.snap.content {
            CardContent::Cache { .. } => row()
                .justify_end()
                .gap_1()
                .child(
                    button(("mcp-dont-send", id.get()), I::X, tr!("Don't send"))
                        .on_click(cx.listener(move |this, _, _, _| this.decide_dont_send(id))),
                )
                .child(
                    primary_button(("mcp-send", id.get()), I::Check, tr!("Send answer"))
                        .on_click(cx.listener(move |this, _, _, _| this.decide_send(id))),
                ),
            CardContent::Statement { .. } => row()
                .justify_end()
                .gap_1()
                .child(
                    button(("mcp-decline", id.get()), I::X, tr!("Decline"))
                        .on_click(cx.listener(move |this, _, _, _| this.decide_decline(id))),
                )
                .child(
                    primary_button(("mcp-run", id.get()), I::Play, run).on_click(cx.listener(
                        move |_this, _, _, cx| {
                            cx.emit(McpEvent::Run(id));
                        },
                    )),
                ),
            CardContent::Result { .. } => row()
                .justify_end()
                .gap_1()
                .child(
                    button(("mcp-withhold", id.get()), I::X, tr!("Don't send"))
                        .on_click(cx.listener(move |this, _, _, _| this.decide_withhold(id))),
                )
                .child(
                    primary_button(("mcp-send-result", id.get()), I::Check, tr!("Send result"))
                        .on_click(cx.listener(move |this, _, _, _| this.decide_send_result(id))),
                ),
        }
    }

    fn toggle_hidden(&mut self, card: CardId, name: &str, cx: &mut Context<Self>) {
        let Some(shown) = self
            .groups
            .iter_mut()
            .flat_map(|group| group.cards.iter_mut())
            .find(|shown| shown.snap.id == card)
        else {
            return;
        };
        let CardContent::Result {
            hidden_column_names,
            ..
        } = &mut shown.snap.content
        else {
            return;
        };
        if !hidden_column_names.remove(name) {
            hidden_column_names.insert(name.to_owned());
        }
        let names: Vec<String> = hidden_column_names.iter().cloned().collect();
        if let Some(endpoint) = &self.endpoint {
            endpoint.set_hidden(card, names);
        }
        cx.notify();
    }

    fn activity_row(&self, index: usize) -> Option<Stateful<Div>> {
        let entry = self.activity.get(index)?;
        let ago = entry.at.elapsed().as_secs();
        let line = if entry.tool.is_cache() && !entry.arguments.is_empty() {
            tr!(
                "{0} s ago  {1} {2} · {3} · {4}",
                ago,
                entry.tool.as_str(),
                show_controls(&entry.arguments),
                outcome_label(entry.outcome),
                kib(entry.byte_length)
            )
        } else {
            tr!(
                "{0} s ago  {1} · {2} · {3}",
                ago,
                entry.tool.as_str(),
                outcome_label(entry.outcome),
                kib(entry.byte_length)
            )
        };
        Some(
            div()
                .id(("mcp-activity", index))
                .h(px(28.))
                .px_2()
                .truncate()
                .text_size(px(12.))
                .text_color(rgb(colors().muted))
                .child(line),
        )
    }
}

impl Render for RequestsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::probe::rendered("McpRequests");
        column()
            .id("mcp-requests-tab")
            .key_context("McpRequests")
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &super::shortcuts::Dismiss, _, cx| {
                cx.emit(McpEvent::Leave);
                cx.stop_propagation();
            }))
            .size_full()
            .bg(rgb(colors().bg))
            .child(
                row()
                    .h(px(48.))
                    .flex_none()
                    .px_3()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(colors().border))
                    .child(icon(I::ListChecks, colors().mint))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr!("Requests")),
                    ),
            )
            .child(
                column()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("mcp-request-cards")
                            .flex_none()
                            .max_h(px(480.))
                            .overflow_y_scroll()
                            .p_3()
                            .when(self.groups.is_empty(), |s| {
                                s.child(label(tr!("No requests are waiting.")))
                            })
                            .children(self.groups.iter().enumerate().map(|(group_at, group)| {
                                column()
                                    .gap_2()
                                    .pb_3()
                                    .child(
                                        div()
                                            .text_size(px(14.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(tr!(
                                                "{0} · {1}",
                                                group.name.clone(),
                                                group.connection.clone()
                                            )),
                                    )
                                    .when_some(group.client.clone(), |s, client| {
                                        s.child(
                                            div()
                                                .text_size(px(12.))
                                                .text_color(rgb(colors().faint))
                                                .child(tr!("says it is {0}", client)),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(colors().muted))
                                            .child(tr!(
                                                "Tools: {0} · Review: {1}",
                                                group.tools.clone(),
                                                group.review.clone()
                                            )),
                                    )
                                    .children(
                                        (0..group.cards.len())
                                            .map(|card_at| self.card_frame(group_at, card_at, cx)),
                                    )
                            })),
                    )
                    .child(
                        column()
                            .flex_1()
                            .min_h(px(120.))
                            .border_t_1()
                            .border_color(rgb(colors().border))
                            .p_2()
                            .gap_1()
                            .child(label(tr!("Activity (this session)")))
                            .when(self.activity.is_empty(), |s| {
                                s.child(label(tr!("No activity yet.")))
                            })
                            .when(!self.activity.is_empty(), |s| {
                                s.child(
                                    div()
                                        .relative()
                                        .flex_1()
                                        .min_h_0()
                                        .child(
                                            uniform_list(
                                                "mcp-activity-entries",
                                                self.activity.len(),
                                                cx.processor(
                                                    move |this, range: Range<usize>, _, _| {
                                                        range
                                                            .filter_map(|index| {
                                                                this.activity_row(index)
                                                            })
                                                            .collect::<Vec<_>>()
                                                    },
                                                ),
                                            )
                                            .size_full()
                                            .track_scroll(&self.scroll),
                                        )
                                        .vertical_scrollbar(&self.scroll),
                                )
                            }),
                    ),
            )
    }
}

/// A token tab: the creation form, the one-time secret, or the token's controls.
struct TokenView {
    phase: Phase,
    id: Option<McpTokenId>,
    record: Option<TokenRecord>,
    name: Entity<InputState>,
    secret: Entity<InputState>,
    name_dirty: bool,
    pending_name: Option<String>,
    connection: Option<ConnectionId>,
    expiry: ExpiryChoice,
    tools: EnabledTools,
    review: ReviewPolicy,
    connections: Vec<SavedConnection>,
    existing_names: Vec<String>,
    endpoint: Option<McpEndpoint>,
    error: Option<String>,
    busy: bool,
    revoked: bool,
    focus: FocusHandle,
    _name_sub: Subscription,
}

impl TokenView {
    fn new(
        record: Option<TokenRecord>,
        existing_names: Vec<String>,
        connections: Vec<SavedConnection>,
        endpoint: Option<McpEndpoint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_text = record
            .as_ref()
            .map(|record| record.name.clone())
            .unwrap_or_default();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("Name"))
                .default_value(name_text)
        });
        let secret = cx.new(|cx| InputState::new(window, cx));
        let name_for_sub = name.clone();
        let _name_sub = cx.subscribe(&name_for_sub, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.name_dirty = true;
                cx.notify();
            }
        });
        let (tools, review, connection, phase, id) = match &record {
            Some(record) => (
                record.tools,
                record.review,
                Some(record.connection_id),
                Phase::Record,
                Some(record.id),
            ),
            None => (
                EnabledTools::default(),
                ReviewPolicy::EveryCall,
                None,
                Phase::Form,
                None,
            ),
        };
        Self {
            phase,
            id,
            record,
            name,
            secret,
            name_dirty: false,
            pending_name: None,
            connection,
            expiry: ExpiryChoice::Days30,
            tools,
            review,
            connections,
            existing_names,
            endpoint,
            error: None,
            busy: false,
            revoked: false,
            focus: cx.focus_handle(),
            _name_sub,
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        match self.phase {
            Phase::Secret => self.secret.update(cx, |input, cx| input.focus(window, cx)),
            Phase::Form | Phase::Record => {
                self.name.update(cx, |input, cx| input.focus(window, cx));
            }
        }
    }

    fn token_id(&self) -> Option<McpTokenId> {
        self.id
    }

    fn title(&self) -> String {
        match self.phase {
            Phase::Form => tr!("New token").to_owned(),
            Phase::Secret | Phase::Record => self
                .record
                .as_ref()
                .map(|record| show_controls(&record.name))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| tr!("New token").to_owned()),
        }
    }

    fn apply(
        &mut self,
        endpoint: Option<McpEndpoint>,
        names: Vec<String>,
        connections: Vec<SavedConnection>,
        tokens: Vec<TokenRecord>,
        cx: &mut Context<Self>,
    ) {
        self.endpoint = endpoint;
        self.existing_names = names;
        self.connections = connections;
        if let Some(id) = self.id {
            let record = tokens.into_iter().find(|token| token.id == id);
            if let Some(record) = &record {
                if !self.name_dirty {
                    self.pending_name = Some(record.name.clone());
                }
                self.tools = record.tools;
                self.review = record.review;
            } else if matches!(self.phase, Phase::Record) {
                self.revoked = true;
            }
            self.record = record;
        }
        cx.notify();
    }

    fn set_connections(&mut self, connections: Vec<SavedConnection>, cx: &mut Context<Self>) {
        self.connections = connections;
        cx.notify();
    }

    fn clear_secret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.secret
            .update(cx, |input, cx| input.set_value("", window, cx));
        if matches!(self.phase, Phase::Secret) {
            self.phase = if self.id.is_some() {
                Phase::Record
            } else {
                Phase::Form
            };
        }
        cx.notify();
    }

    fn selected(&self) -> Option<&SavedConnection> {
        self.connections
            .iter()
            .find(|connection| Some(connection.id) == self.connection)
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let name = self.name.read(cx).value().to_string();
        let except = self.record.as_ref().map(|record| record.name.as_str());
        if name_error(&name, &self.existing_names, except).is_some() {
            self.error = name_error(&name, &self.existing_names, except);
            cx.notify();
            return;
        }
        let Some(connection) = self.selected().cloned() else {
            self.error = Some(tr!("Choose a connection.").to_owned());
            cx.notify();
            return;
        };
        let Some(endpoint) = self.endpoint.clone() else {
            return;
        };
        let expires = expires_at(self.expiry, unix_now_ms());
        let tools = self.tools;
        let review = self.review;
        self.busy = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |weak, cx| {
            let created = jykre::backend::run(async move {
                endpoint
                    .create_token(jykre::mcp::CreateToken {
                        name,
                        connection_id: connection.id,
                        connection_revision: connection.revision,
                        environment_label: connection.environment_key,
                        expires_unix_ms: expires,
                        tools,
                        review,
                    })
                    .await
            })
            .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.busy = false;
                match created {
                    Ok(wire) => this.reveal(wire, window, cx),
                    Err(error) => {
                        this.error = Some(error.to_string());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn reveal(&mut self, wire: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = split_wire(&wire).and_then(|(id, _)| McpTokenId::parse(id)) {
            self.id = Some(id);
        }
        self.secret
            .update(cx, |input, cx| input.set_value(wire, window, cx));
        self.phase = Phase::Secret;
        self.name_dirty = false;
        cx.notify();
    }

    fn rotate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(endpoint), Some(id)) = (self.endpoint.clone(), self.id) else {
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        self.clear_secret(window, cx);
        cx.spawn_in(window, async move |weak, cx| {
            let rotated = jykre::backend::run(async move { endpoint.rotate(id).await }).await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.busy = false;
                match rotated {
                    Ok(wire) => this.reveal(wire, window, cx),
                    Err(error) => {
                        this.phase = Phase::Record;
                        this.error = Some(error.to_string());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn revoke(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            Confirmation {
                title: tr!("Revoke this token?").to_owned(),
                description: tr!("Clients using this token stop working at once.").to_owned(),
                ok: tr!("Revoke"),
                cancel: tr!("Keep it"),
                focus: ConfirmFocus::Dialog,
            },
            move |_, cx| {
                let _ = view.update(cx, |this, cx| this.revoke_now(cx));
            },
        );
    }

    fn revoke_now(&mut self, cx: &mut Context<Self>) {
        let (Some(endpoint), Some(id)) = (self.endpoint.clone(), self.id) else {
            return;
        };
        cx.spawn(async move |weak, cx| {
            let result = jykre::backend::run(async move { endpoint.revoke(id).await }).await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.revoked = true;
                        this.error = None;
                    }
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete_token(&mut self, cx: &mut Context<Self>) {
        let (Some(endpoint), Some(id)) = (self.endpoint.clone(), self.id) else {
            return;
        };
        cx.spawn(async move |weak, cx| {
            let result = jykre::backend::run(async move { endpoint.delete_token(id).await }).await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.revoked = true,
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn save_name(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().to_string();
        let except = self.record.as_ref().map(|record| record.name.as_str());
        if let Some(error) = name_error(&name, &self.existing_names, except) {
            self.error = Some(error);
            cx.notify();
            return;
        }
        let (Some(endpoint), Some(id)) = (self.endpoint.clone(), self.id) else {
            return;
        };
        cx.spawn(async move |weak, cx| {
            let result =
                jykre::backend::run(async move { endpoint.set_name(id, name).await }).await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.name_dirty = false;
                        this.error = None;
                    }
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn save_tools(&mut self, tools: EnabledTools, cx: &mut Context<Self>) {
        self.tools = tools;
        let (Some(endpoint), Some(id)) = (self.endpoint.clone(), self.id) else {
            cx.notify();
            return;
        };
        cx.spawn(async move |weak, cx| {
            let result =
                jykre::backend::run(async move { endpoint.set_tools(id, tools).await }).await;
            let _ = weak.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.error = Some(error.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn save_review(&mut self, review: ReviewPolicy, cx: &mut Context<Self>) {
        self.review = review;
        let (Some(endpoint), Some(id)) = (self.endpoint.clone(), self.id) else {
            cx.notify();
            return;
        };
        cx.spawn(async move |weak, cx| {
            let result =
                jykre::backend::run(async move { endpoint.set_review(id, review).await }).await;
            let _ = weak.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.error = Some(error.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn save_expiry(&mut self, choice: ExpiryChoice, cx: &mut Context<Self>) {
        self.expiry = choice;
        let expires = expires_at(choice, unix_now_ms());
        let (Some(endpoint), Some(id)) = (self.endpoint.clone(), self.id) else {
            cx.notify();
            return;
        };
        cx.spawn(async move |weak, cx| {
            let result =
                jykre::backend::run(async move { endpoint.set_expiry(id, expires).await }).await;
            let _ = weak.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.error = Some(error.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_token(&mut self, cx: &mut Context<Self>) {
        let (Some(endpoint), Some(id)) = (self.endpoint.clone(), self.id) else {
            return;
        };
        cx.spawn(async move |weak, cx| {
            let result = jykre::backend::run(async move { endpoint.confirm_token(id).await }).await;
            let _ = weak.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.error = Some(error.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn ask_run_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.selected().cloned() else {
            self.error = Some(tr!("Choose a connection.").to_owned());
            cx.notify();
            return;
        };
        let production = connection.environment == Some(Environment::Production);
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            Confirmation {
                title: tr!("Allow run_query?").to_owned(),
                description: tr!(
                    "Clients with this token may ask to run statements on {0}. Each statement and each result still waits for you.",
                    connection.name
                ),
                ok: if production {
                    tr!("Allow on production")
                } else {
                    tr!("Allow")
                },
                cancel: tr!("Keep it off"),
                focus: ConfirmFocus::Dialog,
            },
            move |_, cx| {
                let _ = view.update(cx, |this, cx| {
                    let mut tools = this.tools;
                    tools.run_query = true;
                    if matches!(this.phase, Phase::Form) {
                        this.tools = tools;
                        cx.notify();
                    } else {
                        this.save_tools(tools, cx);
                    }
                });
            },
        );
    }

    fn chip(&self, id: impl Into<ElementId>, text: &'static str, on: bool) -> Stateful<Div> {
        div()
            .id(id)
            .role(Role::Button)
            .aria_label(text)
            .aria_toggled(if on { Toggled::True } else { Toggled::False })
            .focusable()
            .tab_index(0)
            .focus_visible(|s| s.border_color(rgb(colors().text)))
            .h(px(28.))
            .px_2()
            .rounded(px(5.))
            .border_1()
            .border_color(rgb(if on { colors().mint } else { colors().border }))
            .when(on, |s| s.bg(rgb(colors().accent_bg)))
            .cursor_pointer()
            .text_size(px(12.))
            .child(text)
    }

    fn form(&self, cx: &mut Context<Self>) -> Div {
        let name = self.name.read(cx).value().to_string();
        let except = self.record.as_ref().map(|record| record.name.as_str());
        let name_problem = name_error(&name, &self.existing_names, except);
        let can_create = name_problem.is_none() && self.connection.is_some() && !self.busy;
        column()
            .gap_3()
            .child(label(tr!("Name")))
            .child(
                Input::new(&self.name)
                    .small()
                    .w_full()
                    .aria_label(tr!("Name")),
            )
            .when_some(name_problem.clone(), |s, problem| {
                s.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(colors().amber))
                        .child(problem),
                )
            })
            .child(label(tr!("Connection")))
            .when(self.connections.is_empty(), |s| {
                s.child(label(tr!(
                    "No saved connections yet. Add one in the explorer."
                )))
            })
            .children(
                self.connections
                    .iter()
                    .enumerate()
                    .map(|(index, connection)| {
                        let id = connection.id;
                        let on = self.connection == Some(id);
                        let production = connection.environment == Some(Environment::Production);
                        row()
                            .id(("mcp-connection", index))
                            .role(Role::Button)
                            .aria_label(connection.name.clone())
                            .aria_toggled(if on { Toggled::True } else { Toggled::False })
                            .focusable()
                            .tab_index(0)
                            .focus_visible(|s| s.bg(rgb(colors().hover)))
                            .h(px(32.))
                            .px_2()
                            .gap_2()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(colors().hover)))
                            .when(on, |s| s.bg(rgb(colors().accent_bg)))
                            .child(dot(connection_color(connection.color)))
                            .child(div().flex_1().truncate().child(connection.name.clone()))
                            .when_some(connection.environment, |s, environment| {
                                s.child(environment_badge(environment))
                            })
                            .when(production, |s| s)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.connection = Some(id);
                                cx.notify();
                            }))
                    }),
            )
            .child(label(tr!("Expires")))
            .child(
                row()
                    .gap_1()
                    .flex_wrap()
                    .children(
                        ExpiryChoice::ALL
                            .into_iter()
                            .enumerate()
                            .map(|(index, choice)| {
                                let on = self.expiry == choice;
                                self.chip(("mcp-expiry", index), choice.label(), on)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.expiry = choice;
                                        if matches!(this.phase, Phase::Record) {
                                            this.save_expiry(choice, cx);
                                        } else {
                                            cx.notify();
                                        }
                                    }))
                            }),
                    ),
            )
            .child(label(tr!("Tools")))
            .child(self.tool_switch(
                "mcp-tool-search",
                tr!("Search objects"),
                self.tools.search_objects,
                cx,
                |tools, on| tools.search_objects = on,
            ))
            .child(self.tool_switch(
                "mcp-tool-describe",
                tr!("Describe tables and views"),
                self.tools.describe_relation,
                cx,
                |tools, on| tools.describe_relation = on,
            ))
            .when(!super::demo::enabled(), |s| {
                s.child(self.run_query_switch(cx))
            })
            .child(label(tr!("Review")))
            .child(
                row()
                    .gap_1()
                    .flex_wrap()
                    .children(
                        ReviewPolicy::ALL
                            .into_iter()
                            .enumerate()
                            .map(|(index, policy)| {
                                let on = self.review == policy;
                                self.chip(("mcp-review", index), review_label(policy), on)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.review = policy;
                                        if matches!(this.phase, Phase::Record) {
                                            this.save_review(policy, cx);
                                        } else {
                                            cx.notify();
                                        }
                                    }))
                            }),
                    ),
            )
            .when(matches!(self.phase, Phase::Form), |s| {
                s.child(
                    primary_button("mcp-create", I::Plus, tr!("Create"))
                        .when(!can_create, |s| s.opacity(0.5))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if can_create {
                                this.create(window, cx);
                            }
                        })),
                )
            })
            .when(matches!(self.phase, Phase::Record), |s| {
                s.child(
                    button("mcp-save-name", I::Check, tr!("Save name"))
                        .on_click(cx.listener(|this, _, _, cx| this.save_name(cx))),
                )
            })
    }

    fn tool_switch(
        &self,
        id: &'static str,
        title: &'static str,
        on: bool,
        cx: &mut Context<Self>,
        set: fn(&mut EnabledTools, bool),
    ) -> Stateful<Div> {
        button(id, if on { I::SquareCheck } else { I::Square }, title)
            .role(Role::CheckBox)
            .aria_label(title)
            .aria_toggled(if on { Toggled::True } else { Toggled::False })
            .when(on, |s| s.text_color(rgb(colors().mint)))
            .on_click(cx.listener(move |this, _, _, cx| {
                let mut tools = this.tools;
                set(&mut tools, !on);
                if matches!(this.phase, Phase::Form) {
                    this.tools = tools;
                    cx.notify();
                } else {
                    this.save_tools(tools, cx);
                }
            }))
    }

    fn run_query_switch(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let on = self.tools.run_query;
        button(
            "mcp-tool-run",
            if on { I::SquareCheck } else { I::Square },
            tr!("Run queries (asks you each time)"),
        )
        .role(Role::CheckBox)
        .aria_label(tr!("Run queries (asks you each time)"))
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .when(on, |s| s.text_color(rgb(colors().mint)))
        .on_click(cx.listener(move |this, _, window, cx| {
            if on {
                let mut tools = this.tools;
                tools.run_query = false;
                if matches!(this.phase, Phase::Form) {
                    this.tools = tools;
                    cx.notify();
                } else {
                    this.save_tools(tools, cx);
                }
            } else {
                this.ask_run_query(window, cx);
            }
        }))
    }

    fn secret_panel(&self, cx: &mut Context<Self>) -> Div {
        let address = format!(
            "http://127.0.0.1:{}/mcp",
            self.endpoint
                .as_ref()
                .map(|endpoint| endpoint.port())
                .unwrap_or(DEFAULT_PORT)
        );
        let snippet = format!("{address}\nAuthorization: Bearer <token>");
        column()
            .gap_3()
            .child(label(tr!("Copy your token")))
            .child(
                Input::new(&self.secret)
                    .w_full()
                    .readonly(true)
                    .font_family(fonts().mono.clone())
                    .aria_label(tr!("Your token")),
            )
            .child(
                button("mcp-copy-token", I::Copy, tr!("Copy")).on_click(cx.listener(
                    |this, _, _, cx| {
                        let wire = this.secret.read(cx).value().to_string();
                        cx.write_to_clipboard(ClipboardItem::new_string(wire));
                    },
                )),
            )
            .child(label(tr!(
                "This is the only time Jykre Studio shows this token. Store it in your MCP client's configuration."
            )))
            .child(
                div()
                    .p_2()
                    .rounded(px(4.))
                    .bg(rgb(colors().surface))
                    .font_family(fonts().mono.clone())
                    .text_size(px(12.))
                    .whitespace_normal()
                    .child(snippet.clone()),
            )
            .child(
                button("mcp-copy-snippet", I::Copy, tr!("Copy snippet")).on_click(cx.listener(
                    move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(snippet.clone()));
                    },
                )),
            )
            .child(
                primary_button("mcp-done", I::Check, tr!("Done")).on_click(cx.listener(
                    |this, _, window, cx| this.clear_secret(window, cx),
                )),
            )
    }

    fn record_panel(&self, cx: &mut Context<Self>) -> Div {
        let now_ms = unix_now_ms();
        let expired = self
            .record
            .as_ref()
            .is_some_and(|record| expired(record, now_ms));
        let suspended = self
            .record
            .as_ref()
            .and_then(|record| record.suspended.as_deref())
            .map(suspension);
        let expires = self
            .record
            .as_ref()
            .map(|record| match record.expires_unix_ms {
                None => tr!("Never").to_owned(),
                Some(ms) => utc_date(ms),
            });
        column()
            .gap_3()
            .when(self.revoked, |s| {
                s.child(label(tr!("This token was revoked.")))
            })
            .when_some(suspended, |s, text| {
                s.child(label(text)).child(
                    button("mcp-confirm-token", I::Check, tr!("Confirm"))
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_token(cx))),
                )
            })
            .when_some(expires, |s, text| s.child(label(tr!("Expires {0}", text))))
            .when(expired, |s| {
                s.child(label(tr!("Expired"))).child(
                    button("mcp-delete-token", I::Trash, tr!("Delete"))
                        .on_click(cx.listener(|this, _, _, cx| this.delete_token(cx))),
                )
            })
            .child(self.form(cx))
            .child(
                row()
                    .gap_2()
                    .child(
                        button("mcp-rotate", I::RefreshCw, tr!("Rotate"))
                            .on_click(cx.listener(|this, _, window, cx| this.rotate(window, cx))),
                    )
                    .child(
                        button("mcp-revoke", I::Trash, tr!("Revoke"))
                            .on_click(cx.listener(|this, _, window, cx| this.revoke(window, cx))),
                    ),
            )
    }
}

impl Render for TokenView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::probe::rendered("McpToken");
        if let Some(name) = self.pending_name.take() {
            let current = self.name.read(cx).value();
            if current.as_ref() != name {
                self.name
                    .update(cx, |input, cx| input.set_value(name, window, cx));
                self.name_dirty = false;
            }
        }
        column()
            .id("mcp-token-tab")
            .key_context("McpToken")
            .track_focus(&self.focus)
            .size_full()
            .bg(rgb(colors().bg))
            .child(
                div()
                    .id("mcp-token-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .child(match self.phase {
                        Phase::Form => self.form(cx),
                        Phase::Secret => self.secret_panel(cx),
                        Phase::Record => self.record_panel(cx),
                    })
                    .when_some(self.error.clone(), |s, error| {
                        s.child(
                            div()
                                .id("mcp-token-error")
                                .role(Role::Alert)
                                .pt_2()
                                .text_size(px(12.))
                                .text_color(rgb(colors().red))
                                .child(error),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{ExpiryChoice, expires_at, prefix, show_controls, utc_date};
    use jykre::mcp::McpTokenId;

    #[test]
    fn controls_and_bidi_marks_are_visible_escapes() {
        crate::app::source_text();
        let shown = show_controls("SELECT\n1\t2\u{202E}x\u{0000}");
        assert!(shown.contains("SELECT\n1\t2"), "{shown}");
        assert!(shown.contains("\\u{202e}"), "{shown}");
        assert!(shown.contains("\\u{0000}"), "{shown}");
        assert!(!shown.contains('\u{202E}'), "{shown}");
        assert!(!shown.contains('\u{0000}'), "{shown}");
        assert_eq!(show_controls("orders"), "orders");
    }

    #[test]
    fn the_prefix_is_the_id_and_thirty_days_is_the_span() {
        crate::app::source_text();
        let id = McpTokenId::parse("abcdefghij");
        let shown = id.as_ref().map(prefix);
        assert_eq!(shown.as_deref(), Some("jykmcp_abcdefghij…"));
        assert_eq!(expires_at(ExpiryChoice::Never, 1_000), None);
        assert_eq!(
            expires_at(ExpiryChoice::Days30, 1_000),
            Some(1_000 + 30 * 24 * 60 * 60 * 1000)
        );
        assert_eq!(utc_date(0), "1970-01-01");
    }
}
