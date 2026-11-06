use crate::{MAIN_SCHEMA, SqliteConfig};
use futures_util::TryStreamExt;
use jykre_core::*;
use sqlx::sqlite::{
    SqliteArguments, SqliteColumn, SqliteConnectOptions, SqliteConnection, SqliteQueryResult,
    SqliteRow,
};
use sqlx::{
    Column, ConnectOptions, Connection, Executor, Row as SqlRow, SqlSafeStr, Statement as _,
    TypeInfo, ValueRef,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{future::Future, time::Duration};
use tokio::sync::{mpsc, oneshot};

/// Opening the file and reading its header.
const OPEN: Duration = Duration::from_secs(15);
/// How long a session may take to stop a statement and be idle again before
/// it is closed.
const RECOVERY: Duration = Duration::from_secs(5);
/// How long a statement waits for a lock another connection holds. Shorter
/// than [`RECOVERY`], since SQLite's busy wait does not see an interrupt.
const BUSY: Duration = Duration::from_secs(4);
const INTROSPECTION: Duration = Duration::from_secs(300);
const INSPECTION: Duration = Duration::from_secs(60);
/// Virtual machine instructions between two checks of the interrupt flag.
const PROGRESS_STEPS: i32 = 1_000;
/// Rows sent from the connection's task to the session handle. A bounded
/// channel paces the reading; dropping the receiver asks the task to stop.
const EVENT_BUFFER: usize = 256;

// SQLite's primary result codes (the low byte of an extended code).
const SQLITE_PERM: i32 = 3;
const SQLITE_READONLY: i32 = 8;
const SQLITE_INTERRUPT: i32 = 9;
const SQLITE_CANTOPEN: i32 = 14;
const SQLITE_AUTH: i32 = 23;

/// Classifies an SQLx error. A database error keeps SQLite's own message
/// and its extended result code; anything else gets a fixed message, since
/// it may name the file.
fn map_error(error: sqlx::Error) -> DriverError {
    match &error {
        sqlx::Error::Database(db) => {
            let extended = db.code().and_then(|code| code.parse::<i32>().ok());
            let code = match extended.map(|code| code & 0xff) {
                Some(SQLITE_PERM | SQLITE_READONLY | SQLITE_AUTH) => ErrorCode::PermissionDenied,
                Some(SQLITE_CANTOPEN) => ErrorCode::NotFound,
                Some(SQLITE_INTERRUPT) => ErrorCode::Cancelled,
                _ => ErrorCode::Database,
            };
            DriverError::from_server(
                code,
                db.message().to_owned(),
                ServerDiagnostic {
                    sqlstate: None,
                    vendor_code: extended.map(|code| code.to_string()),
                    position: None,
                },
            )
        }
        sqlx::Error::Io(_) | sqlx::Error::WorkerCrashed => DriverError::new(
            ErrorCode::Connection,
            "The SQLite connection stopped; open the session again",
        ),
        _ => DriverError::new(
            ErrorCode::Protocol,
            "Database operation failed; check the file, its permissions and the SQL",
        ),
    }
}

fn timed_out() -> DriverError {
    DriverError::new(ErrorCode::Timeout, "Database operation timed out")
}

async fn bounded<T>(
    cancel: &CancellationToken,
    timeout: Option<Duration>,
    work: impl Future<Output = DriverResult<T>>,
) -> DriverResult<T> {
    tokio::select! { biased;
        _ = cancel.cancelled() => Err(DriverError::cancelled()),
        result = async {
            match timeout {
                Some(limit) => tokio::time::timeout(limit, work).await.map_err(|_| timed_out())?,
                None => work.await,
            }
        } => result,
    }
}

/// Opens the configured file, never creating it, and reads its header, so a
/// file that is not a database fails here rather than at the first query.
async fn connect(
    input: &ResolvedConnection,
    cancel: &CancellationToken,
) -> DriverResult<SqliteConnection> {
    let config = SqliteConfig::parse(&input.config.options)?;
    let options = SqliteConnectOptions::new()
        .filename(&config.path)
        .read_only(config.read_only)
        .create_if_missing(false)
        .busy_timeout(BUSY)
        .statement_cache_capacity(16)
        .disable_statement_logging();
    bounded(cancel, Some(OPEN), async {
        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .map_err(|error| match map_error(error) {
                error if error.code == ErrorCode::NotFound => DriverError::new(
                    ErrorCode::NotFound,
                    "Could not open the database file; check that it exists and that you may read it",
                ),
                error => error,
            })?;
        sqlx::raw_sql("SELECT count(*) FROM sqlite_schema")
            .execute(&mut connection)
            .await
            .map_err(map_error)?;
        Ok(connection)
    })
    .await
}

pub async fn test(
    input: &ResolvedConnection,
    cancel: CancellationToken,
) -> DriverResult<ServerInfo> {
    let mut connection = connect(input, &cancel).await?;
    let version = bounded(&cancel, Some(OPEN), async {
        sqlx::query_scalar::<_, String>("SELECT sqlite_version()")
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
    // SQLite stops a running statement when the progress handler returns
    // false: that is how a session is cancelled or times out.
    let interrupt = Arc::new(AtomicBool::new(false));
    let flag = interrupt.clone();
    bounded(&cancel, Some(OPEN), async {
        let mut handle = connection.lock_handle().await.map_err(map_error)?;
        handle.set_progress_handler(PROGRESS_STEPS, move || !flag.load(Ordering::SeqCst));
        Ok(())
    })
    .await?;
    let (commands, receiver) = mpsc::channel(4);
    let open = Arc::new(AtomicBool::new(true));
    let server = Server {
        open: open.clone(),
        interrupt,
    };
    tokio::spawn(server.serve(connection, receiver));
    Ok(Box::new(DatabaseSession { commands, open }))
}

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

    /// Unqualified names resolve in `temp`, `main` and then the attached
    /// databases, in that order, and no statement changes it: `main` is
    /// where a session's names are.
    async fn context(&mut self, cancel: CancellationToken) -> DriverResult<SessionContext> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        Ok(SessionContext {
            catalog: None,
            schema: Some(MAIN_SCHEMA.into()),
        })
    }

    // The transaction state (`sqlite3_get_autocommit`) is not exposed by
    // SQLx without unsafe code, so the default `None` leaves it to the
    // caller, which infers it from the statements it ran.

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
    /// The connection could not be brought back to idle; close it.
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

/// What a session's connection needs after a statement was left unfinished.
#[derive(Clone, Copy, Debug)]
enum Unfinished {
    /// The statement ended or may be about to: dropping its rows is enough.
    Cut,
    /// The statement may still run: it is interrupted.
    Running,
    /// The connection itself failed.
    Lost,
}

/// Why a statement stopped, and what its connection needs.
type Stop = (DriverError, Unfinished);

/// A statement stopped by `error`. A database error ends the statement (its
/// rows end with the error); a cancellation or a timeout may leave it
/// running.
fn stop(error: DriverError) -> Stop {
    let unfinished = match error.code {
        ErrorCode::Connection => Unfinished::Lost,
        ErrorCode::Cancelled | ErrorCode::Timeout | ErrorCode::Protocol => Unfinished::Running,
        _ => Unfinished::Cut,
    };
    (error, unfinished)
}

struct Server {
    open: Arc<AtomicBool>,
    /// Read by the connection's progress handler: while set, the running
    /// statement is interrupted. Cleared only once the connection is idle,
    /// so it never stops the next statement.
    interrupt: Arc<AtomicBool>,
}

impl Server {
    async fn serve(self, mut connection: SqliteConnection, mut commands: mpsc::Receiver<Command>) {
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
                    // Nobody may be listening any more; nothing is open.
                    let _ = events
                        .send(Event::Done(Err(DriverError::validation(
                            "No result has more rows to read",
                        ))))
                        .await;
                    Flow::Ready
                }
                Command::Ping { reply } => {
                    match tokio::time::timeout(RECOVERY, connection.ping()).await {
                        Ok(Ok(())) => {
                            // The handle may have stopped waiting.
                            let _ = reply.send(Ok(()));
                            Flow::Ready
                        }
                        outcome => {
                            self.open.store(false, Ordering::SeqCst);
                            let _ = reply.send(Err(match outcome {
                                Ok(Err(error)) => map_error(error),
                                _ => timed_out(),
                            }));
                            Flow::Broken
                        }
                    }
                }
                Command::Close { reply } => {
                    self.open.store(false, Ordering::SeqCst);
                    let closed = match tokio::time::timeout(RECOVERY, connection.close()).await {
                        Ok(closed) => closed.map_err(map_error),
                        Err(_) => Err(timed_out()),
                    };
                    let _ = reply.send(closed);
                    return;
                }
            };
            match flow {
                Flow::Ready => {}
                Flow::Next(command) => next = Some(command),
                Flow::Closed => break,
                Flow::Broken => {
                    self.open.store(false, Ordering::SeqCst);
                    // Dropping the connection stops its worker thread once
                    // the statement it is in ends; nothing is left to report.
                    let _ = connection.close_hard().await;
                    return;
                }
            }
        }
        self.open.store(false, Ordering::SeqCst);
        // The handle is gone, so nobody hears about a failed close.
        let _ = tokio::time::timeout(RECOVERY, connection.close()).await;
    }

    /// Runs one statement and serves its pages until the result ends or
    /// another command arrives.
    async fn execute(
        &self,
        connection: &mut SqliteConnection,
        request: QueryRequest,
        mut events: mpsc::Sender<Event>,
        mut cancel: CancellationToken,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Flow {
        let mut options = request.options.clone();
        let chosen = options
            .context
            .schema
            .as_ref()
            .or(options.context.catalog.as_ref());
        if let Some(schema) = chosen.filter(|schema| !schema.eq_ignore_ascii_case(MAIN_SCHEMA)) {
            let error = DriverError::validation(format!(
                "SQLite always resolves unqualified names in main first; qualify them with {schema} instead of choosing it as the context"
            ));
            // The handle may have stopped waiting.
            let _ = events.send(Event::Done(Err(error))).await;
            return Flow::Ready;
        }
        let sql = request.with_references(parameter_reference);
        let columns = self
            .step(&Limit::new(&cancel, options.timeout), async {
                let statement = connection
                    .prepare(sqlx::AssertSqlSafe(sql.as_str()).into_sql_str())
                    .await
                    .map_err(map_error)?;
                Ok(statement
                    .columns()
                    .iter()
                    .map(column_info)
                    .collect::<Vec<_>>())
            })
            .await;
        let columns = match columns {
            Ok(columns) => columns,
            Err(stopped) => return self.fail(connection, stopped, &events).await,
        };
        let mut returns_rows = !columns.is_empty();
        // `changes()` keeps the count of the last INSERT, UPDATE or DELETE,
        // so a statement that writes nothing would report that count; the
        // connection's total tells whether this one wrote.
        let before = if returns_rows {
            None
        } else {
            total_changes(connection, &Limit::new(&cancel, options.timeout)).await
        };
        if returns_rows && events.send(Event::Columns(columns)).await.is_err() {
            return self.recover(connection, Unfinished::Cut).await;
        }
        let mut stream = if request.parameters.is_empty() {
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql)).fetch_many(&mut *connection)
        } else {
            // One statement per request, so the executor's stream (not the
            // deprecated multi-statement `Query::fetch_many`) is enough.
            (&mut *connection).fetch_many(bound(sql, &request.parameters))
        };
        let mut reading = Reading {
            ahead: None,
            affected: None,
            describe: if returns_rows {
                Describe::Prepared
            } else {
                Describe::FromRows
            },
        };
        loop {
            let page = self
                .read_page(
                    &mut stream,
                    &mut reading,
                    options.max_rows,
                    &events,
                    &Limit::new(&cancel, options.timeout),
                )
                .await;
            if reading.describe == Describe::Sent {
                returns_rows = true;
            }
            match page {
                Ok(PageEnd::Finished) => {
                    drop(stream);
                    let rows_affected = if returns_rows {
                        None
                    } else {
                        let after =
                            total_changes(connection, &Limit::new(&cancel, options.timeout)).await;
                        match (before, after) {
                            (Some(before), Some(after)) if before == after => Some(0),
                            _ => reading.affected,
                        }
                    };
                    let outcome = DriverOutcome {
                        rows_affected,
                        ..DriverOutcome::default()
                    };
                    // The handle may have stopped waiting; the result ended.
                    let _ = events.send(Event::Done(Ok(outcome))).await;
                    return Flow::Ready;
                }
                Ok(PageEnd::Full) => {
                    let outcome = DriverOutcome {
                        has_more: true,
                        ..DriverOutcome::default()
                    };
                    if events.send(Event::Done(Ok(outcome))).await.is_err() {
                        drop(stream);
                        return self.recover(connection, Unfinished::Cut).await;
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
                            return match self.recover(connection, Unfinished::Cut).await {
                                Flow::Ready => Flow::Next(other),
                                flow => flow,
                            };
                        }
                        None => return Flow::Closed,
                    }
                }
                Ok(PageEnd::Gone) => {
                    drop(stream);
                    return self.recover(connection, Unfinished::Cut).await;
                }
                Err(stopped) => {
                    drop(stream);
                    return self.fail(connection, stopped, &events).await;
                }
            }
        }
    }

    /// Runs one step of a statement within `limit`. Dropping an SQLx SQLite
    /// future is safe (the statement runs on the connection's own thread),
    /// so a step that reaches its limit is dropped and the statement is
    /// interrupted by [`Server::recover`].
    async fn step<T>(
        &self,
        limit: &Limit<'_>,
        work: impl Future<Output = DriverResult<T>>,
    ) -> Result<T, Stop> {
        tokio::select! { biased;
            error = limit.reached() => Err((error, Unfinished::Running)),
            done = work => done.map_err(stop),
        }
    }

    /// Waits until the connection's thread is idle: it runs commands in
    /// order, so a ping answers once the statement before it ended. A
    /// statement that may still run is interrupted first; one that was only
    /// cut short is interrupted only if it does not end in time, since
    /// interrupting a write inside a transaction rolls the transaction back.
    async fn recover(&self, connection: &mut SqliteConnection, unfinished: Unfinished) -> Flow {
        match unfinished {
            Unfinished::Lost => return Flow::Broken,
            Unfinished::Running => self.interrupt.store(true, Ordering::SeqCst),
            Unfinished::Cut => match tokio::time::timeout(RECOVERY, connection.ping()).await {
                Ok(Ok(())) => return Flow::Ready,
                Ok(Err(_)) => return Flow::Broken,
                Err(_) => self.interrupt.store(true, Ordering::SeqCst),
            },
        }
        let idle = tokio::time::timeout(RECOVERY, connection.ping()).await;
        self.interrupt.store(false, Ordering::SeqCst);
        match idle {
            Ok(Ok(())) => Flow::Ready,
            _ => Flow::Broken,
        }
    }

    /// Recovers after a failed statement and reports the error. A database
    /// error keeps the session; a lost connection closes it.
    async fn fail(
        &self,
        connection: &mut SqliteConnection,
        (error, unfinished): Stop,
        events: &mpsc::Sender<Event>,
    ) -> Flow {
        let flow = self.recover(connection, unfinished).await;
        if matches!(flow, Flow::Broken) {
            self.open.store(false, Ordering::SeqCst);
        }
        // The handle may have stopped waiting.
        let _ = events.send(Event::Done(Err(error))).await;
        flow
    }

    /// Sends up to `max_rows` rows, starting with the one read ahead last
    /// time, and reads one row ahead to tell a full page from the end of the
    /// result.
    async fn read_page<S>(
        &self,
        stream: &mut S,
        reading: &mut Reading,
        max_rows: u32,
        events: &mpsc::Sender<Event>,
        limit: &Limit<'_>,
    ) -> Result<PageEnd, Stop>
    where
        S: futures_util::Stream<
                Item = Result<sqlx::Either<SqliteQueryResult, SqliteRow>, sqlx::Error>,
            > + Unpin,
    {
        let mut count = 0u32;
        if let Some(row) = reading.ahead.take() {
            if !deliver(events, Event::Row(row), limit).await? {
                return Ok(PageEnd::Gone);
            }
            count = 1;
        }
        while let Some(item) = self
            .step(limit, async { stream.try_next().await.map_err(map_error) })
            .await?
        {
            match item {
                sqlx::Either::Left(result) => reading.affected = Some(result.rows_affected()),
                sqlx::Either::Right(row) => {
                    if reading.describe == Describe::FromRows {
                        let columns = row.columns().iter().map(column_info).collect();
                        if !deliver(events, Event::Columns(columns), limit).await? {
                            return Ok(PageEnd::Gone);
                        }
                        reading.describe = Describe::Sent;
                    }
                    let cells = decode(&row).map_err(stop)?;
                    if count >= max_rows {
                        reading.ahead = Some(cells);
                        return Ok(PageEnd::Full);
                    }
                    if !deliver(events, Event::Row(cells), limit).await? {
                        return Ok(PageEnd::Gone);
                    }
                    count += 1;
                }
            }
        }
        Ok(PageEnd::Finished)
    }
}

