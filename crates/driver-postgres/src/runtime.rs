use crate::PostgresConfig;
use futures_util::TryStreamExt;
use jykre_core::*;
use secrecy::ExposeSecret;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgSslMode};
use sqlx::{
    Column, Connection, Executor, Row as SqlRow, SqlSafeStr, Statement as _, TypeInfo, ValueRef,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{collections::HashSet, future::Future, time::Duration};
use tokio::sync::{mpsc, oneshot};

/// Classifies an SQLx error. A database error keeps the server's own message,
/// detail and hint, its SQLSTATE and the position it reported; errors from
/// the connection keep the operating system's reason. Anything else (such as
/// configuration, which may hold the connection URL) gets a fixed message.
pub(crate) fn map_error(error: sqlx::Error) -> DriverError {
    match &error {
        sqlx::Error::Database(db) => {
            let state = db.code().map(|code| code.into_owned());
            let code = match state.as_deref() {
                Some("28P01" | "28000") => ErrorCode::Authentication,
                Some("42501") => ErrorCode::PermissionDenied,
                _ => ErrorCode::Database,
            };
            let mut message = db.message().to_owned();
            let mut position = None;
            if let Some(pg) = db.try_downcast_ref::<sqlx::postgres::PgDatabaseError>() {
                if let Some(detail) = pg.detail() {
                    message.push_str("\nDetail: ");
                    message.push_str(detail);
                }
                if let Some(hint) = pg.hint() {
                    message.push_str("\nHint: ");
                    message.push_str(hint);
                }
                if let Some(sqlx::postgres::PgErrorPosition::Original(at)) = pg.position() {
                    position = Some(at);
                }
            }
            DriverError::from_server(
                code,
                message,
                ServerDiagnostic {
                    sqlstate: state,
                    vendor_code: None,
                    position,
                },
            )
        }
        sqlx::Error::Io(io) => DriverError::new(
            ErrorCode::Connection,
            format!("Could not reach the server: {io}"),
        ),
        sqlx::Error::Tls(tls) => DriverError::new(
            ErrorCode::Connection,
            format!("The TLS connection failed: {tls}"),
        ),
        _ => DriverError::new(
            ErrorCode::Protocol,
            "Database operation failed; check connectivity, permissions and SQL",
        ),
    }
}
/// How long a session may take to cancel a statement on the server, and then
/// to get back to a known protocol state, before it is closed.
const RECOVERY: Duration = Duration::from_secs(5);
/// Reads what is left of the previous response up to the server's
/// ReadyForQuery. SQLx reports an ErrorResponse found on the way (such as the
/// one a cancelled statement ends with) as the ping's error and keeps reading
/// on the next call, so database errors are skipped until the connection is
/// ready; any other error means the protocol cannot be trusted.
async fn resynchronize(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    loop {
        match connection.ping().await {
            Err(sqlx::Error::Database(_)) => continue,
            outcome => return outcome,
        }
    }
}
async fn bounded<T>(
    cancel: &CancellationToken,
    timeout: Option<Duration>,
    work: impl Future<Output = DriverResult<T>>,
) -> DriverResult<T> {
    Limit::new(cancel, timeout).bound(work).await
}
fn timed_out() -> DriverError {
    DriverError::new(ErrorCode::Timeout, "Database operation timed out")
}
/// When a statement must stop: when it is cancelled, or when its time is up.
struct Limit<'a> {
    cancel: &'a CancellationToken,
    deadline: Option<tokio::time::Instant>,
}
impl<'a> Limit<'a> {
    fn new(cancel: &'a CancellationToken, timeout: Option<Duration>) -> Self {
        Self {
            cancel,
            deadline: timeout.map(|limit| tokio::time::Instant::now() + limit),
        }
    }

    /// Drives `work` until it ends, or until the limit comes while it waits.
    async fn bound<T>(&self, work: impl Future<Output = DriverResult<T>>) -> DriverResult<T> {
        tokio::select! { biased;
            _ = self.cancel.cancelled() => Err(DriverError::cancelled()),
            result = async {
                match self.deadline {
                    Some(deadline) => tokio::time::timeout_at(deadline, work)
                        .await
                        .map_err(|_| timed_out())?,
                    None => work.await,
                }
            } => result,
        }
    }

    /// The error to stop with, when the statement must stop now. While a
    /// result arrives faster than it is read, SQLx's reads never wait, so the
    /// session's task keeps its thread: [`Limit::bound`] is not polled again
    /// and no worker turns Tokio's timer. The token and the clock are read
    /// here instead.
    fn passed(&self) -> Option<DriverError> {
        if self.cancel.is_cancelled() {
            Some(DriverError::cancelled())
        } else if self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            Some(timed_out())
        } else {
            None
        }
    }
}
async fn connect(
    input: &ResolvedConnection,
    cancel: &CancellationToken,
) -> DriverResult<PgConnection> {
    let config = PostgresConfig::parse(&input.config.options)?;
    let mut options = PgConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.user)
        .database(&config.database)
        .ssl_mode(match config.ssl_mode {
            crate::PostgresSslMode::Disable => PgSslMode::Disable,
            crate::PostgresSslMode::Prefer => PgSslMode::Prefer,
            crate::PostgresSslMode::Require => PgSslMode::Require,
            crate::PostgresSslMode::VerifyCa => PgSslMode::VerifyCa,
            crate::PostgresSslMode::VerifyFull => PgSslMode::VerifyFull,
        });

    if let Some(password) = &input.credentials.password {
        options = options.password(password.expose_secret());
    }
    use sqlx::ConnectOptions;
    options = options.disable_statement_logging();
    bounded(cancel, Some(Duration::from_secs(15)), async {
        PgConnection::connect_with(&options)
            .await
            .map_err(map_error)
    })
    .await
}
pub async fn test(
    input: &ResolvedConnection,
    cancel: CancellationToken,
) -> DriverResult<ServerInfo> {
    let mut connection = connect(input, &cancel).await?;
    let version = bounded(&cancel, Some(Duration::from_secs(15)), async {
        sqlx::query_scalar::<_, String>("SELECT version()")
            .fetch_one(&mut connection)
            .await
            .map_err(map_error)
    })
    .await?;
    connection.close().await.map_err(map_error)?;
    Ok(ServerInfo {
        version: Some(version),
    })
}
pub async fn open(
    input: &ResolvedConnection,
    cancel: CancellationToken,
) -> DriverResult<Box<dyn Session>> {
    let mut connection = connect(input, &cancel).await?;
    let search_path = PostgresConfig::parse(&input.config.options)?.search_path_schemas();
    let backend = bounded(&cancel, Some(Duration::from_secs(15)), async {
        // The configured search_path, before the tab chooses anything.
        if !search_path.is_empty() {
            let schemas: Vec<String> = search_path
                .iter()
                .map(|schema| {
                    if schema == "$user" {
                        "\"$user\"".to_owned()
                    } else {
                        format!("\"{}\"", schema.replace('"', "\"\""))
                    }
                })
                .collect();
            let sql = format!("SET search_path TO {}", schemas.join(", "));
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                .execute(&mut connection)
                .await
                .map_err(map_error)?;
        }
        Ok(sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
            .fetch_one(&mut connection)
            .await
            .map_err(map_error)? as i64)
    })
    .await?;
    let (commands, receiver) = mpsc::channel(4);
    let open = Arc::new(AtomicBool::new(true));
    let server = Server {
        input: input.clone(),
        backend,
        database: PostgresConfig::parse(&input.config.options)?.database,
        open: open.clone(),
        schema: None,
        unread: false,
    };
    tokio::spawn(server.serve(connection, receiver));
    Ok(Box::new(DatabaseSession { commands, open }))
}

/// Rows sent from the connection's task to the session handle. A bounded
/// channel paces the reading; dropping the receiver asks the task to stop.
const EVENT_BUFFER: usize = 256;

enum Command {
    Execute {
        request: QueryRequest,
        events: mpsc::Sender<Event>,
        cancel: CancellationToken,
    },
    Fetch {
        options: ExecutionOptions,
        events: mpsc::Sender<Event>,
        cancel: CancellationToken,
    },
    Ping {
        reply: oneshot::Sender<DriverResult<()>>,
    },
    Context {
        reply: oneshot::Sender<DriverResult<SessionContext>>,
    },
    Transaction {
        reply: oneshot::Sender<DriverResult<Option<TransactionStatus>>>,
    },
    Encryption {
        reply: oneshot::Sender<DriverResult<Encryption>>,
    },
    EditTarget {
        columns: Vec<ColumnInfo>,
        reply: oneshot::Sender<DriverResult<write::EditTarget>>,
        cancel: CancellationToken,
    },
    Write {
        request: Box<write::WriteRequest>,
        reply: oneshot::Sender<DriverResult<write::WriteOutcome>>,
        cancel: CancellationToken,
    },
    TableTarget {
        path: ObjectPath,
        reply: oneshot::Sender<DriverResult<write::EditTarget>>,
        cancel: CancellationToken,
    },
    BeginWrites {
        reply: oneshot::Sender<DriverResult<()>>,
        cancel: CancellationToken,
    },
    EndWrites {
        commit: bool,
        reply: oneshot::Sender<DriverResult<()>>,
    },
    Close {
        reply: oneshot::Sender<DriverResult<()>>,
    },
}

