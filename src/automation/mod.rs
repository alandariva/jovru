//! The automation engine ([specification](../../docs/plans/automation-engine-specification.md),
//! [ADR-0034](../../docs/adr/0034-automation-database-service-and-lock.md) to
//! [ADR-0038](../../docs/adr/0038-external-program-runs.md)).
//!
//! GPUI-free, like `reports`. [`Engine`] is an application-root service: it
//! takes the data folder's lock, recovers what an earlier process left
//! `running`, admits runs within the limits and the queue
//! ([`queue::Limits`]), gives each run one owner task and records what ran,
//! where, when and how it ended in `automation.db`, never a value. A run is
//! never replayed.
//!
//! Three kinds of job run through it: a SQL Job (one statement on a session
//! of its own, [`executor`]), an external program ([`process`], with
//! [`jjp`] for programs that take inputs and return outputs) and a linear
//! chain of published versions ([`chain`]). A [`schedule`] runs a published
//! version or a chain by a rule in a time zone while the application is
//! open, in the instance holding the lock (ADR-0041).

pub mod chain;
mod chain_run;
pub mod definition;
mod engine;
mod executor;
pub mod jjp;
pub mod logs;
pub mod outputs;
pub mod process;
mod program_run;
pub mod queue;
pub mod schedule;
mod snapshot;
mod sql_run;
#[cfg(test)]
mod tests;

pub use engine::{
    Availability, ChainHandle, ChainRequest, ConnectionInput, Engine, JobRequest, OffReason,
    OverviewEntry, Plan, PlanStep, PublishError, Recovery, RunHandle, SaveError,
};
pub use executor::{PrepareError, Prepared, prepare};
pub use jykre_core::automation::{
    Cause, ChainRunId, ChainRunRecord, JobId, JobKind, MissedPolicy, OverlapPolicy, RunId,
    RunOrigin, RunRecord, RunState, ScheduleId, ScheduleOutcome, UtcMillis,
};

use crate::reports::ParameterSpec;
use crate::result::ResultData;
use chain::InputProblem;
use definition::{ChainProblem, SqlBody};
use jykre_core::{ConnectionId, DriverError, SessionContext};
use outputs::{Output, OutputProblem};
use queue::{Blocked, QueueFull};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// The deadline of a job that sets none, as for a tab
/// (`ExecutionOptions::default`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// A SQL Job as one run executes it (specification, section 4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlJob {
    pub connection: ConnectionId,
    /// Where unqualified names resolve; the run's session opens in it.
    pub context: SessionContext,
    /// Exactly one statement; anything else is refused before admission.
    pub sql: String,
    pub parameters: Vec<ParameterSpec>,
    /// The deadline; reaching it fails the run with [`Cause::Timeout`].
    pub timeout: Duration,
    /// Rows kept in the run's result.
    pub max_rows: u32,
}

impl SqlJob {
    /// A job without parameters, with the limits a tab starts with.
    pub fn new(connection: ConnectionId, sql: impl Into<String>) -> Self {
        let defaults = jykre_core::ExecutionOptions::default();
        Self {
            connection,
            context: SessionContext::default(),
            sql: sql.into(),
            parameters: Vec::new(),
            timeout: defaults.timeout.unwrap_or(DEFAULT_TIMEOUT),
            max_rows: defaults.max_rows,
        }
    }

    /// The run form of a stored SQL Job on `connection`.
    pub fn from_body(body: &SqlBody, connection: ConnectionId) -> Self {
        Self {
            connection,
            context: body.context.clone(),
            sql: body.sql.clone(),
            parameters: body.parameters.clone(),
            timeout: Duration::from_millis(body.timeout_ms.max(1)),
            max_rows: body.max_rows.max(1),
        }
    }
}