/// The rows the connection changed since it opened, or `None` when that
/// could not be read in time; then the statement's own count is reported.
async fn total_changes(connection: &mut SqliteConnection, limit: &Limit<'_>) -> Option<i64> {
    tokio::select! { biased;
        _ = limit.reached() => None,
        total = sqlx::query_scalar::<_, i64>("SELECT total_changes()").fetch_one(&mut *connection) => total.ok(),
    }
}

/// When a statement on a session must stop: when it is cancelled, or when
/// the current step runs out of time.
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

    /// Resolves, with the error to report, once the statement must stop.
    async fn reached(&self) -> DriverError {
        let expired = async {
            match self.deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! { biased;
            _ = self.cancel.cancelled() => DriverError::cancelled(),
            _ = expired => timed_out(),
        }
    }
}

/// Sends `event` to the handle unless `limit` comes first; `false` when the
/// handle stopped listening.
async fn deliver(
    events: &mpsc::Sender<Event>,
    event: Event,
    limit: &Limit<'_>,
) -> Result<bool, Stop> {
    tokio::select! { biased;
        error = limit.reached() => Err((error, Unfinished::Running)),
        sent = events.send(event) => Ok(sent.is_ok()),
    }
}

/// Where a statement's columns come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Describe {
    /// Preparing the statement described them before it ran.
    Prepared,
    /// Preparing described none; if rows come, their columns are sent with
    /// the first one.
    FromRows,
    /// They came with the first row and were sent.
    Sent,
}