/// How long reading a table's catalog for editing may take.
const CATALOG: Duration = Duration::from_secs(60);

enum Event {
    Columns(Vec<ColumnInfo>),
    Row(Row),
    Done(DriverResult<DriverOutcome>),
}

/// The handle a tab holds. The connection lives in its own task, so a
/// result can stay open between pages while the handle is free: the task
/// keeps the row stream and waits for the next command.
#[derive(Debug)]
struct DatabaseSession {
    commands: mpsc::Sender<Command>,
    open: Arc<AtomicBool>,
}

fn session_closed() -> DriverError {
    DriverError::new(
        ErrorCode::Connection,
        "Session closed; reconnect before executing",
    )
}

impl DatabaseSession {
    /// Sends a command whose rows arrive as events and forwards them to
    /// `sink`. A sink that stops drops the receiver, which ends the result.
    async fn stream(
        &self,
        command: Command,
        mut events: mpsc::Receiver<Event>,
        sink: &mut dyn RowSink,
    ) -> DriverResult<DriverOutcome> {
        self.commands
            .send(command)
            .await
            .map_err(|_| session_closed())?;
        while let Some(event) = events.recv().await {
            match event {
                Event::Columns(columns) => {
                    if sink.columns(columns) == SinkControl::Stop {
                        return Ok(DriverOutcome::default());
                    }
                }
                Event::Row(row) => {
                    if sink.row(row) == SinkControl::Stop {
                        return Ok(DriverOutcome::default());
                    }
                }
                Event::Done(outcome) => return outcome,
            }
        }
        Err(session_closed())
    }

    async fn ask(
        &self,
        command: impl FnOnce(oneshot::Sender<DriverResult<()>>) -> Command,
    ) -> DriverResult<()> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| session_closed())?;
        answer.await.map_err(|_| session_closed())?
    }
}

#[async_trait]
impl Session for DatabaseSession {
    async fn execute(
        &mut self,
        request: &QueryRequest,
        sink: &mut dyn RowSink,
        cancel: CancellationToken,
    ) -> DriverResult<DriverOutcome> {
        request.validate()?;
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let (events, receiver) = mpsc::channel(EVENT_BUFFER);
        let command = Command::Execute {
            request: request.clone(),
            events,
            cancel,
        };
        self.stream(command, receiver, sink).await
    }
    async fn fetch(
        &mut self,
        options: &ExecutionOptions,
        sink: &mut dyn RowSink,
        cancel: CancellationToken,
    ) -> DriverResult<DriverOutcome> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let (events, receiver) = mpsc::channel(EVENT_BUFFER);
        let command = Command::Fetch {
            options: options.clone(),
            events,
            cancel,
        };
        self.stream(command, receiver, sink).await
    }
    fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }
    async fn ping(&mut self) -> DriverResult<()> {
        self.ask(|reply| Command::Ping { reply }).await
    }
    async fn context(&mut self, cancel: CancellationToken) -> DriverResult<SessionContext> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Context { reply })
            .await
            .map_err(|_| session_closed())?;
        answer.await.map_err(|_| session_closed())?
    }
    async fn transaction(
        &mut self,
        cancel: CancellationToken,
    ) -> DriverResult<Option<TransactionStatus>> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Transaction { reply })
            .await
            .map_err(|_| session_closed())?;
        answer.await.map_err(|_| session_closed())?
    }
    /// Read from `pg_stat_ssl` for this backend, so a profile that only
    /// prefers TLS tells whether it fell back to plain text.
    async fn encryption(&mut self, cancel: CancellationToken) -> DriverResult<Encryption> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Encryption { reply })
            .await
            .map_err(|_| session_closed())?;
        answer.await.map_err(|_| session_closed())?
    }
    /// Read from `pg_class`, `pg_attribute`, `pg_index`, `pg_trigger` and
    /// `pg_constraint` for the relation the columns' OIDs name, inside a
    /// savepoint when a transaction is open.
    async fn edit_target(
        &mut self,
        _statement: &str,
        columns: &[ColumnInfo],
        cancel: CancellationToken,
    ) -> DriverResult<write::EditTarget> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::EditTarget {
                columns: columns.to_vec(),
                reply,
                cancel,
            })
            .await
            .map_err(|_| session_closed())?;
        answer.await.map_err(|_| session_closed())?
    }

    async fn write(
        &mut self,
        request: &write::WriteRequest,
        cancel: CancellationToken,
    ) -> DriverResult<write::WriteOutcome> {
        request.validate()?;
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Write {
                request: Box::new(request.clone()),
                reply,
                cancel,
            })
            .await
            .map_err(|_| session_closed())?;
        answer.await.map_err(|_| session_closed())?
    }

    /// Read from `pg_class`, `pg_attribute`, `pg_index`, `pg_trigger` and
    /// `pg_constraint` for the table at `path`.
    async fn table_target(
        &mut self,
        path: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<write::EditTarget> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::TableTarget {
                path: path.clone(),
                reply,
                cancel,
            })
            .await
            .map_err(|_| session_closed())?;
        answer.await.map_err(|_| session_closed())?
    }

    async fn begin_writes(&mut self, cancel: CancellationToken) -> DriverResult<()> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        self.ask(|reply| Command::BeginWrites { reply, cancel })
            .await
    }

    /// Never cancelled: a commit or a rollback is not left half done.
    async fn end_writes(&mut self, commit: bool, _cancel: CancellationToken) -> DriverResult<()> {
        self.ask(|reply| Command::EndWrites { commit, reply }).await
    }

    async fn close(self: Box<Self>) -> DriverResult<()> {
        if !self.is_open() {
            return Ok(());
        }
        self.ask(|reply| Command::Close { reply }).await
    }
}

/// The PostgreSQL type a parameter's value is read as.
fn parameter_type(kind: ParameterKind) -> &'static str {
    match kind {
        ParameterKind::Text => "text",
        ParameterKind::Number => "numeric",
        ParameterKind::Boolean => "boolean",
        ParameterKind::Date => "date",
        ParameterKind::Timestamp => "timestamp",
    }
}

/// The session setting that holds parameter `index` for a run.
fn parameter_setting(index: usize) -> String {
    format!("jykre.p_{index}")
}

/// What a parameter's marker becomes (ADR-0023): a read of the setting
/// bound before the statement, or a typed NULL. Neither holds the value.
fn parameter_reference(index: usize, parameter: &QueryParameter) -> String {
    let kind = parameter_type(parameter.kind);
    match parameter.value {
        Some(_) => format!("(current_setting('{}')::{kind})", parameter_setting(index)),
        None => format!("CAST(NULL AS {kind})"),
    }
}

/// The settings to bind before the statement, as names and values: one per
/// parameter that is not NULL.
fn parameter_settings(parameters: &[QueryParameter]) -> (Vec<String>, Vec<String>) {
    parameters
        .iter()
        .enumerate()
        .filter_map(|(index, parameter)| {
            let value = parameter.value.clone()?;
            Some((parameter_setting(index), value))
        })
        .unzip()
}

/// Binds the settings with the extended protocol, so the values travel as
/// bound parameters; the user's SQL then reads them with the text protocol.
/// They stay in this session until the next run sets them again: a result
/// can stay open for paging, and nothing else can run until it ends.
const BIND_SETTINGS: &str =
    "SELECT set_config(name, value, false) FROM unnest($1::text[], $2::text[]) AS p(name, value)";

/// What a statement does to the first snapshot of the transaction it runs
/// in. PostgreSQL takes that snapshot at the first query of a transaction,
/// and some commands (`SET TRANSACTION`) are allowed only before it, so the
/// session must not query the server while a just opened transaction has
/// none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotEffect {
    /// `BEGIN`, `START TRANSACTION`, or ending one `AND CHAIN`: a transaction
    /// opens without a snapshot.
    Opens,
    /// `SET`, `SHOW`, `SAVEPOINT`, `RELEASE`, `ROLLBACK TO`: no snapshot is
    /// taken.
    Keeps,
    /// Anything else may take one.
    Takes,
}

/// The [`SnapshotEffect`] of `sql`, from its first words after whitespace and
/// comments.
fn snapshot_effect(sql: &str) -> SnapshotEffect {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, next)| next);
        } else if rest.starts_with("/*") {
            // Block comments nest in PostgreSQL.
            let mut depth = 0usize;
            let mut end = rest.len();
            let bytes = rest.as_bytes();
            let mut at = 0;
            while at + 1 < bytes.len() {
                match (bytes[at], bytes[at + 1]) {
                    (b'/', b'*') => {
                        depth += 1;
                        at += 2;
                    }
                    (b'*', b'/') => {
                        depth -= 1;
                        at += 2;
                        if depth == 0 {
                            end = at;
                            break;
                        }
                    }
                    _ => at += 1,
                }
            }
            rest = &rest[end..];
        } else {
            break;
        }
    }
    let first: String = rest
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_uppercase();
    match first.as_str() {
        "BEGIN" | "START" => SnapshotEffect::Opens,
        "SET" | "SHOW" | "SAVEPOINT" | "RELEASE" => SnapshotEffect::Keeps,
        "COMMIT" | "END" | "ROLLBACK" | "ABORT" => {
            // `[WORK | TRANSACTION] [AND [NO] CHAIN]`, or `ROLLBACK [WORK |
            // TRANSACTION] TO [SAVEPOINT] name`.
            let next: Vec<String> = rest[first.len()..]
                .split(|c: char| !c.is_ascii_alphabetic())
                .filter(|word| !word.is_empty())
                .map(str::to_ascii_uppercase)
                .skip_while(|word| word == "WORK" || word == "TRANSACTION")
                .take(2)
                .collect();
            match next.iter().map(String::as_str).collect::<Vec<_>>()[..] {
                ["AND", "CHAIN"] => SnapshotEffect::Opens,
                ["TO", ..] if first == "ROLLBACK" => SnapshotEffect::Keeps,
                _ => SnapshotEffect::Takes,
            }
        }
        _ => SnapshotEffect::Takes,
    }
}

