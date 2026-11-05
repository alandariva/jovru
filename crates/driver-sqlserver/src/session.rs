//! A SQL session: one connection in a task of its own, which keeps a result
//! open between pages.
//!
//! A statement cannot be interrupted on the server and the session kept:
//! tiberius 0.13 reads a TDS attention's acknowledgement only when it comes
//! in the interrupted message, and SQL Server sends it in a message of its
//! own ([technical debt TD-009](../../../docs/technical-debt.md)). A result
//! cut by the reader is therefore read to its end, for a while; a statement
//! that must stop (cancelled, out of time) closes its session, and the server
//! rolls back what it left open.

use crate::net::{self, Connection, bracketed, map_error, timed_out};
use crate::values;
use futures_util::TryStreamExt;
use jykre_core::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tiberius::error::Error as TdsError;
use tiberius::{QueryItem, QueryStream};
use tokio::sync::{mpsc, oneshot};

/// How long the rest of a result cut by the reader may take to read, or an
/// internal query to answer, before the session is closed.
const RECOVERY: Duration = Duration::from_secs(5);
/// Rows sent from the connection's task to the session handle. A bounded
/// channel paces the reading; dropping the receiver asks the task to stop.
const EVENT_BUFFER: usize = 256;

const VERSION_SQL: &str = "SELECT N'SQL Server ' + CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)) + N' (' + CAST(SERVERPROPERTY('Edition') AS nvarchar(128)) + N')'";
/// The transaction as the server sees it: nesting, whether it can still
/// commit, implicit transactions, and whether the user may read the
/// transaction's log records (to tell whether it wrote anything).
const TRANSACTION_SQL: &str = "SELECT @@TRANCOUNT, CAST(XACT_STATE() AS int), CAST(@@OPTIONS & 2 AS int), HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER STATE')";
const CHANGES_SQL: &str = "SELECT CAST(CASE WHEN EXISTS (SELECT 1 FROM sys.dm_tran_database_transactions WHERE transaction_id = CURRENT_TRANSACTION_ID() AND database_transaction_log_record_count > 0) THEN 1 ELSE 0 END AS int)";
/// What the driver's own explain text starts with (see `jykre_sql::explain`).
const SHOWPLAN_ON: &str = "SET SHOWPLAN_ALL ON";

pub async fn test(
    input: &ResolvedConnection,
    cancel: CancellationToken,
) -> DriverResult<ServerInfo> {
    let mut connection = net::connect(input, &cancel).await?;
    let rows = net::bounded(&cancel, Some(net::CONNECT), connection.rows(VERSION_SQL)).await?;
    let version = match rows.first() {
        Some(row) => row
            .try_get::<&str, _>(0)
            .map_err(map_error)?
            .map(str::to_owned),
        None => None,
    };
    connection.close().await?;
    Ok(ServerInfo { version })
}

pub async fn open(
    input: &ResolvedConnection,
    cancel: CancellationToken,
) -> DriverResult<Box<dyn Session>> {
    let connection = net::connect(input, &cancel).await?;
    let (commands, receiver) = mpsc::channel(4);
    let open = Arc::new(AtomicBool::new(true));
    let server = Server {
        open: open.clone(),
        database: None,
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
    Context {
        reply: oneshot::Sender<DriverResult<SessionContext>>,
    },
    Transaction {
        reply: oneshot::Sender<DriverResult<Option<TransactionStatus>>>,
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
/// result can stay open between pages while the handle is free.
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

    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<DriverResult<T>>) -> Command,
    ) -> DriverResult<T> {
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
    /// The current database, as the catalog, and the user's default schema:
    /// a SQL Server session cannot change the schema unqualified names
    /// resolve in.
    async fn context(&mut self, cancel: CancellationToken) -> DriverResult<SessionContext> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        self.ask(|reply| Command::Context { reply }).await
    }
    /// Read from `@@TRANCOUNT`, `XACT_STATE()` and `@@OPTIONS`; whether an
    /// open transaction wrote anything only when the user may read
    /// `sys.dm_tran_database_transactions`.
    async fn transaction(
        &mut self,
        cancel: CancellationToken,
    ) -> DriverResult<Option<TransactionStatus>> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        self.ask(|reply| Command::Transaction { reply }).await
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

/// What a connection needs after a statement stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unfinished {
    /// The response was read to its end.
    Ended,
    /// The reading stopped between tokens: the rest of the response is read
    /// and dropped, for at most [`RECOVERY`].
    Cut,
    /// The statement must stop, or the protocol is out of step: the session
    /// is closed.
    Lost,
}

