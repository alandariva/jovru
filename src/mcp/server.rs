//! Loopback MCP HTTP server (ADR-0042).
//!
//! `rmcp`'s [`StreamableHttpService`] is a Tower service: it does not bind a
//! socket. This module serves it with HTTP/1 on `127.0.0.1` only. Nothing
//! listens until [`McpEndpoint::listen`] or [`McpEndpoint::bind`], and a
//! disabled or demo endpoint has no socket. A port already taken is
//! [`ListenState::PortInUse`]; the rest of the application keeps running.
//!
//! The bearer check happens before any tool. A token in the query string is
//! refused without being read. Tool answers wait on the broker. Statement
//! text and token secrets are not written to the log.

use super::{
    ActivityEntry, Arrival, Broker, CardId, CardSnapshot, ClientResponse, ClientResult,
    DEFAULT_PORT, EnabledTools, McpSettings, McpTokenId, Outcome, QUERY_DECLINED, QUITTING,
    RunPermit, Settled, TokenRecord, TokenRefusal, TokenRegistry, ToolName, split_wire,
};
use crate::assistant::facts::SchemaScope;
use crate::assistant::released::{self, QueryPreview};
use crate::assistant::run;
use crate::assistant::tools::{CacheTools, DESCRIBE_RELATION, RUN_QUERY, SEARCH_OBJECTS};
use crate::assistant_query::{McpQueryHost, QueryStart, RunOnce};
use crate::connection_appearance::{Appearance, Environment, appearance_key};
use crate::drivers::{
    CancellationToken, ConnectionConfig, ConnectionId, Session, SessionContext, SqlDialect,
    builtin_registry,
};
use crate::tr;
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::server::conn::http1;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use jykre_assistant::ToolCall;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, DiscoverResult, Implementation,
    InitializeRequestParams, JsonObject, ListToolsResult, ProgressNotificationParam,
    ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tracing::Instrument as _;

const BODY_LIMIT: usize = 1024 * 1024;
const MAX_CONNECTIONS: usize = 4;
/// Idle connection. Also hyper's header timer: that timer is one clock for
/// "waiting for the next request" and "reading headers", so it stays at the
/// idle limit. The first request byte starts the separate 10 s budget below.
const IDLE_CONNECTION: Duration = Duration::from_secs(60);
const HEADER_AND_BODY: Duration = Duration::from_secs(10);
const AUTH_WARN_EVERY: Duration = Duration::from_secs(10);
const UNAUTHORIZED: &str = "Unauthorized";
const VAULT_BODY: &str = "Jykre Studio cannot read this token from the system vault.";

/// Whether the loopback socket is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenState {
    /// Disabled, not started, or the bind failed for a reason other than a taken port.
    Off,
    /// Accepting on this loopback address.
    Listening { addr: SocketAddr },
    /// The configured port was already taken. Nothing is listening.
    PortInUse { port: u16 },
    /// Demo: the parameter said not to listen.
    Demo,
}

/// The registry, the broker, and the optional listener.
///
/// `S` defaults to the operating-system vault. Tests pass a store that
/// never calls it. The view uses [`McpEndpoint::listen`] and [`McpEndpoint::off`].
pub struct McpEndpoint<S = jykre_secrets::SystemSecretStore> {
    /// Used when there is no registry (`off` and `demo`).
    local: ListenState,
    shared: Option<Arc<Shared<S>>>,
}

/// The choices for a new token. The endpoint generates its secret and timestamp.
pub struct CreateToken {
    pub name: String,
    pub connection_id: ConnectionId,
    pub connection_revision: i64,
    pub environment_label: String,
    pub expires_unix_ms: Option<u64>,
    pub tools: EnabledTools,
    pub review: super::ReviewPolicy,
}

struct Tasks {
    accept: JoinHandle<()>,
    expire: JoinHandle<()>,
    connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

struct Shared<S> {
    data_dir: PathBuf,
    registry: tokio::sync::Mutex<TokenRegistry<S>>,
    /// A copy of the token records for the view, which reads them without
    /// waiting on the registry lock.
    records: Mutex<Vec<TokenRecord>>,
    broker: Mutex<Broker>,
    tools: Mutex<HashMap<McpTokenId, Arc<CacheTools>>>,
    pending: Mutex<HashMap<CardId, PendingQuery>>,
    /// Statement cards whose connection still needs a password.
    passwords: Mutex<HashMap<CardId, ConnectionId>>,
    /// Statement cards `guard::risk` marks as changing or deleting data.
    risky: Mutex<HashMap<CardId, bool>>,
    /// The client's self-reported name, in memory only. Not logged.
    client_names: Mutex<HashMap<McpTokenId, String>>,
    waiters: Mutex<HashMap<CardId, tokio::sync::oneshot::Sender<Signal>>>,
    warned: Mutex<HashMap<&'static str, Instant>>,
    kept: tokio::sync::Mutex<HashMap<McpTokenId, Box<dyn Session>>>,
    host: McpQueryHost,
    cancel: CancellationToken,
    listen: Mutex<ListenState>,
    port: Mutex<u16>,
    tasks: Mutex<Option<Tasks>>,
    runtime: tokio::runtime::Handle,
}

struct PendingQuery {
    token: McpTokenId,
    preview: QueryPreview,
    sql: String,
    call: ToolCall,
}

enum Signal {
    Settled(Settled),
    Run(RunPermit),
}

struct ConnectionFace {
    name: String,
    config: ConnectionConfig,
    environment: Option<Environment>,
    revision: i64,
}

#[derive(Clone)]
struct Accepted {
    record: TokenRecord,
}

enum WaitStop {
    Cancelled,
    Lost,
}

impl<S> McpEndpoint<S> {
    fn bare(state: ListenState) -> Self {
        Self {
            local: state,
            shared: None,
        }
    }

    fn holding(
        data_dir: PathBuf,
        registry: TokenRegistry<S>,
        state: ListenState,
        port: u16,
        records: Vec<TokenRecord>,
    ) -> Self {
        Self {
            local: ListenState::Off,
            shared: Some(Arc::new(Shared::new(
                data_dir, registry, state, port, records,
            ))),
        }
    }

    async fn abort_tasks(shared: &Shared<S>) {
        let Some(tasks) = lock(&shared.tasks).take() else {
            return;
        };
        tasks.accept.abort();
        tasks.expire.abort();
        // Stop accepting before draining connections; no task may append
        // another connection after the drain or keep the port during a rebind.
        let _ = tasks.accept.await; // Aborted tasks report cancellation.
        let _ = tasks.expire.await;
        let connections = std::mem::take(&mut *lock(&tasks.connections));
        for handle in &connections {
            handle.abort();
        }
        for handle in connections {
            let _ = handle.await; // Aborted tasks report cancellation.
        }
    }
}

impl McpEndpoint<jykre_secrets::SystemSecretStore> {
    /// No registry and no socket.
    pub fn off() -> Self {
        Self::bare(ListenState::Off)
    }

    /// Loads the MCP setting from `data_dir`. `demo` skips the socket
    /// without reading an environment variable. A disabled setting does
    /// not bind. A taken port is reported and does not panic.
    pub async fn listen(data_dir: PathBuf, demo: bool) -> Self {
        if demo {
            return Self::bare(ListenState::Demo);
        }
        let registry = match TokenRegistry::open(&data_dir, jykre_secrets::SystemSecretStore).await
        {
            Ok(registry) => registry,
            Err(_) => return Self::bare(ListenState::Off),
        };
        let port = registry.settings().port;
        let records = registry.settings().tokens.clone();
        if !registry.settings().enabled {
            return Self::holding(data_dir, registry, ListenState::Off, port, records);
        }
        Self::start(data_dir, registry, port).await
    }