/// Where the reading of a result stands between its pages.
struct Reading {
    /// The row read ahead of the last page, sent first in the next one.
    ahead: Option<Row>,
    /// The rows affected, reported by a statement without rows.
    affected: Option<u64>,
    describe: Describe,
}

/// A column as SQLite declares it: the declared type's affinity (`TEXT`,
/// `INTEGER`, `REAL`, `BLOB`, `NUMERIC`) or nothing for an expression.
fn column_info(column: &SqliteColumn) -> ColumnInfo {
    let kind = column.type_info();
    ColumnInfo {
        name: column.name().into(),
        data_type: if kind.is_null() {
            String::new()
        } else {
            kind.name().into()
        },
        nullable: None,
    }
}

/// The cells of a row by the storage class of each value, which SQLite
/// keeps per value rather than per column. Text that is not valid UTF-8
/// stays as its bytes.
fn decode(row: &SqliteRow) -> DriverResult<Row> {
    let invalid = |_| DriverError::new(ErrorCode::Protocol, "Invalid database value");
    let mut cells = Vec::with_capacity(row.len());
    for index in 0..row.len() {
        let value = row.try_get_raw(index).map_err(map_error)?;
        if value.is_null() {
            cells.push(Cell::Null);
            continue;
        }
        let class = value.type_info().name().to_owned();
        let cell = match class.as_str() {
            "INTEGER" => {
                Cell::Integer(<i64 as sqlx::Decode<sqlx::Sqlite>>::decode(value).map_err(invalid)?)
            }
            "REAL" => {
                Cell::Float(<f64 as sqlx::Decode<sqlx::Sqlite>>::decode(value).map_err(invalid)?)
            }
            "BLOB" => Cell::Binary(
                <Vec<u8> as sqlx::Decode<sqlx::Sqlite>>::decode(value).map_err(invalid)?,
            ),
            _ => {
                let bytes =
                    <Vec<u8> as sqlx::Decode<sqlx::Sqlite>>::decode(value).map_err(invalid)?;
                match String::from_utf8(bytes) {
                    Ok(text) => Cell::Text(text),
                    Err(error) => Cell::Binary(error.into_bytes()),
                }
            }
        };
        cells.push(cell);
    }
    Ok(cells)
}

