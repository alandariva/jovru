//! Every name the Automation views take from the program executor's
//! modules (`jykre::automation::process`, `jykre::automation::logs` and
//! `jykre::automation::jjp`), in one place, so they can follow those
//! modules as they change. Nothing else in `src/app/automation/` names
//! them.
//!
//! GPUI-free: what it gives back is plain data and sentences.
use jykre::automation::command_line;
pub use jykre::automation::command_line::{Command, SplitError};
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

/// A program and its arguments as the one command line the editor shows:
/// nothing for a job that names no program yet.
pub fn command_text(program: &str, arguments: &[String]) -> String {
    if program.is_empty() && arguments.is_empty() {
        String::new()
    } else {
        command_line::join(program, arguments)
    }
}

/// The command line as typed, read into the program and its arguments.
pub fn read_command(line: &str) -> Result<Command, SplitError> {
    command_line::split(line)
}

/// Why a command line cannot be read, in words.
pub fn command_problem(error: SplitError) -> String {
    match error {
        SplitError::UnclosedQuote(quote) => tr!(
            "The command opens a quote ({0}) and never closes it. Close it, or remove it.",
            quote
        ),
    }
}

/// How the command is started, where it is typed (section 9.1.2).
pub fn command_note() -> &'static str {
    tr!(
        "Started without a shell. The first word is the program: an absolute path, or a name looked up on PATH when the run starts. Each other word is one argument, passed as it is; quotes ('a b' or \"a b\") keep several words as one."
    )
}

/// A word a shell would act on is passed to the program as it is.
pub fn shell_syntax_note(word: &str) -> String {
    tr!(
        "{0} reaches the program as an argument: there is no shell, so pipes, redirections, && and $NAME do nothing. To use them, start a shell knowingly, for example sh -c '…'.",
        word
    )
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

/// Why the program could not start, in words: which file is missing, by
/// the name the job gives it, and what the system said when it refused.
pub fn start_error(program: &ProgramOutcome) -> Option<String> {
    let error = program.start_error?;
    let named = program.named.as_str();
    let system = program.start_message.as_deref();
    Some(match (error, system) {
        // The file was there when it was checked, and the system still
        // could not find something: a script's interpreter, most often.
        (process::StartError::NotFound, Some(system)) => tr!(
            "The system could not start {0}: {1}. If it is a script, the interpreter its first line names may not be installed.",
            named,
            system
        ),
        (process::StartError::NotFound, None) if Path::new(named).is_absolute() => {
            tr!("The program {0} does not exist.", named)
        }
        (process::StartError::NotFound, None) => tr!(
            "The program {0} was not found: no folder of PATH holds an executable file of that name. Give its full path, or check that it is installed and that the PATH Jykre Studio started with reaches it.",
            named
        ),
        (process::StartError::EntrypointNotFound, _) => tr!(
            "The entrypoint {0} does not exist.",
            program.entrypoint.as_deref().unwrap_or_default()
        ),
        (process::StartError::PermissionDenied, Some(system)) => tr!(
            "The system refused to start {0}: {1}. Check that the file is executable.",
            named,
            system
        ),
        (process::StartError::PermissionDenied, None) => tr!(
            "{0} is on PATH but is not executable: it has no execute permission.",
            named
        ),
        (process::StartError::Logs, _) => {
            tr!("The run's log files could not be created.").to_owned()
        }
        (process::StartError::Listener, _) => {
            tr!("The jjp/1 listener could not take the run.").to_owned()
        }
        (process::StartError::Stopped, _) => {
            tr!("Stopped before the program started: nothing ran.").to_owned()
        }
        // `Other`, and reasons the executor may add later.
        (_, Some(system)) => tr!("The system refused to start {0}: {1}.", named, system),
        (_, None) => tr!("The system refused to start {0}.", named),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(named: &str, error: process::StartError, system: Option<&str>) -> String {
        start_error(&ProgramOutcome {
            named: named.to_owned(),
            entrypoint: Some("/jobs/export.php".to_owned()),
            start_error: Some(error),
            start_message: system.map(str::to_owned),
            ..ProgramOutcome::default()
        })
        .unwrap_or_default()
    }

    #[test]
    fn a_program_that_cannot_start_names_what_is_missing() {
        crate::app::source_text();
        let on_path = failed("php", process::StartError::NotFound, None);
        assert!(
            on_path.starts_with("The program php was not found: no folder of PATH"),
            "{on_path}"
        );
        let absolute = if cfg!(windows) {
            r"C:\tools\php.exe"
        } else {
            "/opt/php/bin/php"
        };
        assert_eq!(
            failed(absolute, process::StartError::NotFound, None),
            format!("The program {absolute} does not exist.")
        );
        assert_eq!(
            failed("php", process::StartError::EntrypointNotFound, None),
            "The entrypoint /jobs/export.php does not exist."
        );
        let refused = failed(
            "/jobs/tool",
            process::StartError::Other,
            Some("Exec format error (os error 8)"),
        );
        assert_eq!(
            refused,
            "The system refused to start /jobs/tool: Exec format error (os error 8)."
        );
        let script = failed(
            "/jobs/tool",
            process::StartError::NotFound,
            Some("No such file or directory (os error 2)"),
        );
        assert!(script.contains("interpreter"), "{script}");
        assert_eq!(
            start_error(&ProgramOutcome::default()),
            None,
            "a program that started has no start error"
        );
    }

    #[test]
    fn a_command_cut_in_a_quote_names_the_quote() {
        crate::app::source_text();
        let error = read_command("php -r \"print(1)").err();
        assert_eq!(
            error.map(command_problem).as_deref(),
            Some("The command opens a quote (\") and never closes it. Close it, or remove it.")
        );
        assert_eq!(command_text("", &[]), "");
        assert_eq!(command_text("php", &["a b".to_owned()]), "php 'a b'");
    }
}