    /// Persists the server switch and returns its replacement endpoint.
    /// A failed settings write leaves this endpoint running. Demo endpoints
    /// stay inert. Application shutdown uses [`Self::shutdown`] instead,
    /// so quitting does not change the user's saved choice.
    pub async fn set_enabled(&self, enabled: bool) -> crate::drivers::DriverResult<Self> {
        if self.local == ListenState::Demo {
            return Ok(self.clone());
        }
        let Some(shared) = &self.shared else {
            return Err(crate::drivers::DriverError::new(
                crate::drivers::ErrorCode::NotFound,
                tr!("The MCP server is off."),
            ));
        };
        shared.registry.lock().await.set_enabled(enabled).await?;
        let data_dir = shared.data_dir.clone();
        let registry = TokenRegistry::open(&data_dir, jykre_secrets::SystemSecretStore).await?;
        let port = registry.settings().port;
        self.shutdown().await;
        Ok(if enabled {
            Self::start(data_dir, registry, port).await
        } else {
            Self::park(data_dir, registry, ListenState::Off)
        })
    }
}

impl<S> McpEndpoint<S>
where
    S: jykre_secrets::McpTokenStore + 'static,
{
    /// Listens on `127.0.0.1:port`. Port 0 asks the operating system for a
    /// free port. Does not consult the enabled flag: [`Self::listen`] does.
    pub async fn bind(data_dir: PathBuf, registry: TokenRegistry<S>, port: u16) -> Self {
        Self::start(data_dir, registry, port).await
    }

    pub fn state(&self) -> ListenState {
        let Some(shared) = &self.shared else {
            return self.local.clone();
        };
        lock(&shared.listen).clone()
    }

    pub fn port(&self) -> u16 {
        let Some(shared) = &self.shared else {
            return DEFAULT_PORT;
        };
        *lock(&shared.port)
    }

    pub fn paused(&self) -> bool {
        let Some(shared) = &self.shared else {
            return false;
        };
        lock(&shared.broker).is_paused()
    }

    pub fn clients(&self) -> usize {
        let Some(shared) = &self.shared else {
            return 0;
        };
        let tasks = lock(&shared.tasks);
        let Some(tasks) = tasks.as_ref() else {
            return 0;
        };
        lock(&tasks.connections)
            .iter()
            .filter(|handle| !handle.is_finished())
            .count()
    }

    pub fn tokens(&self) -> Vec<TokenRecord> {
        let Some(shared) = &self.shared else {
            return Vec::new();
        };
        lock(&shared.records).clone()
    }

    pub fn activity(&self) -> Vec<ActivityEntry> {
        let Some(shared) = &self.shared else {
            return Vec::new();
        };
        lock(&shared.broker).activity().to_vec()
    }

    pub fn client_labels(&self) -> Vec<(McpTokenId, String)> {
        let Some(shared) = &self.shared else {
            return Vec::new();
        };
        lock(&shared.client_names)
            .iter()
            .map(|(id, name)| (*id, name.clone()))
            .collect()
    }

    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        let Some(shared) = &self.shared else {
            return Broker::new().changes();
        };
        lock(&shared.broker).changes()
    }

    /// The connection still needs a password before this statement runs.
    pub fn password_needed(&self, card: CardId) -> Option<ConnectionId> {
        let Some(shared) = &self.shared else {
            return None;
        };
        lock(&shared.passwords).get(&card).copied()
    }

    /// The statement changes or deletes data, from `guard::risk`.
    pub fn changes_data(&self, card: CardId) -> bool {
        let Some(shared) = &self.shared else {
            return false;
        };
        lock(&shared.risky).get(&card).copied().unwrap_or(false)
    }

    /// Persists the port. The caller rebinds. Port 0 is ignored.
    pub fn set_port(&self, port: u16) {
        let Some(shared) = &self.shared else {
            return;
        };
        if port == 0 {
            return;
        }
        *lock(&shared.port) = port;
        let runtime = shared.runtime.clone();
        let shared = Arc::clone(shared);
        runtime.spawn(async move {
            let mut registry = shared.registry.lock().await;
            if registry.set_port(port).await.is_ok() {
                *lock(&shared.records) = registry.settings().tokens.clone();
            }
        });
    }

    /// The setting currently in memory. Empty when this endpoint was not given a registry.
    pub async fn settings(&self) -> McpSettings {
        let Some(shared) = &self.shared else {
            return McpSettings::default();
        };
        shared.registry.lock().await.settings().clone()
    }

    pub fn set_paused(&self, paused: bool) {
        let Some(shared) = &self.shared else {
            return;
        };
        lock(&shared.broker).set_paused(paused);
    }

    /// Open cards, including ones waiting in line or running.
    pub fn waiting(&self) -> usize {
        self.cards().len()
    }

    pub fn cards(&self) -> Vec<CardSnapshot> {
        let Some(shared) = &self.shared else {
            return Vec::new();
        };
        lock(&shared.broker).cards()
    }

    /// Moves the statement card to running and hands the permit to the handler.
    pub fn approve_statement(&self, card: CardId) -> bool {
        let Some(shared) = &self.shared else {
            return false;
        };
        let permit = {
            let mut broker = lock(&shared.broker);
            broker.approve_statement(card)
        };
        let Some(permit) = permit else {
            return false;
        };
        let sender = lock(&shared.waiters).remove(&card);
        let Some(sender) = sender else {
            let settled = lock(&shared.broker).stop(card, Instant::now());
            return shared.deliver(settled).is_some();
        };
        if sender.send(Signal::Run(permit)).is_err() {
            let settled = lock(&shared.broker).stop(card, Instant::now());
            shared.deliver(settled);
            return false;
        }
        true
    }

    pub fn decline(&self, card: CardId) -> bool {
        self.finish_card(card, |broker, card| broker.decline(card, Instant::now()))
    }

    pub fn password_cancelled(&self, card: CardId) -> bool {
        self.finish_card(card, |broker, card| {
            broker.password_cancelled(card, Instant::now())
        })
    }

    pub fn stop(&self, card: CardId) -> bool {
        self.finish_card(card, |broker, card| broker.stop(card, Instant::now()))
    }

    pub fn send(&self, card: CardId) -> bool {
        self.finish_card(card, |broker, card| broker.send(card, Instant::now()))
    }

    pub fn dont_send(&self, card: CardId) -> bool {
        self.finish_card(card, |broker, card| broker.dont_send(card, Instant::now()))
    }

    /// Recomputes the text from the stored preview and the columns hidden now.
    pub fn send_result(&self, card: CardId) -> bool {
        let Some(shared) = &self.shared else {
            return false;
        };
        let pending = lock(&shared.pending).get(&card).map(pending_clone);
        let Some(pending) = pending else {
            return false;
        };
        let names = lock(&shared.broker).hidden_names(pending.token);
        let indexes = pending.preview.hidden_by_name(&names);
        let text = released::release(&pending.preview, &indexes);
        let settled = lock(&shared.broker).send_result(card, &text, Instant::now());
        shared.deliver(settled).is_some()
    }

    /// The withheld sentence is the one the in-app path builds.
    pub fn withhold(&self, card: CardId) -> bool {
        let Some(shared) = &self.shared else {
            return false;
        };
        let pending = lock(&shared.pending).get(&card).map(pending_clone);
        let Some(pending) = pending else {
            return false;
        };
        let text = released::withheld(pending.call, pending.sql, &pending.preview).output;
        let settled = lock(&shared.broker).withhold(card, &text, Instant::now());
        shared.deliver(settled).is_some()
    }

    pub fn set_hidden(&self, card: CardId, names: impl IntoIterator<Item = String>) -> bool {
        let Some(shared) = &self.shared else {
            return false;
        };
        lock(&shared.broker).set_hidden(card, names)
    }

    pub async fn forget(&self, id: McpTokenId) {
        let Some(shared) = &self.shared else {
            return;
        };
        shared.forget_token(id).await;
    }

    /// Generates a token, saves its secret in the vault and stores its record.
    /// Fails when the endpoint is off or the registry cannot save the token.
    pub async fn create_token(&self, options: CreateToken) -> crate::drivers::DriverResult<String> {
        let Some(shared) = &self.shared else {
            return Err(crate::drivers::DriverError::new(
                crate::drivers::ErrorCode::NotFound,
                tr!("The MCP server is off."),
            ));
        };
        let token = crate::mcp::McpToken::generate().map_err(|_| {
            crate::drivers::DriverError::new(
                crate::drivers::ErrorCode::Internal,
                tr!("A new token could not be generated."),
            )
        })?;
        let wire = token.expose_wire();
        let created_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let draft = super::TokenDraft {
            name: options.name,
            connection_id: options.connection_id,
            connection_revision: options.connection_revision,
            environment_label: options.environment_label,
            created_unix_ms,
            expires_unix_ms: options.expires_unix_ms,
            tools: options.tools,
            review: options.review,
            token,
        };
        let mut registry = shared.registry.lock().await;
        registry.create(draft).await?;
        *lock(&shared.records) = registry.settings().tokens.clone();
        Ok(wire)
    }