/// What a parameter's marker becomes (ADR-0023): SQLite's numbered
/// parameter `?N` for the parameter at `index`, bound by [`bound`]. Numbers
/// are bound as their text and read with SQLite's own `NUMERIC` conversion
/// (an integer when the value is one); neither holds the value.
fn parameter_reference(index: usize, parameter: &QueryParameter) -> String {
    let number = index + 1;
    match parameter.kind {
        ParameterKind::Number => format!("CAST(?{number} AS NUMERIC)"),
        _ => format!("?{number}"),
    }
}

/// The statement with every parameter bound in order, so `?N` is the
/// parameter at `N - 1`. Booleans are SQLite's `1` and `0`; dates and
/// timestamps are text, as SQLite's date functions read them.
fn bound(
    sql: String,
    parameters: &[QueryParameter],
) -> sqlx::query::Query<'static, sqlx::Sqlite, SqliteArguments> {
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).persistent(false);
    for parameter in parameters {
        query = match (&parameter.value, parameter.kind) {
            (None, _) => query.bind(None::<String>),
            (Some(value), ParameterKind::Boolean) => query.bind(i64::from(value == "true")),
            (Some(value), _) => query.bind(value.clone()),
        };
    }
    query
}

/// A name quoted for SQL the driver writes itself (the schema of
/// `"aux".sqlite_schema`), with the quote doubled inside.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The schemas of the connection: `main` and the attached databases,
/// without `temp`, ordered by name.
async fn schemas(connection: &mut SqliteConnection) -> DriverResult<Vec<String>> {
    sqlx::query_scalar("SELECT name FROM pragma_database_list WHERE name <> 'temp' ORDER BY name")
        .fetch_all(&mut *connection)
        .await
        .map_err(map_error)
}