/// Why a statement stopped, and what its connection needs.
type Stop = (DriverError, Unfinished);

/// What a failed read leaves. tiberius reports the server's errors once
/// their response ended; anything else breaks the protocol.
fn after(error: &TdsError) -> Unfinished {
    match error {
        TdsError::Server(_) => Unfinished::Ended,
        _ => Unfinished::Lost,
    }
}

fn stopped(error: TdsError) -> Stop {
    let unfinished = after(&error);
    (map_error(error), unfinished)
}

/// When a statement must stop: when it is cancelled, or when it runs out of
/// time.
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

/// SQL as the server receives it: a batch, unchanged, or a statement with
/// bound parameters (`sp_executesql`).
enum Statement<'a> {
    Batch(&'a str),
    Bound(String, Vec<Option<String>>),
}

/// Sends `statement` and waits for the start of its response, within
/// `limit`.
async fn start<'c>(
    client: &'c mut net::Client,
    statement: &'c Statement<'_>,
    limit: &Limit<'_>,
) -> Result<QueryStream<'c>, Stop> {
    let request = async move {
        match statement {
            Statement::Batch(sql) => client.simple_query(*sql).await,
            Statement::Bound(sql, values) => {
                let mut query = tiberius::Query::new(sql.as_str());
                for value in values {
                    query.bind(value.as_deref());
                }
                query.query(client).await
            }
        }
    };
    tokio::select! { biased;
        reason = limit.reached() => Err((reason, Unfinished::Lost)),
        started = request => started.map_err(stopped),
    }
}

/// Reads the next item within `limit`.
async fn next(stream: &mut QueryStream<'_>, limit: &Limit<'_>) -> Result<Option<QueryItem>, Stop> {
    tokio::select! { biased;
        reason = limit.reached() => Err((reason, Unfinished::Lost)),
        item = stream.try_next() => item.map_err(stopped),
    }
}

/// Reads and drops what is left of a cut result, for at most [`RECOVERY`].
async fn drain(stream: &mut QueryStream<'_>) -> Unfinished {
    let rest = async {
        while stream.try_next().await?.is_some() {}
        Ok::<_, TdsError>(())
    };
    match tokio::time::timeout(RECOVERY, rest).await {
        Ok(Ok(())) => Unfinished::Ended,
        Ok(Err(error)) => after(&error),
        Err(_) => Unfinished::Lost,
    }
}

/// Settles a stream that is about to be dropped between tokens.
async fn settle(stream: &mut QueryStream<'_>, unfinished: Unfinished) -> Unfinished {
    match unfinished {
        Unfinished::Cut => drain(stream).await,
        other => other,
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
        error = limit.reached() => Err((error, Unfinished::Lost)),
        sent = events.send(event) => Ok(sent.is_ok()),
    }
}

/// How reading a page ended.
enum PageEnd {
    /// The result ended, and so did the response.
    Finished,
    /// The result ended; more results of the same batch follow.
    Rest,
    /// The page is full and at least one more row was read ahead.
    Full,
    /// The handle stopped listening.
    Gone,
}

/// Where the reading of a result stands between its pages.
#[derive(Default)]
struct Reading {
    /// The row read ahead of the last page, sent first in the next one.
    ahead: Option<Row>,
    /// The result's columns were sent.
    columns: bool,
}

