//! The minimal `jjp/1` protocol (specification, section 9.2): how a program
//! receives its inputs and returns its outputs.
//!
//! One [`JjpHost`] serves the whole application. It listens on
//! `127.0.0.1` at a port the system picks only while a `jjp/1` run is
//! registered: the first [`JjpHost::register`] opens the listener and the
//! last [`JjpSession`] to end closes it, so nothing listens at startup.
//!
//! Each run gets a 256-bit token, which the program reads from the first
//! line of its stdin ([`JjpSession::token_line`]) and presents in `hello`
//! on the single connection the run accepts; `hello` consumes it
//! atomically, so a wrong token, a reused one and a second connection are
//! refused (`-32001`). Messages are NDJSON JSON-RPC 2.0, at most
//! [`MAX_MESSAGE_BYTES`] per line, enforced before the line is buffered. The
//! host always drains the socket, even while the program reads nothing, and
//! [`JjpSession::cancel`] never waits for the program to read.
//!
//! The token, input values and outputs never reach `tracing`, an error or
//! a `Debug` output: [`TokenLine`] and [`NamedValues`] redact themselves.
//! `log` messages and the `fail` message go to the run's `jjp.log` only
//! ([ADR-0037](../../docs/adr/0037-program-logs-on-disk.md)).
mod connection;
mod frame;
mod message;
#[cfg(test)]
mod tests;

use super::definition::OutputSpec;
use super::logs::{LOG_LIMIT, LogSize, LogWriter, Stream};
use jykre_core::CancellationToken;
use jykre_core::automation::RunId;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// The protocol a program names in `hello`.
pub const PROTOCOL: &str = "jjp/1";
/// The longest message, a line without its newline (section 9.2.1).
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// The longest declared output value, in UTF-8 bytes (section 9.3.2).
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
/// The most one step's outputs take together, in UTF-8 bytes.
pub const MAX_OUTPUTS_BYTES: usize = 256 * 1024;
/// The longest `fail` code, of `A-Z a-z 0-9 _ . -`.
pub const MAX_FAIL_CODE: usize = 64;
/// The most of a `fail` message kept in `jjp.log`; the rest is cut.
pub const MAX_FAIL_MESSAGE_BYTES: usize = 4 * 1024;
/// The longest `progress` stage, in characters.
pub const MAX_STAGE_CHARS: usize = 200;
/// How long a program has to say `hello` after it starts (Q-18).
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
/// Traffic from a program beyond which its run fails (Q-28): ten times the
/// log quota.
pub const TRAFFIC_QUOTA: u64 = 10 * LOG_LIMIT;
/// `progress` reaches the interface at most once per interval (10/s).
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// JSON-RPC's error codes and `jjp/1`'s own (section 9.2.2).
pub mod error_code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    /// Invalid or already used token.
    pub const TOKEN: i64 = -32001;
    /// `hello` is required first, or was already sent.
    pub const HELLO: i64 = -32002;
    /// A terminal message was already accepted.
    pub const TERMINAL: i64 = -32003;
    /// Quota exceeded.
    pub const QUOTA: i64 = -32004;
}

/// Room `jjp.log` keeps free for the `fail` line: the message, the code and
/// the line's frame, so a `fail` always reaches the file.
const FAIL_ROOM: u64 = MAX_FAIL_MESSAGE_BYTES as u64 + 128;

/// The host's limits. Production uses [`JjpConfig::default`]; tests
/// shorten the waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JjpConfig {
    /// For `hello`, from registration (a run) or from accepting (a
    /// connection that has not said who it is).
    pub hello_timeout: Duration,
    /// Bytes a run's connection may send in all.
    pub traffic_quota: u64,
    pub progress_interval: Duration,
    /// Bytes of responses a program may leave unread before its run fails
    /// with a quota error; `cancel` is never held back by it.
    pub max_pending_output: usize,
    /// Connections served at once, authenticated or not; more are closed
    /// at once.
    pub max_connections: usize,
    /// How long a closing connection may take to deliver what is queued.
    pub flush: Duration,
}

