//! STUB.
use jykre_core::automation::RunId;
use std::path::Path;
pub fn log_runs(_dir: &Path) -> Vec<RunId> {
    Vec::new()
}
pub fn delete_logs(_dir: &Path, _run: RunId) -> std::io::Result<()> {
    Ok(())
}
/// STUB: a run's log streams (ADR-0037).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream { Stdout, Stderr, Jjp }
/// STUB: what a log file holds, decoded as UTF-8 with replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogText { pub text: String, pub dropped: Option<u64> }
/// STUB.
pub fn read_log(_dir: &Path, _run: RunId, _stream: LogStream) -> Option<LogText> { None }