    pub async fn rotate(&self, id: McpTokenId) -> crate::drivers::DriverResult<String> {
        let Some(shared) = &self.shared else {
            return Err(crate::drivers::DriverError::new(
                crate::drivers::ErrorCode::NotFound,
                tr!("The token is not available."),
            ));
        };
        let token = crate::mcp::McpToken::generate().map_err(|_| {
            crate::drivers::DriverError::new(
                crate::drivers::ErrorCode::Internal,
                tr!("A new token could not be generated."),
            )
        })?;
        let wire = token.expose_wire();
        let mut registry = shared.registry.lock().await;
        registry.rotate(&id, token).await?;
        *lock(&shared.records) = registry.settings().tokens.clone();
        drop(registry);
        shared.forget_token(id).await;
        Ok(wire)
    }

    pub async fn revoke(&self, id: McpTokenId) -> crate::drivers::DriverResult<()> {
        self.remove_token(id).await
    }

    pub async fn delete_token(&self, id: McpTokenId) -> crate::drivers::DriverResult<()> {
        self.remove_token(id).await
    }

    async fn remove_token(&self, id: McpTokenId) -> crate::drivers::DriverResult<()> {
        let Some(shared) = &self.shared else {
            return Ok(());
        };
        let mut registry = shared.registry.lock().await;
        registry.revoke(&id).await?;
        *lock(&shared.records) = registry.settings().tokens.clone();
        drop(registry);
        shared.forget_token(id).await;
        Ok(())
    }

    pub async fn set_name(&self, id: McpTokenId, name: String) -> crate::drivers::DriverResult<()> {
        let Some(shared) = &self.shared else {
            return Ok(());
        };
        let mut registry = shared.registry.lock().await;
        registry.set_name(&id, &name).await?;
        *lock(&shared.records) = registry.settings().tokens.clone();
        Ok(())
    }

    pub async fn set_tools(
        &self,
        id: McpTokenId,
        tools: EnabledTools,
    ) -> crate::drivers::DriverResult<()> {
        let Some(shared) = &self.shared else {
            return Ok(());
        };
        let mut registry = shared.registry.lock().await;
        registry.set_tools(&id, tools).await?;
        *lock(&shared.records) = registry.settings().tokens.clone();
        Ok(())
    }

    pub async fn set_review(
        &self,
        id: McpTokenId,
        review: super::ReviewPolicy,
    ) -> crate::drivers::DriverResult<()> {
        let Some(shared) = &self.shared else {
            return Ok(());
        };
        let mut registry = shared.registry.lock().await;
        registry.set_review(&id, review).await?;
        *lock(&shared.records) = registry.settings().tokens.clone();
        Ok(())
    }

    pub async fn set_expiry(
        &self,
        id: McpTokenId,
        expires: Option<u64>,
    ) -> crate::drivers::DriverResult<()> {
        // The form is the user's confirmed choice, including a later date or never.
        let Some(shared) = &self.shared else {
            return Ok(());
        };
        let mut registry = shared.registry.lock().await;
        registry.set_expiry(&id, expires, true).await?;
        *lock(&shared.records) = registry.settings().tokens.clone();
        Ok(())
    }

    pub async fn confirm_token(&self, id: McpTokenId) -> crate::drivers::DriverResult<()> {
        let Some(shared) = &self.shared else {
            return Ok(());
        };
        let connection = {
            let registry = shared.registry.lock().await;
            registry.record(&id).map(|record| record.connection_id)
        };
        let Some(connection) = connection else {
            return Err(crate::drivers::DriverError::new(
                crate::drivers::ErrorCode::NotFound,
                tr!("The token is not available."),
            ));
        };
        let Some(face) = shared.connection_face(connection).await else {
            return Err(crate::drivers::DriverError::new(
                crate::drivers::ErrorCode::NotFound,
                tr!("The connection is not available."),
            ));
        };
        let environment = crate::mcp::environment_label(face.environment);
        let mut registry = shared.registry.lock().await;
        registry.confirm(&id, face.revision, environment).await?;
        *lock(&shared.records) = registry.settings().tokens.clone();
        Ok(())
    }

    /// Closes cards, sessions and the socket.
    pub async fn shutdown(&self) {
        let Some(shared) = &self.shared else {
            return;
        };
        shared.cancel.cancel();
        let settled = lock(&shared.broker).quit(Instant::now());
        shared.dispatch(settled);
        shared.host.close_all().await;
        shared.kept.lock().await.clear();
        lock(&shared.tools).clear();
        Self::abort_tasks(shared).await;
        *lock(&shared.listen) = ListenState::Off;
    }

    fn finish_card(
        &self,
        card: CardId,
        decide: impl FnOnce(&mut Broker, CardId) -> Option<Settled>,
    ) -> bool {
        let Some(shared) = &self.shared else {
            return false;
        };
        let settled = {
            let mut broker = lock(&shared.broker);
            decide(&mut broker, card)
        };
        shared.deliver(settled).is_some()
    }

    fn park(data_dir: PathBuf, registry: TokenRegistry<S>, state: ListenState) -> Self {
        let port = registry.settings().port;
        let records = registry.settings().tokens.clone();
        Self::holding(data_dir, registry, state, port, records)
    }

    async fn start(data_dir: PathBuf, registry: TokenRegistry<S>, port: u16) -> Self {
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                return Self::park(data_dir, registry, ListenState::PortInUse { port });
            }
            Err(_) => return Self::park(data_dir, registry, ListenState::Off),
        };
        let Ok(addr) = listener.local_addr() else {
            return Self::park(data_dir, registry, ListenState::Off);
        };
        let records = registry.settings().tokens.clone();
        let shared = Arc::new(Shared::new(
            data_dir,
            registry,
            ListenState::Listening { addr },
            addr.port(),
            records,
        ));
        let tasks = spawn_listener(Arc::clone(&shared), listener);
        *lock(&shared.tasks) = Some(tasks);
        Self {
            local: ListenState::Off,
            shared: Some(shared),
        }
    }
}

impl<S> Clone for McpEndpoint<S> {
    fn clone(&self) -> Self {
        Self {
            local: self.local.clone(),
            shared: self.shared.clone(),
        }
    }
}

impl<S> Drop for McpEndpoint<S> {
    fn drop(&mut self) {
        let Some(shared) = self.shared.take() else {
            return;
        };
        // The last clone stops the socket. Earlier clones, including the
        // ones the view holds, leave it running.
        if let Ok(shared) = Arc::try_unwrap(shared) {
            shared.cancel.cancel();
            let tasks = match shared.tasks.into_inner() {
                Ok(tasks) => tasks,
                Err(poison) => poison.into_inner(),
            };
            if let Some(tasks) = tasks {
                tasks.accept.abort();
                tasks.expire.abort();
                let mut handles = match tasks.connections.lock() {
                    Ok(handles) => handles,
                    Err(poison) => poison.into_inner(),
                };
                for handle in handles.drain(..) {
                    handle.abort();
                }
            }
        }
    }
}