impl Default for JjpConfig {
    fn default() -> Self {
        Self {
            hello_timeout: HELLO_TIMEOUT,
            traffic_quota: TRAFFIC_QUOTA,
            progress_interval: PROGRESS_INTERVAL,
            max_pending_output: 4 * MAX_MESSAGE_BYTES,
            max_connections: 16,
            flush: Duration::from_secs(1),
        }
    }
}

/// Named values: a run's inputs or a program's outputs, each a text or
/// NULL, in declaration order. `Debug` shows the names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct NamedValues(Vec<(String, Option<String>)>);

impl NamedValues {
    pub fn new(values: Vec<(String, Option<String>)>) -> Self {
        Self(values)
    }

    /// `None` when there is no such name; `Some(None)` when it is NULL.
    pub fn get(&self, name: &str) -> Option<Option<&str>> {
        self.0
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, value)| value.as_deref())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_deref()))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_inner(self) -> Vec<(String, Option<String>)> {
        self.0
    }
}

impl fmt::Debug for NamedValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_set()
            .entries(self.0.iter().map(|(name, _)| name))
            .finish()
    }
}

/// The terminal message the host accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// Every declared output, checked against its kind and the size limits.
    Complete { outputs: NamedValues },
    /// The program's own failure code; its message went to `jjp.log`.
    Fail { code: String },
}

/// What made a `jjp/1` run fail with cause `Protocol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolProblem {
    /// No `hello` within [`HELLO_TIMEOUT`].
    HelloTimeout,
    /// A line longer than [`MAX_MESSAGE_BYTES`].
    LineTooLong,
    InvalidUtf8,
    InvalidJson,
    /// Traffic beyond [`TRAFFIC_QUOTA`], or responses left unread beyond
    /// [`JjpConfig::max_pending_output`] (`-32004`).
    Quota,
    /// The connection ended before a terminal message.
    Disconnected,
    /// The program exited without a terminal message.
    NoTerminal,
}

impl ProtocolProblem {
    /// A stable name, safe for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            ProtocolProblem::HelloTimeout => "hello_timeout",
            ProtocolProblem::LineTooLong => "line_too_long",
            ProtocolProblem::InvalidUtf8 => "invalid_utf8",
            ProtocolProblem::InvalidJson => "invalid_json",
            ProtocolProblem::Quota => "quota",
            ProtocolProblem::Disconnected => "disconnected",
            ProtocolProblem::NoTerminal => "no_terminal",
        }
    }

    /// The host's note in `jjp.log`.
    fn note(self) -> &'static str {
        match self {
            ProtocolProblem::HelloTimeout => "the program did not say hello in time",
            ProtocolProblem::LineTooLong => "a message longer than 1 MiB closed the connection",
            ProtocolProblem::InvalidUtf8 => "a message that is not UTF-8 closed the connection",
            ProtocolProblem::InvalidJson => "a message that is not JSON closed the connection",
            ProtocolProblem::Quota => "the program exceeded its traffic quota",
            ProtocolProblem::Disconnected => "the connection ended before complete or fail",
            ProtocolProblem::NoTerminal => "the program ended before complete or fail",
        }
    }
}

/// The latest `progress` a program reported. The stage is the program's
/// text, so `Debug` shows only its length.
#[derive(Clone, PartialEq)]
pub struct JjpProgress {
    /// 0 to 100.
    pub percent: f64,
    pub stage: Option<String>,
}

impl fmt::Debug for JjpProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JjpProgress")
            .field("percent", &self.percent)
            .field(
                "stage_chars",
                &self.stage.as_ref().map(|s| s.chars().count()),
            )
            .finish()
    }
}

