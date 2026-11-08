//! Pure types the automation engine and its storage share
//! ([ADR-0034](../../../docs/adr/0034-automation-database-service-and-lock.md),
//! decision 7): identities, states, causes and the records of a run. No I/O.
//!
//! Records hold what ran, where, when and how it ended, never input values,
//! cells, scalars or server messages (specification, section 6.4). Declared
//! outputs ([`RunOutput`]) are the one exception, under ADR-0035.
use crate::{ConnectionId, ErrorCode, ParameterKind};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(uuid::Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                value.parse().map(Self)
            }
        }
    };
}

uuid_id!(
    /// Identity of a job (a SQL Job, an external program or a chain).
    JobId
);
uuid_id!(
    /// Identity of one run of a SQL Job or an external program, generated
    /// at admission.
    RunId
);
uuid_id!(
    /// Identity of one run of a chain, generated when its plan is confirmed.
    ChainRunId
);

/// A point in time as milliseconds since the Unix epoch, UTC. Storage writes
/// it as `YYYY-MM-DDTHH:MM:SS.mmmZ` text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UtcMillis(pub i64);

impl UtcMillis {
    pub fn now() -> Self {
        Self::from(SystemTime::now())
    }

    /// `self` moved forward by `by`, saturating.
    pub fn plus(self, by: Duration) -> Self {
        Self(
            self.0
                .saturating_add(i64::try_from(by.as_millis()).unwrap_or(i64::MAX)),
        )
    }

    /// `self` moved back by `by`, saturating.
    pub fn minus(self, by: Duration) -> Self {
        Self(
            self.0
                .saturating_sub(i64::try_from(by.as_millis()).unwrap_or(i64::MAX)),
        )
    }

    /// The time from `earlier` to `self`, zero when `earlier` is later.
    pub fn since(self, earlier: UtcMillis) -> Duration {
        Duration::from_millis(u64::try_from(self.0.saturating_sub(earlier.0)).unwrap_or(0))
    }
}

impl From<SystemTime> for UtcMillis {
    fn from(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Self(i64::try_from(after.as_millis()).unwrap_or(i64::MAX)),
            Err(before) => Self(-i64::try_from(before.duration().as_millis()).unwrap_or(i64::MAX)),
        }
    }
}