impl<S> Shared<S> {
    fn new(
        data_dir: PathBuf,
        registry: TokenRegistry<S>,
        state: ListenState,
        port: u16,
        records: Vec<TokenRecord>,
    ) -> Self {
        Self {
            data_dir,
            registry: tokio::sync::Mutex::new(registry),
            records: Mutex::new(records),
            broker: Mutex::new(Broker::new()),
            tools: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            passwords: Mutex::new(HashMap::new()),
            risky: Mutex::new(HashMap::new()),
            client_names: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            warned: Mutex::new(HashMap::new()),
            kept: tokio::sync::Mutex::new(HashMap::new()),
            host: McpQueryHost::new(),
            cancel: CancellationToken::new(),
            listen: Mutex::new(state),
            port: Mutex::new(port),
            tasks: Mutex::new(None),
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl<S> Shared<S>
where
    S: jykre_secrets::McpTokenStore + 'static,
{
    fn deliver(&self, settled: Option<Settled>) -> Option<CardId> {
        let settled = settled?;
        let card = settled.card;
        self.dispatch(vec![settled]);
        Some(card)
    }

    fn dispatch(&self, settled: Vec<Settled>) {
        if settled.is_empty() {
            return;
        }
        let mut waiters = lock(&self.waiters);
        let mut pending = lock(&self.pending);
        for one in settled {
            pending.remove(&one.card);
            if let Some(sender) = waiters.remove(&one.card) {
                let _ = sender.send(Signal::Settled(one));
            }
        }
    }

    fn warn_refused(&self, kind: &'static str) {
        let mut seen = lock(&self.warned);
        let now = Instant::now();
        if seen
            .get(kind)
            .is_some_and(|then| now.duration_since(*then) < AUTH_WARN_EVERY)
        {
            return;
        }
        seen.insert(kind, now);
        drop(seen);
        tracing::warn!(auth = kind, "mcp authentication refused");
    }

    async fn forget_token(&self, id: McpTokenId) {
        lock(&self.tools).remove(&id);
        self.kept.lock().await.remove(&id);
        self.host.close_token(id).await;
    }

    async fn connection_face(&self, id: ConnectionId) -> Option<ConnectionFace> {
        let repository = match jykre_storage::ConfigRepository::open(&self.data_dir).await {
            Ok(repository) => repository,
            Err(_) => return None,
        };
        let listed = repository.list_each().await;
        let stored = repository.setting(&appearance_key(id)).await;
        repository.close().await;
        let profile = listed
            .ok()?
            .into_iter()
            .flatten()
            .find(|profile| profile.id == id)?;
        let appearance = Appearance::from_value(stored.ok().flatten().as_ref());
        Some(ConnectionFace {
            name: profile.name,
            config: profile.config,
            environment: appearance.environment,
            revision: profile.revision,
        })
    }

    async fn description(&self, record: &TokenRecord) -> (String, String) {
        let Some(face) = self.connection_face(record.connection_id).await else {
            return (
                "Jykre Studio".to_owned(),
                "This server's connection is not available.".to_owned(),
            );
        };
        let shown = shown_name(&face.name, face.environment);
        let title = format!("Jykre Studio — {shown}");
        let mut instructions = format!("This server reads the metadata cache of {shown}.");
        if record.tools.run_query {
            instructions.push_str(" Statements run only on ");
            instructions.push_str(&shown);
            instructions.push_str(", and only after the user approves them in Jykre Studio.");
        }
        (title, instructions)
    }

    async fn scope_of(&self, id: ConnectionId) -> Option<SchemaScope> {
        let face = self.connection_face(id).await?;
        let (engine, dialect) = engine_of(&face.config);
        Some(SchemaScope {
            dir: self.data_dir.clone(),
            connection: id,
            config: face.config,
            engine,
            dialect,
            context: SessionContext::default(),
        })
    }

    async fn tools_for(&self, id: McpTokenId, connection: ConnectionId) -> Option<Arc<CacheTools>> {
        if let Some(tools) = lock(&self.tools).get(&id).cloned() {
            return Some(tools);
        }
        let scope = self.scope_of(connection).await?;
        let built = Arc::new(CacheTools::new(scope));
        let mut tools = lock(&self.tools);
        Some(match tools.entry(id) {
            std::collections::hash_map::Entry::Occupied(entry) => Arc::clone(entry.get()),
            std::collections::hash_map::Entry::Vacant(entry) => Arc::clone(entry.insert(built)),
        })
    }

    async fn dialect_of(&self, id: ConnectionId) -> Option<SqlDialect> {
        let face = self.connection_face(id).await?;
        engine_of(&face.config).1
    }

    fn hold(&self, card: CardId) -> tokio::sync::oneshot::Receiver<Signal> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        lock(&self.waiters).insert(card, sender);
        receiver
    }

    async fn wait_signal(
        &self,
        card: CardId,
        mut receiver: tokio::sync::oneshot::Receiver<Signal>,
        context: &RequestContext<RoleServer>,
    ) -> Result<Signal, WaitStop> {
        let progress = context.meta.get_progress_token();
        if let Some(token) = &progress {
            // A notification the client did not ask for is ignored.
            let _ = context
                .peer
                .notify_progress(ProgressNotificationParam::new(token.clone(), 1.0))
                .await;
        }
        let mut ticks: u32 = 0;
        loop {
            tokio::select! {
                biased;
                _ = context.ct.cancelled() => {
                    let settled = lock(&self.broker).cancel(card, Instant::now());
                    self.deliver(settled);
                    return Err(WaitStop::Cancelled);
                }
                incoming = &mut receiver => {
                    return incoming.map_err(|_| WaitStop::Lost);
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    let settled = lock(&self.broker).expire(Instant::now());
                    self.dispatch(settled);
                    ticks = ticks.saturating_add(1);
                    if ticks.is_multiple_of(5)
                        && let Some(token) = &progress
                    {
                        let value = f64::from(ticks / 5 + 1);
                        let _ = context
                            .peer
                            .notify_progress(ProgressNotificationParam::new(token.clone(), value))
                            .await;
                    }
                }
            }
        }
    }

    async fn wait_settled(
        &self,
        card: CardId,
        receiver: tokio::sync::oneshot::Receiver<Signal>,
        context: &RequestContext<RoleServer>,
    ) -> ClientResult {
        match self.wait_signal(card, receiver, context).await {
            Ok(Signal::Settled(settled)) => settled.result,
            Ok(Signal::Run(_)) => error_text(Outcome::Declined, QUERY_DECLINED),
            Err(WaitStop::Cancelled) => ClientResult {
                outcome: Outcome::Cancelled,
                response: None,
            },
            Err(WaitStop::Lost) => error_text(Outcome::Cancelled, QUITTING),
        }
    }
}

fn engine_of(config: &ConnectionConfig) -> (String, Option<SqlDialect>) {
    let Ok(registry) = builtin_registry() else {
        return (String::new(), None);
    };
    let Ok(driver) = registry.get(config.driver) else {
        return (String::new(), None);
    };
    let descriptor = driver.descriptor();
    (descriptor.display_name, Some(descriptor.dialect))
}

fn shown_name(name: &str, environment: Option<Environment>) -> String {
    match environment {
        Some(environment) => format!("{name} ({})", environment_word(environment)),
        None => name.to_owned(),
    }
}

fn environment_word(environment: Environment) -> &'static str {
    match environment {
        Environment::Development => tr!("Development"),
        Environment::Staging => tr!("Staging"),
        Environment::Production => tr!("Production"),
    }
}

fn server_config(title: String, instructions: String) -> ServerConfig {
    let info = Implementation::new("jykre-studio", env!("CARGO_PKG_VERSION")).with_title(title);
    ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        .with_server_info(info)
        .with_instructions(instructions)
}

fn pending_clone(pending: &PendingQuery) -> PendingQuery {
    PendingQuery {
        token: pending.token,
        preview: pending.preview.clone(),
        sql: pending.sql.clone(),
        call: pending.call.clone(),
    }
}

fn error_text(outcome: Outcome, text: &str) -> ClientResult {
    ClientResult {
        outcome,
        response: Some(ClientResponse {
            text: text.to_owned(),
            is_error: true,
        }),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn is_plain_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
}

fn logged_tool_name(name: &str) -> &str {
    if is_plain_tool_name(name) {
        name
    } else {
        "other"
    }
}

fn known_tool(name: &str) -> Option<ToolName> {
    match name {
        SEARCH_OBJECTS => Some(ToolName::SearchObjects),
        DESCRIBE_RELATION => Some(ToolName::DescribeRelation),
        RUN_QUERY => Some(ToolName::RunQuery),
        _ => None,
    }
}

fn tool_enabled(tools: EnabledTools, name: ToolName) -> bool {
    match name {
        ToolName::SearchObjects => tools.search_objects,
        ToolName::DescribeRelation => tools.describe_relation,
        ToolName::RunQuery => tools.run_query,
    }
}

fn unknown_tool(name: &str) -> McpError {
    McpError::invalid_params(format!("Unknown tool: {}", logged_tool_name(name)), None)
}

fn leaks_token(query: &str) -> bool {
    for pair in query.split('&') {
        let name = pair.split_once('=').map(|(name, _)| name).unwrap_or(pair);
        let name = name.to_ascii_lowercase();
        if name == "token" || name == "access_token" || name == "api_key" {
            return true;
        }
    }
    query.to_ascii_lowercase().contains("jykmcp_")
}

fn loopback_origin(value: &str) -> bool {
    let Ok(uri) = value.parse::<hyper::Uri>() else {
        return false;
    };
    let scheme = uri.scheme_str();
    if scheme != Some("http") && scheme != Some("https") {
        return false;
    }
    matches!(
        uri.host(),
        Some("127.0.0.1") | Some("localhost") | Some("::1") | Some("[::1]")
    )
}

fn spawn_listener<S>(shared: Arc<Shared<S>>, listener: TcpListener) -> Tasks
where
    S: jykre_secrets::McpTokenStore + 'static,
{
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_max_request_body_bytes(BODY_LIMIT)
        .with_allowed_origins([
            "http://127.0.0.1:*",
            "http://localhost:*",
            "http://[::1]:*",
            "https://127.0.0.1:*",
            "https://localhost:*",
            "https://[::1]:*",
        ])
        .enforce_origin_validation()
        .with_cancellation_token(shared.cancel.child_token());
    let service = StreamableHttpService::new(
        {
            let shared = Arc::clone(&shared);
            move || {
                Ok(Handler {
                    shared: Arc::clone(&shared),
                })
            }
        },
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let gate = Gate {
        shared: Arc::clone(&shared),
        inner: service,
    };
    let limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let connections = Arc::new(Mutex::new(Vec::new()));
    let accept = {
        let cancel = shared.cancel.clone();
        let connections = Arc::clone(&connections);
        tokio::spawn(async move {
            accept_loop(listener, gate, limit, connections, cancel).await;
        })
    };
    let expire = {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shared.cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        let settled = lock(&shared.broker).expire(Instant::now());
                        shared.dispatch(settled);
                    }
                }
            }
        })
    };
    Tasks {
        accept,
        expire,
        connections,
    }
}

