//! The performance run's report: a JSON file with every phase and metric,
//! and a short Markdown summary next to it with the budgets and this run's
//! numbers (`jykre-performance-<timestamp>-<pid>.json` and `.md`).
use super::budget::{Metric, Status};
use super::selection::PathId;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// What one path measured.
#[derive(Clone, Debug)]
pub struct Section {
    pub path: PathId,
    pub metrics: Vec<Metric>,
    /// Each phase's raw numbers.
    pub phases: Vec<Value>,
    pub notes: Vec<String>,
}

impl Section {
    pub fn new(path: PathId) -> Self {
        Self {
            path,
            metrics: Vec::new(),
            phases: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "title": self.path.title(),
            "metrics": self.metrics.iter().map(Metric::to_json).collect::<Vec<_>>(),
            "phases": self.phases,
            "notes": self.notes,
        })
    }
}

/// Where and how the run ran.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Environment {
    pub version: String,
    pub os: String,
    pub architecture: String,
    pub release: bool,
    pub logical_cpus: Option<usize>,
    /// The window's content in logical pixels, and its scale factor.
    pub window: Option<(f32, f32, f32)>,
    /// `JYKRE_PROBE` was on, so frames and stalls were timed.
    pub probe: bool,
}

impl Environment {
    /// This build on this machine; the window is filled in once it opens.
    pub fn current(probe: bool) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").into(),
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            release: !cfg!(debug_assertions),
            logical_cpus: std::thread::available_parallelism().ok().map(usize::from),
            window: None,
            probe,
        }
    }

    pub fn profile(&self) -> &'static str {
        if self.release { "release" } else { "debug" }
    }
}

#[derive(Clone, Debug)]
pub struct Report {
    pub environment: Environment,
    pub sections: Vec<Section>,
    /// Seconds since the Unix epoch when the run started.
    pub started: u64,
    pub elapsed_seconds: f64,
    /// Anything the run could not undo, such as a temporary folder left.
    pub cleanup: Vec<String>,
}

impl Report {
    pub fn to_json(&self) -> Value {
        let environment = &self.environment;
        let mut paths = serde_json::Map::new();
        for section in &self.sections {
            paths.insert(section.path.name().into(), section.to_json());
        }
        // Every phase in one list as well, as earlier reports had them.
        let results: Vec<Value> = self
            .sections
            .iter()
            .flat_map(|section| {
                section.phases.iter().map(|phase| {
                    let mut phase = phase.clone();
                    if let Some(object) = phase.as_object_mut() {
                        object.insert("path".into(), section.path.name().into());
                    }
                    phase
                })
            })
            .collect();
        json!({
            "version": environment.version,
            "os": environment.os,
            "architecture": environment.architecture,
            "build_profile": environment.profile(),
            "debug_build": !environment.release,
            "logical_cpus": environment.logical_cpus,
            "window": environment.window.map(|(width, height, scale)| json!({
                "width": width, "height": height, "scale_factor": scale,
            })),
            "frame_probe": environment.probe,
            "started_unix_seconds": self.started,
            "elapsed_seconds": self.elapsed_seconds,
            "measurement": "frames and UI-thread stalls from the frame probe (JYKRE_PROBE=1) per phase; UI-thread work from the application's own UiTimer timings; process CPU (100% = one logical CPU) and resident memory at phase ends, and sampled during imports",
            "input": "in-process production handlers (keys, toolbar actions, typing into the fields), on timers; not OS input injection",
            "paths": paths,
            "results": results,
            "cleanup": self.cleanup,
        })
    }

