//! The shared run of one approved statement for the assistant and the MCP
//! server (ADR-0042).
//!
//! [`run_once`] is the runtime half of the assistant's query. It calls
//! `query_in_context` and returns a [`QueryPreview`] plus the session to
//! keep. It does not turn that preview into text for a model;
//! [`crate::assistant::released::release`] stays the only formatter.
//! Statement text is not logged here.
//!
//! [`McpQueryHost`] keeps one database session per MCP token, so `src/mcp`
//! does not name a resolved connection, a credential or `query_in_context`.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use jykre_secrets::{SecretStore, SystemSecretStore};
use jykre_storage::ConfigRepository;

use crate::assistant::released::{self, QueryPreview};
use crate::backend::{self, SessionRun};
use crate::connection_store::{self, VAULT_TIMEOUT, VaultCall, vault_within};
use crate::drivers::{
    CancellationToken, ConnectionId, Credentials, DriverError, ErrorCode, ExecutionOptions,
    QueryRequest, ResolvedConnection, Session, SessionContext, builtin_registry,
};
use crate::mcp::McpTokenId;
use crate::tr;

/// The memory one assistant or MCP query may read: far more than the
/// [`released::MAX_ROWS`] rows it may send, far less than a tab's result.
const ASSISTANT_MEMORY_CEILING: usize = 4 << 20;

/// A token's session closes after this long without a finished query.
/// Checked on the next [`McpQueryHost::run`] or [`McpQueryHost::close_token`];
/// nothing waits in the background.
const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);

/// One approved statement, run once.
pub struct RunOnce {
    /// The rows and the error, cut to what may be sent. Not text for a model.
    pub preview: QueryPreview,
    /// The session to keep for the next statement, when it is still usable.
    pub session: Option<Box<dyn Session>>,
    /// The session was left inside a transaction and was closed, which rolls
    /// it back. When true, [`Self::session`] is `None`. The preview stays the
    /// statement's own result. Callers that show or send the result append
    /// [`transaction_closed_note`].
    pub transaction_closed: bool,
}

/// Why an MCP query did not start. A statement that started and then failed
/// is a [`RunOnce`] whose preview carries the error.
#[derive(Debug)]
pub enum QueryStart {
    /// The profile needs a password and none is available. The caller asks;
    /// this does not open a dialog.
    NeedsPassword,
    /// The saved profile or the vault could not be used.
    Unavailable(DriverError),
}

/// Runs one statement the way the assistant does, and the way a report panel
/// closes a session left in a transaction.
///
/// The request reads at most [`released::READ_ROWS`] rows (51), stops at 4
/// MiB and uses the default statement timeout (300 seconds). Nothing is
/// retried. A cancel or a failure to start becomes
/// [`QueryPreview::from_error`]. The preview is always returned; a caller
/// that treats a stop as "not run" decides that itself.
pub async fn run_once(
    connection: Option<ConnectionId>,
    input: ResolvedConnection,
    context: SessionContext,
    session: Option<Box<dyn Session>>,
    sql: String,
    cancel: CancellationToken,
) -> RunOnce {
    let request = read_request(&sql);
    match backend::query_in_context(
        connection,
        input,
        context,
        session,
        vec![request],
        ASSISTANT_MEMORY_CEILING,
        cancel,
    )
    .await
    {
        Ok(run) => finish(run, &sql).await,
        Err(error) => RunOnce {
            preview: QueryPreview::from_error(&error, &sql),
            session: None,
            transaction_closed: false,
        },
    }
}

/// The request [`run_once`] sends: 51 rows and the default statement timeout,
/// which is 300 seconds. The memory ceiling is [`ASSISTANT_MEMORY_CEILING`],
/// passed beside the request.
fn read_request(sql: &str) -> QueryRequest {
    let defaults = ExecutionOptions::default();
    QueryRequest::new(
        sql.to_owned(),
        ExecutionOptions {
            max_rows: released::READ_ROWS,
            timeout: defaults.timeout,
            context: SessionContext::default(),
        },
    )
}

/// `query_in_context` already closes a session it left in a transaction and
/// sets [`SessionRun::transaction_closed`]. Closing `None` does nothing; a
/// session still present would still be inside that transaction, so it is
/// closed once here.
async fn finish(mut run: SessionRun, sql: &str) -> RunOnce {
    let preview = match &run.batch {
        Ok(batch) => QueryPreview::from_batch(batch, sql),
        Err(error) => QueryPreview::from_error(error, sql),
    };
    let transaction_closed = run.transaction_closed;
    let session = if transaction_closed {
        backend::close_session(run.session.take()).await;
        None
    } else {
        run.session.take()
    };
    RunOnce {
        preview,
        session,
        transaction_closed,
    }
}