/// Stable text codes: what is stored, and what a stored code means.
macro_rules! codes {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $code:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            /// The code stored in `automation.db`.
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $code),+
                }
            }

            /// The value a stored code stands for.
            pub fn parse(code: &str) -> Option<Self> {
                match code {
                    $($code => Some($name::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

codes!(
    /// What a job is.
    JobKind {
        /// One statement through the existing drivers (specification, section 4).
        Sql => "sql",
        /// An external program started without a shell (section 9.1).
        Process => "process",
        /// A linear chain of published versions (section 9.3).
        Chain => "chain",
    }
);

codes!(
    /// The persisted state of a run or a chain run (specification, section
    /// 5.4). `Queued`, `Cancelling` and `Completing` live in memory only.
    RunState {
        Running => "running",
        Succeeded => "succeeded",
        /// With a [`Cause`].
        Failed => "failed",
        Cancelled => "cancelled",
        /// Set by recovery only: the application ended while the run was
        /// active.
        Interrupted => "interrupted",
    }
);

impl RunState {
    pub fn is_terminal(self) -> bool {
        self != RunState::Running
    }
}

codes!(
    /// Why a run failed: stable codes, stored with [`RunState::Failed`].
    /// `Timeout` is a cause, not a state.
    Cause {
        /// The driver refused the statement after admission.
        Validation => "validation",
        Connection => "connection",
        Authentication => "authentication",
        PermissionDenied => "permission_denied",
        Database => "database",
        /// The job's or the chain's deadline.
        Timeout => "timeout",
        /// A program or a file that does not exist.
        NotFound => "not_found",
        /// A driver's protocol error, or a `jjp/1` program that broke the
        /// protocol or ended without a terminal message.
        Protocol => "protocol",
        Unsupported => "unsupported",
        Internal => "internal",
        /// A program exited with a nonzero code or a signal.
        Exit => "exit",
        /// A `jjp/1` program sent `fail`.
        Program => "program",
        /// Outputs did not match the declaration.
        Output => "output",
        /// A chain step's mapped inputs did not pass the form's checks.
        Input => "input",
        /// A connection's profile changed between the confirmation and its
        /// step.
        PlanChanged => "plan_changed",
        /// A chain step waited in the queue longer than allowed.
        QueueTimeout => "queue_timeout",
        /// A chain step found the queue full.
        QueueFull => "queue_full",
    }
);

impl Cause {
    /// The cause of a driver error. A cancellation the run's owner did not
    /// ask for cannot come from the user, so it is internal.
    pub fn of(code: ErrorCode) -> Self {
        match code {
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

    /// Whether a SQL statement that failed this way may still have taken
    /// effect: the server may have finished it, or be finishing it, after
    /// the client stopped waiting. A database error, a refused login or a
    /// validation error means the statement did not run or was rolled back
    /// as a whole (it runs in autocommit).
    pub fn leaves_effect_unknown(self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::Connection | Self::Protocol | Self::Internal
        )
    }
}

codes!(
    /// Who started a run. Only manual runs exist in 0.3.0.
    RunOrigin {
        Manual => "manual",
    }
);

/// What a run's history row says, for lists and the run's details. Never a
/// value, a cell or a server message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub id: RunId,
    pub kind: JobKind,
    /// `None` for a draft run, or once its job was deleted.
    pub job_id: Option<JobId>,
    /// The job's revision at admission.
    pub job_revision: Option<i64>,
    /// The published version a chain step ran; `None` outside a chain.
    pub job_version: Option<i64>,
    pub chain_run_id: Option<ChainRunId>,
    /// 0-based; `None` outside a chain.
    pub step_index: Option<i64>,
    pub origin: RunOrigin,
    pub state: RunState,
    pub cause: Option<Cause>,
    pub effect_unknown: bool,
    pub reads_only: bool,
    pub fingerprint: String,
    // SQL runs.
    pub connection_id: Option<ConnectionId>,
    pub connection_revision: Option<i64>,
    pub rows_delivered: Option<i64>,
    pub rows_discarded: Option<i64>,
    pub rows_affected: Option<i64>,
    pub truncated: bool,
    /// SQLSTATE or vendor code, never the message.
    pub server_code: Option<String>,
    // Program runs.
    pub exit_code: Option<i64>,
    pub exit_signal: Option<i64>,
    pub process_group: Option<i64>,
    pub process_started: Option<String>,
    pub program_fingerprint: Option<String>,
    /// The `fail` code of a `jjp/1` program.
    pub failure_code: Option<String>,
    pub log_bytes_stdout: Option<i64>,
    pub log_bytes_stderr: Option<i64>,
    pub log_bytes_jjp: Option<i64>,
    /// `None`: the job does not persist outputs; `Some(false)`: they
    /// exceeded the quota; `Some(true)`: stored (ADR-0035).
    pub outputs_stored: Option<bool>,
    pub started_at: UtcMillis,
    /// `None` while `Running`.
    pub finished_at: Option<UtcMillis>,
}

/// What a chain run's history row says (ADR-0036).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainRunRecord {
    pub id: ChainRunId,
    pub job_id: Option<JobId>,
    pub job_revision: Option<i64>,
    pub origin: RunOrigin,
    pub state: RunState,
    pub cause: Option<Cause>,
    /// The ID, in the chain's definition, of the step that stopped it.
    pub failed_step: Option<String>,
    pub effect_unknown: bool,
    pub fingerprint: String,
    pub started_at: UtcMillis,
    pub finished_at: Option<UtcMillis>,
}

/// One declared output of a run, as stored under ADR-0035. `value` is
/// `None` when the output is NULL or its value expired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    /// Declaration order, from 0.
    pub position: i64,
    pub name: String,
    pub kind: ParameterKind,
    pub value: Option<String>,
    pub is_null: bool,
    /// UTF-8 length of the value.
    pub bytes: i64,
    pub value_expires_at: UtcMillis,
    pub expired: bool,
}

/// The stored text of a [`ParameterKind`].
pub fn parameter_kind_str(kind: ParameterKind) -> &'static str {
    match kind {
        ParameterKind::Text => "text",
        ParameterKind::Number => "number",
        ParameterKind::Boolean => "boolean",
        ParameterKind::Date => "date",
        ParameterKind::Timestamp => "timestamp",
    }
}

/// The [`ParameterKind`] a stored text stands for.
pub fn parse_parameter_kind(text: &str) -> Option<ParameterKind> {
    [
        ParameterKind::Text,
        ParameterKind::Number,
        ParameterKind::Boolean,
        ParameterKind::Date,
        ParameterKind::Timestamp,
    ]
    .into_iter()
    .find(|kind| parameter_kind_str(*kind) == text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_and_unknown_codes_are_refused() {
        for cause in Cause::ALL {
            assert_eq!(Cause::parse(cause.as_str()), Some(*cause));
        }
        for state in RunState::ALL {
            assert_eq!(RunState::parse(state.as_str()), Some(*state));
        }
        for kind in JobKind::ALL {
            assert_eq!(JobKind::parse(kind.as_str()), Some(*kind));
        }
        assert_eq!(Cause::parse("Timeout"), None);
        assert_eq!(RunState::parse("queued"), None);
        assert_eq!(Cause::PlanChanged.as_str(), "plan_changed");
    }

    #[test]
    fn times_convert_and_saturate() {
        let start = UtcMillis(1_000);
        assert_eq!(start.plus(Duration::from_secs(2)), UtcMillis(3_000));
        assert_eq!(start.minus(Duration::from_secs(2)), UtcMillis(-1_000));
        assert_eq!(UtcMillis(3_000).since(start), Duration::from_secs(2));
        assert_eq!(start.since(UtcMillis(3_000)), Duration::ZERO);
        assert_eq!(UtcMillis::from(UNIX_EPOCH), UtcMillis(0));
    }

    #[test]
    fn kinds_round_trip() {
        for text in ["text", "number", "boolean", "date", "timestamp"] {
            let kind = parse_parameter_kind(text).unwrap();
            assert_eq!(parameter_kind_str(kind), text);
        }
        assert_eq!(parse_parameter_kind("money"), None);
    }
}