/// Whether the open transaction wrote something, read with one
/// simple-protocol query. Asked only once the transaction has a snapshot
/// ([`SnapshotEffect`]), so the query takes none.
async fn transaction_wrote(connection: &mut PgConnection) -> DriverResult<bool> {
    let row = sqlx::raw_sql(sqlx::AssertSqlSafe(
        "SELECT (pg_current_xact_id_if_assigned() IS NOT NULL)::text",
    ))
    .fetch_one(&mut *connection)
    .await
    .map_err(map_error)?;
    Ok(row.try_get::<String, _>(0).map_err(map_error)? == "true")
}

/// Where an edit runs, from the transaction state of the last
/// `ReadyForQuery`: `true` inside an open transaction (in a savepoint),
/// `false` outside one; a failed transaction refuses edits until it is
/// rolled back.
async fn edit_scope(connection: &mut PgConnection) -> DriverResult<bool> {
    use sqlx::postgres::PgTransactionStatus;
    match connection.transaction_status().await.map_err(map_error)? {
        PgTransactionStatus::Idle => Ok(false),
        PgTransactionStatus::Transaction => Ok(true),
        PgTransactionStatus::Error => Err(DriverError::validation(
            "The transaction failed; roll it back before editing rows",
        )),
    }
}

/// Runs a session command the driver writes itself (no value in it).
async fn simple(connection: &mut PgConnection, sql: &str) -> DriverResult<()> {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(&mut *connection)
        .await
        .map(|_| ())
        .map_err(map_error)
}

/// Ends what an edit opened, within [`RECOVERY`]: its savepoint (`nested`)
/// or its own transaction, kept (`keep`: released or committed) or rolled
/// back.
async fn end_edit(connection: &mut PgConnection, nested: bool, keep: bool) -> DriverResult<()> {
    let savepoint = crate::edit::SAVEPOINT;
    let sql = match (nested, keep) {
        (true, true) => format!("RELEASE SAVEPOINT {savepoint}"),
        (true, false) => {
            format!("ROLLBACK TO SAVEPOINT {savepoint}; RELEASE SAVEPOINT {savepoint}")
        }
        (false, true) => "COMMIT".to_owned(),
        (false, false) => "ROLLBACK".to_owned(),
    };
    tokio::time::timeout(RECOVERY, simple(connection, &sql))
        .await
        .unwrap_or_else(|_| Err(timed_out()))
}

/// A write whose `COMMIT` got no answer: it may or may not have been
/// written, and it is never sent again.
fn commit_unknown() -> DriverError {
    DriverError::new(
        ErrorCode::Connection,
        "The connection was lost while committing: the changes may or may not have been written. Read the rows again before writing them again.",
    )
}

/// The error of a change of an import's write that affected `matched` rows,
/// not one.
fn mismatch(index: usize, matched: u64) -> DriverError {
    DriverError::validation(format!(
        "The row affected {matched} rows of the table, not one"
    ))
    .at_change(index)
}

/// Commits (`commit`) or rolls back the import's transaction. A commit the
/// server answered with an error rolled it back; one without an answer may
/// or may not have been written, and closes the session.
async fn end_writes(connection: &mut PgConnection, commit: bool) -> (DriverResult<()>, Flow) {
    match end_edit(connection, false, commit).await {
        Ok(()) => (Ok(()), Flow::Ready),
        Err(error) if error.server.is_some() => (Err(error), Flow::Ready),
        Err(_) if commit => (Err(commit_unknown()), Flow::Broken),
        // Closing the session rolls the transaction back.
        Err(error) => (Err(error), Flow::Broken),
    }
}

/// What the connection's task does after a statement.
enum Flow {
    /// Wait for the next command.
    Ready,
    /// A command arrived while a result was open; run it next.
    Next(Command),
    /// The handle is gone.
    Closed,
    /// The protocol cannot be trusted; close the connection.
    Broken,
}

/// How reading a page ended.
enum PageEnd {
    Finished,
    /// The page is full and at least one more row was read ahead.
    Full,
    /// The handle stopped listening.
    Gone,
}

struct Server {
    input: ResolvedConnection,
    backend: i64,
    database: String,
    open: Arc<AtomicBool>,
    /// The schema last put first in `search_path` by a request, or reported
    /// by [`Session::context`]: a request for it is not applied again.
    schema: Option<String>,
    /// A transaction opened and nothing has taken its snapshot yet
    /// ([`SnapshotEffect`]): the session answers about itself without
    /// querying the server, so the user's first query takes the snapshot.
    unread: bool,
}

impl Server {
    async fn serve(mut self, mut connection: PgConnection, mut commands: mpsc::Receiver<Command>) {
        let mut next = None;
        loop {
            let command = match next.take() {
                Some(command) => command,
                None => match commands.recv().await {
                    Some(command) => command,
                    None => break,
                },
            };
            let flow = match command {
                Command::Execute {
                    request,
                    events,
                    cancel,
                } => {
                    self.execute(&mut connection, request, events, cancel, &mut commands)
                        .await
                }
                Command::Fetch { events, .. } => {
                    let _ = events
                        .send(Event::Done(Err(DriverError::validation(
                            "No result has more rows to read",
                        ))))
                        .await;
                    Flow::Ready
                }
                Command::Ping { reply } => {
                    match tokio::time::timeout(RECOVERY, resynchronize(&mut connection)).await {
                        Ok(Ok(())) => {
                            let _ = reply.send(Ok(()));
                            Flow::Ready
                        }
                        outcome => {
                            self.open.store(false, Ordering::SeqCst);
                            let _ = reply.send(Err(match outcome {
                                Ok(Err(error)) => map_error(error),
                                _ => DriverError::new(
                                    ErrorCode::Timeout,
                                    "Database operation timed out",
                                ),
                            }));
                            Flow::Broken
                        }
                    }
                }
                Command::Context { reply } if self.unread => {
                    // Querying would take the new transaction's snapshot.
                    let _ = reply.send(Ok(SessionContext {
                        catalog: Some(self.database.clone()),
                        schema: self.schema.clone(),
                    }));
                    Flow::Ready
                }
                Command::Transaction { reply } => {
                    match tokio::time::timeout(RECOVERY, self.transaction(&mut connection)).await {
                        Ok(status) => {
                            let _ = reply.send(status.map(Some));
                            Flow::Ready
                        }
                        Err(_) => {
                            let _ = reply.send(Err(DriverError::new(
                                ErrorCode::Timeout,
                                "Database operation timed out",
                            )));
                            Flow::Broken
                        }
                    }
                }
                Command::Context { reply } => {
                    let context = tokio::time::timeout(
                        RECOVERY,
                        sqlx::query_as::<_, (String, Option<String>)>(
                            "SELECT current_database(), current_schema()",
                        )
                        .fetch_one(&mut connection),
                    )
                    .await;
                    match context {
                        Ok(Ok((database, schema))) => {
                            self.schema.clone_from(&schema);
                            let _ = reply.send(Ok(SessionContext {
                                catalog: Some(database),
                                schema,
                            }));
                            Flow::Ready
                        }
                        Ok(Err(error)) => {
                            let _ = reply.send(Err(map_error(error)));
                            Flow::Ready
                        }
                        Err(_) => {
                            let _ = reply.send(Err(DriverError::new(
                                ErrorCode::Timeout,
                                "Database operation timed out",
                            )));
                            Flow::Broken
                        }
                    }
                }
                Command::Encryption { reply } if self.unread => {
                    // Querying would take the new transaction's snapshot.
                    let _ = reply.send(Ok(Encryption::Unknown));
                    Flow::Ready
                }
                Command::Encryption { reply } => {
                    let found = tokio::time::timeout(
                        RECOVERY,
                        sqlx::query_as::<_, (bool, Option<String>)>(
                            "SELECT ssl, version FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
                        )
                        .fetch_optional(&mut connection),
                    )
                    .await;
                    match found {
                        Ok(Ok(found)) => {
                            let _ = reply.send(Ok(match found {
                                Some((true, version)) => Encryption::Tls { version },
                                Some((false, _)) => Encryption::Plaintext,
                                // A server without the view's row (a
                                // compatible database) does not tell.
                                None => Encryption::Unknown,
                            }));
                            Flow::Ready
                        }
                        Ok(Err(error)) => {
                            let _ = reply.send(Err(map_error(error)));
                            Flow::Ready
                        }
                        Err(_) => {
                            let _ = reply.send(Err(DriverError::new(
                                ErrorCode::Timeout,
                                "Database operation timed out",
                            )));
                            Flow::Broken
                        }
                    }
                }
                Command::EditTarget {
                    columns,
                    reply,
                    cancel,
                } => {
                    let (result, flow) = self.edit_target(&mut connection, &columns, &cancel).await;
                    // The handle may have stopped waiting.
                    let _ = reply.send(result);
                    flow
                }
                Command::Write {
                    request,
                    reply,
                    cancel,
                } => {
                    let (result, flow) = self.write(&mut connection, &request, &cancel).await;
                    // The handle may have stopped waiting.
                    let _ = reply.send(result);
                    flow
                }
                Command::TableTarget {
                    path,
                    reply,
                    cancel,
                } => {
                    let (result, flow) = self.table_target(&mut connection, &path, &cancel).await;
                    // The handle may have stopped waiting.
                    let _ = reply.send(result);
                    flow
                }
                Command::BeginWrites { reply, cancel } => {
                    let (result, flow) = self.begin_writes(&mut connection, &cancel).await;
                    // The handle may have stopped waiting.
                    let _ = reply.send(result);
                    flow
                }
                Command::EndWrites { commit, reply } => {
                    let (result, flow) = end_writes(&mut connection, commit).await;
                    // The handle may have stopped waiting.
                    let _ = reply.send(result);
                    flow
                }
                Command::Close { reply } => {
                    self.open.store(false, Ordering::SeqCst);
                    let _ = reply.send(connection.close().await.map_err(map_error));
                    return;
                }
            };
            match flow {
                Flow::Ready => {}
                Flow::Next(command) => next = Some(command),
                Flow::Closed => break,
                Flow::Broken => {
                    self.open.store(false, Ordering::SeqCst);
                    let _ = connection.close_hard().await;
                    return;
                }
            }
        }
        self.open.store(false, Ordering::SeqCst);
        let _ = tokio::time::timeout(RECOVERY, connection.close()).await;
    }

