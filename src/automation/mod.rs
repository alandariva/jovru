//! Proof of concept of the automation engine's SQL Job
//! ([plan](../../docs/plans/automation-sql-job-poc.md),
//! [specification](../../docs/plans/automation-engine-specification.md)).
//!
//! GPUI-free, like `reports`, and wired into nothing: no view, no storage,
//! no new contract. A [`SqlJob`] is built in code; [`Engine::submit`] checks
//! it ([`prepare`]), admits it within [`Limits`] and gives the run one owner
//! task, which opens a new session in the job's context, applies the
//! completion rule of the specification's section 4.6, closes the session and
//! publishes exactly one terminal [`RunOutcome`]. History is kept in memory
//! only and records no value, result or server message ([`RunSummary`]).
//!
//! Whether this code becomes the base of phase 1 or is discarded is decided
//! with the plan's findings.

pub mod definition;
mod engine;
mod executor;
pub mod jjp;
pub mod logs;
pub mod process;
#[cfg(test)]
mod tests;

pub use engine::{Engine, Limits, Refusal, RunHandle, RunRequest};
pub use executor::{PrepareError, Prepared, prepare};

use crate::reports::ParameterSpec;
use crate::result::ResultData;
use jykre_core::{ConnectionId, DriverError, ErrorCode, ExecutionOptions, SessionContext};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// The deadline of a job that sets none, as for a tab
/// ([`ExecutionOptions::default`]).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Identity of one run, generated at admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(uuid::Uuid);

impl RunId {
    fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A reusable unit of database work: one statement with typed `:name`
/// inputs, bound to a connection and a pinned context (specification,
/// section 4.1). Built in code; nothing stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlJob {
    pub connection: ConnectionId,
    /// Where unqualified names resolve; the run's session opens in it.
    pub context: SessionContext,
    /// Exactly one statement; anything else is refused before a session opens.
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
        let defaults = ExecutionOptions::default();
        Self {
            connection,
            context: SessionContext::default(),
            sql: sql.into(),
            parameters: Vec::new(),
            timeout: defaults.timeout.unwrap_or(DEFAULT_TIMEOUT),
            max_rows: defaults.max_rows,
        }
    }
}

/// The state of a run (specification, section 5.4). `Interrupted` is set
/// only by recovery after a crash, which needs durable history, so the
/// proof of concept has no such state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running,
    Succeeded,
    /// With a [`Cause`].
    Failed,
    Cancelled,
}

/// Why a run failed. Stable codes, recorded with [`RunState::Failed`];
/// `Timeout` is a cause, not a state.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    Validation,
    Connection,
    Authentication,
    PermissionDenied,
    Database,
    Timeout,
    NotFound,
    Protocol,
    Unsupported,
    Internal,
}

impl Cause {
    /// The cause of a driver error. A cancellation the owner did not ask
    /// for cannot come from the user, so it is internal.
    fn of(error: &DriverError) -> Self {
        match error.code {
            ErrorCode::Validation => Self::Validation,
            ErrorCode::Connection => Self::Connection,
            ErrorCode::Authentication => Self::Authentication,
            ErrorCode::PermissionDenied => Self::PermissionDenied,
            ErrorCode::Database => Self::Database,
            ErrorCode::Timeout => Self::Timeout,
            ErrorCode::NotFound => Self::NotFound,
            ErrorCode::Protocol => Self::Protocol,
            ErrorCode::NotImplemented
            | ErrorCode::DriverNotRegistered
            | ErrorCode::DuplicateDriver => Self::Unsupported,
            _ => Self::Internal,
        }
    }

    /// Whether a statement that failed this way may still have taken
    /// effect: the server may have finished it, or be finishing it, after
    /// the client stopped waiting. A database error, a refused login or a
    /// validation error means the statement did not run or was rolled back
    /// as a whole (it runs in autocommit).
    fn leaves_effect_unknown(self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::Connection | Self::Protocol | Self::Internal
        )
    }
}

/// What an active run is doing, for progress only; never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Admitted; the session is not open yet.
    Admitted,
    Opening,
    /// The statement was sent.
    Executing,
    /// The first page arrived; the rest of a write's result is being read
    /// to confirm the statement finished (section 4.6, rule 2).
    Completing,
    /// Stop or the deadline came first; the driver is bringing the session
    /// back to a known state before it closes.
    Cancelling,
}

/// What subscribers see: progress while the run is active, then its outcome
/// once. The latest value replaces the previous one, so a slow subscriber
/// skips progress but never the outcome (section 5.6).
#[derive(Debug, Clone)]
pub enum RunStatus {
    Active { phase: Phase, rows: u64 },
    Finished(Arc<RunOutcome>),
}

/// How a run ended, with its result. Held in memory only: closing every
/// handle to the run drops the result.
#[derive(Debug)]
pub struct RunOutcome {
    pub id: RunId,
    /// [`RunState::Succeeded`], [`RunState::Failed`] or
    /// [`RunState::Cancelled`].
    pub state: RunState,
    /// Set when `state` is `Failed`.
    pub cause: Option<Cause>,
    /// The driver's error, for the user to read; never recorded.
    pub error: Option<DriverError>,
    /// The statement may have changed data although its completion was not
    /// confirmed (deadline, Stop, lost connection). Never resolved by
    /// running it again.
    pub effect_unknown: bool,
    /// The result holds fewer rows than the statement returned.
    pub truncated: bool,
    /// Rows read and discarded beyond the result's limits while confirming
    /// that a write finished.
    pub rows_discarded: u64,
    /// As the driver reports it: `None` for a statement that returns rows.
    pub rows_affected: Option<u64>,
    /// Ordered columns (duplicate names kept), rows by position, the
    /// database's text and NULL apart from the text `NULL`.
    pub data: ResultData,
    /// The statement left a transaction open (a lone `BEGIN`), which closing
    /// the run's session rolled back.
    pub transaction_rolled_back: bool,
    pub started_at: SystemTime,
    pub elapsed: Duration,
    /// For a write whose first page left rows, the time from that page to
    /// the confirmed end of its result.
    pub completion: Option<Duration>,
}

/// What the history keeps of a run: what ran, where, when and how it ended,
/// never its values, result or server messages (section 6.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub id: RunId,
    pub connection: ConnectionId,
    pub state: RunState,
    pub cause: Option<Cause>,
    pub effect_unknown: bool,
    pub truncated: bool,
    pub rows_delivered: usize,
    pub rows_affected: Option<u64>,
    /// The server's SQLSTATE or vendor code of a database error.
    pub server_code: Option<String>,
    pub started_at: SystemTime,
    /// `None` while the run is active.
    pub elapsed: Option<Duration>,
}

impl RunSummary {
    fn finished(connection: ConnectionId, outcome: &RunOutcome) -> Self {
        Self {
            id: outcome.id,
            connection,
            state: outcome.state,
            cause: outcome.cause,
            effect_unknown: outcome.effect_unknown,
            truncated: outcome.truncated,
            rows_delivered: outcome.data.row_count(),
            rows_affected: outcome.rows_affected,
            server_code: outcome
                .error
                .as_ref()
                .and_then(|error| error.server.as_ref())
                .and_then(|server| server.sqlstate.clone().or(server.vendor_code.clone())),
            started_at: outcome.started_at,
            elapsed: Some(outcome.elapsed),
        }
    }
}
