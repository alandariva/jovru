//! A SQL Job's statement on a session of its own: one statement, bound
//! values, and success only once the statement is confirmed complete
//! (specification, sections 4.2–4.6).
use super::{Phase, Progress, RunStatus, SqlJob};
use crate::backend;
use crate::parameters::ParameterError;
use crate::reports::params;
use crate::result::{ResultBuilder, ResultData};
use crate::sql::{self, SqlDialect};
use jykre_core::{
    CancellationToken, ColumnInfo, Driver, DriverError, DriverOutcome, ExecutionOptions,
    QueryRequest, ResolvedConnection, Row, RowSink, SinkControl, TransactionStatus,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Rows per page read while confirming that a write finished. Larger than
/// a result page because nothing is kept: fewer round trips.
const COMPLETION_PAGE: u32 = 10_000;

/// Why a job cannot run, found before any session opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    /// The dialect's splitter did not find exactly one statement.
    Statements { found: usize },
    /// Missing or malformed values, once per name, in marker order.
    Parameters(Vec<ParameterError>),
    /// The job's limits are not valid options (a zero timeout).
    Options(DriverError),
}

/// A job checked and bound for one run: the request carries the values
/// apart from the SQL text, which is sent as written.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub(super) request: QueryRequest,
    /// Read by the dialect's tokens: the completion rule's first case.
    pub(super) reads_only: bool,
}

impl Prepared {
    /// Whether the statement only reads, by [`sql::reads_only`]: a warning,
    /// not a boundary (section 4.5).
    pub fn reads_only(&self) -> bool {
        self.reads_only
    }

    /// The request the driver receives.
    pub fn request(&self) -> &QueryRequest {
        &self.request
    }
}

/// Checks `job` for `dialect` and binds `values` (what the form holds for
/// each name, as for a report): exactly one statement, every marker with a
/// valid value of its kind. Markers inside strings, comments, quoted names
/// and dollar quotes are not markers, and PostgreSQL's `::` is a cast.
pub fn prepare(
    job: &SqlJob,
    dialect: SqlDialect,
    values: &HashMap<String, String>,
) -> Result<Prepared, PrepareError> {
    let found = sql::statements(&job.sql, Some(dialect))
        .into_iter()
        .filter(|range| {
            job.sql
                .get(range.clone())
                .is_some_and(|text| !text.trim().is_empty())
        })
        .count();
    if found != 1 {
        return Err(PrepareError::Statements { found });
    }
    let options = ExecutionOptions {
        max_rows: job.max_rows,
        timeout: Some(job.timeout),
        context: job.context.clone(),
    };
    let request = params::requests(&job.sql, Some(dialect), &job.parameters, values, &options)
        .map_err(PrepareError::Parameters)?
        .into_iter()
        .next()
        .ok_or(PrepareError::Statements { found: 0 })?;
    request.validate().map_err(PrepareError::Options)?;
    Ok(Prepared {
        reads_only: sql::reads_only(&request.sql, Some(dialect)),
        request,
    })
}

/// What the executor saw, for the run's owner to decide the outcome.
#[derive(Debug, Default)]
pub(super) struct Execution {
    /// The statement reached the driver. Before that, nothing can have
    /// taken effect.
    pub sent: bool,
    /// The driver reported the end of the statement's result: its effect,
    /// if any, is final.
    pub confirmed: bool,
    pub error: Option<DriverError>,
    pub data: ResultData,
    pub truncated: bool,
    pub rows_discarded: u64,
    pub rows_affected: Option<u64>,
    pub completion: Option<Duration>,
    pub transaction_rolled_back: bool,
}

/// Where an execution reports while it runs: its progress, and the moment
/// its statement is confirmed complete.
pub(super) struct Report<'a> {
    pub status: &'a watch::Sender<RunStatus>,
    pub confirmed: &'a AtomicBool,
}

