//! One run of an external program (specification, section 9.1;
//! [ADR-0038](../../docs/adr/0038-external-program-runs.md)).
//!
//! The engine drives a run in two steps:
//!
//! 1. Before admission, [`prepare`] checks the definition against the files
//!    as they are now: it resolves the program, the entrypoint and the
//!    working directory, hashes the entrypoint and, for a published version,
//!    refuses a file that changed ([`Refused`]: nothing runs, no history).
//!    A program that cannot start is not refused: it fails with `NotFound` or
//!    `PermissionDenied` ([`Prepared::CannotStart`]).
//! 2. After admission, [`run`] starts the program without a shell in a
//!    process group (Unix) or a Job Object (Windows), drains stdout and
//!    stderr into the run's log files ([`crate::automation::logs`]), speaks
//!    `jjp/1` through [`JjpHost`] when the job uses it, and ends the whole
//!    tree on Stop, at the deadline or on a protocol failure. It returns a
//!    [`ProgramReport`]; [`verdict`] turns it into the run's state, cause
//!    and `effect_unknown` (section 9.1.5).
//!
//! Recovery after a crash (Unix) uses [`orphan_alive`] and [`end_group`]
//! with the process group and start time the report recorded (9.1.6).
//!
//! No program path, argument, environment value, log content, token, input
//! or output reaches `tracing` or an error from this module.
mod prepare;
mod recovery;
mod run;
#[cfg(test)]
pub(crate) mod test_child;
#[cfg(test)]
mod tests;

pub use prepare::{
    ENV_DENYLIST, JJP_ADDRESS_VARIABLE, Launch, Prepared, Refused, StartError, fingerprint_file,
    is_denied, prepare, program_environment, removed_variables, resolve_program,
};
pub use recovery::{end_group, orphan_alive};
pub use run::run;

use super::definition::Protocol;
use super::jjp::{JjpHost, NamedValues, ProtocolProblem, Terminal};
use jykre_core::CancellationToken;
use jykre_core::automation::{Cause, RunId, RunState};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long Stop and the deadline wait, after asking the program to end,
/// before the tree is killed (Q-17).
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long the pipes and the `jjp/1` connection may take to reach their end
/// once the program's tree has ended. Past it a descendant that left the
/// group (`setsid`) and still holds them is abandoned, so it cannot keep
/// the run open.
pub const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// How long a killed program may take to be reaped before the run stops
/// waiting for its exit status.
pub const REAP_GRACE: Duration = Duration::from_secs(5);

/// The waits of a run. Production uses [`Timings::default`]; tests shorten
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    /// Between asking the program to end and killing its tree.
    pub grace: Duration,
    /// For the pipes and the connection to end after the tree ended.
    pub drain: Duration,
    /// For a killed program's exit status.
    pub reap: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            grace: STOP_GRACE,
            drain: DRAIN_GRACE,
            reap: REAP_GRACE,
        }
    }
}

/// What an active program run is doing, for progress only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramPhase {
    /// Creating the logs and starting the program.
    Starting,
    Running,
    /// Stop, the deadline or a protocol failure came first: the program was
    /// asked to end, or its tree is being killed (**Stopping…**).
    Stopping,
}

/// The latest progress of a run, for the interface. The stage is the
/// program's own text, so `Debug` shows only its length.
#[derive(Clone, PartialEq)]
pub struct ProgramProgress {
    pub phase: ProgramPhase,
    /// A `jjp/1` program said `hello`.
    pub hello: bool,
    /// The latest `progress` percent, 0 to 100.
    pub percent: Option<f64>,
    /// The latest `progress` stage, at most 200 characters.
    pub stage: Option<String>,
}

impl fmt::Debug for ProgramProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProgramProgress")
            .field("phase", &self.phase)
            .field("hello", &self.hello)
            .field("percent", &self.percent)
            .field(
                "stage_chars",
                &self.stage.as_ref().map(|s| s.chars().count()),
            )
            .finish()
    }
}