async fn accept_loop<S>(
    listener: TcpListener,
    gate: Gate<S>,
    limit: Arc<Semaphore>,
    connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
    cancel: CancellationToken,
) where
    S: jykre_secrets::McpTokenStore + 'static,
{
    loop {
        let permit = tokio::select! {
            _ = cancel.cancelled() => break,
            permit = Arc::clone(&limit).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else {
            continue;
        };
        let gate = gate.clone();
        let connections = Arc::clone(&connections);
        let handle = tokio::spawn(async move {
            serve_one(stream, gate, permit).await;
        });
        let mut open = lock(&connections);
        open.retain(|handle| !handle.is_finished());
        open.push(handle);
    }
}

async fn serve_one<S>(stream: TcpStream, gate: Gate<S>, permit: OwnedSemaphorePermit)
where
    S: jykre_secrets::McpTokenStore + 'static,
{
    let _permit = permit;
    let io = TokioIo::new(PaceIo::new(stream));
    let service = TowerToHyperService::new(gate);
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new());
    http.header_read_timeout(IDLE_CONNECTION);
    let connection = http.serve_connection(io, service);
    // A reset or a read timeout. The message can echo a header, so it is not logged.
    let _ = connection.await;
}

/// 60 s with no request bytes, then 10 s for the rest of that request.
/// A response write returns the connection to the idle budget, so a card
/// the user has not answered yet is not cut off by the read limit.
struct PaceIo<T> {
    inner: T,
    reading: bool,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl<T> PaceIo<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            reading: false,
            deadline: Box::pin(tokio::time::sleep(IDLE_CONNECTION)),
        }
    }

    fn arm(deadline: &mut Pin<Box<tokio::time::Sleep>>, after: Duration, cx: &mut Context<'_>) {
        *deadline = Box::pin(tokio::time::sleep(after));
        let _ = deadline.as_mut().poll(cx);
    }

    fn timed_out(deadline: &mut Pin<Box<tokio::time::Sleep>>, cx: &mut Context<'_>) -> bool {
        deadline.as_mut().poll(cx).is_ready()
    }
}

impl<T> AsyncRead for PaceIo<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if Self::timed_out(&mut this.deadline, cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "mcp read timed out",
            )));
        }
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(poll, Poll::Ready(Ok(()))) && buf.filled().len() > before && !this.reading {
            this.reading = true;
            Self::arm(&mut this.deadline, HEADER_AND_BODY, cx);
        }
        poll
    }
}

impl<T> AsyncWrite for PaceIo<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(count)) = poll
            && count > 0
        {
            this.reading = false;
            Self::arm(&mut this.deadline, IDLE_CONNECTION, cx);
        }
        poll
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

struct Gate<S> {
    shared: Arc<Shared<S>>,
    inner: StreamableHttpService<Handler<S>, NeverSessionManager>,
}

impl<S> Clone for Gate<S> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            inner: self.inner.clone(),
        }
    }
}

trait BodyOf {
    type Body;
}

impl<B> BodyOf for Response<B> {
    type Body = B;
}

type InnerService<S> = StreamableHttpService<Handler<S>, NeverSessionManager>;
type InnerBody<S> =
    <<InnerService<S> as tower_service::Service<Request<Incoming>>>::Response as BodyOf>::Body;

impl<S> tower_service::Service<Request<Incoming>> for Gate<S>
where
    S: jykre_secrets::McpTokenStore + 'static,
{
    type Response = Response<GateBody<InnerBody<S>>>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = Result<Response<GateBody<InnerBody<S>>>, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Incoming>) -> Self::Future {
        let shared = Arc::clone(&self.shared);
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let request = match authorize(&shared, request).await {
                Ok(request) => request,
                Err((status, body)) => return Ok(text_response(status, body)),
            };
            let response = tower_service::Service::call(&mut inner, request)
                .await
                .unwrap_or_else(|error| match error {});
            Ok(map_pass(response))
        })
    }
}

async fn authorize<S>(
    shared: &Shared<S>,
    request: Request<Incoming>,
) -> Result<Request<Incoming>, (StatusCode, &'static str)>
where
    S: jykre_secrets::McpTokenStore + 'static,
{
    if request.uri().query().is_some_and(leaks_token) {
        shared.warn_refused("query");
        return Err((StatusCode::FORBIDDEN, ""));
    }
    if let Some(origin) = request.headers().get(hyper::header::ORIGIN) {
        let allowed = origin.to_str().is_ok_and(loopback_origin);
        if !allowed {
            shared.warn_refused("origin");
            return Err((StatusCode::FORBIDDEN, ""));
        }
    }
    let wire = match bearer(request.headers()) {
        Bearer::Missing => {
            shared.warn_refused("missing");
            return Err((StatusCode::UNAUTHORIZED, UNAUTHORIZED));
        }
        Bearer::Malformed => {
            shared.warn_refused("malformed");
            return Err((StatusCode::UNAUTHORIZED, UNAUTHORIZED));
        }
        Bearer::Present(wire) => wire,
    };
    let Some((id_text, secret)) = split_wire(wire) else {
        shared.warn_refused("malformed");
        return Err((StatusCode::UNAUTHORIZED, UNAUTHORIZED));
    };
    let Some(id) = McpTokenId::parse(id_text) else {
        shared.warn_refused("unknown");
        return Err((StatusCode::UNAUTHORIZED, UNAUTHORIZED));
    };
    let refusal = {
        let mut registry = shared.registry.lock().await;
        registry
            .secret_matches(&id, secret, now_unix_ms())
            .await
            .err()
    };
    if let Some(refusal) = refusal {
        let kind = match refusal {
            TokenRefusal::Unknown => "unknown",
            TokenRefusal::Expired => "expired",
            TokenRefusal::Suspended => "suspended",
            TokenRefusal::Mismatch => "mismatch",
            TokenRefusal::VaultUnavailable => "vault",
            TokenRefusal::MissingFromVault => "missing_vault",
        };
        shared.warn_refused(kind);
        if matches!(
            refusal,
            TokenRefusal::Unknown | TokenRefusal::Expired | TokenRefusal::Suspended
        ) {
            shared.forget_token(id).await;
        }
        return Err(match refusal {
            TokenRefusal::VaultUnavailable | TokenRefusal::MissingFromVault => {
                (StatusCode::SERVICE_UNAVAILABLE, VAULT_BODY)
            }
            _ => (StatusCode::UNAUTHORIZED, UNAUTHORIZED),
        });
    }
    let record = {
        let registry = shared.registry.lock().await;
        registry.record(&id).cloned()
    };
    let Some(record) = record else {
        shared.warn_refused("unknown");
        return Err((StatusCode::UNAUTHORIZED, UNAUTHORIZED));
    };
    let mut request = request;
    request.extensions_mut().insert(Accepted { record });
    Ok(request)
}

enum Bearer<'a> {
    Missing,
    Malformed,
    Present(&'a str),
}

fn bearer(headers: &hyper::HeaderMap) -> Bearer<'_> {
    let Some(value) = headers.get(hyper::header::AUTHORIZATION) else {
        return Bearer::Missing;
    };
    let Ok(text) = value.to_str() else {
        return Bearer::Malformed;
    };
    let Some((scheme, rest)) = text.split_once(char::is_whitespace) else {
        return Bearer::Malformed;
    };
    let token = rest.trim();
    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
        Bearer::Present(token)
    } else {
        Bearer::Malformed
    }
}

