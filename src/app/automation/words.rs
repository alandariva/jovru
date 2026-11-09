//! What the Automation views say, in words: every rule of the
//! specification's section 8.1.1 where it applies, a run's state and cause,
//! its counters, why it was refused or waits. GPUI-free, so each sentence
//! is tested without a window; the views only lay them out.
use super::program_bits;
use jykre::automation::chain::InputProblem;
use jykre::automation::definition::{ChainProblem, Problem, RowRule};
use jykre::automation::outputs::OutputProblem;
use jykre::automation::queue::{Blocked, Limits, QueueFull};
use jykre::automation::{
    Cause, JobKind, Phase, PrepareError, Progress, PublishError, Refusal, RunOutcome, RunState,
    SaveError,
};
use jykre::cell_format::{duration, size};
use jykre::parameters::ParameterProblem;
use jykre::sql::SqlDialect;
use jykre::tr;
use jykre_core::automation::RunRecord;
use jykre_storage::{ChainRef, OUTPUT_QUOTA_BYTES};
use std::time::Duration;

/// How a line reads: its colour and icon follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warning,
    Error,
}

/// One sentence of a run's account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub level: Level,
    pub text: String,
}

impl Line {
    fn new(level: Level, text: impl Into<String>) -> Self {
        Self {
            level,
            text: text.into(),
        }
    }
}

pub fn kind_name(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Sql => tr!("SQL Job"),
        JobKind::Process => tr!("Program"),
        JobKind::Chain => tr!("Chain"),
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// A run's state in one or two words. A deadline reads apart from Stop.
pub fn state_label(state: RunState, cause: Option<Cause>, elapsed: Option<Duration>) -> String {
    match (state, cause) {
        (RunState::Running, _) => tr!("Running").to_owned(),
        (RunState::Succeeded, _) => tr!("Succeeded").to_owned(),
        (RunState::Failed, Some(Cause::Timeout)) => match elapsed {
            Some(elapsed) => tr!("Timed out after {0}", duration(millis(elapsed))),
            None => tr!("Timed out").to_owned(),
        },
        (RunState::Failed, _) => tr!("Failed").to_owned(),
        (RunState::Cancelled, _) => tr!("Stopped").to_owned(),
        (RunState::Interrupted, _) => tr!("Interrupted").to_owned(),
    }
}

/// Why a run failed, in words.
pub fn cause_sentence(cause: Cause) -> &'static str {
    match cause {
        Cause::Validation => tr!("The database refused the statement."),
        Cause::Connection => tr!("The connection to the database failed or was lost."),
        Cause::Authentication => tr!("The database refused the sign-in."),
        Cause::PermissionDenied => tr!("Permission denied."),
        Cause::Database => tr!("The database reported an error."),
        Cause::Timeout => tr!("It reached its timeout and was stopped."),
        Cause::NotFound => tr!("The program or a file it needs was not found."),
        Cause::Protocol => tr!(
            "The program broke the jjp/1 protocol or ended without a terminal message, or the driver's protocol failed."
        ),
        Cause::Unsupported => tr!("The driver does not support this."),
        Cause::Internal => tr!("Jykre Studio failed internally."),
        Cause::Exit => tr!("The program exited with a nonzero code or a signal."),
        Cause::Program => tr!("The program reported a failure (fail)."),
        Cause::Output => tr!("Its outputs did not match what the job declares."),
        Cause::Input => tr!("A step's inputs did not pass the form's checks."),
        Cause::PlanChanged => tr!(
            "A connection's settings changed after the plan was confirmed, so the step did not start."
        ),
        Cause::QueueTimeout => tr!("It waited in the queue longer than 60 s."),
        Cause::QueueFull => tr!("The queue was full."),
    }
}

/// The rule of section 4.6 when a run's effect is unknown.
pub fn effect_unknown() -> &'static str {
    tr!("The change may or may not have been applied; it was not run again.")
}

pub fn transaction_rolled_back() -> &'static str {
    tr!("The statement left a transaction open; closing the run's session rolled it back.")
}

pub fn draft_run() -> &'static str {
    tr!("Draft run: the editor's unsaved definition ran.")
}

/// Whether a failure is SQLite's one writer per file: a database error
/// whose code is `SQLITE_BUSY` or `SQLITE_LOCKED` (an extended code keeps
/// the primary one in its low byte).
pub fn is_one_writer(
    cause: Option<Cause>,
    code: Option<&str>,
    dialect: Option<SqlDialect>,
) -> bool {
    cause == Some(Cause::Database)
        && dialect == Some(SqlDialect::Sqlite)
        && code
            .and_then(|code| code.trim().parse::<i64>().ok())
            .is_some_and(|code| matches!(code & 0xff, 5 | 6))
}

