//! The executor of one program run (section 9.1.5): start, drain, decide,
//! end the tree, report.
use super::prepare::{Launch, Prepared, StartError, add_jjp_address};
#[cfg(unix)]
use super::recovery::start_time;
use super::{
    Decider, Exit, LogBytes, ProgramPhase, ProgramProgress, ProgramReport, ProgramRequest,
    ProgressSink, StopResult, Timings,
};
use crate::automation::definition::Protocol;
use crate::automation::jjp::{JjpEvent, JjpProgress, JjpRun, JjpSession, ProtocolProblem};
use crate::automation::logs::{self, LogSize, LogWriter, Stream};
use jykre_core::CancellationToken;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
#[cfg(windows)]
use process_wrap::tokio::{CreationFlags, JobObject};
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tracing::Instrument;

/// Bytes read from a pipe at a time; with the log's buffer, all a stream
/// ever holds in memory.
const DRAIN_CHUNK: usize = 16 * 1024;

/// The token line is far smaller than any pipe's buffer, so writing it never
/// waits; the bound only guards against a system that says otherwise.
const STDIN_WRITE: Duration = Duration::from_secs(1);

/// `kill` and `killpg` report a process or group that no longer exists with
/// `ESRCH`, 3 on Linux, macOS and the BSDs.
#[cfg(unix)]
const ESRCH: i32 = 3;

#[cfg(unix)]
const SIGTERM: i32 = 15;

/// How often the rest of a stopped group is looked for in `/proc`.
#[cfg(target_os = "linux")]
const GROUP_POLL: Duration = Duration::from_millis(50);

/// No console window for the program (`CREATE_NO_WINDOW`).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Runs one program to its end and reports what happened; never fails.
///
/// The engine passes what [`super::prepare`] returned, the form's values,
/// the shared [`crate::automation::jjp::JjpHost`], the data folder, the
/// deadline and the run's Stop token, and gets a [`ProgramReport`]
/// whose [`ProgramReport::verdict`] is the run's outcome. The first of the
/// program's exit, Stop, the deadline and a protocol failure decides
/// (section 5.5). Stop and the deadline ask the program to end (`cancel`
/// under `jjp/1`, `SIGTERM` to its group on Unix otherwise) and kill the
/// whole tree after [`Timings::grace`]; a protocol failure kills it at once.
/// When the program exits on its own, whatever it left in its group or job
/// is killed with it. Dropping the future kills the tree too.
pub async fn run(request: ProgramRequest) -> ProgramReport {
    let span = crate::client_span!(
        "automation_program",
        process.exit.code = tracing::field::Empty
    );
    let report = execute(request).instrument(span.clone()).await;
    if let Some(Exit::Code(code)) = report.exit {
        span.record("process.exit.code", code);
    }
    if let Some(cause) = report.verdict().cause {
        span.record("error.type", cause.as_str());
        span.record("otel.status_code", "error");
    }
    report
}

/// The first event the run's owner observed.
enum Decision {
    Exit(io::Result<ExitStatus>),
    Stop,
    Deadline,
    Protocol(ProtocolProblem),
}

enum Event {
    Exited(io::Result<ExitStatus>),
    Stop,
    Deadline,
    Jjp(Option<JjpEvent>),
}