fn text_response<B>(status: StatusCode, text: &'static str) -> Response<GateBody<B>> {
    let bytes = Bytes::from_static(text.as_bytes());
    let length = bytes.len();
    let mut builder = Response::builder().status(status);
    if length > 0 {
        builder = builder.header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8");
    }
    match builder
        .header(hyper::header::CONTENT_LENGTH, length.to_string())
        .body(GateBody::text(bytes))
    {
        Ok(response) => response,
        Err(_) => {
            let mut response = Response::new(GateBody::text(Bytes::from_static(text.as_bytes())));
            *response.status_mut() = status;
            response
        }
    }
}

fn map_pass<B>(response: Response<B>) -> Response<GateBody<B>> {
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, GateBody::pass(body))
}

struct GateBody<B> {
    kind: GateKind<B>,
}

enum GateKind<B> {
    Text(Option<Bytes>),
    Pass(B),
}

impl<B> GateBody<B> {
    fn text(bytes: Bytes) -> Self {
        Self {
            kind: GateKind::Text(Some(bytes)),
        }
    }

    fn pass(body: B) -> Self {
        Self {
            kind: GateKind::Pass(body),
        }
    }
}

impl<B> Body for GateBody<B>
where
    B: Body<Data = Bytes, Error = Infallible> + Unpin,
{
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        match &mut this.kind {
            GateKind::Text(slot) => {
                let Some(bytes) = slot.take() else {
                    return Poll::Ready(None);
                };
                if bytes.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(Frame::data(bytes))))
                }
            }
            GateKind::Pass(body) => Pin::new(body).poll_frame(cx),
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        match &self.kind {
            GateKind::Text(Some(bytes)) => hyper::body::SizeHint::with_exact(bytes.len() as u64),
            GateKind::Text(None) => hyper::body::SizeHint::with_exact(0),
            GateKind::Pass(body) => body.size_hint(),
        }
    }
}

struct Handler<S> {
    shared: Arc<Shared<S>>,
}