/// What a session reports to the run's owner, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum JjpEvent {
    /// The program presented the run's token.
    Hello,
    /// Coalesced to at most one per [`JjpConfig::progress_interval`].
    Progress(JjpProgress),
    /// A terminal message was accepted; the run still ends with the
    /// program's exit (section 9.2.2).
    Terminal(Terminal),
    /// The run must fail with cause `Protocol`, and its tree be killed.
    Failed(ProtocolProblem),
}

/// What a `jjp/1` run declares to the host.
#[derive(Debug)]
pub struct JjpRun {
    pub run: RunId,
    /// Sent in `hello`; never logged.
    pub inputs: NamedValues,
    /// What `complete` must carry.
    pub outputs: Vec<OutputSpec>,
    /// The run's deadline, for `hello`'s `deadline_ms`.
    pub deadline: Instant,
    /// The data folder; `jjp.log` goes in the run's log folder under it.
    pub data_dir: PathBuf,
}

/// The run's token line for the program's stdin,
/// `{"jjp":"1","token":"<64 hex digits>"}` and a newline. `Debug` never
/// shows it.
pub struct TokenLine(String);

impl TokenLine {
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for TokenLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TokenLine(<redacted>)")
    }
}

/// 256 random bits as 64 lowercase hex digits.
#[derive(Clone)]
struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// From the operating system's secure random source, through the
    /// `ring` provider rustls already uses.
    fn generate() -> io::Result<Self> {
        let mut bytes = [0_u8; 32];
        rustls::crypto::ring::default_provider()
            .secure_random
            .fill(&mut bytes)
            .map_err(|_| io::Error::other("no secure random source"))?;
        let mut text = String::with_capacity(64);
        for byte in bytes {
            text.push_str(&format!("{byte:02x}"));
        }
        Ok(Self(text))
    }

    /// Compares without stopping at the first difference.
    fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let presented = presented.as_bytes();
        expected.len() == presented.len()
            && expected
                .iter()
                .zip(presented)
                .fold(0_u8, |difference, (left, right)| {
                    difference | (left ^ right)
                })
                == 0
    }
}

/// The `jjp/1` host of the application. Cloning shares it.
#[derive(Clone)]
pub struct JjpHost {
    shared: Arc<HostShared>,
}

impl fmt::Debug for JjpHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JjpHost")
            .field("listening", &self.address())
            .finish_non_exhaustive()
    }
}

impl Default for JjpHost {
    fn default() -> Self {
        Self::new()
    }
}

impl JjpHost {
    /// A host with the specification's limits. Nothing listens yet.
    pub fn new() -> Self {
        Self::with_config(JjpConfig::default())
    }

    pub fn with_config(config: JjpConfig) -> Self {
        Self {
            shared: Arc::new(HostShared {
                config,
                state: Mutex::new(HostState::default()),
            }),
        }
    }

    /// Where the host listens, while a run is registered.
    pub fn address(&self) -> Option<SocketAddr> {
        self.shared
            .state()
            .listener
            .as_ref()
            .map(|listening| listening.address)
    }