/// Sends up to `max_rows` rows of the first result, starting with the one
/// read ahead last time, and reads one row ahead to tell a full page from
/// the end of the result.
async fn read_page(
    stream: &mut QueryStream<'_>,
    reading: &mut Reading,
    max_rows: u32,
    events: &mpsc::Sender<Event>,
    limit: &Limit<'_>,
) -> Result<PageEnd, Stop> {
    let mut count = 0u32;
    if let Some(row) = reading.ahead.take() {
        if !deliver(events, Event::Row(row), limit).await? {
            return Ok(PageEnd::Gone);
        }
        count = 1;
    }
    while let Some(item) = next(stream, limit).await? {
        match item {
            QueryItem::Metadata(_) if reading.columns => return Ok(PageEnd::Rest),
            QueryItem::Metadata(metadata) => {
                let columns = metadata
                    .columns()
                    .iter()
                    .map(|column| ColumnInfo {
                        name: column.name().into(),
                        data_type: values::type_name(column.column_type()).into(),
                        nullable: None,
                    })
                    .collect();
                reading.columns = true;
                if !deliver(events, Event::Columns(columns), limit).await? {
                    return Ok(PageEnd::Gone);
                }
            }
            QueryItem::Row(row) => {
                let cells = row
                    .cells()
                    .map(|(column, data)| values::cell(column.column_type(), data))
                    .collect::<DriverResult<Row>>()
                    .map_err(|error| (error, Unfinished::Cut))?;
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

/// Reads and drops the results that follow the first one, within `limit`:
/// the batch runs to its end, as it would without a reader.
async fn rest(stream: &mut QueryStream<'_>, limit: &Limit<'_>) -> Result<(), Stop> {
    while next(stream, limit).await?.is_some() {}
    Ok(())
}

/// How a statement's reading ended, decided while its stream still exists.
struct Ending {
    unfinished: Unfinished,
    /// What to report once the connection is settled.
    report: Option<DriverResult<DriverOutcome>>,
    /// The statement returned no result.
    no_rows: bool,
    /// A command that arrived while a result was open.
    next: Option<Command>,
    /// The handle is gone.
    closed: bool,
}

impl Ending {
    fn report(unfinished: Unfinished, report: DriverResult<DriverOutcome>) -> Self {
        Self {
            unfinished,
            report: Some(report),
            no_rows: false,
            next: None,
            closed: false,
        }
    }

    fn stop((error, unfinished): Stop) -> Self {
        Self::report(unfinished, Err(error))
    }

    fn silent(unfinished: Unfinished) -> Self {
        Self {
            unfinished,
            report: None,
            no_rows: false,
            next: None,
            closed: false,
        }
    }
}

struct Server {
    open: Arc<AtomicBool>,
    /// The database last chosen with `USE` by a request, or reported by
    /// [`Session::context`]: a request for it is not applied again.
    database: Option<String>,
}

impl Server {
    async fn serve(mut self, mut connection: Connection, mut commands: mpsc::Receiver<Command>) {
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
                    match tokio::time::timeout(RECOVERY, connection.rows("SELECT 1")).await {
                        Ok(Ok(_)) => {
                            let _ = reply.send(Ok(()));
                            Flow::Ready
                        }
                        outcome => {
                            self.open.store(false, Ordering::SeqCst);
                            let _ = reply.send(Err(match outcome {
                                Ok(Err(error)) => error,
                                _ => timed_out(),
                            }));
                            Flow::Broken
                        }
                    }
                }
                Command::Context { reply } => {
                    match tokio::time::timeout(
                        RECOVERY,
                        connection.rows("SELECT DB_NAME(), SCHEMA_NAME()"),
                    )
                    .await
                    {
                        Ok(Ok(rows)) => {
                            let text = |index: usize| {
                                rows.first()
                                    .and_then(|row| row.try_get::<&str, _>(index).ok().flatten())
                                    .map(str::to_owned)
                            };
                            let context = SessionContext {
                                catalog: text(0),
                                schema: text(1),
                            };
                            self.database.clone_from(&context.catalog);
                            let _ = reply.send(Ok(context));
                            Flow::Ready
                        }
                        Ok(Err(error)) => {
                            let _ = reply.send(Err(error));
                            Flow::Ready
                        }
                        Err(_) => {
                            let _ = reply.send(Err(timed_out()));
                            Flow::Broken
                        }
                    }
                }
                Command::Transaction { reply } => {
                    match tokio::time::timeout(RECOVERY, transaction(&mut connection)).await {
                        Ok(status) => {
                            let _ = reply.send(status);
                            Flow::Ready
                        }
                        Err(_) => {
                            let _ = reply.send(Err(timed_out()));
                            Flow::Broken
                        }
                    }
                }
                Command::Close { reply } => {
                    self.open.store(false, Ordering::SeqCst);
                    let _ = reply.send(connection.close().await);
                    return;
                }
            };
            match flow {
                Flow::Ready => {}
                Flow::Next(command) => next = Some(command),
                Flow::Closed => break,
                Flow::Broken => {
                    self.open.store(false, Ordering::SeqCst);
                    // Closing still writes to the socket; a server that does
                    // not answer must not keep the task.
                    let _ = tokio::time::timeout(Duration::from_secs(1), connection.close()).await;
                    return;
                }
            }
        }
        self.open.store(false, Ordering::SeqCst);
        let _ = tokio::time::timeout(RECOVERY, connection.close()).await;
    }

    /// Runs one request and serves its pages until the result ends or
    /// another command arrives.
    async fn execute(
        &mut self,
        connection: &mut Connection,
        request: QueryRequest,
        events: mpsc::Sender<Event>,
        cancel: CancellationToken,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Flow {
        let options = request.options.clone();
        // The chosen database is applied only when it changed, so a USE the
        // user ran meanwhile stays in force.
        let apply = options
            .context
            .catalog
            .clone()
            .filter(|database| self.database.as_ref() != Some(database));
        if let Some(database) = &apply {
            let sql = format!("USE {}", bracketed(database));
            let limit = Limit::new(&cancel, options.timeout);
            if let Err(stopped) = self.simple(connection, &sql, &limit).await {
                return self.fail(stopped, &events).await;
            }
            self.database = apply;
        }
        if let Some(statement) = explained(&request.sql) {
            return self
                .explain(connection, statement, options, events, cancel)
                .await;
        }
        let statement = if request.parameters.is_empty() {
            Statement::Batch(&request.sql)
        } else {
            Statement::Bound(
                request.with_references(parameter_reference),
                request.parameters.iter().map(parameter_value).collect(),
            )
        };
        self.run(
            connection,
            statement,
            options,
            events,
            cancel,
            Some(commands),
        )
        .await
    }

    /// Runs `statement` under SHOWPLAN_ALL, so it is compiled and its plan
    /// returned instead of running. The plan mode is turned off whatever
    /// happened to the statement; a session that cannot turn it off is
    /// closed, so no later statement is only explained.
    async fn explain(
        &mut self,
        connection: &mut Connection,
        statement: &str,
        options: ExecutionOptions,
        events: mpsc::Sender<Event>,
        cancel: CancellationToken,
    ) -> Flow {
        let limit = Limit::new(&cancel, options.timeout);
        if let Err(stopped) = self.simple(connection, SHOWPLAN_ON, &limit).await {
            return self.fail(stopped, &events).await;
        }
        let flow = self
            .run(
                connection,
                Statement::Batch(statement),
                options,
                events,
                cancel.clone(),
                None,
            )
            .await;
        if matches!(flow, Flow::Broken) {
            return flow;
        }
        match tokio::time::timeout(RECOVERY, connection.rows("SET SHOWPLAN_ALL OFF")).await {
            Ok(Ok(_)) => flow,
            _ => {
                self.open.store(false, Ordering::SeqCst);
                Flow::Broken
            }
        }
    }

    /// Runs SQL of the driver's own whose result does not matter.
    async fn simple(
        &self,
        connection: &mut Connection,
        sql: &str,
        limit: &Limit<'_>,
    ) -> Result<(), Stop> {
        let statement = Statement::Batch(sql);
        let mut stream = start(&mut connection.client, &statement, limit).await?;
        rest(&mut stream, limit).await
    }

    /// Runs a statement and serves its first result page by page. Without
    /// `commands` the result is not kept open: rows past the first page are
    /// left out and the result is reported as truncated.
    async fn run(
        &mut self,
        connection: &mut Connection,
        statement: Statement<'_>,
        mut options: ExecutionOptions,
        mut events: mpsc::Sender<Event>,
        mut cancel: CancellationToken,
        mut commands: Option<&mut mpsc::Receiver<Command>>,
    ) -> Flow {
        let ending = 'read: {
            let mut stream = match start(
                &mut connection.client,
                &statement,
                &Limit::new(&cancel, options.timeout),
            )
            .await
            {
                Ok(stream) => stream,
                Err(stopped) => break 'read Ending::stop(stopped),
            };
            let mut reading = Reading::default();
            loop {
                let limit = Limit::new(&cancel, options.timeout);
                let page =
                    read_page(&mut stream, &mut reading, options.max_rows, &events, &limit).await;
                match page {
                    Ok(PageEnd::Finished) => {
                        break 'read Ending {
                            no_rows: !reading.columns,
                            ..Ending::report(Unfinished::Ended, Ok(DriverOutcome::default()))
                        };
                    }
                    Ok(PageEnd::Rest) => {
                        break 'read match rest(&mut stream, &limit).await {
                            Ok(()) => {
                                Ending::report(Unfinished::Ended, Ok(DriverOutcome::default()))
                            }
                            Err(stopped) => Ending::stop(stopped),
                        };
                    }
                    Ok(PageEnd::Gone) => {
                        break 'read Ending::silent(settle(&mut stream, Unfinished::Cut).await);
                    }
                    Err((error, unfinished)) => {
                        let unfinished = settle(&mut stream, unfinished).await;
                        break 'read Ending::report(unfinished, Err(error));
                    }
                    Ok(PageEnd::Full) => {}
                }
                let Some(commands) = commands.as_deref_mut() else {
                    let outcome = DriverOutcome {
                        truncated: true,
                        ..DriverOutcome::default()
                    };
                    let unfinished = settle(&mut stream, Unfinished::Cut).await;
                    break 'read Ending::report(unfinished, Ok(outcome));
                };
                let outcome = DriverOutcome {
                    has_more: true,
                    ..DriverOutcome::default()
                };
                if events.send(Event::Done(Ok(outcome))).await.is_err() {
                    break 'read Ending::silent(settle(&mut stream, Unfinished::Cut).await);
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
                        break 'read Ending {
                            next: Some(other),
                            ..Ending::silent(settle(&mut stream, Unfinished::Cut).await)
                        };
                    }
                    None => {
                        break 'read Ending {
                            closed: true,
                            ..Ending::silent(Unfinished::Lost)
                        };
                    }
                }
            }
        };
        if ending.closed {
            return Flow::Closed;
        }
        let flow = self.recover(ending.unfinished);
        if matches!(flow, Flow::Broken) {
            self.open.store(false, Ordering::SeqCst);
        }
        if let Some(mut report) = ending.report {
            if ending.no_rows
                && matches!(flow, Flow::Ready)
                && let Ok(outcome) = &mut report
            {
                outcome.rows_affected = rows_affected(connection).await;
            }
            let _ = events.send(Event::Done(report)).await;
        }
        match (flow, ending.next) {
            (Flow::Ready, Some(command)) => Flow::Next(command),
            (flow, _) => flow,
        }
    }

    /// Whether the connection is in a known state after a statement
    /// stopped. A result still cut here could not be read to its end.
    fn recover(&self, unfinished: Unfinished) -> Flow {
        match unfinished {
            Unfinished::Ended => Flow::Ready,
            Unfinished::Cut | Unfinished::Lost => Flow::Broken,
        }
    }

    /// Reports a failed statement. A database error keeps the session; a
    /// statement that had to stop, or a lost connection, closes it.
    async fn fail(&self, (error, unfinished): Stop, events: &mpsc::Sender<Event>) -> Flow {
        let flow = self.recover(unfinished);
        if matches!(flow, Flow::Broken) {
            self.open.store(false, Ordering::SeqCst);
        }
        let _ = events.send(Event::Done(Err(error))).await;
        flow
    }
}

