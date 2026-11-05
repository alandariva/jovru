//! Synthetic grid workloads and process counters for the opt-in performance run.
use crate::result::{ResultColumn, ResultData};
use std::path::Path;
#[cfg(any(windows, target_os = "linux"))]
use std::process::Command;

pub fn fixture(rows: usize) -> ResultData {
    let values: Vec<_> = (1..=rows)
        .map(|n| {
            [
                n.to_string(),
                format!("customer-{n}"),
                format!("{n}.25"),
                (n % 2 == 0).to_string(),
                "2026-01-01".into(),
            ]
        })
        .collect();
    ResultData::from_rows(
        [
            ("id", "int8"),
            ("name", "text"),
            ("amount", "numeric"),
            ("active", "bool"),
            ("created_at", "date"),
        ]
        .map(|(name, kind)| ResultColumn::new(name, kind))
        .into(),
        values
            .iter()
            .map(|row| row.iter().map(|v| Some(v.as_str()))),
    )
}

#[derive(Clone, Copy, Debug)]
pub struct Counters {
    pub cpu_seconds: f64,
    pub rss_bytes: u64,
}

pub fn cpu_percent(before: Counters, after: Counters, seconds: f64) -> Option<f64> {
    (seconds > 0. && after.cpu_seconds >= before.cpu_seconds)
        .then(|| 100. * (after.cpu_seconds - before.cpu_seconds) / seconds)
}

#[cfg(target_os = "linux")]
pub fn counters() -> Result<Counters, String> {
    let hz = Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .map_err(|e| e.to_string())?;
    let hz = String::from_utf8_lossy(&hz.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|e| e.to_string())?;
    let stat = std::fs::read_to_string("/proc/self/stat").map_err(|e| e.to_string())?;
    let status = std::fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?;
    linux_counters(&stat, &status, hz)
}

#[cfg(any(target_os = "linux", test))]
fn linux_counters(stat: &str, status: &str, hz: f64) -> Result<Counters, String> {
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .ok_or("Invalid process stat")?
        .1
        .split_whitespace()
        .collect();
    let ticks = |index: usize| {
        fields
            .get(index)
            .ok_or("Missing CPU counter")?
            .parse::<u64>()
            .map_err(|_| "Invalid CPU counter")
    };
    if hz <= 0. {
        return Err("Invalid clock frequency".into());
    }
    let rss = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .ok_or("Missing RSS")?
        .split_whitespace()
        .next()
        .ok_or("Missing RSS value")?
        .parse::<u64>()
        .map_err(|e| e.to_string())?;
    Ok(Counters {
        cpu_seconds: (ticks(11)? + ticks(12)?) as f64 / hz,
        rss_bytes: rss * 1024,
    })
}

#[cfg(windows)]
pub fn counters() -> Result<Counters, String> {
    use std::os::windows::process::CommandExt;
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &format!("$p=Get-Process -Id {}; @{{cpu=$p.TotalProcessorTime.TotalSeconds;rss=$p.WorkingSet64}} | ConvertTo-Json -Compress", std::process::id())])
        .creation_flags(0x08000000).output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err("Process counter command failed".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    Ok(Counters {
        cpu_seconds: value["cpu"].as_f64().ok_or("Missing CPU counter")?,
        rss_bytes: value["rss"].as_u64().ok_or("Missing working set")?,
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn counters() -> Result<Counters, String> {
    Err("Process counters are supported on Linux and Windows".into())
}

pub fn write_report(path: &Path, report: &serde_json::Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?;
    std::fs::write(path, bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_uses_one_core_scale_and_rejects_invalid_intervals() {
        let before = Counters {
            cpu_seconds: 1.,
            rss_bytes: 0,
        };
        let after = Counters {
            cpu_seconds: 5.,
            rss_bytes: 0,
        };
        assert_eq!(cpu_percent(before, after, 2.), Some(200.));
        assert_eq!(cpu_percent(before, after, 0.), None);
        assert_eq!(cpu_percent(after, before, 2.), None);
    }
    #[test]
    fn linux_stat_handles_spaces_and_parentheses_in_process_name() {
        let stat = "42 (test (grid)) S 0 0 0 0 0 0 0 0 0 0 150 50";
        let sample = linux_counters(stat, "VmRSS:\t123 kB\n", 100.).expect("fixture");
        assert_eq!(sample.cpu_seconds, 2.);
        assert_eq!(sample.rss_bytes, 123 * 1024);
        assert!(linux_counters("invalid", "", 100.).is_err());
    }
    #[test]
    fn synthetic_results_are_bounded_and_have_five_columns() {
        let data = fixture(1000);
        assert_eq!(data.row_count(), 1000);
        assert_eq!(data.column_count(), 5);
        assert_eq!(data.value(999, 0), Some("1000"));
    }
}