/// What a result card and a sent answer add when [`RunOnce::transaction_closed`]
/// is set. The same sentence automation uses for a statement that left a
/// transaction open. It is not written into [`QueryPreview::error`]: the
/// statement itself may have succeeded.
pub fn transaction_closed_note() -> String {
    tr!("The statement left a transaction open; closing the run's session rolled it back.")
        .to_owned()
}

/// One database session per MCP token.
///
/// A session unused for 10 minutes is closed on the next [`McpQueryHost::run`]
/// or [`McpQueryHost::close_token`]. Profile revisions are the registry's
/// job; [`McpQueryHost::close_token`] is what the server calls when a token
/// is revoked, rotated or suspended.
pub struct McpQueryHost {
    state: tokio::sync::Mutex<HostState>,
}

struct HostState {
    sessions: HashMap<McpTokenId, HeldSession>,
    next_checkout: u64,
}

struct HeldSession {
    session: Option<Box<dyn Session>>,
    /// Checked out by a run. An idle sweep does not take it.
    in_use: bool,
    last_used: Instant,
    connection: ConnectionId,
    /// Bumped on every checkout, so a run that lost the race does not put
    /// its session back over a newer one.
    checkout: u64,
}

impl McpQueryHost {
    pub fn new() -> Self {
        Self {
            state: tokio::sync::Mutex::new(HostState {
                sessions: HashMap::new(),
                next_checkout: 0,
            }),
        }
    }

    /// The profile needs a password and none is available. Does not open a
    /// dialog and does not run a statement.
    pub async fn password_required(data_dir: &Path, connection: ConnectionId) -> bool {
        let secrets = SystemSecretStore;
        matches!(
            load_input(data_dir, connection, &secrets).await,
            Err(QueryStart::NeedsPassword)
        )
    }

    /// Opens or reuses `token`'s session in the connection's default context
    /// (an empty [`SessionContext`], the same as a new session).
    ///
    /// Loads the saved profile from `data_dir` and, when the profile remembers
    /// one, the password from the vault within [`VAULT_TIMEOUT`] (10 seconds).
    /// A profile that needs a password and does not have one returns
    /// [`QueryStart::NeedsPassword`] and does not run.
    ///
    /// The host keeps the session for the token. The returned
    /// [`RunOnce::session`] is always `None`; [`RunOnce::transaction_closed`]
    /// is set when that session was closed because a transaction was open.
    pub async fn run(
        &self,
        data_dir: &Path,
        token: McpTokenId,
        connection: ConnectionId,
        sql: String,
        cancel: CancellationToken,
    ) -> Result<RunOnce, QueryStart> {
        self.close_idle().await;
        let secrets = SystemSecretStore;
        let input = load_input(data_dir, connection, &secrets).await?;
        let (session, checkout, discard) = {
            let mut state = self.state.lock().await;
            checkout_session(&mut state, token, connection)
        };
        close_each(discard).await;
        let mut once = run_once(
            Some(connection),
            input,
            SessionContext::default(),
            session,
            sql,
            cancel,
        )
        .await;
        let returned = once.session.take();
        let stray = {
            let mut state = self.state.lock().await;
            match state.sessions.get_mut(&token) {
                Some(held) if held.checkout == checkout => {
                    held.in_use = false;
                    held.last_used = Instant::now();
                    held.session = returned;
                    None
                }
                _ => returned,
            }
        };
        backend::close_session(stray).await;
        Ok(once)
    }

    /// Closes `token`'s session, and any other session that has been idle for
    /// 10 minutes.
    pub async fn close_token(&self, token: McpTokenId) {
        self.close_idle().await;
        let session = {
            let mut state = self.state.lock().await;
            state.sessions.remove(&token).and_then(|held| held.session)
        };
        backend::close_session(session).await;
    }

    /// Closes every token's session.
    pub async fn close_all(&self) {
        let sessions: Vec<Box<dyn Session>> = {
            let mut state = self.state.lock().await;
            state
                .sessions
                .drain()
                .filter_map(|(_, held)| held.session)
                .collect()
        };
        close_each(sessions).await;
    }

    async fn close_idle(&self) {
        let idle = {
            let mut state = self.state.lock().await;
            take_idle(&mut state, Instant::now())
        };
        close_each(idle).await;
    }
}