pub fn one_writer() -> &'static str {
    tr!(
        "SQLite has one writer per file: another session (a tab, or another run) was writing to the same file, and this run gave up after waiting 4 s for its lock. Nothing was changed by it."
    )
}

/// The rows a SQL run kept, discarded or left unread (section 4.6).
fn counters(
    reads_only: bool,
    kept: Option<u64>,
    discarded: Option<u64>,
    affected: Option<u64>,
    truncated: bool,
) -> Vec<Line> {
    let mut lines = Vec::new();
    if let Some(kept) = kept {
        let text = if truncated && reads_only {
            tr!(
                "Kept {0} rows; the query had more, which were left unread at the row limit.",
                kept
            )
        } else if discarded.is_some_and(|discarded| discarded > 0) {
            tr!(
                "Kept {0} rows; {1} more were read and discarded to confirm that the statement finished.",
                kept,
                discarded.unwrap_or(0)
            )
        } else {
            tr!("Kept {0} rows.", kept)
        };
        lines.push(Line::new(Level::Info, text));
    }
    if let Some(affected) = affected {
        lines.push(Line::new(Level::Info, tr!("{0} rows affected.", affected)));
    }
    lines
}

fn program_lines(
    exit_code: Option<i64>,
    exit_signal: Option<i64>,
    failure: Option<&str>,
    logs: [Option<u64>; 3],
) -> Vec<Line> {
    let mut lines = Vec::new();
    if let Some(code) = exit_code {
        lines.push(Line::new(Level::Info, tr!("Exit code {0}.", code)));
    }
    if let Some(signal) = exit_signal {
        lines.push(Line::new(Level::Info, tr!("Ended by signal {0}.", signal)));
    }
    if let Some(failure) = failure {
        lines.push(Line::new(
            Level::Error,
            tr!("The program reported the failure code {0}.", failure),
        ));
    }
    let names = ["stdout", "stderr", "jjp"];
    let kept: Vec<String> = names
        .iter()
        .zip(logs)
        .filter_map(|(name, bytes)| Some(format!("{name} {}", size(bytes?))))
        .collect();
    if !kept.is_empty() {
        lines.push(Line::new(
            Level::Info,
            tr!("Logs kept: {0}.", kept.join(" · ")),
        ));
    }
    lines
}

/// Whether the run's declared outputs were stored, and why not.
pub fn outputs_stored(stored: Option<bool>) -> Option<Line> {
    match stored {
        None => None,
        Some(true) => Some(Line::new(
            Level::Info,
            tr!("Its outputs were stored; their values are deleted 7 days after the run."),
        )),
        Some(false) => Some(Line::new(
            Level::Warning,
            tr!(
                "Its outputs were not stored: together they took more than {0} bytes. They were used in memory only.",
                OUTPUT_QUOTA_BYTES
            ),
        )),
    }
}

/// What a stored run's history row says, in order.
pub fn record_lines(record: &RunRecord, dialect: Option<SqlDialect>) -> Vec<Line> {
    let mut lines = Vec::new();
    let elapsed = record
        .finished_at
        .map(|finished| finished.since(record.started_at));
    match record.state {
        RunState::Failed => {
            let state = state_label(record.state, record.cause, elapsed);
            let cause = record.cause.map_or("", cause_sentence);
            lines.push(Line::new(Level::Error, format!("{state}. {cause}")));
        }
        RunState::Cancelled => lines.push(Line::new(
            Level::Warning,
            tr!("Stopped by the user before it finished."),
        )),
        RunState::Interrupted => lines.push(Line::new(
            Level::Warning,
            tr!("Interrupted: Jykre Studio closed while it ran. It was not run again."),
        )),
        RunState::Succeeded => lines.push(Line::new(Level::Success, tr!("Succeeded."))),
        RunState::Running => lines.push(Line::new(Level::Info, tr!("Running."))),
    }
    if is_one_writer(record.cause, record.server_code.as_deref(), dialect) {
        lines.push(Line::new(Level::Warning, one_writer()));
    }
    if record.effect_unknown {
        lines.push(Line::new(Level::Warning, effect_unknown()));
    }
    if record.job_id.is_none() && record.chain_run_id.is_none() {
        lines.push(Line::new(
            Level::Info,
            tr!("Draft run, or its job was deleted since."),
        ));
    }
    if record.kind == JobKind::Sql {
        let count = |value: Option<i64>| value.and_then(|value| u64::try_from(value).ok());
        lines.extend(counters(
            record.reads_only,
            count(record.rows_delivered),
            count(record.rows_discarded),
            count(record.rows_affected),
            record.truncated,
        ));
        if let Some(code) = &record.server_code {
            lines.push(Line::new(Level::Info, tr!("Server code {0}.", code)));
        }
    } else {
        let bytes = |value: Option<i64>| value.and_then(|value| u64::try_from(value).ok());
        lines.extend(program_lines(
            record.exit_code,
            record.exit_signal,
            record.failure_code.as_deref(),
            [
                bytes(record.log_bytes_stdout),
                bytes(record.log_bytes_stderr),
                bytes(record.log_bytes_jjp),
            ],
        ));
    }
    lines.extend(outputs_stored(record.outputs_stored));
    lines
}

