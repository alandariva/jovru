//! Every name the Automation views take from the program executor's
//! modules (`jykre::automation::process`, `jykre::automation::logs` and
//! `jykre::automation::jjp`), in one place, so they can follow those
//! modules as they change. Nothing else in `src/app/automation/` names
//! them.
//!
//! GPUI-free: what it gives back is plain data and sentences.
use jykre::automation::definition::EnvironmentSpec;
use jykre::automation::{ProgramOutcome, RunId};
use jykre::automation::{jjp, logs, process};
use jykre::tr;
use std::path::Path;

/// The variables a program never inherits (specification, section 9.1.3),
/// as the editor lists them.
pub fn denylist() -> &'static [&'static str] {
    process::ENV_DENYLIST
}

/// The variables of Jykre Studio's environment now that the program will
/// not receive: the denylist's and the job's own unset names. Reads the
/// environment's names, never its values.
pub fn removed_variables(environment: &EnvironmentSpec) -> Vec<String> {
    let mut names: Vec<String> = process::removed_variables(std::env::vars_os(), environment)
        .into_iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The most a log file keeps (ADR-0037).
pub const LOG_LIMIT: u64 = logs::LOG_LIMIT;

/// Why a program could not start, in words, and the file a run was refused
/// for when it changed since its version was published (which **Publish**
/// fixes).
pub fn refused(refused: &process::Refused) -> (String, Option<String>) {
    match refused {
        process::Refused::ProgramChanged { path } => {
            let path = path.display().to_string();
            (
                tr!(
                    "The file {0} is missing or changed since this version was published, so it does not run. Publish the job again to run the file as it is now.",
                    path
                ),
                Some(path),
            )
        }
        process::Refused::EntrypointUnreadable { path } => (
            tr!(
                "The file {0} cannot be read to check it against the published version.",
                path.display()
            ),
            None,
        ),
        process::Refused::WorkingDirectory { path } => (
            tr!(
                "The working folder {0} must be an absolute path to a folder that exists.",
                path.display()
            ),
            None,
        ),
        process::Refused::EntrypointPath { path } => (
            tr!(
                "The entrypoint {0} is relative, and there is no absolute working folder to read it from.",
                path.display()
            ),
            None,
        ),
        process::Refused::NoProgram => (tr!("Name the program to start.").to_owned(), None),
        process::Refused::ShellScript => (
            tr!(".bat and .cmd files run through cmd.exe, so they cannot be the program.")
                .to_owned(),
            None,
        ),
        process::Refused::ProgramPath => (
            tr!(
                "The program must be an absolute path or a bare name looked up on PATH (not bin/tool or ./tool)."
            )
            .to_owned(),
            None,
        ),
    }
}

/// Why publishing a program failed: its file, or a check that could not
/// run.
pub fn publish_refused(refused: Option<&process::Refused>) -> String {
    match refused {
        Some(refused) => self::refused(refused).0,
        None => tr!("The program's file could not be checked; nothing was published.").to_owned(),
    }
}

/// The bytes kept of each stream of a program run, from its outcome.
pub fn log_sizes(program: &ProgramOutcome) -> [Option<u64>; 3] {
    let sizes = &program.log_bytes;
    [sizes.stdout, sizes.stderr, sizes.jjp]
}

/// After Stop or the deadline: `Some(true)` when the tree was killed,
/// `Some(false)` when it ended on its own within the 5 s.
pub fn killed(program: &ProgramOutcome) -> Option<bool> {
    program.stop.map(|stop| stop == process::StopResult::Killed)
}

/// How a `jjp/1` program broke the protocol, in words.
pub fn protocol_problem(program: &ProgramOutcome) -> Option<String> {
    let problem = program.protocol?;
    Some(
        match problem {
            jjp::ProtocolProblem::HelloTimeout => {
                tr!("The program did not send hello within 30 s.")
            }
            jjp::ProtocolProblem::LineTooLong => {
                tr!("The program sent a message longer than 1 MiB.")
            }
            jjp::ProtocolProblem::InvalidUtf8 => tr!("The program sent text that is not UTF-8."),
            jjp::ProtocolProblem::InvalidJson => tr!("The program sent a line that is not JSON."),
            jjp::ProtocolProblem::Quota => tr!("The program sent more than jjp/1 allows."),
            jjp::ProtocolProblem::Disconnected => {
                tr!("The program closed its connection before complete or fail.")
            }
            jjp::ProtocolProblem::NoTerminal => {
                tr!("The program ended without sending complete or fail.")
            }
        }
        .to_owned(),
    )
}

/// Why the program could not start, in words.
pub fn start_error(program: &ProgramOutcome) -> Option<String> {
    let error = program.start_error?;
    Some(
        match error {
            process::StartError::NotFound => tr!("The program or its entrypoint was not found."),
            process::StartError::PermissionDenied => {
                tr!("The system refused to start the program (not executable, or no permission).")
            }
            process::StartError::Logs => tr!("The run's log files could not be created."),
            process::StartError::Listener => {
                tr!("The jjp/1 listener could not take the run.")
            }
            process::StartError::Other => tr!("The system refused to start the program."),
        }
        .to_owned(),
    )
}

/// One of a program run's log files (ADR-0037).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
    Jjp,
}

impl Stream {
    pub fn name(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
            Stream::Jjp => "jjp",
        }
    }
}

/// What a log file holds, decoded as UTF-8 with replacement, its size, and
/// whether the text is all of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Log {
    pub text: String,
    pub bytes: u64,
    pub complete: bool,
}

/// Reads one stream of a run's logs from the data folder, on the runtime,
/// only when the user opens the log. `Ok(None)` when the run has no such
/// file.
pub async fn read_log(data_dir: &Path, run: RunId, stream: Stream) -> std::io::Result<Option<Log>> {
    let stream = match stream {
        Stream::Stdout => logs::Stream::Stdout,
        Stream::Stderr => logs::Stream::Stderr,
        Stream::Jjp => logs::Stream::Jjp,
    };
    Ok(logs::read_log(data_dir, run, stream).await?.map(|log| Log {
        text: log.text,
        bytes: log.bytes,
        complete: log.complete,
    }))
}