/// The rows the last statement changed, for a statement without a result.
async fn rows_affected(connection: &mut Connection) -> Option<u64> {
    let rows = tokio::time::timeout(
        RECOVERY,
        connection.rows("SELECT CAST(@@ROWCOUNT AS bigint)"),
    )
    .await
    .ok()?
    .ok()?;
    let count = rows.first()?.try_get::<i64, _>(0).ok()??;
    u64::try_from(count).ok()
}

/// Reads the transaction state without reading any table, so it starts no
/// implicit transaction and takes no lock.
async fn transaction(connection: &mut Connection) -> DriverResult<Option<TransactionStatus>> {
    let rows = connection.rows(TRANSACTION_SQL).await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let number = |index: usize| row.try_get::<i32, _>(index).ok().flatten();
    let (Some(count), Some(state), Some(implicit)) = (number(0), number(1), number(2)) else {
        return Ok(None);
    };
    let may_read_log = number(3) == Some(1);
    if state == -1 {
        return Ok(Some(TransactionStatus::Failed));
    }
    if count > 0 {
        // A failed read of the log (a permission revoked meanwhile) is not
        // an error of the session: the changes are then unknown.
        let changes = if may_read_log {
            match connection.rows(CHANGES_SQL).await {
                Ok(rows) => rows
                    .first()
                    .and_then(|row| row.try_get::<i32, _>(0).ok().flatten())
                    .map(|written| written == 1),
                Err(_) => None,
            }
        } else {
            None
        };
        return Ok(Some(TransactionStatus::Open { changes }));
    }
    Ok(Some(if implicit != 0 {
        TransactionStatus::Manual
    } else {
        TransactionStatus::Idle
    }))
}