impl<S> ServerHandler for Handler<S>
where
    S: jykre_secrets::McpTokenStore + 'static,
{
    fn get_info(&self) -> ServerConfig {
        server_config(
            "Jykre Studio".to_owned(),
            "This server's connection is not available.".to_owned(),
        )
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::InitializeResult, McpError> {
        // The name stays in memory for the Requests tab. It is not logged.
        if let Some(record) = accepted(&context) {
            let name = request.client_info.name.clone();
            if !name.is_empty() {
                lock(&self.shared.client_names).insert(record.id, name);
            }
        }
        let mut info = self.negotiate_initialize(&request)?;
        if let Some(record) = accepted(&context) {
            let (title, instructions) = self.shared.description(&record).await;
            info.server_info =
                Implementation::new("jykre-studio", env!("CARGO_PKG_VERSION")).with_title(title);
            info.instructions = Some(instructions);
        }
        Ok(info)
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, McpError> {
        let (title, instructions) = match accepted(&context) {
            Some(record) => self.shared.description(&record).await,
            None => (
                "Jykre Studio".to_owned(),
                "This server's connection is not available.".to_owned(),
            ),
        };
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            server_config(title, instructions),
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let Some(record) = accepted(&context) else {
            return Err(McpError::invalid_request(UNAUTHORIZED, None));
        };
        let tools = CacheTools::specs(true)
            .into_iter()
            .filter(|spec| {
                known_tool(&spec.name).is_some_and(|tool| tool_enabled(record.tools, tool))
            })
            .map(mcp_tool)
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        let Some(record) = accepted(&context) else {
            return Err(McpError::invalid_request(UNAUTHORIZED, None));
        };
        let span = tracing::info_span!(
            "mcp_tool",
            jykre.assistant.tool = logged_tool_name(&request.name),
            jykre.mcp.outcome = tracing::field::Empty,
            jykre.bytes = tracing::field::Empty,
            jykre.connection.id = %record.connection_id,
            elapsed_ms = tracing::field::Empty,
        );
        let started = Instant::now();
        let finished = self
            .dispatch_tool(record, request, &context)
            .instrument(span.clone())
            .await;
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        span.record("jykre.mcp.outcome", finished.outcome);
        span.record(
            "jykre.bytes",
            crate::telemetry::attributes::count(finished.bytes),
        );
        span.record("elapsed_ms", elapsed);
        finished.result.map(Into::into)
    }
}

struct Finished {
    result: Result<CallToolResult, McpError>,
    outcome: &'static str,
    bytes: usize,
}

impl<S> Handler<S>
where
    S: jykre_secrets::McpTokenStore + 'static,
{
    async fn dispatch_tool(
        &self,
        record: TokenRecord,
        request: CallToolRequestParams,
        context: &RequestContext<RoleServer>,
    ) -> Finished {
        let name = request.name.to_string();
        let Some(tool) = known_tool(&name) else {
            return finished_error(&name, "disabled");
        };
        if !tool_enabled(record.tools, tool) {
            return finished_error(&name, "disabled");
        }
        let call = ToolCall {
            id: String::new(),
            name,
            arguments: request
                .arguments
                .clone()
                .map(serde_json::Value::Object)
                .unwrap_or(serde_json::Value::Null),
        };
        if tool == ToolName::RunQuery {
            self.run_query(&record, call, context).await
        } else {
            self.cache_tool(&record, tool, call, context).await
        }
    }

    async fn cache_tool(
        &self,
        record: &TokenRecord,
        tool: ToolName,
        call: ToolCall,
        context: &RequestContext<RoleServer>,
    ) -> Finished {
        let Some(tools) = self.shared.tools_for(record.id, record.connection_id).await else {
            return finished_client(
                error_text(Outcome::NotSent, "Error: this connection is not available."),
                false,
            );
        };
        let answer = tools.call(&call).await;
        let arguments = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_owned());
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let arrival = {
            let mut broker = lock(&self.shared.broker);
            let arrival = broker.submit_cache(
                record.id,
                tool,
                record.review,
                &arguments,
                &answer.text,
                Instant::now(),
            );
            if let Arrival::Held(card) = &arrival {
                lock(&self.shared.waiters).insert(*card, sender);
            }
            arrival
        };
        let result = match arrival {
            Arrival::Ready(result) => result,
            Arrival::Held(card) => self.shared.wait_settled(card, receiver, context).await,
        };
        finished_client(result, answer.failed)
    }

    async fn run_query(
        &self,
        record: &TokenRecord,
        call: ToolCall,
        context: &RequestContext<RoleServer>,
    ) -> Finished {
        let dialect = self.shared.dialect_of(record.connection_id).await;
        let sql = match run::query_sql(&call, dialect) {
            Ok(sql) => sql,
            Err(reason) => {
                return finished_client(error_text(Outcome::Declined, &reason), false)
                    .with_outcome("rejected");
            }
        };
        let purpose = run::query_purpose(&call).unwrap_or("");
        let risky = crate::guard::risk(&sql, dialect).is_some();
        let needs_password =
            McpQueryHost::password_required(&self.shared.data_dir, record.connection_id).await;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let arrival = {
            let mut broker = lock(&self.shared.broker);
            let arrival = broker.submit_query(record.id, &sql, purpose, Instant::now());
            if let Arrival::Held(card) = &arrival {
                lock(&self.shared.waiters).insert(*card, sender);
                lock(&self.shared.risky).insert(*card, risky);
                if needs_password {
                    lock(&self.shared.passwords).insert(*card, record.connection_id);
                }
            }
            arrival
        };
        let card = match arrival {
            Arrival::Held(card) => card,
            Arrival::Ready(result) => return finished_client(result, false),
        };
        let signal = self.shared.wait_signal(card, receiver, context).await;
        let permit = match signal {
            Ok(Signal::Run(permit)) => permit,
            Ok(Signal::Settled(settled)) => return finished_client(settled.result, false),
            Err(WaitStop::Cancelled) => {
                return finished_client(
                    ClientResult {
                        outcome: Outcome::Cancelled,
                        response: None,
                    },
                    false,
                );
            }
            Err(WaitStop::Lost) => {
                return finished_client(error_text(Outcome::Cancelled, QUITTING), false);
            }
        };
        self.execute_statement(record, call, sql, permit, context)
            .await
    }

    async fn execute_statement(
        &self,
        record: &TokenRecord,
        call: ToolCall,
        sql: String,
        permit: RunPermit,
        context: &RequestContext<RoleServer>,
    ) -> Finished {
        let card = permit.card();
        let cancel = CancellationToken::new();
        let bridge = cancel.clone();
        let stop = self.shared.cancel.clone();
        let request_stop = context.ct.clone();
        let watch = tokio::spawn(async move {
            tokio::select! {
                _ = stop.cancelled() => bridge.cancel(),
                _ = request_stop.cancelled() => bridge.cancel(),
            }
        });
        let host = &self.shared.host;
        let data_dir: &Path = self.shared.data_dir.as_path();
        let token_id = record.id;
        let connection_id = record.connection_id;
        let ran = crate::assistant_query::McpQueryHost::run(
            host,
            data_dir,
            token_id,
            connection_id,
            sql.clone(),
            cancel,
        )
        .await;
        watch.abort();
        if context.ct.is_cancelled() {
            let settled = lock(&self.shared.broker).cancel(card, Instant::now());
            self.shared.deliver(settled);
            return finished_client(
                ClientResult {
                    outcome: Outcome::Cancelled,
                    response: None,
                },
                false,
            );
        }
        let preview = match ran {
            Ok(RunOnce {
                preview,
                session,
                transaction_closed,
            }) => {
                if transaction_closed {
                    drop(session);
                    self.shared.kept.lock().await.remove(&token_id);
                    self.shared.host.close_token(token_id).await;
                } else if let Some(session) = session {
                    self.shared.kept.lock().await.insert(token_id, session);
                }
                preview
            }
            Err(QueryStart::NeedsPassword) => {
                return self.decline_running(card);
            }
            Err(QueryStart::Unavailable(error)) => QueryPreview::from_error(&error, &sql),
        };
        let hidden = lock(&self.shared.broker).hidden_names(token_id);
        let indexes = preview.hidden_by_name(&hidden);
        let text = released::release(&preview, &indexes);
        let names: Vec<String> = indexes
            .iter()
            .filter_map(|index| {
                preview
                    .columns
                    .get(*index)
                    .map(|column| column.name.to_lowercase())
            })
            .collect();
        let rows = preview.rows.len();
        lock(&self.shared.pending).insert(
            card,
            PendingQuery {
                token: token_id,
                preview,
                sql,
                call,
            },
        );
        let receiver = self.shared.hold(card);
        let finished =
            lock(&self.shared.broker).finish_running(permit, &text, rows, names, Instant::now());
        if !finished {
            lock(&self.shared.waiters).remove(&card);
            lock(&self.shared.pending).remove(&card);
            let quitting = lock(&self.shared.broker).is_quitting();
            let text = if quitting { QUITTING } else { QUERY_DECLINED };
            let outcome = if quitting {
                Outcome::Cancelled
            } else {
                Outcome::Declined
            };
            return finished_client(error_text(outcome, text), false);
        }
        finished_client(
            self.shared.wait_settled(card, receiver, context).await,
            false,
        )
    }

    fn decline_running(&self, card: CardId) -> Finished {
        let settled = {
            let mut broker = lock(&self.shared.broker);
            broker
                .password_cancelled(card, Instant::now())
                .or_else(|| broker.decline(card, Instant::now()))
                .or_else(|| broker.stop(card, Instant::now()))
        };
        match settled {
            Some(settled) => {
                self.shared.deliver(Some(settled.clone()));
                finished_client(settled.result, false)
            }
            None => finished_client(error_text(Outcome::Declined, QUERY_DECLINED), false),
        }
    }
}

fn finished_error(name: &str, outcome: &'static str) -> Finished {
    let error = unknown_tool(name);
    let bytes = error.message.len();
    Finished {
        result: Err(error),
        outcome,
        bytes,
    }
}

fn finished_client(mut result: ClientResult, failed: bool) -> Finished {
    if failed
        && result.outcome == Outcome::Sent
        && let Some(response) = &mut result.response
    {
        response.is_error = true;
    }
    let outcome = result.outcome.as_str();
    let Some(response) = result.response else {
        return Finished {
            result: Err(McpError::invalid_request("cancelled", None)),
            outcome,
            bytes: 0,
        };
    };
    let bytes = response.text.len();
    let block = ContentBlock::text(response.text);
    let call = if response.is_error {
        CallToolResult::error(vec![block])
    } else {
        CallToolResult::success(vec![block])
    };
    Finished {
        result: Ok(call),
        outcome,
        bytes,
    }
}

impl Finished {
    fn with_outcome(mut self, outcome: &'static str) -> Self {
        self.outcome = outcome;
        self
    }
}

fn accepted(context: &RequestContext<RoleServer>) -> Option<TokenRecord> {
    context
        .extensions
        .get::<hyper::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<Accepted>())
        .map(|accepted| accepted.record.clone())
}

fn mcp_tool(spec: jykre_assistant::ToolSpec) -> Tool {
    let annotations = match spec.name.as_str() {
        RUN_QUERY => ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .open_world(false),
        _ => ToolAnnotations::new().read_only(true).open_world(false),
    };
    let title = match spec.name.as_str() {
        SEARCH_OBJECTS => "Search objects",
        DESCRIBE_RELATION => "Describe a table or view",
        RUN_QUERY => "Run a query (asks the user)",
        _ => "Tool",
    };
    Tool::new(
        spec.name,
        spec.description,
        input_schema(&spec.input_schema),
    )
    .with_title(title)
    .with_annotations(annotations)
}