/// The root holds the schemas; a schema holds no further namespace.
pub async fn discover(
    input: &ResolvedConnection,
    parent: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Vec<MetadataObject>> {
    if !parent.0.is_empty() {
        return Ok(Vec::new());
    }
    let mut connection = connect(input, &cancel).await?;
    bounded(&cancel, Some(INSPECTION), async {
        Ok(schemas(&mut connection)
            .await?
            .into_iter()
            .map(|name| MetadataObject::new(ObjectKind::Schema, parent, name))
            .collect())
    })
    .await
}

/// Tables (virtual ones too) and views of a schema, without SQLite's own
/// `sqlite_` tables; the second parameter filters one relation and may be
/// NULL.
const RELATIONS_SQL: &str = r"SELECT name, type FROM pragma_table_list WHERE schema = ?1 AND (?2 IS NULL OR name = ?2) AND type IN ('table', 'view', 'virtual') AND name NOT LIKE 'sqlite\_%' ESCAPE '\' ORDER BY name";
/// Every column of a relation, generated ones included; `hidden = 1` marks
/// the hidden columns of a virtual table.
const COLUMNS_SQL: &str = r#"SELECT name, type, "notnull", dflt_value, pk, hidden FROM pragma_table_xinfo(?1, ?2) ORDER BY cid"#;
/// One row per column of each foreign key, in key order. `to` is NULL when
/// the key references the parent's primary key.
const FOREIGN_KEYS_SQL: &str = r#"SELECT id, "table", "from", "to", on_update, on_delete FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq"#;
const PRIMARY_KEY_SQL: &str = "SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk";

type ColumnRow = (String, String, i64, Option<String>, i64, i64);
type ForeignKeyRow = (i64, String, String, Option<String>, String, String);

/// A foreign key of a table, as SQLite lists it.
struct ForeignKey {
    parent: String,
    columns: Vec<String>,
    referenced: Vec<String>,
    on_update: String,
    on_delete: String,
}

impl ForeignKey {
    /// SQLite does not keep the names of foreign keys, so a key is named by
    /// what it links: `(customer_id) → customers`.
    fn name(&self) -> String {
        format!("({}) → {}", self.columns.join(", "), self.parent)
    }
}

/// The foreign keys of `table`, with the referenced columns of a key that
/// names none read from the parent's primary key.
async fn foreign_keys(
    connection: &mut SqliteConnection,
    schema: &str,
    table: &str,
) -> DriverResult<Vec<ForeignKey>> {
    let rows: Vec<ForeignKeyRow> = sqlx::query_as(FOREIGN_KEYS_SQL)
        .bind(table)
        .bind(schema)
        .fetch_all(&mut *connection)
        .await
        .map_err(map_error)?;
    let mut keys: Vec<(i64, ForeignKey, bool)> = Vec::new();
    for (id, parent, from, to, on_update, on_delete) in rows {
        if keys.last().is_none_or(|(last, _, _)| *last != id) {
            keys.push((
                id,
                ForeignKey {
                    parent,
                    columns: Vec::new(),
                    referenced: Vec::new(),
                    on_update,
                    on_delete,
                },
                false,
            ));
        }
        if let Some((_, key, implicit)) = keys.last_mut() {
            key.columns.push(from);
            match to {
                Some(to) => key.referenced.push(to),
                None => *implicit = true,
            }
        }
    }
    let mut found = Vec::with_capacity(keys.len());
    for (_, mut key, implicit) in keys {
        if implicit {
            key.referenced = sqlx::query_scalar(PRIMARY_KEY_SQL)
                .bind(&key.parent)
                .bind(schema)
                .fetch_all(&mut *connection)
                .await
                .map_err(map_error)?;
        }
        found.push(key);
    }
    Ok(found)
}

pub async fn introspect(
    input: &ResolvedConnection,
    scope: &ObjectPath,
    level: IntrospectionLevel,
    sink: &mut dyn IntrospectionSink,
    cancel: CancellationToken,
) -> DriverResult<IntrospectionReport> {
    let full = level == IntrospectionLevel::Full;
    let mut connection = connect(input, &cancel).await?;
    bounded(&cancel, Some(INTROSPECTION), async {
        let wanted = scope.name_of(ObjectKind::Schema);
        let relation = scope.relation();
        let column = scope.name_of(ObjectKind::Column);
        let root = ObjectPath::default();
        for schema in schemas(&mut connection).await? {
            if wanted.is_some_and(|wanted| wanted != schema) {
                continue;
            }
            let namespace = MetadataObject::new(ObjectKind::Schema, &root, schema.as_str());
            let relations: Vec<(String, String)> = sqlx::query_as(RELATIONS_SQL)
                .bind(schema.as_str())
                .bind(relation)
                .fetch_all(&mut connection)
                .await
                .map_err(map_error)?;
            let mut batch = vec![namespace.clone()];
            for (name, kind) in relations {
                let kind = if kind == "view" {
                    ObjectKind::View
                } else {
                    ObjectKind::Table
                };
                let object = MetadataObject::new(kind, &namespace.path, name.as_str());
                let path = object.path.clone();
                batch.push(object);
                if full {
                    let columns: Vec<ColumnRow> = sqlx::query_as(COLUMNS_SQL)
                        .bind(name.as_str())
                        .bind(schema.as_str())
                        .fetch_all(&mut connection)
                        .await
                        .map_err(map_error)?;
                    let mut position = 0u32;
                    for (column, data_type, not_null, default, key, hidden) in columns {
                        if hidden == 1 {
                            continue;
                        }
                        position += 1;
                        let mut object = MetadataObject::new(ObjectKind::Column, &path, column);
                        object.ordinal = Some(position);
                        object.data_type = Some(data_type).filter(|kind| !kind.is_empty());
                        object.nullable = Some(not_null == 0);
                        object.default_value = default;
                        object.primary_key = u32::try_from(key).ok().filter(|key| *key > 0);
                        batch.push(object);
                    }
                    if column.is_none() && kind == ObjectKind::Table {
                        for key in foreign_keys(&mut connection, &schema, &name).await? {
                            let mut object =
                                MetadataObject::new(ObjectKind::ForeignKey, &path, key.name());
                            object.references = Some(ForeignKeyTarget {
                                relation: namespace.path.child(ObjectKind::Table, key.parent),
                                columns: key.columns,
                                referenced: key.referenced,
                            });
                            batch.push(object);
                        }
                    }
                }
                batch.retain(|object| object.path.is_within(scope));
                if !batch.is_empty() {
                    sink.batch(std::mem::take(&mut batch)).await?;
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

/// Indexes, keys and triggers of a table or a view; the `AUTOINCREMENT`
/// counters of a schema.
pub async fn inspect(
    input: &ResolvedConnection,
    path: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Inspection> {
    let Some(schema) = path.name_of(ObjectKind::Schema) else {
        return Err(DriverError::validation(
            "Choose a schema, a table or a view",
        ));
    };
    let mut connection = connect(input, &cancel).await?;
    bounded(&cancel, Some(INSPECTION), async {
        let mut items = Vec::new();
        let item = |kind, name: String, detail: String| InspectionItem { kind, name, detail };
        let catalog = format!("{}.sqlite_schema", quoted(schema));
        let Some(table) = path.relation() else {
            let counters: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {catalog} WHERE type = 'table' AND name = 'sqlite_sequence'"
            )))
            .fetch_one(&mut connection)
            .await
            .map_err(map_error)?;
            if counters > 0 {
                let rows: Vec<(String, Option<i64>)> =
                    sqlx::query_as(sqlx::AssertSqlSafe(format!(
                        "SELECT name, seq FROM {}.sqlite_sequence ORDER BY name",
                        quoted(schema)
                    )))
                    .fetch_all(&mut connection)
                    .await
                    .map_err(map_error)?;
                for (name, last) in rows {
                    let detail = last.map(|last| format!("AUTOINCREMENT, last {last}"));
                    items.push(item(
                        InspectedKind::Sequence,
                        name,
                        detail.unwrap_or_default(),
                    ));
                }
            }
            return Ok(Inspection { items });
        };
        let indexes: Vec<(String, i64, String)> = sqlx::query_as(
            r#"SELECT name, "unique", origin FROM pragma_index_list(?1, ?2) ORDER BY name"#,
        )
        .bind(table)
        .bind(schema)
        .fetch_all(&mut connection)
        .await
        .map_err(map_error)?;
        let mut primary = false;
        for (name, unique, origin) in indexes {
            let columns: Vec<Option<String>> =
                sqlx::query_scalar("SELECT name FROM pragma_index_info(?1, ?2) ORDER BY seqno")
                    .bind(name.as_str())
                    .bind(schema)
                    .fetch_all(&mut connection)
                    .await
                    .map_err(map_error)?;
            let columns = columns
                .into_iter()
                .map(|column| column.unwrap_or_else(|| "<expression>".into()))
                .collect::<Vec<_>>()
                .join(", ");
            let sql: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT sql FROM {catalog} WHERE type = 'index' AND name = ?1"
            )))
            .bind(name.as_str())
            .fetch_optional(&mut connection)
            .await
            .map_err(map_error)?
            .flatten();
            let kind = match origin.as_str() {
                "pk" => {
                    primary = true;
                    InspectedKind::PrimaryKey
                }
                "u" => InspectedKind::Unique,
                _ => InspectedKind::Index,
            };
            let detail = sql.unwrap_or_else(|| {
                format!("{}({columns})", if unique != 0 { "UNIQUE " } else { "" })
            });
            items.push(item(kind, name, detail));
        }
        // A rowid table's INTEGER PRIMARY KEY has no index of its own.
        if !primary {
            let key: Vec<String> = sqlx::query_scalar(PRIMARY_KEY_SQL)
                .bind(table)
                .bind(schema)
                .fetch_all(&mut connection)
                .await
                .map_err(map_error)?;
            if !key.is_empty() {
                items.push(item(
                    InspectedKind::PrimaryKey,
                    "PRIMARY KEY".into(),
                    format!("({})", key.join(", ")),
                ));
            }
        }
        for key in foreign_keys(&mut connection, schema, table).await? {
            let mut detail = format!("REFERENCES {}({})", key.parent, key.referenced.join(", "));
            for (event, action) in [("UPDATE", &key.on_update), ("DELETE", &key.on_delete)] {
                if action != "NO ACTION" {
                    detail.push_str(&format!(" ON {event} {action}"));
                }
            }
            items.push(item(InspectedKind::ForeignKey, key.name(), detail));
        }
        let triggers: Vec<(String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT name, sql FROM {catalog} WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name"
        )))
        .bind(table)
        .fetch_all(&mut connection)
        .await
        .map_err(map_error)?;
        for (name, sql) in triggers {
            items.push(item(InspectedKind::Trigger, name, sql.unwrap_or_default()));
        }
        Ok(Inspection { items })
    })
    .await
}