    /// The transaction state the server reported in its last `ReadyForQuery`
    /// message, which takes no query. Whether an open transaction wrote is
    /// known to be nothing while it has no snapshot, and is otherwise asked
    /// with [`transaction_wrote`].
    async fn transaction(
        &mut self,
        connection: &mut PgConnection,
    ) -> DriverResult<TransactionStatus> {
        use sqlx::postgres::PgTransactionStatus;
        let status = connection.transaction_status().await.map_err(map_error)?;
        if status != PgTransactionStatus::Transaction {
            // Whatever the statements suggested, no transaction waits for
            // its snapshot.
            self.unread = false;
        }
        Ok(match status {
            PgTransactionStatus::Idle => TransactionStatus::Idle,
            PgTransactionStatus::Error => TransactionStatus::Failed,
            PgTransactionStatus::Transaction if self.unread => TransactionStatus::Open {
                changes: Some(false),
            },
            PgTransactionStatus::Transaction => TransactionStatus::Open {
                changes: Some(transaction_wrote(connection).await?),
            },
        })
    }

    /// Runs one statement and serves its pages until the result ends or
    /// another command arrives.
    async fn execute(
        &mut self,
        connection: &mut PgConnection,
        request: QueryRequest,
        mut events: mpsc::Sender<Event>,
        mut cancel: CancellationToken,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Flow {
        let sql = request.with_references(parameter_reference);
        let (names, values) = parameter_settings(&request.parameters);
        // Until it ends well, the statement may have taken a snapshot (or
        // failed the transaction): the session queries the server again.
        let effect = snapshot_effect(&sql);
        let unread = std::mem::take(&mut self.unread);
        let mut options = request.options.clone();
        // The chosen schema goes first in search_path only when it changed,
        // so a SET search_path the user ran meanwhile stays in force.
        let apply = options
            .context
            .schema
            .clone()
            .filter(|schema| self.schema.as_ref() != Some(schema));
        let database = self.database.clone();
        let applied = bounded(&cancel, options.timeout, async {
            if options
                .context
                .catalog
                .as_ref()
                .is_some_and(|v| v != &database)
            {
                return Err(DriverError::validation(
                    "PostgreSQL catalog cannot change within a session",
                ));
            }
            if let Some(schema) = &apply {
                let sql = format!(
                    "SET search_path TO \"{}\", \"$user\", public",
                    schema.replace('"', "\"\"")
                );
                sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                    .execute(&mut *connection)
                    .await
                    .map_err(map_error)?;
            }
            if !names.is_empty() {
                sqlx::query(BIND_SETTINGS)
                    .bind(&names)
                    .bind(&values)
                    .execute(&mut *connection)
                    .await
                    .map_err(map_error)?;
            }
            Ok(())
        })
        .await;
        if let Err(error) = applied {
            return self.fail(connection, error, &events).await;
        }
        if apply.is_some() {
            self.schema = apply;
        }
        let columns = bounded(&cancel, options.timeout, async {
            let description = connection
                .prepare(sqlx::AssertSqlSafe(sql.as_str()).into_sql_str())
                .await
                .map_err(map_error)?;
            Ok(description
                .columns()
                .iter()
                .map(|c| ColumnInfo {
                    name: c.name().into(),
                    data_type: c.type_info().name().into(),
                    nullable: None,
                    origin: crate::edit::origin(c.relation_id(), c.relation_attribute_no()),
                })
                .collect::<Vec<_>>())
        })
        .await;
        let columns = match columns {
            Ok(columns) => columns,
            Err(error) => return self.fail(connection, error, &events).await,
        };
        let returns_rows = !columns.is_empty();
        if returns_rows && events.send(Event::Columns(columns)).await.is_err() {
            return self.recover(connection, false).await;
        }
        let mut stream =
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str())).fetch_many(&mut *connection);
        let mut ahead = None;
        let mut affected = None;
        loop {
            let limit = Limit::new(&cancel, options.timeout);
            let page = limit
                .bound(read_page(
                    &mut stream,
                    &mut ahead,
                    &mut affected,
                    options.max_rows,
                    &events,
                    &limit,
                ))
                .await;
            match page {
                Ok(PageEnd::Finished) => {
                    drop(stream);
                    self.unread = match effect {
                        SnapshotEffect::Opens => true,
                        SnapshotEffect::Keeps => unread,
                        SnapshotEffect::Takes => false,
                    };
                    let outcome = DriverOutcome {
                        rows_affected: if returns_rows { None } else { affected },
                        has_more: false,
                        ..DriverOutcome::default()
                    };
                    let _ = events.send(Event::Done(Ok(outcome))).await;
                    return Flow::Ready;
                }
                Ok(PageEnd::Full) => {
                    let outcome = DriverOutcome {
                        rows_affected: None,
                        has_more: true,
                        ..DriverOutcome::default()
                    };
                    if events.send(Event::Done(Ok(outcome))).await.is_err() {
                        drop(stream);
                        return self.recover(connection, true).await;
                    }
                    // The result stays open until the next command.
                    match commands.recv().await {
                        Some(Command::Fetch {
                            options: next,
                            events: next_events,
                            cancel: next_cancel,
                        }) => {
                            options = next;
                            events = next_events;
                            cancel = next_cancel;
                        }
                        Some(other) => {
                            drop(stream);
                            return match self.recover(connection, true).await {
                                Flow::Ready => Flow::Next(other),
                                flow => flow,
                            };
                        }
                        None => return Flow::Closed,
                    }
                }
                Ok(PageEnd::Gone) => {
                    drop(stream);
                    return self.recover(connection, true).await;
                }
                Err(error) => {
                    drop(stream);
                    return self.fail(connection, error, &events).await;
                }
            }
        }
    }

    /// Reads what a result with `columns` can be written back to. Inside an
    /// open transaction the catalog is read in a savepoint, rolled back
    /// afterwards, so neither a failure nor a cancellation reaches that
    /// transaction.
    async fn edit_target(
        &mut self,
        connection: &mut PgConnection,
        columns: &[ColumnInfo],
        cancel: &CancellationToken,
    ) -> (DriverResult<write::EditTarget>, Flow) {
        let nested = match edit_scope(connection).await {
            Ok(nested) => nested,
            Err(error) => return (Err(error), Flow::Ready),
        };
        if nested {
            // The catalog's queries take the transaction's snapshot.
            self.unread = false;
            if let Err(error) =
                simple(connection, &format!("SAVEPOINT {}", crate::edit::SAVEPOINT)).await
            {
                let flow = self.abandon(connection, None, &error).await;
                return (Err(error), flow);
            }
        }
        let read = bounded(
            cancel,
            Some(CATALOG),
            crate::edit::edit_target(connection, &self.database, columns),
        )
        .await;
        match read {
            Ok(target) => {
                let flow = if nested && end_edit(connection, true, false).await.is_err() {
                    Flow::Broken
                } else {
                    Flow::Ready
                };
                (Ok(target), flow)
            }
            Err(error) => {
                let flow = self
                    .abandon(connection, nested.then_some(true), &error)
                    .await;
                (Err(error), flow)
            }
        }
    }

    /// The table at `path` as an import's target. Read in the session's
    /// snapshot, outside any transaction an import opens later.
    async fn table_target(
        &mut self,
        connection: &mut PgConnection,
        path: &ObjectPath,
        cancel: &CancellationToken,
    ) -> (DriverResult<write::EditTarget>, Flow) {
        let read = bounded(
            cancel,
            Some(CATALOG),
            crate::edit::table_target(connection, &self.database, path),
        )
        .await;
        match read {
            Ok(target) => (Ok(target), Flow::Ready),
            Err(error) => {
                let flow = self.abandon(connection, None, &error).await;
                (Err(error), flow)
            }
        }
    }

    /// Opens the import's transaction, which only the session's own
    /// `end_writes` ends.
    async fn begin_writes(
        &mut self,
        connection: &mut PgConnection,
        cancel: &CancellationToken,
    ) -> (DriverResult<()>, Flow) {
        match edit_scope(connection).await {
            Ok(false) => {}
            Ok(true) => {
                return (
                    Err(DriverError::validation(
                        "The session already holds a transaction",
                    )),
                    Flow::Ready,
                );
            }
            Err(error) => return (Err(error), Flow::Ready),
        }
        match bounded(cancel, Some(RECOVERY), simple(connection, "BEGIN")).await {
            Ok(()) => {
                self.unread = false;
                (Ok(()), Flow::Ready)
            }
            Err(error) => {
                let flow = self.abandon(connection, None, &error).await;
                (Err(error), flow)
            }
        }
    }

    /// A write that joins the import's transaction (`WriteScope::Caller`):
    /// the changes run with nothing committed or rolled back, and one that
    /// fails or affects another number of rows than one fails the write at
    /// its index. The transaction is left to `end_writes`.
    async fn write_in_caller(
        &mut self,
        connection: &mut PgConnection,
        request: &write::WriteRequest,
        cancel: &CancellationToken,
    ) -> (DriverResult<write::WriteOutcome>, Flow) {
        match edit_scope(connection).await {
            Ok(true) => {}
            Ok(false) => {
                return (
                    Err(DriverError::validation("Begin the writes first")),
                    Flow::Ready,
                );
            }
            Err(error) => return (Err(error), Flow::Ready),
        }
        let applied = bounded(
            cancel,
            request.timeout,
            crate::edit::apply(connection, &self.database, request),
        )
        .await;
        match applied {
            Ok(crate::edit::Applied::Written(rows)) => (
                Ok(write::WriteOutcome::Written {
                    rows,
                    committed: false,
                }),
                Flow::Ready,
            ),
            Ok(crate::edit::Applied::Mismatch { index, matched }) => {
                (Err(mismatch(index, matched)), Flow::Ready)
            }
            Err(error) => {
                let flow = self.abandon(connection, None, &error).await;
                (Err(error), flow)
            }
        }
    }

    /// Writes `request` in a transaction of its own, or in a savepoint of the
    /// open one, and commits (or releases the savepoint) only when every
    /// change affected exactly one row. A failure, a cancellation or the
    /// timeout rolls back what the write did. A write with
    /// `WriteScope::Caller` joins the import's transaction instead.
    async fn write(
        &mut self,
        connection: &mut PgConnection,
        request: &write::WriteRequest,
        cancel: &CancellationToken,
    ) -> (DriverResult<write::WriteOutcome>, Flow) {
        if request.scope == write::WriteScope::Caller {
            return self.write_in_caller(connection, request, cancel).await;
        }
        let nested = match edit_scope(connection).await {
            Ok(nested) => nested,
            Err(error) => return (Err(error), Flow::Ready),
        };
        let open = if nested {
            format!("SAVEPOINT {}", crate::edit::SAVEPOINT)
        } else {
            "BEGIN".to_owned()
        };
        if let Err(error) = simple(connection, &open).await {
            let flow = self.abandon(connection, None, &error).await;
            return (Err(error), flow);
        }
        self.unread = false;
        let applied = bounded(
            cancel,
            request.timeout,
            crate::edit::apply(connection, &self.database, request),
        )
        .await;
        match applied {
            Ok(crate::edit::Applied::Written(rows)) => {
                match end_edit(connection, nested, true).await {
                    Ok(()) => (
                        Ok(write::WriteOutcome::Written {
                            rows,
                            committed: !nested,
                        }),
                        Flow::Ready,
                    ),
                    // The server answered: it rolled the transaction back (a
                    // deferred constraint failed at COMMIT).
                    Err(error) if error.server.is_some() => (Err(error), Flow::Ready),
                    Err(_) => (Err(commit_unknown()), Flow::Broken),
                }
            }
            Ok(crate::edit::Applied::Mismatch { index, matched }) => {
                match end_edit(connection, nested, false).await {
                    Ok(()) => (
                        Ok(write::WriteOutcome::RolledBack { index, matched }),
                        Flow::Ready,
                    ),
                    // Closing the session rolls the write back.
                    Err(error) => (Err(error), Flow::Broken),
                }
            }
            Err(error) => {
                let flow = self.abandon(connection, Some(nested), &error).await;
                (Err(error), flow)
            }
        }
    }

    /// After a failed edit step: brings the protocol back (cancelling a
    /// statement that may still run), then rolls back what the edit opened
    /// (`opened`). A connection that cannot be brought back is closed, which
    /// rolls it back too.
    async fn abandon(
        &self,
        connection: &mut PgConnection,
        opened: Option<bool>,
        error: &DriverError,
    ) -> Flow {
        if error.code == ErrorCode::Connection {
            return Flow::Broken;
        }
        let running = matches!(
            error.code,
            ErrorCode::Cancelled | ErrorCode::Timeout | ErrorCode::Protocol
        );
        if let Flow::Broken = self.recover(connection, running).await {
            return Flow::Broken;
        }
        let Some(nested) = opened else {
            return Flow::Ready;
        };
        match end_edit(connection, nested, false).await {
            Ok(()) => Flow::Ready,
            Err(_) => Flow::Broken,
        }
    }

    /// Brings the protocol back to a known state after a statement was left
    /// unfinished (cancelling it on the server when it may still run), or
    /// reports that the connection must be closed.
    async fn recover(&self, connection: &mut PgConnection, running: bool) -> Flow {
        if running {
            // A cancel request that reaches an idle backend is ignored.
            let cancel_server = async {
                let mut control = connect(&self.input, &CancellationToken::new()).await?;
                sqlx::query("SELECT pg_cancel_backend($1)")
                    .bind(self.backend as i32)
                    .execute(&mut control)
                    .await
                    .map_err(map_error)?;
                Ok::<_, DriverError>(())
            };
            let _ = tokio::time::timeout(RECOVERY, cancel_server).await;
        }
        match tokio::time::timeout(RECOVERY, resynchronize(connection)).await {
            Ok(Ok(())) => Flow::Ready,
            _ => Flow::Broken,
        }
    }

    /// Recovers after a failed statement and reports the error. A database
    /// error keeps the session; a lost connection closes it.
    async fn fail(
        &self,
        connection: &mut PgConnection,
        error: DriverError,
        events: &mpsc::Sender<Event>,
    ) -> Flow {
        let flow = if error.code == ErrorCode::Connection {
            Flow::Broken
        } else {
            let running = matches!(
                error.code,
                ErrorCode::Cancelled | ErrorCode::Timeout | ErrorCode::Protocol
            );
            self.recover(connection, running).await
        };
        if matches!(flow, Flow::Broken) {
            self.open.store(false, Ordering::SeqCst);
        }
        let _ = events.send(Event::Done(Err(error))).await;
        flow
    }
}