async fn execute(request: ProgramRequest) -> ProgramReport {
    let started = Instant::now();
    let ProgramRequest {
        run,
        prepared,
        inputs,
        host,
        data_dir,
        deadline,
        stop,
        progress,
        started: on_started,
        timings,
    } = request;
    let mut progress = Progress::new(progress);
    progress.phase(ProgramPhase::Starting);
    let launch = match prepared {
        Prepared::Ready(launch) => launch,
        Prepared::CannotStart { error, protocol } => {
            return not_started(empty_report(run, protocol, None), error, started);
        }
    };
    let mut report = empty_report(run, launch.protocol, launch.fingerprint.clone());

    // A run that cannot keep its logs does not start.
    let stdout_log = LogWriter::create(&data_dir, run, Stream::Stdout).await;
    let stderr_log = LogWriter::create(&data_dir, run, Stream::Stderr).await;
    let (stdout_log, stderr_log) = match (stdout_log, stderr_log) {
        (Ok(stdout), Ok(stderr)) => (stdout, stderr),
        (stdout, stderr) => {
            let kind = stdout.err().or(stderr.err()).map(|error| error.kind());
            tracing::warn!(run = %run, error_kind = ?kind, "program logs could not be created");
            discard_logs(&data_dir, run).await;
            return not_started(report, StartError::Logs, started);
        }
    };

    let mut session = None;
    let mut environment = launch.environment.clone();
    if launch.protocol == Protocol::Jjp1 {
        let registered = host
            .register(JjpRun {
                run,
                inputs,
                outputs: launch.outputs.clone(),
                deadline,
                data_dir: data_dir.clone(),
            })
            .await;
        match registered {
            Ok(registered) => {
                add_jjp_address(&mut environment, registered.address(), cfg!(windows));
                session = Some(registered);
            }
            Err(error) => {
                tracing::warn!(run = %run, error_kind = ?error.kind(), "jjp/1 host could not take the run");
                discard_logs(&data_dir, run).await;
                return not_started(report, StartError::Listener, started);
            }
        }
    }

    let child = match spawn(&launch, &environment) {
        Ok(child) => child,
        Err(error) => {
            let kind = StartError::of(&error);
            tracing::debug!(run = %run, error_kind = ?error.kind(), "program could not start");
            if let Some(session) = session {
                session.finish(Duration::ZERO).await;
            }
            drop((stdout_log, stderr_log));
            discard_logs(&data_dir, run).await;
            return not_started(report, kind, started);
        }
    };
    drop(environment);
    let pid = child.id();
    let mut tree = Tree {
        child,
        group: pid,
        ended: false,
    };
    #[cfg(unix)]
    if let Some(pid) = pid {
        report.process_group = Some(i64::from(pid));
        report.process_started = start_time(pid).await;
    }
    if let Some(on_started) = &on_started {
        on_started(report.process_group, report.process_started.clone());
    }

    let drains = Drains::start(
        tree.child.stdout().take(),
        tree.child.stderr().take(),
        stdout_log,
        stderr_log,
    );
    if let Some(mut stdin) = tree.child.stdin().take() {
        if let Some(session) = &session {
            let line = session.token_line();
            let written = tokio::time::timeout(STDIN_WRITE, stdin.write_all(line.as_bytes())).await;
            if !matches!(written, Ok(Ok(()))) {
                // The program ended or closed its stdin: it will not say
                // `hello`, and the protocol's rules decide.
                tracing::debug!(run = %run, "program did not take its token line");
            }
        }
        // End of file: under `none` at once, under `jjp/1` after the token.
        drop(stdin);
    }
    progress.phase(ProgramPhase::Running);
    tracing::debug!(run = %run, pid = ?tree.group, "program started");

    let mut jjp_open = session.is_some();
    let decision = loop {
        let event = {
            let leader = tree.child.inner_mut().wait();
            tokio::select! {
                biased;
                status = leader => Event::Exited(status),
                () = stop.cancelled() => Event::Stop,
                () = tokio::time::sleep_until(deadline.into()) => Event::Deadline,
                event = next_event(&mut session), if jjp_open => Event::Jjp(event),
            }
        };
        match event {
            Event::Exited(status) => break Decision::Exit(status),
            Event::Stop => break Decision::Stop,
            Event::Deadline => break Decision::Deadline,
            Event::Jjp(Some(JjpEvent::Failed(problem))) => break Decision::Protocol(problem),
            Event::Jjp(Some(JjpEvent::Hello)) => progress.hello(),
            Event::Jjp(Some(JjpEvent::Progress(update))) => progress.update(update),
            // Recorded by the session; the run still ends with the exit.
            Event::Jjp(Some(JjpEvent::Terminal(_))) => {}
            Event::Jjp(None) => jjp_open = false,
        }
    };

    match decision {
        Decision::Exit(status) => {
            report.decided_by = Decider::Exit;
            report.exit = Some(exit_of(&status));
            // What the program left in its group or job ends with it, so it
            // cannot hold the pipes open; whether anything was left does not
            // change the outcome.
            tree.ended = kill_tree(&mut tree).is_ok();
        }
        Decision::Stop | Decision::Deadline => {
            report.decided_by = if matches!(decision, Decision::Stop) {
                Decider::Stop
            } else {
                Decider::Deadline
            };
            progress.phase(ProgramPhase::Stopping);
            match &session {
                Some(session) => session.cancel(),
                None => ask_to_end(tree.child.as_ref()),
            }
            let (exit, ended) = end_within_grace(&mut tree, timings).await;
            report.exit = Some(exit);
            report.stop = Some(ended);
        }
        Decision::Protocol(problem) => {
            report.decided_by = Decider::Protocol;
            report.protocol_problem = Some(problem);
            progress.phase(ProgramPhase::Stopping);
            let (exit, ended) = kill_now(&mut tree, timings).await;
            report.exit = Some(exit);
            report.stop = Some(ended);
        }
    }

    // The pipes and the connection end with the tree; a descendant that left
    // the group still holding them is abandoned after the drain grace.
    let wait = if report.decided_by == Decider::Exit {
        timings.drain
    } else {
        Duration::ZERO
    };
    let ((stdout, stderr), summary) = tokio::join!(drains.finish(timings.drain), async {
        match session {
            Some(session) => Some(session.finish(wait).await),
            None => None,
        }
    });
    report.logs.stdout = Some(stdout.bytes);
    report.logs.stderr = Some(stderr.bytes);
    if let Some(summary) = summary {
        report.hello = summary.hello;
        report.terminal = summary.terminal;
        report.logs.jjp = Some(summary.log.bytes);
        if report.protocol_problem.is_none() {
            report.protocol_problem = summary.problem;
        }
        if report.decided_by == Decider::Exit
            && report.terminal.is_none()
            && report.protocol_problem.is_none()
        {
            report.protocol_problem = Some(ProtocolProblem::NoTerminal);
        }
    }
    report.elapsed = started.elapsed();
    let (exit_code, signal) = match report.exit {
        Some(Exit::Code(code)) => (Some(code), None),
        Some(Exit::Signal(signal)) => (None, Some(signal)),
        _ => (None, None),
    };
    tracing::debug!(
        run = %run,
        decided_by = ?report.decided_by,
        exit_code = ?exit_code,
        signal = ?signal,
        stop = ?report.stop,
        problem = report.protocol_problem.map(ProtocolProblem::as_str),
        stdout_bytes = stdout.bytes,
        stderr_bytes = stderr.bytes,
        stdout_dropped = stdout.dropped_bytes,
        stderr_dropped = stderr.dropped_bytes,
        elapsed_ms = u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
        "program run ended"
    );
    report
}

