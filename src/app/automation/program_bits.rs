//! Every name the Automation views take from the program executor's
//! modules (`jykre::automation::process` and `jykre::automation::logs`),
//! in one place, so they can follow those modules as they change. Nothing
//! else in `src/app/automation/` names them.
//!
//! GPUI-free: what it gives back is plain data.
use jykre::automation::RunId;
use jykre::automation::{logs, process};
use jykre::tr;
use std::path::Path;

/// The variables a program never inherits (specification, section 9.1.3),
/// as the editor lists them.
pub fn denylist() -> &'static [&'static str] {
    process::ENV_DENYLIST
}

/// Why a program could not start, in words, and the file a run was refused
/// for when it changed since its version was published (which **Publish**
/// fixes).
pub fn refused(refused: &process::Refused) -> (String, Option<String>) {
    match refused {
        process::Refused::Changed { path } => (
            tr!(
                "The file {0} changed since this version was published, so it does not run. Publish the job again to run the file as it is now.",
                path
            ),
            Some(path.clone()),
        ),
        process::Refused::NotFound { path } => (
            tr!("The program {0} was not found.", path),
            None,
        ),
        process::Refused::PermissionDenied { path } => (
            tr!("The program {0} cannot be started: permission denied.", path),
            None,
        ),
        process::Refused::MissingDirectory { path } => (
            tr!("The working folder {0} does not exist.", path),
            None,
        ),
        // The executor may refuse for reasons added later; they read alike.
        _ => (
            tr!(
                "The program cannot be started: its file cannot be read, or it is a .bat or .cmd file, which would run through cmd.exe."
            )
            .to_owned(),
            None,
        ),
    }
}

/// The bytes kept of each stream of a program run, from its outcome.
pub fn log_sizes(sizes: &process::LogSizes) -> [Option<u64>; 3] {
    [sizes.stdout, sizes.stderr, sizes.jjp]
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

/// What a log file holds, decoded as UTF-8 with replacement, and how many
/// bytes past its 1 MiB were dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Log {
    pub text: String,
    pub dropped: Option<u64>,
}

/// Reads one stream of a run's logs from the data folder. Blocking: call
/// it off the UI thread, and only when the user opens the log.
pub fn read_log(data_dir: &Path, run: RunId, stream: Stream) -> Option<Log> {
    let stream = match stream {
        Stream::Stdout => logs::LogStream::Stdout,
        Stream::Stderr => logs::LogStream::Stderr,
        Stream::Jjp => logs::LogStream::Jjp,
    };
    logs::read_log(data_dir, run, stream).map(|log| Log {
        text: log.text,
        dropped: log.dropped,
    })
}