fn input_schema(value: &serde_json::Value) -> Arc<JsonObject> {
    match value.as_object() {
        Some(object) => Arc::new(object.clone()),
        None => Arc::new(JsonObject::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        DirectorySettings, EnabledTools, McpSettings, ReviewPolicy, SETTINGS_KEY, TokenRecord,
        TokenRegistry,
    };
    use super::{ListenState, McpEndpoint, UNAUTHORIZED, lock};
    use crate::drivers::{ConnectionId, DriverResult, SecretString};
    use jykre_core::async_trait;
    use jykre_secrets::{McpTokenAccount, McpTokenStore};
    use jykre_storage::ConfigRepository;
    use secrecy::ExposeSecret as _;
    use std::collections::HashMap;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::net::{TcpListener, TcpStream};

    struct MemoryVault {
        entries: StdMutex<HashMap<String, String>>,
    }

    impl MemoryVault {
        fn new() -> Self {
            Self {
                entries: StdMutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl McpTokenStore for MemoryVault {
        async fn get(&self, account: McpTokenAccount) -> DriverResult<Option<SecretString>> {
            let entries = lock(&self.entries);
            Ok(entries
                .get(&account.name())
                .cloned()
                .map(SecretString::from))
        }

        async fn set(&self, account: McpTokenAccount, token: SecretString) -> DriverResult<()> {
            let mut entries = lock(&self.entries);
            entries.insert(account.name(), token.expose_secret().to_owned());
            Ok(())
        }

        async fn delete(&self, account: McpTokenAccount) -> DriverResult<()> {
            lock(&self.entries).remove(&account.name());
            Ok(())
        }
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("jykre-mcp-server-{}", ConnectionId::new()))
    }

    async fn save_fixture_settings(dir: &Path, settings: &McpSettings) {
        // Each fixture owns a folder. The application's global latest-write
        // queue would let a parallel fixture supersede this setup.
        let repository = ConfigRepository::open(dir).await.unwrap();
        repository
            .set_setting(SETTINGS_KEY, &settings.to_value())
            .await
            .unwrap();
        repository.close().await;
    }

    async fn listening() -> (McpEndpoint<MemoryVault>, SocketAddr, String) {
        let token = crate::mcp::McpToken::generate().unwrap();
        let wire = token.expose_wire();
        let vault = MemoryVault::new();
        let account = McpTokenAccount::parse(token.id().as_str()).unwrap();
        vault
            .set(
                account,
                SecretString::from(token.expose_secret().to_owned()),
            )
            .await
            .unwrap();
        let record = TokenRecord {
            id: *token.id(),
            name: "laptop".to_owned(),
            connection_id: ConnectionId::new(),
            connection_revision: 1,
            environment_label: String::new(),
            created_unix_ms: 1,
            expires_unix_ms: None,
            tools: EnabledTools::default(),
            review: ReviewPolicy::EveryCall,
            suspended: None,
        };
        let settings = McpSettings {
            enabled: true,
            port: 1,
            tokens: vec![record],
        };
        let dir = temp_dir();
        let registry = TokenRegistry::new(vault, DirectorySettings::new(&dir), settings);
        let endpoint = McpEndpoint::bind(dir, registry, 0).await;
        let addr = match endpoint.state() {
            ListenState::Listening { addr } => Some(addr),
            _ => None,
        }
        .expect("ephemeral loopback bind");
        (endpoint, addr, wire)
    }

    async fn round_trip(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0_u8; 2048];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(left, stream.read(&mut buf)).await {
                Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                Ok(Ok(count)) => raw.extend_from_slice(&buf[..count]),
            }
        }
        String::from_utf8_lossy(&raw).into_owned()
    }

    fn status_of(raw: &str) -> u16 {
        raw.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0)
    }

    fn body_of(raw: &str) -> &str {
        raw.split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("")
    }

    #[tokio::test]
    async fn disabled_endpoint_has_no_socket() {
        let dir = temp_dir();
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let settings = McpSettings {
            enabled: false,
            port,
            tokens: Vec::new(),
        };
        save_fixture_settings(&dir, &settings).await;
        let endpoint = McpEndpoint::listen(dir, false).await;
        assert!(matches!(endpoint.state(), ListenState::Off));
        let again = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await;
        assert!(again.is_ok());
        endpoint.shutdown().await;
    }

    #[tokio::test]
    async fn server_switch_persists_and_releases_the_port_before_restarting() {
        let _settings_guard = super::super::SETTINGS_TEST_LOCK.lock().await;
        let dir = temp_dir();
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = probe.local_addr().unwrap();
        let settings = McpSettings {
            port: addr.port(),
            ..McpSettings::default()
        };
        save_fixture_settings(&dir, &settings).await;
        let endpoint = McpEndpoint::listen(dir.clone(), false).await;
        assert_eq!(endpoint.state(), ListenState::Off);

        let endpoint = endpoint.set_enabled(true).await.unwrap();
        assert_eq!(
            endpoint.state(),
            ListenState::PortInUse { port: addr.port() }
        );
        assert!(super::super::load_settings(&dir).await.unwrap().enabled);
        drop(probe);

        let endpoint = endpoint.set_enabled(true).await.unwrap();
        assert_eq!(endpoint.state(), ListenState::Listening { addr });
        assert!(
            round_trip(
                addr,
                "GET /mcp HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .await
            .contains("401")
        );

        // Rebinding a running endpoint must await the old listener's exit.
        let endpoint = endpoint.set_enabled(true).await.unwrap();
        assert_eq!(endpoint.state(), ListenState::Listening { addr });
        endpoint.shutdown().await;
        assert_eq!(endpoint.state(), ListenState::Off);
        assert!(super::super::load_settings(&dir).await.unwrap().enabled);
        let endpoint = McpEndpoint::listen(dir.clone(), false).await;
        assert_eq!(endpoint.state(), ListenState::Listening { addr });

        // A settings write failure must not close the working listener.
        let backup = dir.with_extension("backup");
        tokio::fs::rename(&dir, &backup).await.unwrap();
        tokio::fs::write(&dir, b"not a directory").await.unwrap();
        assert!(endpoint.set_enabled(false).await.is_err());
        assert_eq!(endpoint.state(), ListenState::Listening { addr });
        assert!(TcpStream::connect(addr).await.is_ok());
        tokio::fs::remove_file(&dir).await.unwrap();
        tokio::fs::rename(&backup, &dir).await.unwrap();
        assert!(super::super::load_settings(&dir).await.unwrap().enabled);

        let endpoint = endpoint.set_enabled(false).await.unwrap();
        assert_eq!(endpoint.state(), ListenState::Off);
        let saved = super::super::load_settings(&dir).await.unwrap();
        assert!(!saved.enabled);
        assert_eq!(saved.port, addr.port());
        let probe = TcpListener::bind(addr).await.unwrap();
        let endpoint = McpEndpoint::listen(dir.clone(), false).await;
        assert_eq!(endpoint.state(), ListenState::Off);
        endpoint.shutdown().await;
        drop(probe);
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn demo_does_not_bind() {
        let dir = temp_dir();
        let endpoint = McpEndpoint::listen(dir.clone(), true).await;
        assert!(matches!(endpoint.state(), ListenState::Demo));
        let endpoint = endpoint.set_enabled(true).await.unwrap();
        assert_eq!(endpoint.state(), ListenState::Demo);
        assert!(!dir.exists());
        endpoint.shutdown().await;
    }

    #[tokio::test]
    async fn taken_port_is_reported_without_panicking() {
        let dir = temp_dir();
        let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = probe.local_addr().unwrap().port();
        let settings = McpSettings {
            enabled: true,
            port,
            tokens: Vec::new(),
        };
        save_fixture_settings(&dir, &settings).await;
        let endpoint = McpEndpoint::listen(dir, false).await;
        assert!(
            matches!(
                endpoint.state(),
                ListenState::PortInUse { port: got } if got == port
            ),
            "state: {:?}",
            endpoint.state(),
        );
        drop(probe);
        endpoint.shutdown().await;
    }

    #[tokio::test]
    async fn a_bad_bearer_is_unauthorized() {
        let (endpoint, addr, _) = listening().await;
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer not-a-token\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            port = addr.port()
        );
        let raw = round_trip(addr, &request).await;
        assert_eq!(status_of(&raw), 401);
        assert_eq!(body_of(&raw), UNAUTHORIZED);
        endpoint.shutdown().await;
    }

    #[tokio::test]
    async fn a_token_in_the_query_string_is_forbidden() {
        let (endpoint, addr, wire) = listening().await;
        let request = format!(
            "POST /mcp?token=present HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {wire}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            port = addr.port()
        );
        let raw = round_trip(addr, &request).await;
        assert_eq!(status_of(&raw), 403);
        assert!(body_of(&raw).is_empty());
        endpoint.shutdown().await;
    }

    #[tokio::test]
    async fn a_non_loopback_origin_is_forbidden() {
        let (endpoint, addr, wire) = listening().await;
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://example.com\r\nAuthorization: Bearer {wire}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            port = addr.port()
        );
        let raw = round_trip(addr, &request).await;
        assert_eq!(status_of(&raw), 403);
        assert!(body_of(&raw).is_empty());
        endpoint.shutdown().await;
    }

    #[tokio::test]
    async fn tools_list_for_a_new_token_has_no_run_query() {
        let (endpoint, addr, wire) = listening().await;
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nAuthorization: Bearer {wire}\r\nConnection: close\r\nContent-Length: {len}\r\n\r\n{body}",
            port = addr.port(),
            len = body.len()
        );
        let raw = round_trip(addr, &request).await;
        assert_eq!(status_of(&raw), 200, "{raw}");
        assert!(raw.contains("search_objects"), "{raw}");
        assert!(!raw.contains("run_query"), "{raw}");
        endpoint.shutdown().await;
    }
}