/// Receives progress. It is called from the run's task, at most about ten
/// times a second plus each phase change, and must return at once (send to
/// a `watch` channel, for example): the run never waits for it.
pub type ProgressSink = Arc<dyn Fn(&ProgramProgress) + Send + Sync>;

/// Receives the process group (Unix) and its leader's start time once the
/// program started. It must return at once.
pub type StartedSink = Arc<dyn Fn(Option<i64>, Option<String>) + Send + Sync>;

/// Everything one run needs. Built by the engine after admission.
pub struct ProgramRequest {
    pub run: RunId,
    /// What [`prepare`] returned for the definition.
    pub prepared: Prepared,
    /// The form's values, sent in `hello` under `jjp/1`; ignored otherwise.
    pub inputs: NamedValues,
    /// The application's `jjp/1` host; it listens only while a `jjp/1` run
    /// is registered.
    pub host: JjpHost,
    /// The data folder; logs go to `automation/logs/<run>/` under it.
    pub data_dir: PathBuf,
    /// The job's deadline; reaching it fails the run with `Timeout`.
    pub deadline: Instant,
    /// **Stop**: cancelling it cancels the run.
    pub stop: CancellationToken,
    pub progress: Option<ProgressSink>,
    /// Told the process group and its leader's start time once the program
    /// started, so the engine records them before anything can crash
    /// (section 9.1.6).
    pub started: Option<StartedSink>,
    pub timings: Timings,
}

impl fmt::Debug for ProgramRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProgramRequest")
            .field("run", &self.run)
            .field("prepared", &self.prepared)
            .field("inputs", &self.inputs)
            .field("timings", &self.timings)
            .finish_non_exhaustive()
    }
}

/// How the program's leader ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// Killed by this Unix signal.
    Signal(i32),
    /// Its status could not be read (it was never reaped in time).
    Unknown,
}

/// Which event decided the run: the first the run's owner observed
/// (section 5.5). Later events are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decider {
    /// The program never started.
    NotStarted(StartError),
    /// The program's leader exited on its own.
    Exit,
    /// **Stop**.
    Stop,
    /// The job's deadline.
    Deadline,
    /// The `jjp/1` program broke the protocol, or never said `hello`.
    Protocol,
}

/// How the tree ended after Stop, the deadline or a protocol failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopResult {
    /// It ended within the grace after being asked.
    EndedOnItsOwn,
    /// It was killed.
    Killed,
}

/// The size of each log file after the run; `None` when the file was not
/// written (the program never started, or its protocol has no `jjp.log`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogBytes {
    pub stdout: Option<u64>,
    pub stderr: Option<u64>,
    pub jjp: Option<u64>,
}

/// What happened in one run, as the executor saw it: what the engine
/// records and what [`verdict`] decides from. Holds no value but the
/// declared outputs, whose `Debug` output is redacted.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramReport {
    pub run: RunId,
    pub protocol: Protocol,
    pub decided_by: Decider,
    /// What the system said when it refused to start the program (its own
    /// words for the error, such as "Exec format error"), for the user to
    /// read. Never recorded or logged.
    pub start_message: Option<String>,
    /// The exit code or Unix signal, recorded also when Stop, the deadline
    /// or a protocol failure decided.
    pub exit: Option<Exit>,
    /// Set when the run had to end the tree.
    pub stop: Option<StopResult>,
    /// The `complete` or `fail` the host accepted, if any.
    pub terminal: Option<Terminal>,
    /// What went wrong with `jjp/1`, if anything did.
    pub protocol_problem: Option<ProtocolProblem>,
    /// The program said `hello`.
    pub hello: bool,
    pub logs: LogBytes,
    /// The process group (Unix), for recovery.
    pub process_group: Option<i64>,
    /// The group leader's start time as the system reports it (Linux:
    /// field 22 of `/proc/<pid>/stat`), for recovery.
    pub process_started: Option<String>,
    /// `sha256:` and the hex of the entrypoint that ran.
    pub program_fingerprint: Option<String>,
    pub elapsed: Duration,
}