    /// The summary: the budgets with this run's numbers, the build, the
    /// platform and the window.
    pub fn markdown(&self) -> String {
        let environment = &self.environment;
        let mut text = String::from("# Jykre Studio performance run\n\n");
        text.push_str(&format!(
            "- Version {}, {} build\n- Platform: {} {}, {} logical CPUs\n",
            environment.version,
            environment.profile(),
            environment.os,
            environment.architecture,
            environment
                .logical_cpus
                .map_or_else(|| "unknown".into(), |count| count.to_string()),
        ));
        match environment.window {
            Some((width, height, scale)) => text.push_str(&format!(
                "- Window: {width:.0} × {height:.0} logical pixels, scale factor {scale}\n"
            )),
            None => text.push_str("- Window: not known\n"),
        }
        text.push_str(&format!(
            "- Frame probe: {}\n- Paths: {}\n- Elapsed: {:.0} s\n\n",
            if environment.probe {
                "on (JYKRE_PROBE=1)"
            } else {
                "off; frames and stalls not measured"
            },
            self.sections
                .iter()
                .map(|section| section.path.name())
                .collect::<Vec<_>>()
                .join(", "),
            self.elapsed_seconds,
        ));
        text.push_str("| Path | Metric | This run | Budget | Status |\n");
        text.push_str("| --- | --- | --- | --- | --- |\n");
        for section in &self.sections {
            for metric in &section.metrics {
                text.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    cell(section.path.title()),
                    cell(&metric.name),
                    cell(&metric.value_text()),
                    cell(metric.budget.map_or("—", |budget| budget.text)),
                    metric.status().as_str(),
                ));
            }
        }
        let missing: Vec<String> = self
            .sections
            .iter()
            .flat_map(|section| {
                section
                    .metrics
                    .iter()
                    .filter(|metric| metric.status() == Status::NotMeasured)
                    .map(move |metric| {
                        format!(
                            "- {}, {}: {}\n",
                            section.path.title(),
                            metric.name,
                            metric.reason.as_deref().unwrap_or("no reason given")
                        )
                    })
            })
            .collect();
        if !missing.is_empty() {
            text.push_str("\n## Not measured\n\n");
            text.extend(missing);
        }
        let notes: Vec<String> = self
            .sections
            .iter()
            .flat_map(|section| {
                section
                    .notes
                    .iter()
                    .map(move |note| format!("- {}: {note}\n", section.path.title()))
            })
            .chain(self.cleanup.iter().map(|note| format!("- {note}\n")))
            .collect();
        if !notes.is_empty() {
            text.push_str("\n## Notes\n\n");
            text.extend(notes);
        }
        text
    }
}

/// A table cell: one line, its pipes escaped.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// The report's file name without its extension.
pub fn file_stem(stamp_nanos: u128, pid: u32) -> String {
    format!("jykre-performance-{stamp_nanos}-{pid}")
}

/// Writes `<stem>.json` and `<stem>.md` into `dir`.
pub fn write(dir: &Path, stem: &str, report: &Report) -> Result<(PathBuf, PathBuf), String> {
    let json = dir.join(format!("{stem}.json"));
    let markdown = dir.join(format!("{stem}.md"));
    super::write_report(&json, &report.to_json())?;
    std::fs::write(&markdown, report.markdown()).map_err(|error| error.to_string())?;
    Ok((json, markdown))
}

#[cfg(test)]
mod tests {
    use super::super::budget::{GRID_FRAME_P95, STALL};
    use super::*;

    fn report() -> Report {
        let mut grid = Section::new(PathId::Grid);
        grid.metrics.push(Metric::checked(
            "Frame p95 | selection",
            3.5,
            "ms",
            GRID_FRAME_P95,
        ));
        grid.phases
            .push(json!({"phase": "selection", "rows": 1000}));
        let mut import = Section::new(PathId::Import);
        import.metrics.push(Metric::missing(
            "Longest stall",
            "ms",
            Some(STALL),
            "probe off",
        ));
        import.notes.push("synthetic CSV of 10 rows".into());
        Report {
            environment: Environment {
                window: Some((1480.0, 920.0, 1.5)),
                ..Environment::current(false)
            },
            sections: vec![grid, import],
            started: 1,
            elapsed_seconds: 12.0,
            cleanup: vec!["temporary folder left".into()],
        }
    }

    #[test]
    fn the_json_has_a_section_per_path_and_every_phase() {
        let value = report().to_json();
        let metric = &value["paths"]["grid"]["metrics"][0];
        assert_eq!(metric["status"], "pass");
        assert_eq!(metric["budget"], "≤ 8 ms p95");
        assert_eq!(
            value["paths"]["import"]["metrics"][0]["status"],
            "not measured"
        );
        assert_eq!(value["results"][0]["path"], "grid");
        assert_eq!(value["window"]["width"], 1480.0);
    }

    #[test]
    fn the_summary_lists_budgets_numbers_and_what_was_not_measured() {
        let text = report().markdown();
        assert!(
            text.contains(
                "| Result grid (A14) | Frame p95 \\| selection | 3.50 ms | ≤ 8 ms p95 | pass |"
            ),
            "{text}"
        );
        assert!(text.contains("1480 × 920"), "{text}");
        assert!(text.contains("## Not measured"), "{text}");
        assert!(text.contains("probe off"), "{text}");
        assert!(text.contains("temporary folder left"), "{text}");
        assert!(file_stem(5, 7) == "jykre-performance-5-7");
    }

    #[test]
    fn both_files_are_written() {
        let folder = super::super::temp::TempFolder::create().unwrap();
        let (json, markdown) = write(folder.path(), "report", &report()).unwrap();
        assert!(std::fs::read_to_string(json).unwrap().contains("\"paths\""));
        assert!(
            std::fs::read_to_string(markdown)
                .unwrap()
                .starts_with("# Jykre")
        );
        folder.remove().unwrap();
    }
}
