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

fn map_error(error: sqlx::Error) -> DriverError {
    let code = match &error {
        sqlx::Error::Database(db) => match db.code().as_deref() {
            Some("28P01" | "28000" | "1045") => ErrorCode::Authentication,
            Some("42501" | "1142" | "1044") => ErrorCode::PermissionDenied,
            _ => ErrorCode::Database,
        },
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) => ErrorCode::Connection,
        _ => ErrorCode::Protocol,
    };
    DriverError::new(
        code,
        "Database operation failed; check connectivity, permissions and SQL",
    )
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
    tokio::select! { biased;
    _ = cancel.cancelled() => Err(DriverError::cancelled()),
    result = async { match timeout { Some(limit) => tokio::time::timeout(limit, work).await.map_err(|_| DriverError::new(ErrorCode::Timeout,"Database operation timed out"))?, None => work.await } } => result }
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
    let backend = bounded(&cancel, Some(Duration::from_secs(15)), async {
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
    database: String,
    open: Arc<AtomicBool>,
}

impl Server {
    async fn serve(self, mut connection: PgConnection, mut commands: mpsc::Receiver<Command>) {
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

    /// Runs one statement and serves its pages until the result ends or
    /// another command arrives.
    async fn execute(
        &self,
        connection: &mut PgConnection,
        request: QueryRequest,
        mut events: mpsc::Sender<Event>,
        mut cancel: CancellationToken,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Flow {
        let mut options = request.options.clone();
        let columns = bounded(&cancel, options.timeout, async {
            if options
                .context
                .catalog
                .as_ref()
                .is_some_and(|v| v != &self.database)
            {
                return Err(DriverError::validation(
                    "PostgreSQL catalog cannot change within a session",
                ));
            }
            if let Some(schema) = &options.context.schema {
                let sql = format!("SET search_path TO \"{}\"", schema.replace('"', "\"\""));
                sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                    .execute(&mut *connection)
                    .await
                    .map_err(map_error)?;
            }
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
        let columns = match columns {
            Ok(columns) => columns,
            Err(error) => return self.fail(connection, error, &events).await,
        };
        let returns_rows = !columns.is_empty();
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
                    options.max_rows,
                    &events,
                ),
            )
            .await;
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
async fn read_page<S>(
    stream: &mut S,
    ahead: &mut Option<Row>,
    affected: &mut Option<u64>,
    max_rows: u32,
    events: &mpsc::Sender<Event>,
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
    while let Some(item) = stream.try_next().await.map_err(map_error)? {
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
pub async fn introspect(
    input: &ResolvedConnection,
    scope: &ObjectPath,
    sink: &mut dyn IntrospectionSink,
    cancel: CancellationToken,
) -> DriverResult<IntrospectionReport> {
    if scope
        .name_of(ObjectKind::Database)
        .is_some_and(|name| input.config.options["database"].as_str() != Some(name))
    {
        return Err(DriverError::validation(
            "Metadata scope belongs to another database",
        ));
    }
    let mut connection = connect(input, &cancel).await?;
    bounded(&cancel, Some(Duration::from_secs(300)), async {
        let mut seen = HashSet::new();
        let mut rows = sqlx::query(METADATA_SQL)
            .bind(scope.name_of(ObjectKind::Schema))
            .bind(
                scope
                    .name_of(ObjectKind::Table)
                    .or(scope.name_of(ObjectKind::View)),
            )
            .bind(scope.name_of(ObjectKind::Column))
            .fetch(&mut connection);
        while let Some(row) = rows.try_next().await.map_err(map_error)? {
            let database: String = row.try_get("db").map_err(map_error)?;
            let schema: String = row.try_get("ns").map_err(map_error)?;
            let table: Option<String> = row.try_get("rel").map_err(map_error)?;
            let root = ObjectPath::default();
            let db = MetadataObject::new(ObjectKind::Database, &root, database);
            let mut batch = Vec::new();
            if seen.insert(db.path.clone()) {
                batch.push(db.clone());
            }
            let parent = {
                let object = MetadataObject::new(ObjectKind::Schema, &db.path, schema);
                if seen.insert(object.path.clone()) {
                    batch.push(object.clone());
                }
                object.path
            };
            if let Some(table) = table {
                let relation_type: Option<String> = row.try_get("reltype").map_err(map_error)?;
                let relation = MetadataObject::new(
                    if relation_type.as_deref() == Some("VIEW") {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    },
                    &parent,
                    table,
                );
                if seen.insert(relation.path.clone()) {
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
                    batch.push(column);
                }
            }
            batch.retain(|object| object.path.is_within(scope));
            if !batch.is_empty() {
                sink.batch(batch).await?;
            }
        }
        Ok(IntrospectionReport::default())
    })
    .await
}
const METADATA_SQL: &str = r#"SELECT current_database() AS db,s.schema_name AS ns,t.table_name AS rel,t.table_type AS reltype,c.column_name AS col,c.data_type AS dtype,c.is_nullable AS isnull,c.ordinal_position::bigint AS pos FROM information_schema.schemata s LEFT JOIN information_schema.tables t ON t.table_schema=s.schema_name LEFT JOIN information_schema.columns c ON c.table_schema=t.table_schema AND c.table_name=t.table_name WHERE s.schema_name NOT IN ('pg_catalog','information_schema') AND ($1::text IS NULL OR s.schema_name=$1) AND ($2::text IS NULL OR t.table_name=$2) AND ($3::text IS NULL OR c.column_name=$3) ORDER BY s.schema_name,t.table_name,c.ordinal_position"#;