/// What a run that just ended says, with what only memory holds: the
/// database's message, the transaction rolled back, how Stop ended a
/// program.
pub fn outcome_lines(outcome: &RunOutcome, dialect: Option<SqlDialect>) -> Vec<Line> {
    let mut lines = Vec::new();
    match outcome.state {
        RunState::Failed => {
            let state = state_label(outcome.state, outcome.cause, Some(outcome.elapsed));
            let cause = outcome.cause.map_or("", cause_sentence);
            lines.push(Line::new(Level::Error, format!("{state}. {cause}")));
        }
        RunState::Cancelled => lines.push(Line::new(
            Level::Warning,
            tr!("Stopped after {0}.", duration(millis(outcome.elapsed))),
        )),
        _ => lines.push(Line::new(
            Level::Success,
            tr!("Succeeded in {0}.", duration(millis(outcome.elapsed))),
        )),
    }
    if let Some(error) = &outcome.error {
        lines.push(Line::new(Level::Error, error.message.clone()));
    }
    let code = outcome
        .sql
        .as_ref()
        .and_then(|sql| sql.server_code.as_deref());
    if is_one_writer(outcome.cause, code, dialect) {
        lines.push(Line::new(Level::Warning, one_writer()));
    }
    if outcome.effect_unknown {
        lines.push(Line::new(Level::Warning, effect_unknown()));
    }
    if let Some(sql) = &outcome.sql {
        if sql.transaction_rolled_back {
            lines.push(Line::new(Level::Warning, transaction_rolled_back()));
        }
        lines.extend(counters(
            sql.reads_only,
            Some(sql.data.row_count() as u64),
            Some(sql.rows_discarded),
            sql.rows_affected,
            sql.truncated,
        ));
        if let Some(code) = &sql.server_code {
            lines.push(Line::new(Level::Info, tr!("Server code {0}.", code)));
        }
    }
    if let Some(program) = &outcome.program {
        match program_bits::killed(program) {
            Some(true) => lines.push(Line::new(
                Level::Warning,
                tr!("The program did not end within 5 s of being asked, so its process tree was killed."),
            )),
            Some(false) => lines.push(Line::new(
                Level::Info,
                tr!("The program ended on its own after it was asked to stop."),
            )),
            None => {}
        }
        if let Some(text) = program_bits::start_error(program) {
            lines.push(Line::new(Level::Error, text));
        }
        if let Some(text) = program_bits::protocol_problem(program) {
            lines.push(Line::new(Level::Error, text));
        }
        lines.extend(program_lines(
            program.exit_code,
            program.exit_signal,
            program.failure_code.as_deref(),
            program_bits::log_sizes(program),
        ));
    }
    if let Some(Err(problem)) = &outcome.outputs {
        lines.push(Line::new(Level::Error, output_problem(problem)));
    }
    lines.extend(outputs_stored(outcome.outputs_stored));
    if outcome.history_failed {
        lines.push(Line::new(
            Level::Warning,
            tr!(
                "Its end could not be written to the history; the next start marks it interrupted. It is never run again."
            ),
        ));
    }
    lines
}

pub fn output_problem(problem: &OutputProblem) -> String {
    match problem {
        OutputProblem::NoRow => {
            tr!("The result had no rows and the rule asks for exactly one.").to_owned()
        }
        OutputProblem::SeveralRows => {
            tr!("The result had several rows and the rule does not take the first.").to_owned()
        }
        OutputProblem::MissingColumn { name, column } => tr!(
            "The output {0} reads column {1}, which the result does not have.",
            name,
            column
        ),
        OutputProblem::Missing { name } => tr!("The output {0} was not given.", name),
        OutputProblem::Undeclared { name } => {
            tr!(
                "The program gave the output {0}, which is not declared.",
                name
            )
        }
        OutputProblem::WrongKind { name, .. } => {
            tr!("The value of the output {0} is not of its type.", name)
        }
        OutputProblem::ValueTooLarge { name } => {
            tr!("The value of the output {0} is larger than 64 KiB.", name)
        }
        OutputProblem::TooLarge => tr!("The outputs together are larger than 256 KiB.").to_owned(),
    }
}