    /// Registers a run before its program starts: creates its `jjp.log`,
    /// generates its token and, for the first run, opens the listener. Must
    /// be called inside the Tokio runtime. Fails when the log cannot be
    /// created, no secure random source answers, or the port cannot be
    /// opened; nothing was started then.
    pub async fn register(&self, run: JjpRun) -> io::Result<JjpSession> {
        let token = Token::generate()?;
        let log = LogWriter::create(&run.data_dir, run.run, Stream::Jjp).await?;
        let (events_sender, events) = mpsc::channel(EVENTS);
        let (progress_sender, progress) = watch::channel(None);
        let (cancel, _) = watch::channel(false);
        let shared = Arc::new(RunShared {
            run: run.run,
            inputs: run.inputs,
            outputs: run.outputs,
            deadline: run.deadline,
            log: tokio::sync::Mutex::new(Some(log)),
            progress: progress_sender,
            cancel,
            stop: CancellationToken::new(),
            hello: AtomicBool::new(false),
            ended: Mutex::new(Ended::default()),
        });
        let (key, address) = {
            let mut state = self.shared.state();
            let address = match &state.listener {
                Some(listening) => listening.address,
                None => {
                    // A loopback bind never waits, like opening a lock file.
                    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
                    listener.set_nonblocking(true)?;
                    let address = listener.local_addr()?;
                    let listener = tokio::net::TcpListener::from_std(listener)?;
                    let stop = CancellationToken::new();
                    // Tied to `stop`, which the last session cancels.
                    tokio::spawn(connection::accept(
                        listener,
                        self.shared.clone(),
                        stop.clone(),
                    ));
                    state.listener = Some(Listening { address, stop });
                    tracing::debug!(port = address.port(), "jjp/1 listener opened");
                    address
                }
            };
            let key = state.next_key;
            state.next_key += 1;
            state.registrations.push(Registration {
                key,
                token: token.clone(),
                link: Some(Link {
                    run: shared.clone(),
                    events: events_sender,
                }),
            });
            (key, address)
        };
        tracing::debug!(run = %shared.run, "jjp/1 run registered");
        Ok(JjpSession {
            host: self.shared.clone(),
            key,
            address,
            token,
            hello_deadline: Instant::now() + self.shared.config.hello_timeout,
            run: shared,
            events,
            progress,
            hello: false,
            timed_out: false,
            closed: false,
            decided: None,
        })
    }
}

/// Control events per run: `hello`, a terminal and a failure, at most once
/// each, so the channel never fills and the connection never waits on it.
const EVENTS: usize = 8;

pub(super) struct HostShared {
    config: JjpConfig,
    state: Mutex<HostState>,
}

#[derive(Default)]
struct HostState {
    listener: Option<Listening>,
    registrations: Vec<Registration>,
    next_key: u64,
}

struct Listening {
    address: SocketAddr,
    stop: CancellationToken,
}

struct Registration {
    key: u64,
    token: Token,
    /// Taken by the connection whose `hello` presents the token.
    link: Option<Link>,
}

/// What a connection gets when its `hello` is accepted.
pub(super) struct Link {
    run: Arc<RunShared>,
    events: mpsc::Sender<JjpEvent>,
}

/// What `hello` found for a token.
enum Claim {
    Bound(Link),
    Refused,
}

impl HostShared {
    /// The state, even after a panic elsewhere left the lock poisoned:
    /// every update leaves it consistent.
    fn state(&self) -> MutexGuard<'_, HostState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Consumes the run's token: the first `hello` that presents it gets the
    /// run, every later one is refused, as is an unknown token. Every
    /// registered token is compared, so the time taken says nothing about
    /// which matched.
    fn claim(&self, presented: &str) -> Claim {
        let mut state = self.state();
        let mut found = None;
        for (index, registration) in state.registrations.iter().enumerate() {
            if registration.token.matches(presented) {
                found = Some(index);
            }
        }
        match found
            .and_then(|index| state.registrations.get_mut(index))
            .and_then(|registration| registration.link.take())
        {
            Some(link) => Claim::Bound(link),
            None => Claim::Refused,
        }
    }

    /// Makes the run's token unusable, returning its link if no connection
    /// took it (for the caller to drop after the lock is released).
    fn revoke(&self, key: u64) -> Option<Link> {
        let mut state = self.state();
        state
            .registrations
            .iter_mut()
            .find(|registration| registration.key == key)
            .and_then(|registration| registration.link.take())
    }

    /// Forgets the run; the last one closes the listener.
    fn unregister(&self, key: u64) -> Option<Link> {
        let mut state = self.state();
        let index = state
            .registrations
            .iter()
            .position(|registration| registration.key == key)?;
        let registration = state.registrations.remove(index);
        if state.registrations.is_empty()
            && let Some(listening) = state.listener.take()
        {
            listening.stop.cancel();
            tracing::debug!("jjp/1 listener closed");
        }
        registration.link
    }
}

