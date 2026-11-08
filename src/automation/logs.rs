//! STUB.
use jykre_core::automation::RunId;
use std::path::Path;
pub fn log_runs(_dir: &Path) -> Vec<RunId> {
    Vec::new()
}
pub fn delete_logs(_dir: &Path, _run: RunId) -> std::io::Result<()> {
    Ok(())
}