fn empty_report(
    run: jykre_core::automation::RunId,
    protocol: Protocol,
    fingerprint: Option<String>,
) -> ProgramReport {
    ProgramReport {
        run,
        protocol,
        decided_by: Decider::Exit,
        exit: None,
        stop: None,
        terminal: None,
        protocol_problem: None,
        hello: false,
        logs: LogBytes::default(),
        process_group: None,
        process_started: None,
        program_fingerprint: fingerprint,
        elapsed: Duration::ZERO,
    }
}

fn not_started(mut report: ProgramReport, error: StartError, started: Instant) -> ProgramReport {
    report.decided_by = Decider::NotStarted(error);
    report.elapsed = started.elapsed();
    report
}

/// A run that never started keeps no logs.
async fn discard_logs(data_dir: &Path, run: jykre_core::automation::RunId) {
    if let Err(error) = logs::delete_logs(data_dir, run).await {
        tracing::warn!(run = %run, error_kind = ?error.kind(), "empty program logs could not be removed");
    }
}

/// Starts the program without a shell, in its own process group (Unix) or
/// Job Object without a console window (Windows), with every pipe captured.
fn spawn(
    launch: &Launch,
    environment: &[(OsString, OsString)],
) -> io::Result<Box<dyn ChildWrapper>> {
    let mut command = tokio::process::Command::new(&launch.program);
    command
        .args(&launch.arguments)
        .current_dir(&launch.working_directory)
        .env_clear()
        .envs(
            environment
                .iter()
                .map(|(name, value)| (name.as_os_str(), value.as_os_str())),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut wrapped = CommandWrap::from(command);
    #[cfg(unix)]
    wrapped.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    {
        wrapped.wrap(JobObject);
        // `PROCESS_CREATION_FLAGS` belongs to the `windows` crate, which
        // this crate does not name: start from its default and set the bits.
        let mut flags = CreationFlags(Default::default());
        flags.0.0 = CREATE_NO_WINDOW;
        wrapped.wrap(flags);
    }
    // With a Job Object this also kills the job when its handle closes,
    // so Windows ends the tree if Studio crashes.
    wrapped.wrap(KillOnDrop);
    wrapped.spawn()
}

/// The program's tree. Dropped before it ended (the run's future was
/// dropped), it is killed.
struct Tree {
    child: Box<dyn ChildWrapper>,
    /// The leader's PID, which is the group's ID on Unix; kept because the
    /// child forgets it once reaped.
    group: Option<u32>,
    ended: bool,
}

impl Drop for Tree {
    fn drop(&mut self) {
        if !self.ended {
            // Best effort: the run is being abandoned and has no one to
            // report a failure to; the leader is also killed on drop.
            let _ = self.child.start_kill();
        }
    }
}

/// Kills every process in the group (Unix, `SIGKILL`) or job (Windows).
/// `Ok(false)` when nothing was left to kill, which only Unix can tell.
fn kill_tree(tree: &mut Tree) -> io::Result<bool> {
    match tree.child.start_kill() {
        Ok(()) => Ok(true),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(ESRCH) => Ok(false),
        Err(error) => {
            tracing::warn!(error_kind = ?error.kind(), "program tree could not be killed");
            Err(error)
        }
    }
}

/// Asks the program to end without the protocol: `SIGTERM` to its group.
#[cfg(unix)]
fn ask_to_end(child: &dyn ChildWrapper) {
    if let Err(error) = child.signal(SIGTERM)
        && error.raw_os_error() != Some(ESRCH)
    {
        tracing::warn!(error_kind = ?error.kind(), "program could not be asked to end");
    }
}

/// Windows cannot be asked: `CTRL_BREAK_EVENT` needs
/// `GenerateConsoleCtrlEvent`, an FFI call the workspace's
/// `unsafe_code = "forbid"` rules out, and a console shared with the
/// program, which a GUI application starting it with `CREATE_NO_WINDOW` does
/// not have. The grace still applies: the job ends on its own or is killed.
#[cfg(not(unix))]
fn ask_to_end(_child: &dyn ChildWrapper) {}

/// Waits for the tree to end within the grace, then kills it.
async fn end_within_grace(tree: &mut Tree, timings: Timings) -> (Exit, StopResult) {
    let deadline = tokio::time::Instant::now() + timings.grace;
    match tokio::time::timeout_at(deadline, tree_ended(tree)).await {
        Ok(status) => {
            let exit = exit_of(&status);
            let ended = rest_ended(tree, deadline).await;
            (exit, ended)
        }
        Err(_) => kill_now(tree, timings).await,
    }
}

/// On Unix, the leader's exit: the rest of its group is checked after.
#[cfg(unix)]
async fn tree_ended(tree: &mut Tree) -> io::Result<ExitStatus> {
    tree.child.inner_mut().wait().await
}

/// On Windows, the whole job: the Job Object reports when its last process
/// ended.
#[cfg(not(unix))]
async fn tree_ended(tree: &mut Tree) -> io::Result<ExitStatus> {
    tree.child.wait().await
}

/// After the leader ended within the grace. On Linux the rest of the group
/// gets what is left of the grace to end too, watched in `/proc` without a
/// signal; whatever is still there then is killed and the tree counts as
/// killed. Elsewhere on Unix nothing tells whether the group is empty
/// without signalling it, so it is killed at once and counts as ended on
/// its own only if nothing was left.
#[cfg(unix)]
async fn rest_ended(tree: &mut Tree, deadline: tokio::time::Instant) -> StopResult {
    #[cfg(target_os = "linux")]
    if let Some(group) = tree.group {
        loop {
            if !super::recovery::group_running(i64::from(group)).await {
                // No running member is left to start another, and a
                // signal could only find zombies, which would read as a
                // kill.
                tree.ended = true;
                return StopResult::EndedOnItsOwn;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            tokio::time::sleep_until(deadline.min(now + GROUP_POLL)).await;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = deadline;
    let killed = kill_tree(tree);
    tree.ended = killed.is_ok();
    if matches!(killed, Ok(false)) {
        StopResult::EndedOnItsOwn
    } else {
        StopResult::Killed
    }
}

/// On Windows the whole job ended.
#[cfg(not(unix))]
async fn rest_ended(tree: &mut Tree, _deadline: tokio::time::Instant) -> StopResult {
    tree.ended = true;
    StopResult::EndedOnItsOwn
}

/// Kills the tree and waits, within the reap grace, for the leader's
/// status.
async fn kill_now(tree: &mut Tree, timings: Timings) -> (Exit, StopResult) {
    let killed = kill_tree(tree);
    let status = tokio::time::timeout(timings.reap, tree.child.inner_mut().wait()).await;
    let exit = match status {
        Ok(status) => {
            tree.ended = killed.is_ok();
            exit_of(&status)
        }
        Err(_) => {
            tracing::warn!("killed program was not reaped in time");
            Exit::Unknown
        }
    };
    let ended = if matches!(killed, Ok(false)) {
        StopResult::EndedOnItsOwn
    } else {
        StopResult::Killed
    };
    (exit, ended)
}

fn exit_of(status: &io::Result<ExitStatus>) -> Exit {
    match status {
        Ok(status) => match status.code() {
            Some(code) => Exit::Code(code),
            None => signal_of(status),
        },
        Err(_) => Exit::Unknown,
    }
}

#[cfg(unix)]
fn signal_of(status: &ExitStatus) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map_or(Exit::Unknown, Exit::Signal)
}

#[cfg(not(unix))]
fn signal_of(_status: &ExitStatus) -> Exit {
    Exit::Unknown
}

async fn next_event(session: &mut Option<JjpSession>) -> Option<JjpEvent> {
    match session {
        Some(session) => session.next().await,
        None => std::future::pending().await,
    }
}

/// Cancels a token when dropped.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// The two tasks that drain stdout and stderr into the logs. They read
/// until the end of their pipe, or until stopped; dropping this stops them.
struct Drains {
    stop: CancelOnDrop,
    stdout: JoinHandle<LogSize>,
    stderr: JoinHandle<LogSize>,
}

impl Drains {
    fn start(
        stdout: Option<tokio::process::ChildStdout>,
        stderr: Option<tokio::process::ChildStderr>,
        stdout_log: LogWriter,
        stderr_log: LogWriter,
    ) -> Self {
        let stop = CancellationToken::new();
        Self {
            stdout: tokio::spawn(drain(stdout, stdout_log, stop.clone())),
            stderr: tokio::spawn(drain(stderr, stderr_log, stop.clone())),
            stop: CancelOnDrop(stop),
        }
    }

    /// Waits up to `grace` for both pipes to end, then stops reading them;
    /// either way the logs are finished.
    async fn finish(self, grace: Duration) -> (LogSize, LogSize) {
        let Drains {
            stop,
            stdout,
            stderr,
        } = self;
        let both = async { tokio::join!(stdout, stderr) };
        tokio::pin!(both);
        let (stdout, stderr) = tokio::select! {
            biased;
            done = &mut both => done,
            () = tokio::time::sleep(grace) => {
                stop.0.cancel();
                both.await
            }
        };
        // A drain that panicked lost its sizes; its file stays as written.
        (stdout.unwrap_or_default(), stderr.unwrap_or_default())
    }
}

/// Reads a pipe into its log until the end or `stop`, always reading, so
/// the program never blocks on a full pipe; past the log's limit the bytes
/// are counted and discarded.
async fn drain(
    pipe: Option<impl AsyncRead + Unpin>,
    mut log: LogWriter,
    stop: CancellationToken,
) -> LogSize {
    if let Some(mut pipe) = pipe {
        let mut buffer = vec![0_u8; DRAIN_CHUNK];
        loop {
            let read = tokio::select! {
                biased;
                () = stop.cancelled() => break,
                read = pipe.read(&mut buffer) => read,
            };
            match read {
                Ok(0) | Err(_) => break,
                Ok(read) => log.write(buffer.get(..read).unwrap_or_default()).await,
            }
        }
    }
    log.finish().await
}

/// Progress for the interface; phase changes and `hello` are sent at once,
/// `progress` as the session coalesced it (at most ten a second).
struct Progress {
    sink: Option<ProgressSink>,
    state: ProgramProgress,
}

impl Progress {
    fn new(sink: Option<ProgressSink>) -> Self {
        Self {
            sink,
            state: ProgramProgress {
                phase: ProgramPhase::Starting,
                hello: false,
                percent: None,
                stage: None,
            },
        }
    }

    fn phase(&mut self, phase: ProgramPhase) {
        self.state.phase = phase;
        self.emit();
    }

    fn hello(&mut self) {
        self.state.hello = true;
        self.emit();
    }

    fn update(&mut self, update: JjpProgress) {
        self.state.percent = Some(update.percent);
        self.state.stage = update.stage;
        self.emit();
    }

    fn emit(&self) {
        if let Some(sink) = &self.sink {
            sink(&self.state);
        }
    }
}