/// What an active run is doing, for progress only; never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Admitted; nothing was sent or started yet.
    Admitted,
    /// SQL: the session is opening.
    Opening,
    /// SQL: the statement was sent.
    Executing,
    /// SQL: the first page arrived; the rest of a write's result is being
    /// read to confirm the statement finished (section 4.6, rule 2).
    Completing,
    /// A program is starting.
    Starting,
    /// A program is running.
    Running,
    /// Stop or the deadline came first: the driver is bringing the session
    /// back to a known state, or the program was asked to end and is killed
    /// after 5 s.
    Stopping,
}

/// Progress of an active run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub phase: Phase,
    /// SQL: rows read so far, kept or not.
    pub rows: u64,
    /// A `jjp/1` program's latest `progress`.
    pub percent: Option<u8>,
    pub stage: Option<String>,
}

impl Progress {
    pub fn new(phase: Phase) -> Self {
        Self {
            phase,
            rows: 0,
            percent: None,
            stage: None,
        }
    }
}

/// What subscribers see of one run. The latest value replaces the previous
/// one, so a slow subscriber skips progress but never the end (section 5.6).
#[derive(Debug, Clone)]
pub enum RunStatus {
    /// Waiting in the queue, in memory only (section 9.3.6).
    Queued {
        position: usize,
        leaves_at: Instant,
        blocked: Blocked,
    },
    Active(Progress),
    Finished(Arc<RunOutcome>),
    /// It never ran and left no history (section 4.3).
    Refused(Refusal),
}

/// Why a run or a chain did not run. Nothing was sent or started and no
/// history row was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Automation cannot run here (another instance, a newer file).
    Unavailable,
    /// The SQL Job cannot run as written or with these values.
    Invalid(PrepareError),
    /// The form's values did not pass their checks.
    Inputs(Vec<InputProblem>),
    /// The job's connection is not given or not usable.
    NoConnection,
    /// The queue is full.
    QueueFull(QueueFull),
    /// It waited in the queue longer than allowed.
    QueueTimeout,
    /// Stop came while it waited in the queue.
    Withdrawn,
    /// The engine is shutting down.
    ShuttingDown,
    /// The history row could not be written, so nothing was sent.
    History,
    /// A program that cannot start: not found, `.bat`/`.cmd`, or its
    /// working folder is missing.
    Program(process::Refused),
    /// A chain's definition does not match the versions it names.
    Chain(Vec<ChainProblem>),
    /// Something the chain needs is not stored (a version was deleted).
    Missing,
}

/// The SQL side of a run's end. `Debug` shows counts, never cells.
#[derive(Default)]
pub struct SqlOutcome {
    /// What the dialect's tokens said at admission: a warning, not a
    /// boundary (section 4.5).
    pub reads_only: bool,
    /// The result holds fewer rows than the statement returned.
    pub truncated: bool,
    /// Rows read and discarded beyond the result's limits while confirming
    /// that a write finished.
    pub rows_discarded: u64,
    /// As the driver reports it: `None` for a statement that returns rows.
    pub rows_affected: Option<u64>,
    /// Ordered columns (duplicate names kept), rows by position, the
    /// database's text and NULL apart from the text `NULL`. Memory only.
    pub data: ResultData,
    /// The statement left a transaction open (a lone `BEGIN`), which closing
    /// the run's session rolled back.
    pub transaction_rolled_back: bool,
    /// For a write whose first page left rows, the time from that page to
    /// the confirmed end of its result.
    pub completion: Option<Duration>,
    /// The server's SQLSTATE or vendor code of a database error.
    pub server_code: Option<String>,
}

/// The program side of a run's end.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProgramOutcome {
    pub exit_code: Option<i64>,
    pub exit_signal: Option<i64>,
    /// A `jjp/1` program's `fail` code.
    pub failure_code: Option<String>,
    /// After Stop, the deadline or a protocol failure: whether the tree
    /// ended on its own within 5 s or was killed.
    pub stop: Option<process::StopResult>,
    /// What the run asked to start, resolved.
    pub program: Option<PathBuf>,
    pub program_fingerprint: Option<String>,
    pub log_bytes: process::LogBytes,
    /// How a `jjp/1` program broke the protocol.
    pub protocol: Option<jjp::ProtocolProblem>,
    /// Why the program could not start.
    pub start_error: Option<process::StartError>,
}