/// `confirmed` is set the moment the driver reports the end of the
/// statement's result, before the transaction check and the close, so the
/// run's owner knows a write finished even if Stop comes while the session
/// closes.
///
/// Runs `prepared` on a new session opened in its context, the same way a
/// report panel's session opens ([`Driver::session_connection`]), and
/// always closes it. `cancel` stops the work (Stop or the deadline, as the
/// owner decided); `deadline` bounds every driver call as its timeout.
///
/// A statement that only reads keeps up to its row limit and memory
/// ceiling and stops there. Any other statement keeps the same rows but
/// never stops its result early: the rest is read with [`Session::fetch`]
/// into a sink that counts and discards, until the driver reports the end.
/// Asking a driver to stop a write's result would let it cancel the
/// statement, and a page is no proof that it finished.
///
/// [`Session::fetch`]: jykre_core::Session::fetch
pub(super) async fn execute(
    driver: Arc<dyn Driver>,
    input: ResolvedConnection,
    prepared: &Prepared,
    memory_ceiling: usize,
    cancel: CancellationToken,
    deadline: Instant,
    report: Report<'_>,
) -> Execution {
    let Report { status, confirmed } = report;
    let mut execution = Execution::default();
    let request = &prepared.request;
    let input = match driver.session_connection(&input, &request.options.context) {
        Ok(input) => input,
        Err(error) => {
            execution.error = Some(error);
            return execution;
        }
    };
    publish(status, Phase::Opening, 0);
    let span = crate::client_span!("open_session");
    let mut session =
        match tracing::Instrument::instrument(driver.open_session(&input, cancel.clone()), span)
            .await
        {
            Ok(session) => session,
            Err(error) => {
                execution.error = Some(error);
                return execution;
            }
        };
    let mut request = request.clone();
    request.options.timeout = Some(remaining(deadline));
    publish(status, Phase::Executing, 0);
    execution.sent = true;
    let mut sink = JobSink::new(
        request.options.max_rows,
        memory_ceiling,
        !prepared.reads_only,
    );
    match session.execute(&request, &mut sink, cancel.clone()).await {
        Ok(outcome) if prepared.reads_only => {
            execution.confirmed = true;
            confirmed.store(true, Ordering::SeqCst);
            execution.rows_affected = outcome.rows_affected;
            // The rest stays unread; closing the session discards it.
            execution.truncated = outcome.has_more || outcome.truncated;
        }
        Ok(first) => {
            let completed = complete(
                &mut *session,
                &request.options,
                first,
                &mut sink,
                &cancel,
                deadline,
                status,
            )
            .await;
            execution.completion = completed.started.map(|started| started.elapsed());
            match completed.last {
                Ok(last) => {
                    execution.confirmed = true;
                    confirmed.store(true, Ordering::SeqCst);
                    confirmed.store(true, Ordering::SeqCst);
                    execution.rows_affected = completed.rows_affected;
                    execution.truncated = last.truncated;
                }
                Err(error) => execution.error = Some(error),
            }
        }
        Err(error) => execution.error = Some(error),
    }
    let (data, discarded, limited) = sink.finish();
    execution.data = data;
    execution.rows_discarded = discarded;
    execution.truncated |= limited;
    // A statement that opened a transaction (`BEGIN` alone) leaves it to
    // the close, which rolls it back. Only a write can, and its result has
    // been read to the end, so asking discards nothing.
    if execution.confirmed && !prepared.reads_only && session.is_open() {
        match session.transaction(cancel.clone()).await {
            Ok(Some(TransactionStatus::Idle)) | Ok(None) => {}
            Ok(Some(_)) => execution.transaction_rolled_back = true,
            Err(error) => {
                tracing::debug!(error_code = ?error.code, "job session's transaction state unavailable");
            }
        }
    }
    backend::close_session(Some(session)).await;
    execution
}

/// How reading a write's result to its end went.
struct Completed {
    /// When the first page left rows to read.
    started: Option<Instant>,
    rows_affected: Option<u64>,
    /// The last page's outcome, or why reading stopped.
    last: Result<DriverOutcome, DriverError>,
}

/// Reads the rest of a write's result after its first page, until the
/// driver reports the end (rule 2 of section 4.6).
async fn complete(
    session: &mut dyn jykre_core::Session,
    options: &ExecutionOptions,
    first: DriverOutcome,
    sink: &mut JobSink,
    cancel: &CancellationToken,
    deadline: Instant,
    status: &watch::Sender<RunStatus>,
) -> Completed {
    let mut rows_affected = first.rows_affected;
    let mut last = first;
    let mut started = None;
    while last.has_more {
        let started = *started.get_or_insert_with(Instant::now);
        publish(status, Phase::Completing, sink.rows());
        let page = ExecutionOptions {
            max_rows: COMPLETION_PAGE,
            timeout: Some(remaining(deadline)),
            context: options.context.clone(),
        };
        match session.fetch(&page, sink, cancel.clone()).await {
            Ok(outcome) => {
                rows_affected = outcome.rows_affected.or(rows_affected);
                last = outcome;
            }
            Err(error) => {
                tracing::debug!(
                    error_code = ?error.code,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "job result not read to its end"
                );
                return Completed {
                    started: Some(started),
                    rows_affected,
                    last: Err(error),
                };
            }
        }
    }
    Completed {
        started,
        rows_affected,
        last: Ok(last),
    }
}

/// Time left before `deadline`, never zero: a zero timeout is not a valid
/// option, and the owner stops the run at the deadline anyway.
fn remaining(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1))
}

fn publish(status: &watch::Sender<RunStatus>, phase: Phase, rows: u64) {
    status.send_replace(RunStatus::Active(Progress {
        rows,
        ..Progress::new(phase)
    }));
}

/// Keeps the rows a result may hold. For a write (`drains`), it never asks
/// the driver to stop: rows past the limits are counted and dropped.
struct JobSink {
    builder: ResultBuilder,
    drains: bool,
    /// The builder stopped keeping rows.
    full: bool,
    kept: u64,
    discarded: u64,
}

impl JobSink {
    fn new(max_rows: u32, memory_ceiling: usize, drains: bool) -> Self {
        Self {
            builder: ResultBuilder::new(max_rows, memory_ceiling),
            drains,
            full: false,
            kept: 0,
            discarded: 0,
        }
    }

    /// Rows read so far, kept or not.
    pub(super) fn rows(&self) -> u64 {
        self.kept + self.discarded
    }

    /// The rows kept, the rows dropped, and whether a limit dropped any.
    fn finish(self) -> (ResultData, u64, bool) {
        let (data, limit) = self.builder.finish();
        (data, self.discarded, limit.is_some() || self.discarded > 0)
    }
}

impl RowSink for JobSink {
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> SinkControl {
        self.builder.columns(columns)
    }

    fn row(&mut self, row: Row) -> SinkControl {
        if !self.full && self.builder.row(row) == SinkControl::Continue {
            self.kept += 1;
            return SinkControl::Continue;
        }
        self.full = true;
        if self.drains {
            self.discarded += 1;
            SinkControl::Continue
        } else {
            SinkControl::Stop
        }
    }
}