/// Sends up to `max_rows` rows, starting with the one read ahead last time,
/// and reads one row ahead to tell a full page from the end of the result.
/// `limit` is checked before every read, so a statement stops at its next
/// row even when its reads never wait.
async fn read_page<S>(
    stream: &mut S,
    ahead: &mut Option<Row>,
    affected: &mut Option<u64>,
    max_rows: u32,
    events: &mpsc::Sender<Event>,
    limit: &Limit<'_>,
) -> DriverResult<PageEnd>
where
    S: futures_util::Stream<
            Item = Result<
                sqlx::Either<sqlx::postgres::PgQueryResult, sqlx::postgres::PgRow>,
                sqlx::Error,
            >,
        > + Unpin,
{
    let mut count = 0u32;
    if let Some(row) = ahead.take() {
        if events.send(Event::Row(row)).await.is_err() {
            return Ok(PageEnd::Gone);
        }
        count = 1;
    }
    loop {
        if let Some(error) = limit.passed() {
            return Err(error);
        }
        let Some(item) = stream.try_next().await.map_err(map_error)? else {
            break;
        };
        match item {
            sqlx::Either::Left(result) => *affected = Some(result.rows_affected()),
            sqlx::Either::Right(row) => {
                let mut cells = Vec::with_capacity(row.len());
                for index in 0..row.len() {
                    let value = row.try_get_raw(index).map_err(map_error)?;
                    if value.is_null() {
                        cells.push(Cell::Null);
                        continue;
                    }
                    let text = value.as_str().map_err(|_| {
                        DriverError::new(ErrorCode::Protocol, "Invalid database text encoding")
                    })?;
                    cells.push(Cell::Text(text.to_owned()));
                }
                if count >= max_rows {
                    *ahead = Some(cells);
                    return Ok(PageEnd::Full);
                }
                if events.send(Event::Row(cells)).await.is_err() {
                    return Ok(PageEnd::Gone);
                }
                count += 1;
            }
        }
    }
    Ok(PageEnd::Finished)
}
/// The connection to read `database` with: the configured one, or another
/// database of the server when the profile offers them all.
fn database_input(input: &ResolvedConnection, database: &str) -> DriverResult<ResolvedConnection> {
    database_connection(
        input,
        database,
        "Metadata scope belongs to another database",
    )
}