/// How a run's connection ended, recorded before the session hears of it,
/// so [`JjpSession::finish`] never misses it.
#[derive(Default)]
struct Ended {
    terminal: Option<Terminal>,
    problem: Option<ProtocolProblem>,
}

/// One run, as its session and its connection share it.
pub(super) struct RunShared {
    run: RunId,
    inputs: NamedValues,
    outputs: Vec<OutputSpec>,
    deadline: Instant,
    /// `None` once the session finished the file.
    log: tokio::sync::Mutex<Option<LogWriter>>,
    progress: watch::Sender<Option<JjpProgress>>,
    /// `true` once the run's owner asked the program to end.
    cancel: watch::Sender<bool>,
    /// Cancelled when the session ends: the connection closes.
    stop: CancellationToken,
    hello: AtomicBool,
    ended: Mutex<Ended>,
}

impl RunShared {
    fn ended(&self) -> MutexGuard<'_, Ended> {
        self.ended
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn terminal_accepted(&self) -> bool {
        self.ended().terminal.is_some()
    }

    /// Records the terminal message if none was accepted yet.
    fn accept_terminal(&self, terminal: Terminal) -> bool {
        let mut ended = self.ended();
        if ended.terminal.is_some() {
            return false;
        }
        ended.terminal = Some(terminal);
        true
    }

    /// Records the first problem before a terminal; `false` when it changes
    /// nothing (a terminal was accepted, or a problem already decided).
    fn record_problem(&self, problem: ProtocolProblem) -> bool {
        let mut ended = self.ended();
        if ended.terminal.is_some() || ended.problem.is_some() {
            return false;
        }
        ended.problem = Some(problem);
        true
    }

    /// Writes a line to `jjp.log`, unless the file is full or finished.
    async fn write_log(&self, line: &[u8], keep_free: u64) {
        if let Some(writer) = self.log.lock().await.as_mut() {
            writer.write_message(line, keep_free).await;
        }
    }

    /// The host's own note in `jjp.log`.
    async fn note(&self, text: &str) {
        self.write_log(format!("[jykre] {text}\n").as_bytes(), FAIL_ROOM)
            .await;
    }
}

/// One registered `jjp/1` run, held by the run's owner. Dropping it closes
/// the run's connection and, for the last run, the listener; prefer
/// [`JjpSession::finish`], which also completes `jjp.log`.
pub struct JjpSession {
    host: Arc<HostShared>,
    key: u64,
    address: SocketAddr,
    token: Token,
    run: Arc<RunShared>,
    events: mpsc::Receiver<JjpEvent>,
    progress: watch::Receiver<Option<JjpProgress>>,
    hello_deadline: Instant,
    hello: bool,
    timed_out: bool,
    /// The run's connection ended (or can no longer come).
    closed: bool,
    /// A failure decided by the session itself, kept until it is returned:
    /// its note in `jjp.log` is written first, and a caller that drops
    /// [`JjpSession::next`] meanwhile still gets it on the next call.
    decided: Option<JjpEvent>,
}

impl fmt::Debug for JjpSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JjpSession")
            .field("run", &self.run.run)
            .field("address", &self.address)
            .field("hello", &self.connected())
            .finish_non_exhaustive()
    }
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JjpSummary {
    pub hello: bool,
    pub terminal: Option<Terminal>,
    /// The problem that decided, or was found, before a terminal.
    pub problem: Option<ProtocolProblem>,
    pub log: LogSize,
}

impl JjpSession {
    /// The listener, for [`super::process::JJP_ADDRESS_VARIABLE`].
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The line to write to the program's stdin before closing it.
    pub fn token_line(&self) -> TokenLine {
        TokenLine(format!(
            "{{\"jjp\":\"1\",\"token\":\"{}\"}}\n",
            self.token.0
        ))
    }

