use crate::MySqlConfig;
use futures_util::TryStreamExt;
use jykre_core::*;
use secrecy::ExposeSecret;
use sqlx::mysql::{MySqlConnectOptions, MySqlConnection, MySqlSslMode};
use sqlx::{
    Column, Connection, Executor, Row as SqlRow, SqlSafeStr, Statement as _, TypeInfo, ValueRef,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{collections::HashSet, future::Future, time::Duration};
use tokio::sync::{mpsc, oneshot};

/// Classifies an SQLx error. A database error keeps the server's own message
/// and its error number and SQLSTATE; errors from the connection keep the
/// operating system's reason. Anything else (such as configuration, which may
/// hold the connection URL) gets a fixed message.
fn map_error(error: sqlx::Error) -> DriverError {
    if matches!(&error, sqlx::Error::Protocol(message) if message.contains("RSA auth backend disabled"))
    {
        return DriverError::new(
            ErrorCode::Authentication,
            "This MySQL authentication method requires TLS; select required or verify-identity",
        );
    }
    match &error {
        sqlx::Error::Database(db) => {
            // SQLx reports the SQLSTATE as the code; the MySQL error number is
            // the precise one (1045 access denied, 1142 table access denied).
            let number = db
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .map(|my| my.number());
            let code = match (number, db.code().as_deref()) {
                (Some(1045 | 1698), _) | (_, Some("28000")) => ErrorCode::Authentication,
                (Some(1044 | 1142 | 1143 | 1227 | 1370), _) | (_, Some("42501")) => {
                    ErrorCode::PermissionDenied
                }
                _ => ErrorCode::Database,
            };
            DriverError::from_server(
                code,
                db.message().to_owned(),
                ServerDiagnostic {
                    sqlstate: db.code().map(|state| state.into_owned()),
                    vendor_code: number.map(|number| number.to_string()),
                    position: None,
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
/// How long a session may take to cancel a statement on the server, or to
/// read what is left of a result, before it is closed.
const RECOVERY: Duration = Duration::from_secs(5);
/// Reads what is left of the previous response, until the server answers a
/// ping. SQLx reports an error packet found on the way (such as the one a
/// killed statement ends with) as the ping's error and keeps its state
/// consistent, so database errors are skipped; any other error means the
/// protocol cannot be trusted.
async fn resynchronize(connection: &mut MySqlConnection) -> Result<(), sqlx::Error> {
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
    tokio::select! { biased;
    _ = cancel.cancelled() => Err(DriverError::cancelled()),
    result = async { match timeout { Some(limit) => tokio::time::timeout(limit, work).await.map_err(|_| DriverError::new(ErrorCode::Timeout,"Database operation timed out"))?, None => work.await } } => result }
}
async fn connect(
    input: &ResolvedConnection,
    cancel: &CancellationToken,
) -> DriverResult<MySqlConnection> {
    let config = MySqlConfig::parse(&input.config.options)?;
    let mut options = MySqlConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.user)
        .ssl_mode(match config.ssl_mode {
            crate::MySqlSslMode::Disabled => MySqlSslMode::Disabled,
            crate::MySqlSslMode::Preferred => MySqlSslMode::Preferred,
            crate::MySqlSslMode::Required => MySqlSslMode::Required,
            crate::MySqlSslMode::VerifyCa => MySqlSslMode::VerifyCa,
            crate::MySqlSslMode::VerifyIdentity => MySqlSslMode::VerifyIdentity,
        });
    if let Some(database) = &config.database {
        options = options.database(database);
    }
    if let Some(password) = &input.credentials.password {
        options = options.password(password.expose_secret());
    }
    use sqlx::ConnectOptions;
    options = options.disable_statement_logging();
    bounded(cancel, Some(Duration::from_secs(15)), async {
        MySqlConnection::connect_with(&options)
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
    let backend = bounded(&cancel, Some(Duration::from_secs(15)), async {
        Ok(sqlx::query_scalar::<_, u64>("SELECT CONNECTION_ID()")
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
        open: open.clone(),
        database: None,
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
    Close {
        reply: oneshot::Sender<DriverResult<()>>,
    },
}

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
    async fn close(self: Box<Self>) -> DriverResult<()> {
        if !self.is_open() {
            return Ok(());
        }
        self.ask(|reply| Command::Close { reply }).await
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
    open: Arc<AtomicBool>,
    /// The database last chosen with `USE` by a request, or reported by
    /// [`Session::context`]: a request for it is not applied again.
    database: Option<String>,
}

impl Server {
    async fn serve(
        mut self,
        mut connection: MySqlConnection,
        mut commands: mpsc::Receiver<Command>,
    ) {
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
                Command::Context { reply } => {
                    // The text protocol, like the statements the user runs:
                    // a prepared statement may answer with the database the
                    // connection was described with.
                    let context = tokio::time::timeout(
                        RECOVERY,
                        sqlx::raw_sql("SELECT DATABASE()").fetch_one(&mut connection),
                    )
                    .await
                    .map(|row| {
                        row.and_then(|row| sqlx::Row::try_get::<Option<String>, _>(&row, 0))
                    });
                    match context {
                        Ok(Ok(database)) => {
                            self.database.clone_from(&database);
                            let _ = reply.send(Ok(SessionContext {
                                catalog: None,
                                schema: database,
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
                    // SQLx's hard close still shuts down TLS. A server may keep
                    // that handshake open; dropping the timed-out future closes
                    // the socket.
                    let _ =
                        tokio::time::timeout(Duration::from_secs(1), connection.close_hard()).await;
                    return;
                }
            }
        }
        self.open.store(false, Ordering::SeqCst);
        let _ = tokio::time::timeout(RECOVERY, connection.close()).await;
    }

    /// Runs one statement and serves its pages until the result ends or
    /// another command arrives.
    async fn execute(
        &mut self,
        connection: &mut MySqlConnection,
        request: QueryRequest,
        mut events: mpsc::Sender<Event>,
        mut cancel: CancellationToken,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Flow {
        let mut options = request.options.clone();
        // The chosen database is applied only when it changed, so a USE the
        // user ran meanwhile stays in force.
        let apply = options
            .context
            .schema
            .clone()
            .or(options.context.catalog.clone())
            .filter(|database| self.database.as_ref() != Some(database));
        if let Some(database) = &apply {
            let sql = format!("USE `{}`", database.replace('`', "``"));
            let applied = bounded(&cancel, options.timeout, async {
                sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                    .execute(&mut *connection)
                    .await
                    .map_err(map_error)
                    .map(|_| ())
            })
            .await;
            if let Err(error) = applied {
                return self.fail(connection, error, &events).await;
            }
            self.database = apply;
        }
        let columns = bounded(&cancel, options.timeout, async {
            let description = connection
                .prepare(sqlx::AssertSqlSafe(request.sql.as_str()).into_sql_str())
                .await
                .map_err(map_error)?;
            Ok(description
                .columns()
                .iter()
                .map(|c| ColumnInfo {
                    name: c.name().into(),
                    data_type: c.type_info().name().into(),
                    nullable: None,
                })
                .collect::<Vec<_>>())
        })
        .await;
        // Some statements (USE, and others MySQL cannot prepare: error 1295)
        // run as text only; their columns, if any, come with their rows.
        let mut describe = Describe::Prepared;
        let columns = match columns {
            Ok(columns) => columns,
            Err(error)
                if error
                    .server
                    .as_ref()
                    .and_then(|server| server.vendor_code.as_deref())
                    == Some("1295") =>
            {
                describe = Describe::FromRows;
                Vec::new()
            }
            Err(error) => return self.fail(connection, error, &events).await,
        };
        let mut returns_rows = !columns.is_empty();
        if returns_rows && events.send(Event::Columns(columns)).await.is_err() {
            return self.recover(connection, false).await;
        }
        let mut stream =
            sqlx::raw_sql(sqlx::AssertSqlSafe(request.sql.as_str())).fetch_many(&mut *connection);
        let mut ahead = None;
        let mut affected = None;
        loop {
            let page = bounded(
                &cancel,
                options.timeout,
                read_page(
                    &mut stream,
                    &mut ahead,
                    &mut affected,
                    &mut describe,
                    options.max_rows,
                    &events,
                ),
            )
            .await;
            if describe == Describe::Sent {
                returns_rows = true;
            }
            match page {
                Ok(PageEnd::Finished) => {
                    drop(stream);
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
                        return self.recover(connection, false).await;
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
                            return match self.recover(connection, false).await {
                                Flow::Ready => Flow::Next(other),
                                flow => flow,
                            };
                        }
                        None => return Flow::Closed,
                    }
                }
                Ok(PageEnd::Gone) => {
                    drop(stream);
                    return self.recover(connection, false).await;
                }
                Err(error) => {
                    drop(stream);
                    return self.fail(connection, error, &events).await;
                }
            }
        }
    }

    /// Brings the protocol back to a known state after a statement was left
    /// unfinished, or reports that the connection must be closed. A result
    /// cut by the reader is drained first, since its statement may already be
    /// done; a statement that may still run, or a result too long to drain
    /// in time, is killed from another connection.
    async fn recover(&self, connection: &mut MySqlConnection, running: bool) -> Flow {
        if !running
            && let Ok(Ok(())) = tokio::time::timeout(RECOVERY, resynchronize(connection)).await
        {
            return Flow::Ready;
        }
        let kill = async {
            let mut control = connect(&self.input, &CancellationToken::new()).await?;
            let sql = format!("KILL QUERY {}", self.backend);
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                .execute(&mut control)
                .await
                .map_err(map_error)?;
            Ok::<_, DriverError>(())
        };
        let _ = tokio::time::timeout(RECOVERY, kill).await;
        match tokio::time::timeout(RECOVERY, resynchronize(connection)).await {
            Ok(Ok(())) => Flow::Ready,
            _ => Flow::Broken,
        }
    }

    /// Recovers after a failed statement and reports the error. A database
    /// error keeps the session; a lost connection closes it.
    async fn fail(
        &self,
        connection: &mut MySqlConnection,
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
/// Where a statement's columns come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Describe {
    /// Preparing the statement described them before it ran.
    Prepared,
    /// The statement could not be prepared; columns come with the first row.
    FromRows,
    /// They came with the first row and were sent.
    Sent,
}

async fn read_page<S>(
    stream: &mut S,
    ahead: &mut Option<Row>,
    affected: &mut Option<u64>,
    describe: &mut Describe,
    max_rows: u32,
    events: &mpsc::Sender<Event>,
) -> DriverResult<PageEnd>
where
    S: futures_util::Stream<
            Item = Result<
                sqlx::Either<sqlx::mysql::MySqlQueryResult, sqlx::mysql::MySqlRow>,
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
    while let Some(item) = stream.try_next().await.map_err(map_error)? {
        match item {
            sqlx::Either::Left(result) => *affected = Some(result.rows_affected()),
            sqlx::Either::Right(row) => {
                if *describe == Describe::FromRows {
                    let columns = row
                        .columns()
                        .iter()
                        .map(|column| ColumnInfo {
                            name: column.name().into(),
                            data_type: column.type_info().name().into(),
                            nullable: None,
                        })
                        .collect();
                    if events.send(Event::Columns(columns)).await.is_err() {
                        return Ok(PageEnd::Gone);
                    }
                    *describe = Describe::Sent;
                }
                let mut cells = Vec::with_capacity(row.len());
                for index in 0..row.len() {
                    let value = row.try_get_raw(index).map_err(map_error)?;
                    if value.is_null() {
                        cells.push(Cell::Null);
                        continue;
                    }
                    let bytes =
                        <Vec<u8> as sqlx::Decode<sqlx::MySql>>::decode(value).map_err(|_| {
                            DriverError::new(ErrorCode::Protocol, "Invalid database value")
                        })?;
                    let kind = row.columns()[index].type_info().name();
                    if matches!(
                        kind,
                        "BLOB"
                            | "TINYBLOB"
                            | "MEDIUMBLOB"
                            | "LONGBLOB"
                            | "BINARY"
                            | "VARBINARY"
                            | "BIT"
                    ) {
                        cells.push(Cell::Binary(bytes));
                    } else {
                        cells.push(Cell::Text(String::from_utf8(bytes).map_err(|_| {
                            DriverError::new(ErrorCode::Protocol, "Invalid database text encoding")
                        })?));
                    }
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
pub async fn introspect(
    input: &ResolvedConnection,
    scope: &ObjectPath,
    level: IntrospectionLevel,
    sink: &mut dyn IntrospectionSink,
    cancel: CancellationToken,
) -> DriverResult<IntrospectionReport> {
    let names_only = level == IntrospectionLevel::Names;
    let mut connection = connect(input, &cancel).await?;
    bounded(&cancel, Some(Duration::from_secs(300)), async {
        let mut seen = HashSet::new();
        let database = scope.name_of(ObjectKind::Database);
        let relation = scope.relation();
        let column = scope.name_of(ObjectKind::Column);
        let mut rows = sqlx::query(METADATA_SQL)
            .bind(names_only)
            .bind(database)
            .bind(database)
            .bind(relation)
            .bind(relation)
            .bind(column)
            .bind(column)
            .fetch(&mut connection);
        while let Some(row) = rows.try_next().await.map_err(map_error)? {
            let db = MetadataObject::new(
                ObjectKind::Database,
                &ObjectPath::default(),
                row.try_get::<String, _>("db").map_err(map_error)?,
            );
            let mut batch = Vec::new();
            if seen.insert(db.path.clone()) {
                batch.push(db.clone());
            }
            if let Some(table) = row.try_get::<Option<String>, _>("rel").map_err(map_error)? {
                let view = row
                    .try_get::<Option<String>, _>("reltype")
                    .map_err(map_error)?
                    .as_deref()
                    == Some("VIEW");
                let mut relation = MetadataObject::new(
                    if view {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    },
                    &db.path,
                    table,
                );
                if seen.insert(relation.path.clone()) {
                    // MySQL reports "VIEW" as the comment of every view.
                    relation.comment = row
                        .try_get::<Option<String>, _>("relcomment")
                        .map_err(map_error)?
                        .filter(|comment| !(view && comment == "VIEW"));
                    batch.push(relation.clone());
                }
                if let Some(name) = row.try_get::<Option<String>, _>("col").map_err(map_error)? {
                    let mut column = MetadataObject::new(ObjectKind::Column, &relation.path, name);
                    column.data_type = row.try_get("dtype").map_err(map_error)?;
                    column.nullable = row
                        .try_get::<Option<String>, _>("isnull")
                        .map_err(map_error)?
                        .map(|v| v == "YES");
                    column.ordinal = row
                        .try_get::<Option<i64>, _>("pos")
                        .map_err(map_error)?
                        .and_then(|v| u32::try_from(v).ok());
                    column.default_value = row.try_get("dflt").map_err(map_error)?;
                    column.comment = row.try_get("colcomment").map_err(map_error)?;
                    column.primary_key = row
                        .try_get::<Option<i64>, _>("pk")
                        .map_err(map_error)?
                        .and_then(|v| u32::try_from(v).ok());
                    batch.push(column);
                }
            }
            batch.retain(|object| object.path.is_within(scope));
            if !batch.is_empty() {
                sink.batch(batch).await?;
            }
        }
        drop(rows);
        // A chosen database that lists nothing is either gone or hidden by
        // privileges; only USE tells them apart.
        if let Some(database) = database.filter(|_| seen.is_empty()) {
            let sql = format!("USE `{}`", database.replace('`', "``"));
            if let Err(error) = sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                .execute(&mut connection)
                .await
                .map_err(map_error)
                && error.code == ErrorCode::PermissionDenied
            {
                return Ok(IntrospectionReport {
                    warnings: vec![IntrospectionWarning {
                        path: ObjectPath::default().child(ObjectKind::Database, database),
                        message: "No permission to use this database".into(),
                    }],
                });
            }
            return Ok(IntrospectionReport::default());
        }
        if names_only {
            return Ok(IntrospectionReport::default());
        }
        if column.is_none() {
            let mut keys = sqlx::query(FOREIGN_KEYS_SQL)
                .bind(database)
                .bind(database)
                .bind(relation)
                .bind(relation)
                .fetch(&mut connection);
            // One row per key column, in key order: consecutive rows of the
            // same constraint make one key.
            let mut current: Option<MetadataObject> = None;
            let mut batch = Vec::new();
            while let Some(row) = keys.try_next().await.map_err(map_error)? {
                let table = ObjectPath::default()
                    .child(
                        ObjectKind::Database,
                        row.try_get::<String, _>("db").map_err(map_error)?,
                    )
                    .child(
                        ObjectKind::Table,
                        row.try_get::<String, _>("rel").map_err(map_error)?,
                    );
                let path = table.child(
                    ObjectKind::ForeignKey,
                    row.try_get::<String, _>("name").map_err(map_error)?,
                );
                if current.as_ref().is_none_or(|key| key.path != path) {
                    batch.extend(current.take());
                    if !seen.contains(&table) {
                        continue;
                    }
                    let mut key = MetadataObject::new(
                        ObjectKind::ForeignKey,
                        &table,
                        path.0
                            .last()
                            .map(|part| part.name.clone())
                            .unwrap_or_default(),
                    );
                    key.references = Some(ForeignKeyTarget {
                        relation: ObjectPath::default()
                            .child(
                                ObjectKind::Database,
                                row.try_get::<String, _>("refdb").map_err(map_error)?,
                            )
                            .child(
                                ObjectKind::Table,
                                row.try_get::<String, _>("refrel").map_err(map_error)?,
                            ),
                        columns: Vec::new(),
                        referenced: Vec::new(),
                    });
                    current = Some(key);
                }
                if let Some(target) = current.as_mut().and_then(|key| key.references.as_mut()) {
                    target
                        .columns
                        .push(row.try_get::<String, _>("col").map_err(map_error)?);
                    target
                        .referenced
                        .push(row.try_get::<String, _>("refcol").map_err(map_error)?);
                }
            }
            batch.extend(current);
            batch.retain(|object| object.path.is_within(scope));
            if !batch.is_empty() {
                sink.batch(batch).await?;
            }
        }
        Ok(IntrospectionReport::default())
    })
    .await
}
/// Indexes, constraints and triggers of a table; routines of a database.
pub async fn inspect(
    input: &ResolvedConnection,
    path: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Inspection> {
    let Some(database) = path.name_of(ObjectKind::Database) else {
        return Err(DriverError::validation(
            "Choose a database, a table or a view",
        ));
    };
    let mut connection = connect(input, &cancel).await?;
    bounded(&cancel, Some(Duration::from_secs(60)), async {
        let mut items = Vec::new();
        let item = |kind, name: String, detail: Option<String>| InspectionItem {
            kind,
            name,
            detail: detail.unwrap_or_default(),
        };
        if let Some(table) = path.relation() {
            let indexes: Vec<(String, i64, Option<String>)> = sqlx::query_as(
                "SELECT index_name, CAST(MAX(non_unique) AS SIGNED), GROUP_CONCAT(column_name ORDER BY seq_in_index SEPARATOR ', ') FROM information_schema.statistics WHERE table_schema = ? AND table_name = ? AND index_name <> 'PRIMARY' GROUP BY index_name ORDER BY index_name",
            )
            .bind(database)
            .bind(table)
            .fetch_all(&mut connection)
            .await
            .map_err(map_error)?;
            for (name, non_unique, columns) in indexes {
                let detail = columns.map(|columns| {
                    format!("{}({columns})", if non_unique == 0 { "UNIQUE " } else { "" })
                });
                items.push(item(InspectedKind::Index, name, detail));
            }
            let constraints: Vec<(String, String, Option<String>)> = sqlx::query_as(
                "SELECT t.constraint_name, t.constraint_type, COALESCE(c.check_clause, (SELECT GROUP_CONCAT(k.column_name ORDER BY k.ordinal_position SEPARATOR ', ') FROM information_schema.key_column_usage k WHERE k.constraint_schema = t.constraint_schema AND k.table_name = t.table_name AND k.constraint_name = t.constraint_name)) FROM information_schema.table_constraints t LEFT JOIN information_schema.check_constraints c ON c.constraint_schema = t.constraint_schema AND c.constraint_name = t.constraint_name WHERE t.table_schema = ? AND t.table_name = ? ORDER BY t.constraint_type, t.constraint_name",
            )
            .bind(database)
            .bind(table)
            .fetch_all(&mut connection)
            .await
            .map_err(map_error)?;
            for (name, kind, detail) in constraints {
                let kind = match kind.as_str() {
                    "PRIMARY KEY" => InspectedKind::PrimaryKey,
                    "UNIQUE" => InspectedKind::Unique,
                    "CHECK" => InspectedKind::Check,
                    "FOREIGN KEY" => InspectedKind::ForeignKey,
                    _ => continue,
                };
                items.push(item(kind, name, detail));
            }
            let triggers: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
                "SELECT trigger_name, action_timing, event_manipulation, action_statement FROM information_schema.triggers WHERE event_object_schema = ? AND event_object_table = ? ORDER BY trigger_name",
            )
            .bind(database)
            .bind(table)
            .fetch_all(&mut connection)
            .await
            .map_err(map_error)?;
            for (name, timing, event, statement) in triggers {
                let detail = format!("{timing} {event}: {}", statement.unwrap_or_default());
                items.push(item(InspectedKind::Trigger, name, Some(detail)));
            }
            return Ok(Inspection { items });
        }
        let routines: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT r.routine_name, r.routine_type, (SELECT GROUP_CONCAT(TRIM(CONCAT(COALESCE(p.parameter_mode, ''), ' ', p.parameter_name, ' ', p.dtd_identifier)) ORDER BY p.ordinal_position SEPARATOR ', ') FROM information_schema.parameters p WHERE p.specific_schema = r.routine_schema AND p.specific_name = r.specific_name AND p.ordinal_position > 0), NULLIF(r.dtd_identifier, '') FROM information_schema.routines r WHERE r.routine_schema = ? ORDER BY r.routine_name",
        )
        .bind(database)
        .fetch_all(&mut connection)
        .await
        .map_err(map_error)?;
        for (name, kind, parameters, result) in routines {
            let kind = if kind == "PROCEDURE" {
                InspectedKind::Procedure
            } else {
                InspectedKind::Function
            };
            let mut detail = format!("({})", parameters.unwrap_or_default());
            if let Some(result) = result {
                detail.push_str(&format!(" → {result}"));
            }
            items.push(item(kind, name, Some(detail)));
        }
        Ok(Inspection { items })
    })
    .await
}

/// The `SHOW CREATE` text of a table or view.
pub async fn definition(
    input: &ResolvedConnection,
    path: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<String> {
    let (Some(database), Some(relation)) = (path.name_of(ObjectKind::Database), path.relation())
    else {
        return Err(DriverError::validation("Choose a table or a view"));
    };
    let keyword = if path.name_of(ObjectKind::View).is_some() {
        "VIEW"
    } else {
        "TABLE"
    };
    let quoted = |name: &str| format!("`{}`", name.replace('`', "``"));
    let sql = format!(
        "SHOW CREATE {keyword} {}.{}",
        quoted(database),
        quoted(relation)
    );
    let mut connection = connect(input, &cancel).await?;
    bounded(&cancel, Some(Duration::from_secs(60)), async {
        let row = sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_one(&mut connection)
            .await
            .map_err(map_error)?;
        // The second column holds the statement for tables and views alike.
        let value = row.try_get_raw(1).map_err(map_error)?;
        let bytes = <Vec<u8> as sqlx::Decode<sqlx::MySql>>::decode(value)
            .map_err(|_| DriverError::new(ErrorCode::Protocol, "Invalid database value"))?;
        String::from_utf8(bytes)
            .map_err(|_| DriverError::new(ErrorCode::Protocol, "Invalid database value"))
    })
    .await
}

/// The root holds the databases the user can see, without the system ones;
/// a database holds no further namespace.
pub async fn discover(
    input: &ResolvedConnection,
    parent: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Vec<MetadataObject>> {
    if !parent.0.is_empty() {
        return Ok(Vec::new());
    }
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
const DATABASES_SQL: &str = r#"SELECT schema_name AS db FROM information_schema.schemata WHERE schema_name NOT IN ('mysql','information_schema','performance_schema','sys') ORDER BY schema_name"#;
/// Databases, relations and columns, with defaults, comments and each
/// column's position in the primary key. The first placeholder reads names
/// only, without columns; pairs of placeholders then filter the database,
/// the relation and the column, each pair may be NULL.
const METADATA_SQL: &str = r#"SELECT s.schema_name AS db, t.table_name AS rel, t.table_type AS reltype, NULLIF(t.table_comment, '') AS relcomment, c.column_name AS col, c.column_type AS dtype, c.is_nullable AS isnull, CAST(c.ordinal_position AS SIGNED) AS pos, c.column_default AS dflt, NULLIF(c.column_comment, '') AS colcomment, CAST(k.ordinal_position AS SIGNED) AS pk
FROM information_schema.schemata s
LEFT JOIN information_schema.tables t ON t.table_schema = s.schema_name
LEFT JOIN information_schema.columns c ON c.table_schema = t.table_schema AND c.table_name = t.table_name AND NOT ?
LEFT JOIN information_schema.key_column_usage k ON k.table_schema = c.table_schema AND k.table_name = c.table_name AND k.column_name = c.column_name AND k.constraint_name = 'PRIMARY'
WHERE s.schema_name NOT IN ('mysql','information_schema','performance_schema','sys') AND (? IS NULL OR s.schema_name = ?) AND (? IS NULL OR t.table_name = ?) AND (? IS NULL OR c.column_name = ?)
ORDER BY s.schema_name, t.table_name, c.ordinal_position"#;
/// One row per column of each foreign key, in key order. Pairs of
/// placeholders filter the database and the table; each pair may be NULL.
const FOREIGN_KEYS_SQL: &str = r#"SELECT k.table_schema AS db, k.table_name AS rel, k.constraint_name AS name, k.referenced_table_schema AS refdb, k.referenced_table_name AS refrel, k.column_name AS col, k.referenced_column_name AS refcol
FROM information_schema.key_column_usage k
WHERE k.referenced_table_name IS NOT NULL AND k.table_schema NOT IN ('mysql','information_schema','performance_schema','sys') AND (? IS NULL OR k.table_schema = ?) AND (? IS NULL OR k.table_name = ?)
ORDER BY k.table_schema, k.table_name, k.constraint_name, k.ordinal_position"#;