/// The connection to `database` on the same server, with the same user and
/// credentials: `input` itself for its own database, a copy that names the
/// other one when the profile offers the server's other databases, and a
/// validation error with `refusal` otherwise.
pub(crate) fn database_connection(
    input: &ResolvedConnection,
    database: &str,
    refusal: &'static str,
) -> DriverResult<ResolvedConnection> {
    let config = PostgresConfig::parse(&input.config.options)?;
    if database == config.database {
        return Ok(input.clone());
    }
    if !config.all_databases {
        return Err(DriverError::validation(refusal));
    }
    let mut other = input.clone();
    other.config.options["database"] = serde_json::Value::String(database.to_owned());
    Ok(other)
}

pub async fn introspect(
    input: &ResolvedConnection,
    scope: &ObjectPath,
    level: IntrospectionLevel,
    sink: &mut dyn IntrospectionSink,
    cancel: CancellationToken,
) -> DriverResult<IntrospectionReport> {
    let configured = PostgresConfig::parse(&input.config.options)?.database;
    let database = scope
        .name_of(ObjectKind::Database)
        .map_or(configured, str::to_owned);
    let input = database_input(input, &database)?;
    let names_only = level == IntrospectionLevel::Names;
    let mut connection = connect(&input, &cancel).await?;
    bounded(&cancel, Some(Duration::from_secs(300)), async {
        let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), database);
        // A chosen schema the user may not use keeps its last good read,
        // with a warning, instead of disappearing.
        if let Some(schema) = scope.name_of(ObjectKind::Schema) {
            let usable: Option<bool> = sqlx::query_scalar(SCHEMA_USABLE_SQL)
                .bind(schema)
                .fetch_optional(&mut connection)
                .await
                .map_err(map_error)?;
            if usable == Some(false) {
                return Ok(IntrospectionReport {
                    warnings: vec![IntrospectionWarning {
                        path: db.path.child(ObjectKind::Schema, schema),
                        message: "No permission to use this schema".into(),
                    }],
                });
            }
        }
        let mut out = Batches::new(sink, scope);
        // Namespaces and relations delivered so far: keys and partitions
        // are only delivered under a relation the user can see.
        let mut seen = HashSet::new();
        let mut partitions = Vec::new();
        let mut rows = sqlx::query(RELATIONS_SQL)
            .bind(scope.name_of(ObjectKind::Schema))
            .bind(scope.relation())
            .bind(scope.name_of(ObjectKind::Column))
            .bind(names_only)
            .fetch(&mut connection);
        while let Some(row) = rows.try_next().await.map_err(map_error)? {
            if seen.insert(db.path.clone()) {
                out.push(db.clone()).await?;
            }
            let schema = MetadataObject::new(
                ObjectKind::Schema,
                &db.path,
                row.try_get::<String, _>("ns").map_err(map_error)?,
            );
            if seen.insert(schema.path.clone()) {
                out.push(schema.clone()).await?;
            }
            let Some(name) = row.try_get::<Option<String>, _>("rel").map_err(map_error)? else {
                continue;
            };
            let comment = row
                .try_get::<Option<String>, _>("relcomment")
                .map_err(map_error)?;
            if let Some(root) = row
                .try_get::<Option<String>, _>("root")
                .map_err(map_error)?
            {
                // A partition hangs under its root table, whatever schema
                // either is in.
                let root_schema = row
                    .try_get::<Option<String>, _>("rootns")
                    .map_err(map_error)?
                    .unwrap_or_else(|| schema.name.clone());
                let parent = db
                    .path
                    .child(ObjectKind::Schema, root_schema)
                    .child(ObjectKind::Table, root);
                let mut partition =
                    MetadataObject::new(ObjectKind::Partition, &parent, name.clone());
                partition.comment = comment;
                if parent.name_of(ObjectKind::Schema) != Some(schema.name.as_str()) {
                    partition.stored_at = Some(schema.path.child(ObjectKind::Table, name));
                }
                partitions.push(partition);
                continue;
            }
            let kind = match row
                .try_get::<Option<String>, _>("relkind")
                .map_err(map_error)?
                .as_deref()
            {
                Some("v") => ObjectKind::View,
                Some("m") => ObjectKind::MaterializedView,
                _ => ObjectKind::Table,
            };
            let mut relation = MetadataObject::new(kind, &schema.path, name);
            if seen.insert(relation.path.clone()) {
                relation.comment = comment;
                out.push(relation.clone()).await?;
            }
            let Some(column) = row.try_get::<Option<String>, _>("col").map_err(map_error)? else {
                continue;
            };
            let mut column = MetadataObject::new(ObjectKind::Column, &relation.path, column);
            column.data_type = row.try_get("dtype").map_err(map_error)?;
            column.nullable = row
                .try_get::<Option<bool>, _>("notnull")
                .map_err(map_error)?
                .map(|not_null| !not_null);
            column.ordinal = row
                .try_get::<Option<i32>, _>("pos")
                .map_err(map_error)?
                .and_then(|value| u32::try_from(value).ok());
            column.default_value = row.try_get("dflt").map_err(map_error)?;
            column.comment = row.try_get("colcomment").map_err(map_error)?;
            column.primary_key = row
                .try_get::<Option<i32>, _>("pk")
                .map_err(map_error)?
                .and_then(|value| u32::try_from(value).ok());
            out.push(column).await?;
        }
        drop(rows);
        if names_only {
            out.flush().await?;
            return Ok(IntrospectionReport::default());
        }
        // A partition whose root table is not visible here stays out rather
        // than hanging under a missing node.
        for partition in partitions {
            if partition
                .path
                .parent()
                .is_some_and(|parent| seen.contains(&parent))
            {
                out.push(partition).await?;
            }
        }
        if scope.name_of(ObjectKind::Column).is_none() {
            let mut keys = sqlx::query(FOREIGN_KEYS_SQL)
                .bind(scope.name_of(ObjectKind::Schema))
                .bind(scope.relation())
                .fetch(&mut connection);
            while let Some(row) = keys.try_next().await.map_err(map_error)? {
                let table = db
                    .path
                    .child(
                        ObjectKind::Schema,
                        row.try_get::<String, _>("ns").map_err(map_error)?,
                    )
                    .child(
                        ObjectKind::Table,
                        row.try_get::<String, _>("rel").map_err(map_error)?,
                    );
                if !seen.contains(&table) {
                    continue;
                }
                let mut key = MetadataObject::new(
                    ObjectKind::ForeignKey,
                    &table,
                    row.try_get::<String, _>("name").map_err(map_error)?,
                );
                key.references = Some(ForeignKeyTarget {
                    relation: db
                        .path
                        .child(
                            ObjectKind::Schema,
                            row.try_get::<String, _>("refns").map_err(map_error)?,
                        )
                        .child(
                            ObjectKind::Table,
                            row.try_get::<String, _>("refrel").map_err(map_error)?,
                        ),
                    columns: row.try_get("cols").map_err(map_error)?,
                    referenced: row.try_get("refcols").map_err(map_error)?,
                });
                out.push(key).await?;
            }
        }
        out.flush().await?;
        Ok(IntrospectionReport::default())
    })
    .await
}

/// The oid of the relation at `path`, in the connection's database.
async fn relation_oid(connection: &mut PgConnection, path: &ObjectPath) -> DriverResult<u32> {
    let (Some(schema), Some(relation)) = (
        path.name_of(ObjectKind::Schema),
        path.relation().or(path.name_of(ObjectKind::Partition)),
    ) else {
        return Err(DriverError::validation("Choose a table or a view"));
    };
    let oid: Option<sqlx::postgres::types::Oid> = sqlx::query_scalar(
        "SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2",
    )
    .bind(schema)
    .bind(relation)
    .fetch_optional(&mut *connection)
    .await
    .map_err(map_error)?;
    oid.map(|oid| oid.0)
        .ok_or_else(|| DriverError::new(ErrorCode::NotFound, "The object no longer exists"))
}