fn checkout_session(
    state: &mut HostState,
    token: McpTokenId,
    connection: ConnectionId,
) -> (Option<Box<dyn Session>>, u64, Vec<Box<dyn Session>>) {
    let checkout = state.next_checkout;
    state.next_checkout = state.next_checkout.wrapping_add(1);
    let mut discard = Vec::new();
    if let Some(held) = state.sessions.get_mut(&token) {
        let session = if held.connection == connection {
            held.session.take()
        } else {
            discard.extend(held.session.take());
            None
        };
        held.connection = connection;
        held.in_use = true;
        held.checkout = checkout;
        held.last_used = Instant::now();
        return (session, checkout, discard);
    }
    state.sessions.insert(
        token,
        HeldSession {
            session: None,
            in_use: true,
            last_used: Instant::now(),
            connection,
            checkout,
        },
    );
    (None, checkout, discard)
}

/// Sessions that have been unused for [`IDLE_AFTER`] and are not checked out.
fn take_idle(state: &mut HostState, now: Instant) -> Vec<Box<dyn Session>> {
    let idle: Vec<McpTokenId> = state
        .sessions
        .iter()
        .filter(|(_, held)| !held.in_use && idle_for(held.last_used, now))
        .map(|(id, _)| *id)
        .collect();
    let mut sessions = Vec::new();
    for id in idle {
        if let Some(held) = state.sessions.remove(&id)
            && let Some(session) = held.session
        {
            sessions.push(session);
        }
    }
    sessions
}

fn idle_for(last_used: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_used) >= IDLE_AFTER
}

async fn close_each(sessions: Vec<Box<dyn Session>>) {
    for session in sessions {
        backend::close_session(Some(session)).await;
    }
}