/// The statement of a request that asks for its plan (`SET SHOWPLAN_ALL ON`
/// followed by the statement), or `None`. SQL Server refuses that `SET`
/// with anything else in its batch, so no request the user writes means
/// something else.
fn explained(sql: &str) -> Option<&str> {
    let trimmed = sql.trim_start();
    let head = trimmed.get(..SHOWPLAN_ON.len())?;
    if !head.eq_ignore_ascii_case(SHOWPLAN_ON) {
        return None;
    }
    let rest = &trimmed[SHOWPLAN_ON.len()..];
    let rest = match rest.strip_prefix(';') {
        Some(rest) => rest,
        None if rest.starts_with(char::is_whitespace) => rest,
        None => return None,
    };
    let rest = rest.trim();
    (!rest.is_empty()).then_some(rest)
}

/// A number as plain decimal text, so `CAST(… AS decimal)` reads it:
/// SQL Server does not read an exponent there. Exact, digit for digit.
fn plain_number(value: &str) -> String {
    let Some((mantissa, exponent)) = value.split_once(['e', 'E']) else {
        return value.to_owned();
    };
    let Ok(exponent) = exponent.parse::<i64>() else {
        return value.to_owned();
    };
    if exponent.abs() > 100 {
        return value.to_owned();
    }
    let (sign, unsigned) = match mantissa.strip_prefix('-') {
        Some(unsigned) => ("-", unsigned),
        None => ("", mantissa.strip_prefix('+').unwrap_or(mantissa)),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let digits = format!("{whole}{fraction}");
    let point = whole.len() as i64 + exponent;
    let plain = if point <= 0 {
        format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize))
    } else if point as usize >= digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        let (whole, fraction) = digits.split_at(point as usize);
        format!("{whole}.{fraction}")
    };
    format!("{sign}{plain}")
}