/// Indexes, constraints and triggers of a relation; routines, sequences and
/// types of a schema.
pub async fn inspect(
    input: &ResolvedConnection,
    path: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Inspection> {
    let configured = PostgresConfig::parse(&input.config.options)?.database;
    let database = path
        .name_of(ObjectKind::Database)
        .map_or(configured, str::to_owned);
    let input = database_input(input, &database)?;
    let mut connection = connect(&input, &cancel).await?;
    bounded(&cancel, Some(Duration::from_secs(60)), async {
        let mut items = Vec::new();
        let item = |kind, name: String, detail: Option<String>| InspectionItem {
            kind,
            name,
            detail: detail.unwrap_or_default(),
        };
        let relation = path.relation().is_some()
            || path.name_of(ObjectKind::Partition).is_some();
        if relation {
            let oid = relation_oid(&mut connection, path).await?;
            let oid = sqlx::postgres::types::Oid(oid);
            let indexes: Vec<(String, Option<String>, bool)> = sqlx::query_as(
                "SELECT i.relname::text, pg_get_indexdef(x.indexrelid), x.indisprimary FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = $1 ORDER BY i.relname",
            )
            .bind(oid)
            .fetch_all(&mut connection)
            .await
            .map_err(map_error)?;
            for (name, definition, primary) in indexes {
                // The primary key's index is listed with the constraint.
                if !primary {
                    items.push(item(InspectedKind::Index, name, definition));
                }
            }
            let constraints: Vec<(String, String, Option<String>)> = sqlx::query_as(
                "SELECT conname::text, contype::text, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = $1 ORDER BY contype, conname",
            )
            .bind(oid)
            .fetch_all(&mut connection)
            .await
            .map_err(map_error)?;
            for (name, kind, definition) in constraints {
                let kind = match kind.as_str() {
                    "p" => InspectedKind::PrimaryKey,
                    "u" => InspectedKind::Unique,
                    "c" => InspectedKind::Check,
                    "f" => InspectedKind::ForeignKey,
                    "x" => InspectedKind::Exclusion,
                    _ => continue,
                };
                items.push(item(kind, name, definition));
            }
            let triggers: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT tgname::text, pg_get_triggerdef(oid) FROM pg_trigger WHERE tgrelid = $1 AND NOT tgisinternal ORDER BY tgname",
            )
            .bind(oid)
            .fetch_all(&mut connection)
            .await
            .map_err(map_error)?;
            for (name, definition) in triggers {
                items.push(item(InspectedKind::Trigger, name, definition));
            }
            return Ok(Inspection { items });
        }
        let Some(schema) = path.name_of(ObjectKind::Schema) else {
            return Err(DriverError::validation("Choose a schema, a table or a view"));
        };
        let routines: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT p.proname::text, p.prokind::text, '(' || pg_get_function_identity_arguments(p.oid) || ')' || coalesce(' → ' || pg_get_function_result(p.oid), '') FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = $1 ORDER BY p.proname, 3",
        )
        .bind(schema)
        .fetch_all(&mut connection)
        .await
        .map_err(map_error)?;
        for (name, kind, signature) in routines {
            let kind = if kind == "p" {
                InspectedKind::Procedure
            } else {
                InspectedKind::Function
            };
            items.push(item(kind, name, signature));
        }
        let sequences: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT c.relname::text, format_type(s.seqtypid, NULL) || ', start ' || s.seqstart || ', increment ' || s.seqincrement FROM pg_sequence s JOIN pg_class c ON c.oid = s.seqrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 ORDER BY c.relname",
        )
        .bind(schema)
        .fetch_all(&mut connection)
        .await
        .map_err(map_error)?;
        for (name, detail) in sequences {
            items.push(item(InspectedKind::Sequence, name, detail));
        }
        let types: Vec<(String, Option<String>)> = sqlx::query_as(TYPES_SQL)
            .bind(schema)
            .fetch_all(&mut connection)
            .await
            .map_err(map_error)?;
        for (name, detail) in types {
            items.push(item(InspectedKind::Type, name, detail));
        }
        Ok(Inspection { items })
    })
    .await
}

/// The source of a view or materialized view, or a `CREATE TABLE` built from
/// the catalog for a table (PostgreSQL keeps no table DDL).
pub async fn definition(
    input: &ResolvedConnection,
    path: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<String> {
    let configured = PostgresConfig::parse(&input.config.options)?.database;
    let database = path
        .name_of(ObjectKind::Database)
        .map_or(configured, str::to_owned);
    let input = database_input(input, &database)?;
    let mut connection = connect(&input, &cancel).await?;
    bounded(&cancel, Some(Duration::from_secs(60)), async {
        let oid = sqlx::postgres::types::Oid(relation_oid(&mut connection, path).await?);
        let quoted = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
        let name = format!(
            "{}.{}",
            quoted(path.name_of(ObjectKind::Schema).unwrap_or_default()),
            quoted(path.0.last().map_or("", |part| part.name.as_str()))
        );
        let (kind, view): (String, Option<String>) = sqlx::query_as(
            "SELECT relkind::text, CASE WHEN relkind IN ('v','m') THEN pg_get_viewdef(oid, true) END FROM pg_class WHERE oid = $1",
        )
        .bind(oid)
        .fetch_one(&mut connection)
        .await
        .map_err(map_error)?;
        if let Some(view) = view {
            let keyword = if kind == "m" { "MATERIALIZED VIEW" } else { "VIEW" };
            return Ok(format!("CREATE {keyword} {name} AS\n{}", view.trim_end()));
        }
        let columns: Vec<(String, String, bool, Option<String>)> = sqlx::query_as(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
        )
        .bind(oid)
        .fetch_all(&mut connection)
        .await
        .map_err(map_error)?;
        let constraints: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT conname::text, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = $1 ORDER BY contype, conname",
        )
        .bind(oid)
        .fetch_all(&mut connection)
        .await
        .map_err(map_error)?;
        let mut lines: Vec<String> = columns
            .into_iter()
            .map(|(column, data_type, not_null, default)| {
                let mut line = format!("    {} {data_type}", quoted(&column));
                if let Some(default) = default {
                    line.push_str(&format!(" DEFAULT {default}"));
                }
                if not_null {
                    line.push_str(" NOT NULL");
                }
                line
            })
            .collect();
        lines.extend(constraints.into_iter().filter_map(|(constraint, definition)| {
            Some(format!("    CONSTRAINT {} {}", quoted(&constraint), definition?))
        }));
        Ok(format!("CREATE TABLE {name} (\n{}\n);", lines.join(",\n")))
    })
    .await
}

/// Collects objects inside the scope and hands them to the sink in
/// batches, so a large schema is not delivered one row at a time.
struct Batches<'a> {
    sink: &'a mut dyn IntrospectionSink,
    scope: &'a ObjectPath,
    pending: Vec<MetadataObject>,
}

impl<'a> Batches<'a> {
    const SIZE: usize = 500;

    fn new(sink: &'a mut dyn IntrospectionSink, scope: &'a ObjectPath) -> Self {
        Self {
            sink,
            scope,
            pending: Vec::new(),
        }
    }