impl ProgramReport {
    /// The run's outcome (section 9.1.5).
    pub fn verdict(&self) -> Verdict {
        verdict(
            self.protocol,
            self.decided_by,
            self.exit,
            self.terminal.as_ref(),
        )
    }

    /// The `fail` code of a `jjp/1` program, recorded with the run.
    pub fn failure_code(&self) -> Option<&str> {
        match &self.terminal {
            Some(Terminal::Fail { code }) => Some(code),
            _ => None,
        }
    }
}

/// A run's outcome: its terminal state, its cause when it failed, and
/// whether the program may have done something Studio cannot know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub state: RunState,
    pub cause: Option<Cause>,
    pub effect_unknown: bool,
}

/// The outcome table of section 9.1.5, with the arbitration of 5.5: the
/// event that `decided_by` names wins over anything after it.
///
/// | Decided by | Protocol | Terminal | Exit | Run |
/// | --- | --- | --- | --- | --- |
/// | not started | any | — | — | `Failed`: `NotFound`, `PermissionDenied`, else `Internal`; effect known |
/// | exit | `none` | — | 0 | `Succeeded` |
/// | exit | `none` | — | not 0, a signal | `Failed`, `Exit` |
/// | exit | `jjp/1` | `complete` | 0 | `Succeeded` with its outputs |
/// | exit | `jjp/1` | `complete` | not 0, a signal | `Failed`, `Exit` |
/// | exit | `jjp/1` | `fail` | any | `Failed`, `Program` |
/// | exit | `jjp/1` | none | any | `Failed`, `Protocol` |
/// | exit | any | — | unknown | `Failed`, `Internal` |
/// | Stop | any | any | any | `Cancelled` |
/// | deadline | any | any | any | `Failed`, `Timeout` |
/// | protocol | `jjp/1` | any | any | `Failed`, `Protocol` |
///
/// Every run that started and did not succeed has `effect_unknown`:
/// Studio cannot know what a program did.
pub fn verdict(
    protocol: Protocol,
    decided_by: Decider,
    exit: Option<Exit>,
    terminal: Option<&Terminal>,
) -> Verdict {
    let failed = |cause| Verdict {
        state: RunState::Failed,
        cause: Some(cause),
        effect_unknown: true,
    };
    match decided_by {
        // Stopped before it was started: nothing ran, so nothing is unknown.
        Decider::NotStarted(StartError::Stopped) => Verdict {
            state: RunState::Cancelled,
            cause: None,
            effect_unknown: false,
        },
        Decider::NotStarted(error) => Verdict {
            state: RunState::Failed,
            cause: Some(match error {
                StartError::NotFound | StartError::EntrypointNotFound => Cause::NotFound,
                StartError::PermissionDenied => Cause::PermissionDenied,
                StartError::Logs
                | StartError::Listener
                | StartError::Other
                | StartError::Stopped => Cause::Internal,
            }),
            effect_unknown: false,
        },
        Decider::Stop => Verdict {
            state: RunState::Cancelled,
            cause: None,
            effect_unknown: true,
        },
        Decider::Deadline => failed(Cause::Timeout),
        Decider::Protocol => failed(Cause::Protocol),
        Decider::Exit => {
            let exit = exit.unwrap_or(Exit::Unknown);
            if exit == Exit::Unknown {
                return failed(Cause::Internal);
            }
            let clean = exit == Exit::Code(0);
            let succeeded = Verdict {
                state: RunState::Succeeded,
                cause: None,
                effect_unknown: false,
            };
            match protocol {
                Protocol::None if clean => succeeded,
                Protocol::None => failed(Cause::Exit),
                Protocol::Jjp1 => match terminal {
                    Some(Terminal::Complete { .. }) if clean => succeeded,
                    Some(Terminal::Complete { .. }) => failed(Cause::Exit),
                    Some(Terminal::Fail { .. }) => failed(Cause::Program),
                    None => failed(Cause::Protocol),
                },
            }
        }
    }
}
