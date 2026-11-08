//! STUB (removed before merging the program executor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    NotFound { path: String },
    PermissionDenied { path: String },
    Changed { path: String },
    ShellScript,
    MissingDirectory { path: String },
    Unreadable,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogSizes {
    pub stdout: Option<u64>,
    pub stderr: Option<u64>,
    pub jjp: Option<u64>,
}
#[derive(Debug, Clone, Copy, Default)]
pub struct Timings;
pub struct Pinned {
    pub path: String,
    pub sha256: String,
}
pub fn pin(_body: &super::definition::ProcessBody) -> Result<Pinned, Refused> {
    Err(Refused::Unreadable)
}
pub fn orphan_alive(_group: i64, _started: &str) -> bool {
    false
}
pub fn end_group(_group: i64) -> std::io::Result<()> {
    Ok(())
}