    async fn push(&mut self, object: MetadataObject) -> DriverResult<()> {
        if object.path.is_within(self.scope) {
            self.pending.push(object);
        }
        if self.pending.len() >= Self::SIZE {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> DriverResult<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.sink.batch(std::mem::take(&mut self.pending)).await
    }
}

/// The root holds the configured database, the only one a session can use,
/// or every database that accepts connections when the profile offers them
/// all; a database holds the schemas the user can see, without the system,
/// TOAST and temporary ones, read through a connection to it.
pub async fn discover(
    input: &ResolvedConnection,
    parent: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Vec<MetadataObject>> {
    let config = PostgresConfig::parse(&input.config.options)?;
    match parent.0.as_slice() {
        [] if config.all_databases => {
            let mut connection = connect(input, &cancel).await?;
            bounded(&cancel, Some(Duration::from_secs(60)), async {
                let names: Vec<String> = sqlx::query_scalar(DATABASES_SQL)
                    .fetch_all(&mut connection)
                    .await
                    .map_err(map_error)?;
                Ok(names
                    .into_iter()
                    .map(|name| MetadataObject::new(ObjectKind::Database, parent, name))
                    .collect())
            })
            .await
        }
        [] => Ok(vec![MetadataObject::new(
            ObjectKind::Database,
            parent,
            config.database,
        )]),
        [only] if only.kind == ObjectKind::Database => {
            let input = database_input(input, &only.name)?;
            let mut connection = connect(&input, &cancel).await?;
            bounded(&cancel, Some(Duration::from_secs(60)), async {
                let names: Vec<String> = sqlx::query_scalar(SCHEMAS_SQL)
                    .fetch_all(&mut connection)
                    .await
                    .map_err(map_error)?;
                Ok(names
                    .into_iter()
                    .map(|name| MetadataObject::new(ObjectKind::Schema, parent, name))
                    .collect())
            })
            .await
        }
        [_, ..] => Ok(Vec::new()),
    }
}
/// The databases of the server that accept connections, without templates.
const DATABASES_SQL: &str = "SELECT datname::text FROM pg_database WHERE datallowconn AND NOT datistemplate AND has_database_privilege(datname, 'CONNECT') ORDER BY datname";
const SCHEMAS_SQL: &str = r#"SELECT schema_name::text FROM information_schema.schemata WHERE schema_name NOT IN ('pg_catalog','information_schema') AND schema_name NOT LIKE 'pg\_toast%' AND schema_name NOT LIKE 'pg\_temp\_%' ORDER BY schema_name"#;
/// Relations and their columns from the catalog, visible under the same
/// rule as `information_schema` (the owner's role or some privilege). A
/// partition is listed with its root table, in whatever schema the root is,
/// instead of its columns; reading the root's schema finds partitions kept
/// in others. `$1` schema, `$2` relation, `$3` column, each may be NULL;
/// `$4` reads names only, without columns.
const RELATIONS_SQL: &str = r#"SELECT n.nspname::text AS ns, c.relname::text AS rel, c.relkind::text AS relkind, rt.name AS root, rt.ns AS rootns, obj_description(c.oid, 'pg_class') AS relcomment, a.attname::text AS col, format_type(a.atttypid, a.atttypmod) AS dtype, a.attnotnull AS notnull, a.attnum::int4 AS pos, pg_get_expr(d.adbin, d.adrelid) AS dflt, col_description(c.oid, a.attnum) AS colcomment, array_position(pk.conkey, a.attnum)::int4 AS pk
FROM pg_namespace n
LEFT JOIN pg_class c ON c.relnamespace = n.oid AND c.relkind IN ('r','p','v','m','f')
    AND (pg_has_role(c.relowner, 'USAGE') OR has_table_privilege(c.oid, 'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER') OR has_any_column_privilege(c.oid, 'SELECT, INSERT, UPDATE, REFERENCES'))
LEFT JOIN LATERAL (SELECT r.relname::text AS name, rn.nspname::text AS ns FROM pg_class r JOIN pg_namespace rn ON rn.oid = r.relnamespace WHERE c.relispartition AND r.oid = pg_partition_root(c.oid) AND r.oid <> c.oid) rt ON true
LEFT JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped AND rt.name IS NULL AND NOT $4::bool AND ($3::text IS NULL OR a.attname = $3)
LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
LEFT JOIN pg_constraint pk ON pk.conrelid = c.oid AND pk.contype = 'p'
WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg\_toast%' AND n.nspname NOT LIKE 'pg\_temp\_%'
    AND (pg_has_role(n.nspowner, 'USAGE') OR has_schema_privilege(n.oid, 'CREATE, USAGE'))
    AND ($1::text IS NULL OR n.nspname = $1 OR rt.ns = $1)
    AND ($2::text IS NULL OR c.relname = $2 OR rt.name = $2)
ORDER BY n.nspname, c.relname, a.attnum"#;
/// Whether the user may use schema `$1`; no row when it does not exist.
const SCHEMA_USABLE_SQL: &str = "SELECT pg_has_role(nspowner, 'USAGE') OR has_schema_privilege(oid, 'USAGE') FROM pg_namespace WHERE nspname = $1";
/// Enums (with their labels), domains and composite types of schema `$1`.
const TYPES_SQL: &str = r#"SELECT t.typname::text, CASE t.typtype WHEN 'e' THEN 'enum: ' || (SELECT string_agg(e.enumlabel, ', ' ORDER BY e.enumsortorder) FROM pg_enum e WHERE e.enumtypid = t.oid) WHEN 'd' THEN 'domain over ' || format_type(t.typbasetype, t.typtypmod) ELSE 'composite' END
FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE n.nspname = $1 AND (t.typtype IN ('e', 'd') OR (t.typtype = 'c' AND EXISTS (SELECT 1 FROM pg_class r WHERE r.oid = t.typrelid AND r.relkind = 'c')))
ORDER BY t.typname"#;
/// Foreign keys of tables (partitions inherit their root's), with their
/// columns in key order. `$1` schema, `$2` relation; each may be NULL.
const FOREIGN_KEYS_SQL: &str = r#"SELECT n.nspname::text AS ns, c.relname::text AS rel, k.conname::text AS name, rn.nspname::text AS refns, rc.relname::text AS refrel,
    ARRAY(SELECT a.attname::text FROM unnest(k.conkey) WITH ORDINALITY u(num, ord) JOIN pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = u.num ORDER BY u.ord) AS cols,
    ARRAY(SELECT a.attname::text FROM unnest(k.confkey) WITH ORDINALITY u(num, ord) JOIN pg_attribute a ON a.attrelid = k.confrelid AND a.attnum = u.num ORDER BY u.ord) AS refcols
FROM pg_constraint k
JOIN pg_class c ON c.oid = k.conrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_class rc ON rc.oid = k.confrelid
JOIN pg_namespace rn ON rn.oid = rc.relnamespace
WHERE k.contype = 'f' AND NOT c.relispartition
    AND ($1::text IS NULL OR n.nspname = $1)
    AND ($2::text IS NULL OR c.relname = $2)
ORDER BY n.nspname, c.relname, k.conname"#;

#[cfg(test)]
mod tests {
    use super::{
        EVENT_BUFFER, Limit, SnapshotEffect, parameter_reference, parameter_settings, read_page,
        snapshot_effect,
    };
    use futures_util::Stream as _;
    use jykre_core::{
        CancellationToken, ErrorCode, ExecutionOptions, ParameterKind, QueryParameter, QueryRequest,
    };
    use sqlx::postgres::{PgQueryResult, PgRow};
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn marked(sql: &str, name: &str, kind: ParameterKind, value: Option<&str>) -> QueryParameter {
        QueryParameter {
            name: name.into(),
            kind,
            value: value.map(Into::into),
            markers: sql
                .match_indices(&format!(":{name}"))
                .map(|(start, found)| start..start + found.len())
                .collect(),
        }
    }

    #[test]
    fn markers_become_reads_of_bound_settings_and_values_stay_out_of_the_sql() {
        let sql =
            "SELECT * FROM t WHERE name = :who AND total > :min AND day = :day AND :who <> ''";
        let request = QueryRequest {
            parameters: vec![
                marked(
                    sql,
                    "who",
                    ParameterKind::Text,
                    Some("x'; DROP TABLE t; --"),
                ),
                marked(sql, "min", ParameterKind::Number, None),
                marked(sql, "day", ParameterKind::Date, Some("2024-02-29")),
            ],
            ..QueryRequest::new(sql, ExecutionOptions::default())
        };
        assert_eq!(request.validate(), Ok(()));
        let bound = request.with_references(parameter_reference);
        assert_eq!(
            bound,
            "SELECT * FROM t WHERE name = (current_setting('jykre.p_0')::text) \
             AND total > CAST(NULL AS numeric) AND day = (current_setting('jykre.p_2')::date) \
             AND (current_setting('jykre.p_0')::text) <> ''"
        );
        assert!(!bound.contains("DROP"));
        let (names, values) = parameter_settings(&request.parameters);
        assert_eq!(names, ["jykre.p_0", "jykre.p_2"]);
        assert_eq!(values, ["x'; DROP TABLE t; --", "2024-02-29"]);
    }

    #[test]
    fn statements_that_open_keep_or_take_the_first_snapshot() {
        for sql in [
            "BEGIN",
            "begin;",
            "START TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            // Ending a transaction AND CHAIN opens the next one at once.
            "COMMIT AND CHAIN",
            "end work and chain;",
            "ROLLBACK TRANSACTION AND CHAIN",
            "abort and chain",
        ] {
            assert_eq!(snapshot_effect(sql), SnapshotEffect::Opens, "{sql}");
        }
        for sql in [
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            "-- isolation\nset search_path to a",
            "/* a /* nested */ b */ SHOW search_path",
            "SAVEPOINT a",
            "RELEASE SAVEPOINT a",
            "ROLLBACK TO SAVEPOINT a",
            // A savepoint named like the keyword is still a name.
            "rollback work to chain",
        ] {
            assert_eq!(snapshot_effect(sql), SnapshotEffect::Keeps, "{sql}");
        }
        for sql in [
            "SELECT 1",
            "  insert into t values (1)",
            "",
            "-- only",
            "/* open",
            "COMMIT",
            "COMMIT AND NO CHAIN",
            "ROLLBACK",
        ] {
            assert_eq!(snapshot_effect(sql), SnapshotEffect::Takes, "{sql}");
        }
    }

    /// While a result arrives faster than it is read, SQLx's reads never
    /// wait, so the session's task keeps its thread: a cancellation's wake-up
    /// is not seen, and no worker turns Tokio's timer. A stream that is
    /// always ready does the same here, and blocking the runtime's only
    /// thread lets the deadline pass without the timer.
    #[tokio::test]
    async fn a_page_that_never_waits_still_stops_at_its_limit() {
        let results = |cancel_at_ten: Option<CancellationToken>| {
            futures_util::stream::iter((0..10_000).map(move |index| {
                if index == 10
                    && let Some(cancel) = &cancel_at_ten
                {
                    cancel.cancel();
                }
                Ok::<_, sqlx::Error>(sqlx::Either::<_, PgRow>::Left(PgQueryResult::default()))
            }))
        };
        let (events, _receiver) = mpsc::channel(EVENT_BUFFER);

        let cancel = CancellationToken::new();
        let limit = Limit::new(&cancel, None);
        let mut stream = results(Some(cancel.clone()));
        let page = limit
            .bound(read_page(
                &mut stream,
                &mut None,
                &mut None,
                1_000,
                &events,
                &limit,
            ))
            .await;
        assert_eq!(
            page.err().map(|error| error.code),
            Some(ErrorCode::Cancelled)
        );
        assert!(
            stream.size_hint().0 > 9_900,
            "read on after the cancellation"
        );

        let cancel = CancellationToken::new();
        let limit = Limit::new(&cancel, Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(20));
        let mut stream = results(None);
        let page = limit
            .bound(read_page(
                &mut stream,
                &mut None,
                &mut None,
                1_000,
                &events,
                &limit,
            ))
            .await;
        assert_eq!(page.err().map(|error| error.code), Some(ErrorCode::Timeout));
    }
}