/// Which limit a queued run met, the runs that hold it, its place and the
/// time it may still wait (section 9.3.6).
pub fn queued(position: usize, left: Duration, blocked: &Blocked, holders: &[String]) -> String {
    let limit = match blocked {
        Blocked::Global { limit, .. } => tr!("at most {0} runs at once", limit),
        Blocked::Connection { .. } => tr!("at most 1 run per connection").to_owned(),
    };
    let held = if holders.is_empty() {
        String::new()
    } else {
        format!(" {}", tr!("Held by: {0}.", holders.join(", ")))
    };
    tr!(
        "Queued, place {0}: waiting for {1}.{2} It leaves the queue in {3} if it cannot start.",
        position,
        limit,
        held,
        duration(millis(left).div_ceil(1000) * 1000)
    )
}

/// What an active run is doing.
pub fn progress(progress: &Progress, kind: JobKind) -> String {
    let phase = match progress.phase {
        Phase::Admitted => tr!("Starting…"),
        Phase::Opening => tr!("Opening a session…"),
        Phase::Executing => tr!("Running the statement…"),
        Phase::Completing => {
            tr!("Reading the rest of the result to confirm the statement finished…")
        }
        Phase::Starting => tr!("Starting the program…"),
        Phase::Running => tr!("The program is running…"),
        Phase::Stopping if kind == JobKind::Process => {
            tr!("Stopping… the program was asked to end and is killed after 5 s.")
        }
        Phase::Stopping => tr!("Stopping… the driver is bringing the session back."),
    };
    let mut text = phase.to_owned();
    if progress.rows > 0 {
        text.push(' ');
        text.push_str(&tr!("{0} rows read.", progress.rows));
    }
    if let Some(percent) = progress.percent {
        text.push(' ');
        text.push_str(&tr!("{0} %", percent));
    }
    if let Some(stage) = &progress.stage {
        text.push_str(" · ");
        text.push_str(stage);
    }
    text
}

fn parameter_problem(problem: &ParameterProblem) -> &'static str {
    match problem {
        ParameterProblem::Missing => tr!("required"),
        ParameterProblem::Invalid => tr!("not a value of its type"),
        ParameterProblem::NotAChoice => tr!("not one of its choices"),
    }
}

pub fn input_problems(problems: &[InputProblem]) -> String {
    let parts: Vec<String> = problems
        .iter()
        .map(|problem| {
            format!(
                "{} ({})",
                problem.input,
                parameter_problem(&problem.problem)
            )
        })
        .collect();
    tr!("Fill in the inputs marked: {0}.", parts.join(", "))
}

pub fn prepare_error(error: &PrepareError) -> String {
    match error {
        PrepareError::Statements { found: 0 } => {
            tr!("The job holds no statement: write the one statement it runs.").to_owned()
        }
        PrepareError::Statements { found } => tr!(
            "The job holds {0} statements; a job runs exactly one. Remove the extra statements marked in the editor.",
            found
        ),
        PrepareError::Parameters(errors) => {
            let parts: Vec<String> = errors
                .iter()
                .map(|error| format!("{} ({})", error.name, parameter_problem(&error.problem)))
                .collect();
            tr!("Fill in the inputs marked: {0}.", parts.join(", "))
        }
        PrepareError::Options(error) => error.message.clone(),
    }
}

/// Why a run did not run. Nothing was sent and nothing was recorded.
pub fn refusal(refusal: &Refusal) -> String {
    let reason = match refusal {
        Refusal::Unavailable => {
            tr!("Automation cannot run here now (another window holds it, or it is off).")
                .to_owned()
        }
        Refusal::Invalid(error) => prepare_error(error),
        Refusal::Inputs(problems) => input_problems(problems),
        Refusal::NoConnection => {
            tr!("The job's connection is not available. Choose one in the editor.").to_owned()
        }
        Refusal::QueueFull(QueueFull::Global { limit }) => tr!(
            "The queue is full: {0} runs already wait. Try again when one has started.",
            limit
        ),
        Refusal::QueueFull(QueueFull::Connection { limit, .. }) => tr!(
            "The queue is full for this connection: {0} runs already wait for it.",
            limit
        ),
        Refusal::QueueTimeout => tr!(
            "It waited in the queue for 60 s without a free place, so it left the queue without running."
        )
        .to_owned(),
        Refusal::Withdrawn => tr!("Stopped before it started (in the queue, or while its file was checked): it never ran.").to_owned(),
        Refusal::ShuttingDown => tr!("Jykre Studio is closing.").to_owned(),
        Refusal::History => tr!(
            "Its history row could not be written, so nothing was sent."
        )
        .to_owned(),
        Refusal::Program(refused) => program_bits::refused(refused).0,
        Refusal::Chain(problems) => {
            let parts: Vec<String> = problems.iter().map(chain_problem).collect();
            parts.join(" ")
        }
        Refusal::Missing => {
            tr!("Something the run needs is no longer stored (a version was deleted).").to_owned()
        }
    };
    tr!("Not run: {0} Refused runs are not recorded.", reason)
}