/// How a run ended. Held in memory only: closing every handle to the run
/// drops its result. `Debug` never shows a cell, a value or a server
/// message.
pub struct RunOutcome {
    pub id: RunId,
    pub kind: JobKind,
    /// `Succeeded`, `Failed` or `Cancelled`.
    pub state: RunState,
    /// Set when `state` is `Failed`.
    pub cause: Option<Cause>,
    /// The driver's error, for the user to read; never recorded.
    pub error: Option<DriverError>,
    /// The statement or program may have changed something although its
    /// completion was not confirmed. Never resolved by running it again.
    pub effect_unknown: bool,
    pub sql: Option<SqlOutcome>,
    pub program: Option<ProgramOutcome>,
    /// The declared outputs, when the job declares any and it got that far.
    pub outputs: Option<Result<Vec<Output>, OutputProblem>>,
    /// ADR-0035: `None` when the job does not persist its outputs.
    pub outputs_stored: Option<bool>,
    /// The terminal state could not be written; recovery will mark the run
    /// interrupted at the next start. The run is never sent again.
    pub history_failed: bool,
    pub started_at: SystemTime,
    pub elapsed: Duration,
}

impl RunOutcome {
    /// The outputs, when the run produced them.
    pub fn output_values(&self) -> Option<&[Output]> {
        match &self.outputs {
            Some(Ok(outputs)) => Some(outputs),
            _ => None,
        }
    }
}

/// What subscribers see of one chain run.
#[derive(Debug, Clone)]
pub enum ChainStatus {
    /// The step at `step` (from 0) is being prepared, queued or active;
    /// `run` is its run, whose own status shows its progress.
    Running {
        step: usize,
        run: Option<RunId>,
    },
    Finished(Arc<ChainOutcome>),
    Refused(Refusal),
}

/// How a chain run ended.
#[derive(Debug)]
pub struct ChainOutcome {
    pub id: ChainRunId,
    pub state: RunState,
    pub cause: Option<Cause>,
    /// The step that stopped the chain, by its ID in the definition.
    pub failed_step: Option<String>,
    pub effect_unknown: bool,
    /// Each step that ran, in order; later steps never started.
    pub steps: Vec<Arc<RunOutcome>>,
    /// The step IDs, all of them, in order.
    pub step_ids: Vec<String>,
    /// The step's mapped inputs that failed (cause `Input`).
    pub input_problem: Option<InputProblem>,
    pub history_failed: bool,
    pub started_at: SystemTime,
    pub elapsed: Duration,
}

impl ChainOutcome {
    /// The last step's outputs, when every step succeeded.
    pub fn outputs(&self) -> Option<&[Output]> {
        (self.state == RunState::Succeeded)
            .then(|| self.steps.last())
            .flatten()
            .and_then(|step| step.output_values())
    }
}

impl std::fmt::Debug for SqlOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlOutcome")
            .field("reads_only", &self.reads_only)
            .field("truncated", &self.truncated)
            .field("rows_kept", &self.data.row_count())
            .field("columns", &self.data.column_count())
            .field("rows_discarded", &self.rows_discarded)
            .field("rows_affected", &self.rows_affected)
            .field("transaction_rolled_back", &self.transaction_rolled_back)
            .field("completion", &self.completion)
            .field("server_code", &self.server_code)
            .finish()
    }
}

impl std::fmt::Debug for RunOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunOutcome")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("state", &self.state)
            .field("cause", &self.cause)
            .field("error_code", &self.error.as_ref().map(|error| error.code))
            .field("effect_unknown", &self.effect_unknown)
            .field("sql", &self.sql)
            .field("program", &self.program)
            .field(
                "outputs",
                &self
                    .outputs
                    .as_ref()
                    .map(|outputs| outputs.as_ref().map(Vec::len)),
            )
            .field("outputs_stored", &self.outputs_stored)
            .field("history_failed", &self.history_failed)
            .field("elapsed", &self.elapsed)
            .finish_non_exhaustive()
    }
}