/// The value bound for a parameter: numbers as plain decimals, the rest as
/// written. NULL stays NULL.
fn parameter_value(parameter: &QueryParameter) -> Option<String> {
    let value = parameter.value.as_deref()?;
    Some(match parameter.kind {
        ParameterKind::Number => plain_number(value),
        // `datetime2` reads `2024-02-29 10:00` but not `2024-02-29T10:00`.
        ParameterKind::Timestamp => value.replacen('T', " ", 1),
        _ => value.to_owned(),
    })
}

/// What a parameter's marker becomes (ADR-0023): a read of the bound
/// parameter `@Pn`, cast to the parameter's type. The value never enters
/// the SQL text.
fn parameter_reference(index: usize, parameter: &QueryParameter) -> String {
    let bound = format!("@P{}", index + 1);
    match parameter.kind {
        ParameterKind::Text => bound,
        ParameterKind::Number => {
            // A NULL keeps ten decimals, as a number of any scale may.
            let scale = parameter_value(parameter)
                .map_or(10, |value| {
                    value
                        .split_once('.')
                        .map_or(0, |(_, fraction)| fraction.len())
                })
                .min(38);
            format!("CAST({bound} AS decimal(38,{scale}))")
        }
        ParameterKind::Boolean => format!("CAST({bound} AS bit)"),
        ParameterKind::Date => format!("CAST({bound} AS date)"),
        ParameterKind::Timestamp => format!("CAST({bound} AS datetime2(7))"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn markers_become_bound_parameters_and_values_stay_out_of_the_sql() {
        let sql = "SELECT :who, :amount, :e, :flag, :day, :at, :none";
        let request = QueryRequest {
            parameters: vec![
                marked(
                    sql,
                    "who",
                    ParameterKind::Text,
                    Some("x'; DROP TABLE t; --"),
                ),
                marked(sql, "amount", ParameterKind::Number, Some("-12.345")),
                marked(sql, "e", ParameterKind::Number, Some("1.5e3")),
                marked(sql, "flag", ParameterKind::Boolean, Some("false")),
                marked(sql, "day", ParameterKind::Date, Some("2024-02-29")),
                marked(
                    sql,
                    "at",
                    ParameterKind::Timestamp,
                    Some("2024-02-29 10:00"),
                ),
                marked(sql, "none", ParameterKind::Number, None),
            ],
            ..QueryRequest::new(sql, ExecutionOptions::default())
        };
        assert_eq!(request.validate(), Ok(()));
        let bound = request.with_references(parameter_reference);
        assert_eq!(
            bound,
            "SELECT @P1, CAST(@P2 AS decimal(38,3)), CAST(@P3 AS decimal(38,0)), \
             CAST(@P4 AS bit), CAST(@P5 AS date), CAST(@P6 AS datetime2(7)), \
             CAST(@P7 AS decimal(38,10))"
        );
        assert!(!bound.contains("DROP"));
        let values: Vec<_> = request.parameters.iter().map(parameter_value).collect();
        assert_eq!(values[2].as_deref(), Some("1500"));
        assert_eq!(values[6], None);
    }

    #[test]
    fn numbers_lose_their_exponent_but_no_digit() {
        assert_eq!(plain_number("12.5"), "12.5");
        assert_eq!(plain_number("1e3"), "1000");
        assert_eq!(plain_number("-1.25E-3"), "-0.00125");
        assert_eq!(plain_number("+12.345e1"), "123.45");
        assert_eq!(plain_number("5e-1"), "0.5");
    }

    #[test]
    fn only_the_drivers_explain_text_is_explained() {
        assert_eq!(
            explained("SET SHOWPLAN_ALL ON;\nSELECT 1"),
            Some("SELECT 1")
        );
        assert_eq!(explained("set showplan_all on\nSELECT 1"), Some("SELECT 1"));
        assert_eq!(explained("SET SHOWPLAN_ALL ON"), None);
        assert_eq!(explained("SET SHOWPLAN_ALL ON;"), None);
        assert_eq!(explained("SET SHOWPLAN_ALL ONLY SELECT 1"), None);
        assert_eq!(explained("SELECT 1"), None);
        assert_eq!(explained("é"), None);
    }
}