/// The `CREATE` statement of a table or a view as SQLite keeps it, followed
/// by those of its indexes and triggers.
pub async fn definition(
    input: &ResolvedConnection,
    path: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<String> {
    let (Some(schema), Some(relation)) = (path.name_of(ObjectKind::Schema), path.relation()) else {
        return Err(DriverError::validation("Choose a table or a view"));
    };
    let catalog = format!("{}.sqlite_schema", quoted(schema));
    let mut connection = connect(input, &cancel).await?;
    bounded(&cancel, Some(INSPECTION), async {
        let own: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT sql FROM {catalog} WHERE type IN ('table', 'view') AND name = ?1"
        )))
        .bind(relation)
        .fetch_optional(&mut connection)
        .await
        .map_err(map_error)?
        .flatten();
        let Some(own) = own else {
            return Err(DriverError::new(
                ErrorCode::NotFound,
                "The table or view is no longer in the database",
            ));
        };
        let related: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT sql FROM {catalog} WHERE type IN ('index', 'trigger') AND tbl_name = ?1 AND sql IS NOT NULL ORDER BY type, name"
        )))
        .bind(relation)
        .fetch_all(&mut connection)
        .await
        .map_err(map_error)?;
        Ok(std::iter::once(own)
            .chain(related)
            .map(|statement| format!("{};", statement.trim_end().trim_end_matches(';')))
            .collect::<Vec<_>>()
            .join("\n\n"))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::parameter_reference;
    use jykre_core::{ExecutionOptions, ParameterKind, QueryParameter, QueryRequest};

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
    fn markers_become_numbered_parameters_and_values_stay_out_of_the_sql() {
        let sql = "SELECT :who, :amount, :flag, :day, :none, :who";
        let request = QueryRequest {
            parameters: vec![
                marked(
                    sql,
                    "who",
                    ParameterKind::Text,
                    Some("x'; DROP TABLE t; --"),
                ),
                marked(sql, "amount", ParameterKind::Number, Some("-12.345")),
                marked(sql, "flag", ParameterKind::Boolean, Some("false")),
                marked(sql, "day", ParameterKind::Date, Some("2024-02-29")),
                marked(sql, "none", ParameterKind::Number, None),
            ],
            ..QueryRequest::new(sql, ExecutionOptions::default())
        };
        assert_eq!(request.validate(), Ok(()));
        let bound = request.with_references(parameter_reference);
        assert_eq!(
            bound,
            "SELECT ?1, CAST(?2 AS NUMERIC), ?3, ?4, CAST(?5 AS NUMERIC), ?1"
        );
        assert!(!bound.contains("DROP"));
    }
}