/// The file a refused program run names when it changed since its version
/// was published: the view offers **Publish**.
pub fn changed_file(refusal: &Refusal) -> Option<String> {
    match refusal {
        Refusal::Program(refused) => program_bits::refused(refused).1,
        _ => None,
    }
}

pub fn problem(problem: &Problem) -> String {
    match problem {
        Problem::NoName => tr!("Give the job a name.").to_owned(),
        Problem::NoConnection => tr!("Choose the connection the job runs on.").to_owned(),
        Problem::NoSql => tr!("Write the statement the job runs.").to_owned(),
        Problem::NoProgram => tr!("Name the program to start.").to_owned(),
        Problem::ShellScript => tr!(
            ".bat and .cmd files run through cmd.exe, so they cannot be the program. Name cmd.exe itself to choose a shell knowingly."
        )
        .to_owned(),
        Problem::NoTimeout => tr!("The timeout must be more than 0 s.").to_owned(),
        Problem::OutputName(name) => tr!(
            "The output name {0} is not valid or repeats another (letters, digits and _).",
            name
        ),
        Problem::OutputColumn(name) => {
            tr!("The output {0} needs a column position from 1.", name)
        }
        Problem::NeedsProtocol => {
            tr!("Only a jjp/1 program takes inputs and returns outputs.").to_owned()
        }
        Problem::EnvironmentName(name) => tr!(
            "The environment variable name {0} is empty or holds = or NUL.",
            name
        ),
        Problem::StepCount(count) => tr!("A chain has 2 to 16 steps; this one has {0}.", count),
        Problem::StepId(id) => tr!(
            "The step ID {0} must be unique and use lowercase letters, digits and _.",
            id
        ),
        Problem::StepVersion(id) => tr!("Choose a published version for the step {0}.", id),
    }
}

pub fn chain_problem(problem: &ChainProblem) -> String {
    match problem {
        ChainProblem::UnknownVersion { step } if step.is_empty() => {
            tr!("Each step needs a job and a published version.").to_owned()
        }
        ChainProblem::UnknownVersion { step } => {
            tr!("Step {0}: its job or version no longer exists.", step)
        }
        ChainProblem::NestedChain { step } => {
            tr!("Step {0}: a chain cannot be a step of another chain.", step)
        }
        ChainProblem::Unbound { step, input } => tr!(
            "Step {0}: the required input {1} has no source and no default.",
            step,
            input
        ),
        ChainProblem::UnknownInput { step, input } => {
            tr!("Step {0}: the version has no input {1}.", step, input)
        }
        ChainProblem::UnknownChainInput {
            step,
            input,
            source,
        } => tr!(
            "Step {0}: the input {1} reads the chain input {2}, which does not exist.",
            step,
            input,
            source
        ),
        ChainProblem::ForwardReference {
            step,
            input,
            source,
        } => tr!(
            "Step {0}: the input {1} reads the step {2}, which is not an earlier step.",
            step,
            input,
            source
        ),
        ChainProblem::UnknownOutput {
            step,
            input,
            source,
            output,
        } => tr!(
            "Step {0}: the input {1} reads the output {3} of {2}, which that version does not declare.",
            step,
            input,
            source,
            output
        ),
        ChainProblem::KindMismatch { step, input } => tr!(
            "Step {0}: the source of {1} is of another type (only a text input takes any type).",
            step,
            input
        ),
        ChainProblem::InvalidConstant { step, input } => tr!(
            "Step {0}: the constant of {1} is not a value of its type or one of its choices.",
            step,
            input
        ),
    }
}