/// The saved profile and its vault password. A driver that does not sign in
/// with a password is not read from the vault.
async fn load_input(
    data_dir: &Path,
    connection: ConnectionId,
    secrets: &dyn SecretStore,
) -> Result<ResolvedConnection, QueryStart> {
    let repository = ConfigRepository::open(data_dir)
        .await
        .map_err(QueryStart::Unavailable)?;
    let listed = repository.list_each().await;
    repository.close().await;
    let profile = listed
        .map_err(QueryStart::Unavailable)?
        .into_iter()
        .find_map(|item| match item {
            Ok(profile) if profile.id == connection => Some(profile),
            _ => None,
        })
        .ok_or_else(|| {
            QueryStart::Unavailable(DriverError::new(
                ErrorCode::NotFound,
                tr!("The connection is not available."),
            ))
        })?;
    builtin_registry()
        .and_then(|registry| registry.get(profile.config.driver))
        .and_then(|driver| driver.validate_connection(&profile.config))
        .map_err(QueryStart::Unavailable)?;
    let credentials = if profile.remember_password
        && connection_store::signs_in_with_password(profile.config.driver)
    {
        vault_within(
            VaultCall::new("connection_password", "get"),
            VAULT_TIMEOUT,
            secrets.get(profile.id),
        )
        .await
        .map_err(QueryStart::Unavailable)?
    } else {
        Credentials::default()
    };
    let input = ResolvedConnection {
        config: profile.config,
        credentials,
    };
    if connection_store::needs_password(&input) {
        return Err(QueryStart::NeedsPassword);
    }
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::{
        ASSISTANT_MEMORY_CEILING, HeldSession, HostState, IDLE_AFTER, McpQueryHost, QueryStart,
        idle_for, load_input, read_request, run_once, take_idle, transaction_closed_note,
    };
    use crate::backend;
    use crate::drivers::{
        CancellationToken, ConnectionConfig, ConnectionId, ConnectionProfile, Credentials,
        DriverError, DriverKind, DriverResult, ErrorCode, ExecutionOptions, SecretString,
        SessionContext, async_trait,
    };
    use crate::mcp::McpTokenId;
    use jykre_secrets::SecretStore;
    use jykre_storage::ConfigRepository;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("jykre-assistant-query-{}", ConnectionId::new()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sqlite_file(dir: &Path) -> PathBuf {
        let path = dir.join("db.sqlite");
        std::fs::write(&path, b"").unwrap();
        path
    }

    fn sqlite_input(path: &Path) -> crate::drivers::ResolvedConnection {
        crate::drivers::ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Sqlite,
                options: json!({ "path": path.to_string_lossy() }),
            },
            credentials: Credentials::default(),
        }
    }

    fn postgres_profile(remember_password: bool) -> ConnectionProfile {
        ConnectionProfile {
            id: ConnectionId::new(),
            name: "Synthetic fixture".into(),
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: json!({
                    "host": "localhost",
                    "user": "fixture",
                    "database": "fixture",
                    "sslMode": "disable"
                }),
            },
            remember_password,
            revision: 0,
        }
    }

    async fn save_profile(dir: &Path, profile: &ConnectionProfile) {
        let repository = ConfigRepository::open(dir).await.unwrap();
        repository.save(profile).await.unwrap();
        repository.close().await;
    }

    fn unavailable(result: Result<crate::drivers::ResolvedConnection, QueryStart>) -> ErrorCode {
        match result {
            Err(QueryStart::Unavailable(error)) => error.code,
            _ => ErrorCode::Internal,
        }
    }

    fn first_cell(preview: &crate::assistant::released::QueryPreview) -> Option<&str> {
        preview
            .rows
            .first()
            .and_then(|row| row.first())
            .and_then(|cell| cell.as_deref())
    }

    /// A vault that records whether it was asked. Synthetic passwords only.
    struct MemoryVault {
        password: Mutex<Option<SecretString>>,
        fail: Option<ErrorCode>,
        reads: Mutex<u32>,
    }

    #[async_trait]
    impl SecretStore for MemoryVault {
        async fn get(&self, _id: ConnectionId) -> DriverResult<Credentials> {
            *self.reads.lock().unwrap() += 1;
            if let Some(code) = self.fail {
                return Err(DriverError::new(code, "fixture vault refused"));
            }
            Ok(Credentials {
                password: self.password.lock().unwrap().clone(),
            })
        }

        async fn set(&self, _id: ConnectionId, _password: SecretString) -> DriverResult<()> {
            Ok(())
        }

        async fn delete(&self, _id: ConnectionId) -> DriverResult<()> {
            Ok(())
        }
    }

    impl MemoryVault {
        fn empty() -> Self {
            Self {
                password: Mutex::new(None),
                fail: None,
                reads: Mutex::new(0),
            }
        }

        fn reads(&self) -> u32 {
            *self.reads.lock().unwrap()
        }
    }

    #[test]
    fn a_read_uses_51_rows_4_mib_and_the_default_statement_timeout() {
        let request = read_request("SELECT 1");
        assert_eq!(request.options.max_rows, 51);
        assert_eq!(
            request.options.max_rows,
            crate::assistant::released::READ_ROWS
        );
        assert_eq!(request.options.timeout, Some(Duration::from_secs(300)));
        assert_eq!(request.options.timeout, ExecutionOptions::default().timeout);
        assert!(request.parameters.is_empty());
        assert_eq!(ASSISTANT_MEMORY_CEILING, 4 << 20);
    }

    #[test]
    fn ten_minutes_idle_is_the_deadline_and_a_checkout_is_kept() {
        let start = Instant::now();
        assert_eq!(IDLE_AFTER, Duration::from_secs(10 * 60));
        let short = IDLE_AFTER - Duration::from_secs(1);
        assert!(!idle_for(start, start + short));
        assert!(idle_for(start, start + IDLE_AFTER));
        let token = McpTokenId::parse("aaaaaaaaaa").unwrap();
        let mut state = HostState {
            sessions: std::collections::HashMap::new(),
            next_checkout: 0,
        };
        let last_used = Instant::now();
        state.sessions.insert(
            token,
            HeldSession {
                session: None,
                in_use: true,
                last_used,
                connection: ConnectionId::new(),
                checkout: 0,
            },
        );
        let closed = take_idle(&mut state, last_used + Duration::from_secs(3_600));
        assert!(closed.is_empty());
        assert!(state.sessions.contains_key(&token));
        state.sessions.get_mut(&token).unwrap().in_use = false;
        let closed = take_idle(&mut state, last_used + short);
        assert!(closed.is_empty() && state.sessions.contains_key(&token));
        let closed = take_idle(&mut state, last_used + IDLE_AFTER);
        assert!(closed.is_empty());
        assert!(!state.sessions.contains_key(&token));
    }

    #[tokio::test]
    async fn run_once_returns_a_preview_of_select_1_and_no_transaction() {
        let scratch = Scratch::new();
        let path = sqlite_file(scratch.path());
        let run = run_once(
            None,
            sqlite_input(&path),
            SessionContext::default(),
            None,
            "SELECT 1".to_owned(),
            CancellationToken::new(),
        )
        .await;
        assert!(!run.transaction_closed);
        assert!(run.session.is_some());
        assert!(run.preview.error.is_none());
        assert_eq!(first_cell(&run.preview), Some("1"));
        backend::close_session(run.session).await;
    }

    #[tokio::test]
    async fn a_cancelled_run_still_returns_the_preview() {
        let scratch = Scratch::new();
        let path = sqlite_file(scratch.path());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let run = run_once(
            None,
            sqlite_input(&path),
            SessionContext::default(),
            None,
            "SELECT 1".to_owned(),
            cancel,
        )
        .await;
        assert!(run.preview.error.is_some());
        assert!(!run.transaction_closed);
        assert!(run.session.is_none());
    }

    #[tokio::test]
    async fn a_begin_closes_the_session_and_the_preview_says_so() {
        let scratch = Scratch::new();
        let path = sqlite_file(scratch.path());
        let run = run_once(
            None,
            sqlite_input(&path),
            SessionContext::default(),
            None,
            "BEGIN".to_owned(),
            CancellationToken::new(),
        )
        .await;
        assert!(run.transaction_closed);
        assert!(run.session.is_none());
        assert!(run.preview.error.is_none());
        assert_eq!(
            transaction_closed_note(),
            "The statement left a transaction open; closing the run's session rolled it back."
        );
        backend::close_session(run.session).await;
    }

    #[tokio::test]
    async fn a_missing_profile_and_a_missing_password_do_not_run() {
        let scratch = Scratch::new();
        let vault = MemoryVault::empty();
        let missing = load_input(scratch.path(), ConnectionId::new(), &vault).await;
        assert_eq!(unavailable(missing), ErrorCode::NotFound);
        assert_eq!(vault.reads(), 0);

        let profile = postgres_profile(false);
        save_profile(scratch.path(), &profile).await;
        let needs = load_input(scratch.path(), profile.id, &vault).await;
        assert!(matches!(needs, Err(QueryStart::NeedsPassword)));
        assert_eq!(vault.reads(), 0, "no password was remembered");

        let mut remembered = postgres_profile(true);
        remembered.name = "Remembers a password".into();
        save_profile(scratch.path(), &remembered).await;
        let empty = load_input(scratch.path(), remembered.id, &vault).await;
        assert!(matches!(empty, Err(QueryStart::NeedsPassword)));
        assert_eq!(vault.reads(), 1);

        let down = MemoryVault {
            password: Mutex::new(None),
            fail: Some(ErrorCode::Timeout),
            reads: Mutex::new(0),
        };
        let refused = load_input(scratch.path(), remembered.id, &down).await;
        assert_eq!(unavailable(refused), ErrorCode::Timeout);

        let held = MemoryVault {
            password: Mutex::new(Some(SecretString::from("synthetic-fixture"))),
            fail: None,
            reads: Mutex::new(0),
        };
        let ready = load_input(scratch.path(), remembered.id, &held)
            .await
            .expect("a remembered password");
        assert!(ready.credentials.password.is_some());
    }

    #[tokio::test]
    async fn an_invalid_profile_does_not_read_the_vault() {
        let scratch = Scratch::new();
        let mut profile = postgres_profile(true);
        profile.config.options = json!({});
        save_profile(scratch.path(), &profile).await;
        let vault = MemoryVault {
            password: Mutex::new(None),
            fail: Some(ErrorCode::Internal),
            reads: Mutex::new(0),
        };
        let loaded = load_input(scratch.path(), profile.id, &vault).await;
        assert_eq!(unavailable(loaded), ErrorCode::Validation);
        assert_eq!(vault.reads(), 0);
    }

    #[tokio::test]
    async fn a_token_keeps_its_session_until_it_is_closed() {
        let scratch = Scratch::new();
        let path = sqlite_file(scratch.path());
        let mut profile = postgres_profile(false);
        profile.name = "Synthetic sqlite".into();
        profile.config = ConnectionConfig {
            driver: DriverKind::Sqlite,
            options: json!({ "path": path.to_string_lossy() }),
        };
        save_profile(scratch.path(), &profile).await;
        let host = McpQueryHost::new();
        let token = McpTokenId::parse("bbbbbbbbbb").unwrap();
        let run = |sql: &str| {
            let host = &host;
            let dir = scratch.path().to_owned();
            let id = profile.id;
            let sql = sql.to_owned();
            async move {
                host.run(&dir, token, id, sql, CancellationToken::new())
                    .await
                    .expect("the query starts")
            }
        };
        let created = run("CREATE TEMP TABLE kept (n INTEGER)").await;
        assert!(created.preview.error.is_none());
        assert!(created.session.is_none(), "the host keeps the session");
        assert!(!created.transaction_closed);
        let inserted = run("INSERT INTO kept VALUES (1)").await;
        assert!(inserted.preview.error.is_none());
        let read = run("SELECT n FROM kept").await;
        assert_eq!(first_cell(&read.preview), Some("1"));
        host.close_token(token).await;
        let after = run("SELECT n FROM kept").await;
        assert!(after.preview.error.is_some());
        host.close_all().await;
    }
}