    /// Whether the program said `hello`.
    pub fn connected(&self) -> bool {
        self.hello || self.run.hello.load(Ordering::Acquire)
    }

    /// Asks the program to end with the `cancel` notification: now if it is
    /// connected, right after its `hello` otherwise. Returns at once; the
    /// notification is queued and never waits for the program to read.
    pub fn cancel(&self) {
        self.run.cancel.send_replace(true);
    }

    /// The next event, or `None` once the connection ended and nothing is
    /// left to report. Cancel-safe. Fails the run with
    /// [`ProtocolProblem::HelloTimeout`] when no `hello` came in time, and
    /// with [`ProtocolProblem::Disconnected`] when the connection ended
    /// before a terminal message.
    pub async fn next(&mut self) -> Option<JjpEvent> {
        loop {
            if let Some(event) = self.decided.take() {
                return Some(event);
            }
            if self.closed {
                if self.progress.has_changed().unwrap_or(false)
                    && let Some(progress) = self.progress.borrow_and_update().clone()
                {
                    return Some(JjpEvent::Progress(progress));
                }
                return None;
            }
            let waiting_for_hello = !self.hello && !self.timed_out;
            let problem = tokio::select! {
                biased;
                event = self.events.recv() => match event {
                    Some(event) => {
                        if matches!(event, JjpEvent::Hello) {
                            self.hello = true;
                        }
                        return Some(event);
                    }
                    None => {
                        self.closed = true;
                        if !(self.connected() && self.run.record_problem(ProtocolProblem::Disconnected)) {
                            continue;
                        }
                        ProtocolProblem::Disconnected
                    }
                },
                changed = self.progress.changed() => {
                    if changed.is_ok()
                        && let Some(progress) = self.progress.borrow_and_update().clone()
                    {
                        return Some(JjpEvent::Progress(progress));
                    }
                    continue;
                }
                () = tokio::time::sleep_until(self.hello_deadline.into()), if waiting_for_hello => {
                    self.timed_out = true;
                    // A `hello` accepted at this very moment wins.
                    if self.run.hello.load(Ordering::Acquire) {
                        continue;
                    }
                    // Revoking the token makes a late `hello` fail; dropping
                    // the link ends `events`, so the session closes.
                    drop(self.host.revoke(self.key));
                    if !self.run.record_problem(ProtocolProblem::HelloTimeout) {
                        continue;
                    }
                    ProtocolProblem::HelloTimeout
                }
            };
            self.decided = Some(JjpEvent::Failed(problem));
            self.run.note(problem.note()).await;
        }
    }

    /// Ends the session: waits up to `wait` for the connection to end on
    /// its own (after the program exited, so a terminal message it sent
    /// just before is not lost), then closes it, completes `jjp.log` and
    /// unregisters the run, closing the listener after the last one.
    pub async fn finish(mut self, wait: Duration) -> JjpSummary {
        if !wait.is_zero() && self.connected() && !self.closed {
            // Bounded; past it the connection is closed below.
            let _ =
                tokio::time::timeout(wait, async { while self.next().await.is_some() {} }).await;
        }
        drop(self.host.unregister(self.key));
        self.run.stop.cancel();
        let flush = self.host.config.flush;
        // The connection drops its link when it stops; bounded in case it is
        // still delivering a response.
        let _ = tokio::time::timeout(flush, async { while self.events.recv().await.is_some() {} })
            .await;
        let writer = self.run.log.lock().await.take();
        let log = match writer {
            Some(writer) => writer.finish().await,
            None => LogSize::default(),
        };
        let (terminal, problem) = {
            let ended = self.run.ended();
            (ended.terminal.clone(), ended.problem)
        };
        JjpSummary {
            hello: self.connected(),
            terminal,
            problem,
            log,
        }
    }
}

impl Drop for JjpSession {
    fn drop(&mut self) {
        self.run.stop.cancel();
        drop(self.host.unregister(self.key));
    }
}