fn chain_names(chains: &[ChainRef]) -> String {
    chains
        .iter()
        .map(|chain| chain.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn save_error(error: &SaveError) -> String {
    match error {
        SaveError::Unavailable => {
            tr!("Automation is read-only or off here, so nothing can be saved.").to_owned()
        }
        SaveError::Problems(problems) => problems.iter().map(problem).collect::<Vec<_>>().join(" "),
        SaveError::Chain(problems) => problems
            .iter()
            .map(chain_problem)
            .collect::<Vec<_>>()
            .join(" "),
        SaveError::InUse(chains) => tr!(
            "Refused: the chains {0} use it. Change those chains first.",
            chain_names(chains)
        ),
        SaveError::Scheduled => tr!(
            "Refused: the job's schedule runs this version. Point the schedule at another version, or delete it, first."
        )
        .to_owned(),
        SaveError::Conflict => tr!(
            "The job changed or was deleted elsewhere since it was opened. Close the tab and open it again."
        )
        .to_owned(),
        SaveError::Storage(message) => tr!("Could not write the job: {0}", message),
    }
}

pub fn publish_error(error: &PublishError) -> String {
    match error {
        PublishError::Save(error) => save_error(error),
        PublishError::Chain => tr!("A chain is not published; its steps are.").to_owned(),
        PublishError::Program(refused) => program_bits::publish_refused(refused.as_ref()),
    }
}

pub fn row_rule_name(rule: RowRule) -> &'static str {
    match rule {
        RowRule::ExactlyOne => tr!("Exactly one row"),
        RowRule::AtMostOne => tr!("At most one row"),
        RowRule::First => tr!("The first row"),
    }
}

/// What a row rule does with zero, one and several rows (section 9.3.2).
pub fn row_rule_cases(rule: RowRule) -> [&'static str; 3] {
    match rule {
        RowRule::ExactlyOne => [
            tr!("No rows: the run fails (cause Output)."),
            tr!("One row: its values."),
            tr!("Several rows: the run fails (cause Output)."),
        ],
        RowRule::AtMostOne => [
            tr!("No rows: every output is NULL."),
            tr!("One row: its values."),
            tr!("Several rows: the run fails (cause Output)."),
        ],
        RowRule::First => [
            tr!("No rows: every output is NULL."),
            tr!("One row: its values."),
            tr!("Several rows: the first row, in the result's order."),
        ],
    }
}

// ---------------------------------------------------------------------------
// The rules the editor states (section 8.1.1).

pub fn limits_note() -> String {
    let limits = Limits::default();
    tr!(
        "At most {0} runs at once and {1} per connection. A run beyond them waits in a queue of at most {2} runs ({3} per connection) for at most {4}; a full queue refuses the run.",
        limits.global,
        limits.per_connection,
        limits.queue_global,
        limits.queue_per_connection,
        duration(millis(limits.queue_wait))
    )
}

pub fn timeout_note(timeout_ms: u64, default_ms: u64) -> String {
    let effective = duration(timeout_ms);
    if timeout_ms == default_ms {
        tr!(
            "Effective timeout: {0} (the default). A run that reaches it ends as Timed out, not as Stopped.",
            effective
        )
    } else {
        tr!(
            "Effective timeout: {0}. A run that reaches it ends as Timed out, not as Stopped.",
            effective
        )
    }
}

pub fn chain_timeout_note(timeout_ms: u64) -> String {
    tr!(
        "The whole chain stops after {0} (default 3,600 s); each step keeps its own job's timeout too.",
        duration(timeout_ms)
    )
}

pub fn one_statement_note() -> &'static str {
    tr!("A job runs exactly one statement, in a session of its own, in autocommit.")
}

pub fn refused_note() -> &'static str {
    tr!(
        "Refused runs are not recorded: a run stopped before it started (a missing value, more than one statement, a full queue) leaves no history."
    )
}

pub fn outputs_note() -> String {
    tr!(
        "Storing outputs keeps their values on this computer: at most {0} bytes per run (a run over it stores none and says so), values for 7 days, their names, types and sizes for 90 days with the run. Outputs are not for passwords or tokens.",
        OUTPUT_QUOTA_BYTES
    )
}

pub fn production_outputs(connection: &str) -> String {
    tr!(
        "{0} is a production connection. Every run of this job will store its outputs' values on this computer for 7 days.",
        connection
    )
}

pub fn logs_note() -> &'static str {
    tr!(
        "Program logs (stdout, stderr and jjp/1 messages) are kept on this computer for 7 days, at most 1 MiB per stream. Do not print passwords, tokens or personal data."
    )
}

/// How Stop ends a program (Q-17).
pub fn stop_note() -> &'static str {
    tr!(
        "Stop asks the program to end (jjp/1's cancel, or a signal to its process group on Unix) and kills its whole process tree if it has not ended 5 s later. On Windows a program without jjp/1 cannot be asked: Stop waits the 5 s, then kills the tree."
    )
}

pub fn stdin_note() -> &'static str {
    tr!(
        "stdin is reserved: a jjp/1 program reads its token from its first line; a program without a protocol gets end of file at once."
    )
}

pub fn environment_note(denylist: &[&str]) -> String {
    tr!(
        "The program inherits Jykre Studio's environment, minus {0} and the names you unset here. Anything else in the environment, secrets included, reaches the program.",
        denylist.join(", ")
    )
}

pub fn trusted_note() -> &'static str {
    tr!(
        "External code runs with your user's privileges and is not sandboxed. Grants protect Jykre Studio's own APIs, not your computer."
    )
}

pub fn imports_note() -> &'static str {
    tr!(
        "Only the entrypoint is checked, by its SHA-256: local imports, node_modules and the runtime are neither copied nor checked."
    )
}

pub fn protocol_note() -> &'static str {
    tr!(
        "Only a jjp/1 program takes inputs and returns outputs. A program without a protocol runs with its fixed arguments, and a chain cannot read it."
    )
}

pub fn working_directory_note() -> &'static str {
    tr!("Empty: the entrypoint's folder. The folder must exist when the run starts.")
}

