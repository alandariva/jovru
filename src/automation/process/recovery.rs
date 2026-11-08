//! Programs left running by a crash (specification, section 9.1.6; Q-29).
//!
//! On Unix a program's process group survives a crash of Studio. Its run
//! records the group ID (the leader's PID) and the leader's start time as
//! the system reports it; at the next start, a group is offered for ending
//! only while its leader still exists **with that start time**, so a PID the
//! system gave to another process is never touched. Linux reports the start
//! time in `/proc/<pid>/stat`; other systems record none, so nothing is ever
//! offered there. On Windows the Job Object ends the tree with Studio.
use std::io;

/// The leader's start time: field 22 of `/proc/<pid>/stat` (clock ticks
/// since boot), kept as the text the system wrote. `None` outside Linux,
/// or when the process already ended.
#[cfg(unix)]
pub(super) async fn start_time(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let stat = tokio::fs::read_to_string(format!("/proc/{pid}/stat"))
            .await
            .ok()?;
        parse_stat(&stat).map(|stat| stat.started)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// The fields of `/proc/<pid>/stat` recovery needs.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stat {
    /// Field 3: `R`, `S`, `Z` (a zombie), …
    pub state: char,
    /// Field 5.
    pub group: i64,
    /// Field 22, as written.
    pub started: String,
}

/// Reads `/proc/<pid>/stat`. The command name (field 2) is in parentheses
/// and may hold spaces and parentheses itself, so the fields are counted
/// from the last `)`.
#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_stat(stat: &str) -> Option<Stat> {
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `fields[0]` is field 3.
    Some(Stat {
        state: fields.first()?.chars().next()?,
        group: fields.get(2)?.parse().ok()?,
        started: (*fields.get(19)?).to_owned(),
    })
}

/// Whether the leader of process group `group` still exists, leads that
/// group, has not ended (a zombie has) and started at `started`. Always
/// `false` outside Linux and for an empty `started`. Reads `/proc`
/// (blocking, but never waits on a disk).
pub fn orphan_alive(group: i64, started: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        if group <= 1 || started.is_empty() {
            return false;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{group}/stat")) else {
            return false;
        };
        parse_stat(&stat).is_some_and(|stat| {
            stat.group == group && stat.started == started && !matches!(stat.state, 'Z' | 'X' | 'x')
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (group, started);
        false
    }
}

/// Whether any process of group `group` is still running (not a zombie),
/// read from `/proc` without signalling anything. An unreadable `/proc`
/// counts as running, so the caller falls back to killing.
#[cfg(target_os = "linux")]
pub(super) async fn group_running(group: i64) -> bool {
    tokio::task::spawn_blocking(move || {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return true;
        };
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .filter(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
            else {
                return false;
            };
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| parse_stat(&stat))
                .is_some_and(|stat| stat.group == group && !matches!(stat.state, 'Z' | 'X' | 'x'))
        })
    })
    .await
    .unwrap_or(true)
}

/// Sends `SIGKILL` to process group `group`, only if [`orphan_alive`] still
/// holds for it with `started`; `Ok(false)` when it does not, and nothing
/// was sent. Runs `kill -KILL -- -<group>`, since the workspace forbids the
/// `unsafe` a direct `killpg` needs; blocks until that command ends.
/// Refuses groups 0 and 1, which would signal this application's own group
/// or every process the user may signal. Unsupported on Windows, where the
/// Job Object already ended the tree.
pub fn end_group(group: i64, started: &str) -> io::Result<bool> {
    if group <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a program's process group",
        ));
    }
    if !cfg!(unix) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process groups are a Unix feature",
        ));
    }
    if !orphan_alive(group, started) {
        return Ok(false);
    }
    let status = std::process::Command::new("kill")
        .arg("-KILL")
        .arg("--")
        .arg(format!("-{group}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(true)
    } else {
        Err(io::Error::other("kill could not signal the process group"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_fields_are_counted_from_the_last_parenthesis() {
        let line = "4242 (a (weird) name) S 1 4242 4242 0 -1 4194560 120 0 0 0 1 2 0 0 20 0 \
                    1 0 987654 1000 100 18446744073709551615";
        assert_eq!(
            parse_stat(line),
            Some(Stat {
                state: 'S',
                group: 4242,
                started: "987654".into(),
            })
        );
        assert_eq!(parse_stat("4242 (short) S 1"), None);
        assert_eq!(parse_stat("no parenthesis"), None);
    }

    #[test]
    fn groups_zero_and_one_are_never_signalled() {
        assert_eq!(
            end_group(1, "1").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            end_group(0, "1").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            end_group(-5, "1").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!orphan_alive(1, ""));
    }
}