pub fn versions_note() -> &'static str {
    tr!(
        "A chain runs a published version, never the draft. Editing the job changes only its draft; a version a chain names cannot be deleted."
    )
}

pub fn pinned(path: &str, sha256: &str) -> String {
    tr!("Expects the file {0} with {1}.", path, sha256)
}

/// The trusted-code warning before the first run of a program's revision
/// or version (Q-10).
pub fn trusted_warning(program: &str) -> String {
    tr!(
        "{0} runs for the first time in this version. {1} {2}",
        program,
        trusted_note(),
        logs_note()
    )
}

/// A stored time as `YYYY-MM-DD HH:MM UTC`, as the history and the
/// versions list show it.
pub fn utc(at: jykre::automation::UtcMillis) -> String {
    let seconds = at.0.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (proleptic Gregorian).
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let of_era = shifted - era * 146_097;
    let year_of_era = (of_era - of_era / 1_460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * of_year + 2) / 153;
    let day = of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        of_day / 3_600,
        (of_day % 3_600) / 60
    )
}

/// The quit question while runs are active (section 5.2).
pub fn quit(names: &[String]) -> (String, String) {
    let title = if names.len() == 1 {
        tr!("A run is active: {0}", names[0])
    } else {
        tr!("{0} runs are active", names.len())
    };
    let description = tr!(
        "Closing Jykre Studio stops them and empties the queue: {0}. A stopped write may or may not have been applied; nothing is run again. Closing waits up to about 11 s for them to stop.",
        names.join(", ")
    );
    (title, description)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jykre::automation::{JobId, RunId, UtcMillis};
    use jykre_core::ConnectionId;
    use jykre_core::automation::RunOrigin;

    fn record(kind: JobKind) -> RunRecord {
        RunRecord {
            id: RunId::new(),
            kind,
            job_id: Some(JobId::new()),
            job_revision: Some(1),
            job_version: None,
            chain_run_id: None,
            step_index: None,
            origin: RunOrigin::Manual,
            state: RunState::Succeeded,
            cause: None,
            effect_unknown: false,
            reads_only: true,
            fingerprint: String::new(),
            connection_id: Some(ConnectionId::new()),
            connection_revision: Some(1),
            rows_delivered: None,
            rows_discarded: None,
            rows_affected: None,
            truncated: false,
            server_code: None,
            exit_code: None,
            exit_signal: None,
            process_group: None,
            process_started: None,
            program_fingerprint: None,
            failure_code: None,
            log_bytes_stdout: None,
            log_bytes_stderr: None,
            log_bytes_jjp: None,
            outputs_stored: None,
            schedule_id: None,
            due_at: None,
            catch_up: false,
            started_at: UtcMillis(0),
            finished_at: Some(UtcMillis(300_000)),
        }
    }

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn a_timeout_reads_apart_from_stop() {
        crate::app::source_text();
        assert_eq!(
            state_label(
                RunState::Failed,
                Some(Cause::Timeout),
                Some(Duration::from_secs(300))
            ),
            "Timed out after 5 min 0 s"
        );
        assert_eq!(state_label(RunState::Cancelled, None, None), "Stopped");
        assert_eq!(
            state_label(RunState::Failed, Some(Cause::Database), None),
            "Failed"
        );
        let mut timed_out = record(JobKind::Sql);
        timed_out.state = RunState::Failed;
        timed_out.cause = Some(Cause::Timeout);
        timed_out.effect_unknown = true;
        let lines = record_lines(&timed_out, None);
        assert!(lines[0].text.starts_with("Timed out after 5 min 0 s."));
        assert!(texts(&lines).contains(&effect_unknown()));
        let mut stopped = timed_out.clone();
        stopped.state = RunState::Cancelled;
        stopped.cause = None;
        assert_eq!(
            record_lines(&stopped, None)[0].text,
            "Stopped by the user before it finished."
        );
    }

    #[test]
    fn counters_tell_rows_kept_from_rows_discarded_or_left_unread() {
        crate::app::source_text();
        let mut query = record(JobKind::Sql);
        query.rows_delivered = Some(1000);
        query.truncated = true;
        assert!(texts(&record_lines(&query, None)).contains(
            &"Kept 1000 rows; the query had more, which were left unread at the row limit."
        ));
        let mut write = record(JobKind::Sql);
        write.reads_only = false;
        write.rows_delivered = Some(1000);
        write.rows_discarded = Some(250);
        write.rows_affected = Some(1250);
        let lines = record_lines(&write, None);
        assert!(texts(&lines).contains(
            &"Kept 1000 rows; 250 more were read and discarded to confirm that the statement finished."
        ));
        assert!(texts(&lines).contains(&"1250 rows affected."));
    }

    #[test]
    fn sqlite_one_writer_is_named_only_for_its_codes() {
        crate::app::source_text();
        assert!(is_one_writer(
            Some(Cause::Database),
            Some("5"),
            Some(SqlDialect::Sqlite)
        ));
        // SQLITE_BUSY_TIMEOUT keeps SQLITE_BUSY in its low byte.
        assert!(is_one_writer(
            Some(Cause::Database),
            Some("773"),
            Some(SqlDialect::Sqlite)
        ));
        assert!(!is_one_writer(
            Some(Cause::Database),
            Some("19"),
            Some(SqlDialect::Sqlite)
        ));
        assert!(!is_one_writer(
            Some(Cause::Database),
            Some("5"),
            Some(SqlDialect::Postgres)
        ));
        let mut busy = record(JobKind::Sql);
        busy.state = RunState::Failed;
        busy.cause = Some(Cause::Database);
        busy.server_code = Some("5".into());
        assert!(texts(&record_lines(&busy, Some(SqlDialect::Sqlite))).contains(&one_writer()));
    }

    #[test]
    fn programs_say_their_exit_failure_logs_and_outputs() {
        crate::app::source_text();
        let mut run = record(JobKind::Process);
        run.state = RunState::Failed;
        run.cause = Some(Cause::Program);
        run.exit_code = Some(3);
        run.failure_code = Some("no_rows".into());
        run.log_bytes_stdout = Some(2048);
        run.outputs_stored = Some(false);
        run.job_id = None;
        let lines = record_lines(&run, None);
        let all = texts(&lines);
        assert!(all.contains(&"Exit code 3."));
        assert!(all.contains(&"The program reported the failure code no_rows."));
        assert!(all.contains(&"Logs kept: stdout 2 KB."));
        assert!(all.contains(&"Draft run, or its job was deleted since."));
        assert!(all.iter().any(|line| line.contains("were not stored")));
    }

    #[test]
    fn a_queued_run_names_its_limit_holders_place_and_time() {
        crate::app::source_text();
        let text = queued(
            2,
            Duration::from_millis(44_100),
            &Blocked::Connection {
                connection: ConnectionId::new(),
                holders: vec![1],
            },
            &["Nightly export".into()],
        );
        assert_eq!(
            text,
            "Queued, place 2: waiting for at most 1 run per connection. Held by: Nightly export. It leaves the queue in 45.0 s if it cannot start."
        );
        assert!(
            refusal(&Refusal::QueueFull(QueueFull::Global { limit: 8 }))
                .contains("The queue is full: 8 runs already wait")
        );
        assert!(refusal(&Refusal::QueueTimeout).ends_with("Refused runs are not recorded."));
    }

    #[test]
    fn the_rules_state_the_limits_of_the_engine() {
        crate::app::source_text();
        assert_eq!(
            limits_note(),
            "At most 2 runs at once and 1 per connection. A run beyond them waits in a queue of at most 8 runs (4 per connection) for at most 1 min 0 s; a full queue refuses the run."
        );
        assert!(outputs_note().contains("4096 bytes"));
        assert!(timeout_note(300_000, 300_000).contains("5 min 0 s (the default)"));
        assert!(environment_note(&["JYKRE_*", "OTEL_*"]).contains("minus JYKRE_*, OTEL_*"));
        assert_eq!(
            row_rule_cases(RowRule::AtMostOne)[2],
            "Several rows: the run fails (cause Output)."
        );
        assert_eq!(
            row_rule_cases(RowRule::First)[2],
            "Several rows: the first row, in the result's order."
        );
        let (title, description) = quit(&["Lookup".into(), "Export".into()]);
        assert_eq!(title, "2 runs are active");
        assert!(description.contains("Lookup, Export"));
        assert!(description.contains("stops them"));
    }

    #[test]
    fn stored_times_read_as_utc_dates() {
        crate::app::source_text();
        assert_eq!(utc(UtcMillis(0)), "1970-01-01 00:00 UTC");
        // 2026-11-08T18:02:13Z.
        assert_eq!(utc(UtcMillis(1_794_160_933_000)), "2026-11-08 18:02 UTC");
        // A leap day.
        assert_eq!(utc(UtcMillis(951_782_400_000)), "2000-02-29 00:00 UTC");
    }

    #[test]
    fn deleting_what_a_chain_uses_names_the_chains() {
        crate::app::source_text();
        let text = save_error(&SaveError::InUse(vec![
            ChainRef {
                id: JobId::new(),
                name: "Nightly".into(),
            },
            ChainRef {
                id: JobId::new(),
                name: "Weekly".into(),
            },
        ]));
        assert_eq!(
            text,
            "Refused: the chains Nightly, Weekly use it. Change those chains first."
        );
        assert_eq!(
            prepare_error(&PrepareError::Statements { found: 3 }),
            "The job holds 3 statements; a job runs exactly one. Remove the extra statements marked in the editor."
        );
    }
}
