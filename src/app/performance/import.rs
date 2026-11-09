//! Importing a file (A11): a large synthetic CSV and an `.xlsx` with a large
//! shared strings table, imported into a temporary SQLite database through
//! the production pipeline (`jykre::import::session::run`, the driver's own
//! session, all or nothing), with the dialog's progress line in the window.
//! Resident memory is sampled during each import; a last import is
//! cancelled midway and the table checked.
use super::*;
use jykre::drivers::{CancellationToken, DriverResult};
use jykre::import::pipeline::{ImportPlan, Outcome, Progress, Shown as Showing, TimeLimit};
use jykre::import::session::ImportJob;
use jykre::import::{CsvOptions, ReadOptions, XlsxOptions};
use jykre::performance::budget::{
    CANCEL_SHOWN, OTHER_FRAME_P95, PROGRESS_SHOWN, ROWS_LEFT_BY_CANCEL, STALL,
};
use jykre::performance::data;
use jykre::performance::memory::{Peak, Sampler};
use std::path::Path;
use tokio::sync::watch;

const CSV_ROWS: usize = 1_500_000;
const XLSX_ROWS: usize = 250_000;
const SAMPLE_EVERY: Duration = Duration::from_millis(100);
/// When the cancelled import is cancelled: past the 2 s at which **Cancel**
/// shows.
const CANCEL_AFTER: Duration = Duration::from_secs(3);
const MIB: f64 = 1024.0 * 1024.0;

/// Every row of the file reaches the table.
const ROWS_MISSING: Budget = Budget {
    limit: 0.0,
    text: "0 rows",
    source: "ADR-0040: an import that ends well writes every row",
};

/// The import the window shows, as the dialog's footer does.
pub(super) struct Shown {
    pub(super) title: String,
    pub(super) started: Instant,
    pub(super) progress: watch::Receiver<Progress>,
    pub(super) cancel: CancellationToken,
    /// How it ended, in words.
    pub(super) outcome: Option<String>,
    /// When a frame first drew the progress, and the elapsed time and
    /// **Cancel**.
    progress_at: Option<Duration>,
    cancel_at: Option<Duration>,
}

impl Shown {
    /// Called by each frame while it runs: what this frame shows, the first
    /// time it shows it.
    pub(super) fn mark(&mut self) {
        if self.outcome.is_some() {
            return;
        }
        let elapsed = self.started.elapsed();
        match Showing::after(elapsed) {
            Showing::Nothing => {}
            Showing::Progress => {
                self.progress_at.get_or_insert(elapsed);
            }
            Showing::ProgressAndCancel => {
                self.progress_at.get_or_insert(elapsed);
                self.cancel_at.get_or_insert(elapsed);
            }
        }
    }
}

/// Cancels the import if the run stops before it ends (the window closed).
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Csv,
    Xlsx,
    /// The CSV again, cancelled midway.
    Cancelled,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Csv => "csv",
            Kind::Xlsx => "xlsx",
            Kind::Cancelled => "cancelled",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Csv => "CSV",
            Kind::Xlsx => "XLSX",
            Kind::Cancelled => "Cancelled CSV",
        }
    }
}

/// The files and the database, in the run's folder.
struct Files {
    csv: PathBuf,
    csv_bytes: u64,
    xlsx: PathBuf,
    xlsx_bytes: u64,
    database: PathBuf,
}

fn prepare(folder: &Path) -> Result<Files, String> {
    let folder = folder.join("import");
    std::fs::create_dir(&folder).map_err(|error| error.to_string())?;
    let csv = folder.join("synthetic.csv");
    let csv_bytes = data::write_csv(&csv, CSV_ROWS).map_err(|error| error.to_string())?;
    let xlsx = folder.join("synthetic.xlsx");
    let xlsx_bytes = data::write_xlsx(&xlsx, &folder, XLSX_ROWS)?;
    Ok(Files {
        csv,
        csv_bytes,
        xlsx,
        xlsx_bytes,
        database: folder.join("import.sqlite"),
    })
}

pub(super) async fn run(
    run: &Run,
    cx: &mut AsyncWindowContext,
    section: &mut Section,
) -> Result<(), Closed> {
    run.status(
        cx,
        phase_status(
            PathId::Import,
            "preparing",
            "writing the CSV and the workbook",
        ),
    )?;
    let folder = run.folder.clone();
    let generated = Instant::now();
    let files = match cx
        .background_executor()
        .spawn(async move { prepare(&folder) })
        .await
    {
        Ok(files) => files,
        Err(error) => {
            section
                .notes
                .push(format!("The files could not be written: {error}"));
            missing_all(section, "the synthetic files could not be written");
            return Ok(());
        }
    };
    let database = files.database.clone();
    let created: DriverResult<Option<String>> =
        jykre::backend::run(
            async move { data::sqlite_run(&database, &[data::IMPORT_TABLE_SQL]).await },
        )
        .await;
    if let Err(error) = created {
        section
            .notes
            .push(format!("The database could not be made: {}", error.message));
        missing_all(section, "the synthetic database could not be made");
        return Ok(());
    }
    section.notes.push(format!(
        "A CSV of {CSV_ROWS} rows ({:.1} MiB) and an .xlsx of {XLSX_ROWS} rows with a distinct name in each ({:.1} MiB), written in {:.1} s, imported into a SQLite table of six columns.",
        files.csv_bytes as f64 / MIB,
        files.xlsx_bytes as f64 / MIB,
        generated.elapsed().as_secs_f64()
    ));
    for kind in [Kind::Csv, Kind::Xlsx, Kind::Cancelled] {
        import(run, cx, section, &files, kind).await?;
    }
    run.update(cx, |view, _, cx| {
        view.stage = Stage::Empty;
        cx.notify();
    })?;
    Ok(())
}

fn missing_all(section: &mut Section, reason: &str) {
    for (name, budget) in [
        ("CSV: progress first shown", Some(PROGRESS_SHOWN)),
        (
            "CSV: elapsed time and Cancel first shown",
            Some(CANCEL_SHOWN),
        ),
        ("CSV: throughput", None),
        ("CSV: peak resident memory during the import", None),
        (
            "Cancelled CSV: rows the cancelled import left",
            Some(ROWS_LEFT_BY_CANCEL),
        ),
    ] {
        section
            .metrics
            .push(Metric::missing(name, "", budget, reason));
    }
}

async fn count(database: &Path) -> Option<u64> {
    let database = database.to_path_buf();
    jykre::backend::run(async move { data::sqlite_count(&database, data::IMPORT_TABLE).await })
        .await
        .ok()
}

async fn import(
    run: &Run,
    cx: &mut AsyncWindowContext,
    section: &mut Section,
    files: &Files,
    kind: Kind,
) -> Result<(), Closed> {
    let (path, bytes, options, columns) = match kind {
        Kind::Xlsx => (
            files.xlsx.clone(),
            files.xlsx_bytes,
            ReadOptions::Xlsx(XlsxOptions::new("Sheet1")),
            &data::XLSX_COLUMNS[..],
        ),
        Kind::Csv | Kind::Cancelled => (
            files.csv.clone(),
            files.csv_bytes,
            ReadOptions::Csv(CsvOptions::default()),
            &data::CSV_COLUMNS[..],
        ),
    };
    let rows_in_file = if kind == Kind::Xlsx {
        XLSX_ROWS
    } else {
        CSV_ROWS
    };
    let before = count(&files.database).await;
    let job = ImportJob {
        connection: data::sqlite(&files.database),
        table: data::import_table(),
        plan: ImportPlan {
            path,
            options,
            mapping: data::import_mapping(columns),
        },
        limit: TimeLimit::default().duration(),
    };
    let label = kind.label();
    let clock = run
        .begin(cx, phase_status(PathId::Import, kind.name(), "importing"))
        .await?;
    let sampler = Sampler::start(SAMPLE_EVERY);
    let (sender, receiver) = watch::channel(Progress::default());
    let cancel = CancellationToken::new();
    let _guard = CancelOnDrop(cancel.clone());
    let title = format!(
        "Importing the synthetic {label} file into main.{}",
        data::IMPORT_TABLE
    );
    let started = run.update(cx, |view, _, cx| {
        let started = Instant::now();
        view.stage = Stage::Import(Shown {
            title,
            started,
            progress: receiver,
            cancel: cancel.clone(),
            outcome: None,
            progress_at: None,
            cancel_at: None,
        });
        // Redrawn as the dialog redraws it.
        super::super::import::redraw_while(cx, PerformanceView::import_running);
        cx.notify();
        started
    })?;
    let token = cancel.clone();
    let task = cx
        .background_executor()
        .spawn(jykre::backend::run(async move {
            Ok(jykre::import::session::run(job, token, sender).await)
        }));
    let mut cancelled_at = None;
    if kind == Kind::Cancelled {
        run.wait(cx, CANCEL_AFTER).await;
        // As **Cancel import** does.
        run.update(cx, |view, _, cx| view.cancel_import(cx))?;
        cancelled_at = Some(started.elapsed());
    }
    let outcome = task.await;
    let elapsed = started.elapsed();
    let peak: Peak = cx
        .background_executor()
        .spawn(async move { sampler.stop() })
        .await;
    let text = match &outcome {
        Ok(outcome) => outcome.message(&format!("main.{}", data::IMPORT_TABLE)),
        Err(error) => error.message.clone(),
    };
    let (progress_at, cancel_at) = run.update(cx, |view, _, cx| {
        let marks = match &mut view.stage {
            Stage::Import(shown) => {
                shown.outcome = Some(text);
                (shown.progress_at, shown.cancel_at)
            }
            _ => (None, None),
        };
        cx.notify();
        marks
    })?;
    let numbers = run.end(cx, clock).await;
    let after = count(&files.database).await;
    let added = before
        .zip(after)
        .map(|(before, after)| after.saturating_sub(before));
    let imported = match &outcome {
        Ok(Outcome::Imported { rows }) => Some(*rows),
        _ => None,
    };
    let outcome_name = match &outcome {
        Ok(Outcome::Imported { .. }) => "imported".to_owned(),
        Ok(Outcome::NothingImported { reason }) => format!("nothing imported ({:?})", reason.code),
        Ok(Outcome::Unknown { error }) => format!("unknown ({:?})", error.code),
        Err(error) => format!("failed ({:?})", error.code),
    };
    let seconds = elapsed.as_secs_f64();
    let growth = peak
        .peak_bytes
        .zip(numbers.rss_before)
        .map(|(peak, before)| peak.saturating_sub(before));
    section.phases.push(numbers.to_json(
        kind.name(),
        json!({
            "file_bytes": bytes,
            "rows_in_file": rows_in_file,
            "outcome": outcome_name,
            "rows_imported": imported,
            "rows_added_to_table": added,
            "import_seconds": seconds,
            "progress_first_shown_ms": progress_at.map(ms),
            "cancel_first_shown_ms": cancel_at.map(ms),
            "cancelled_after_ms": cancelled_at.map(ms),
            "memory": {
                "interval_ms": SAMPLE_EVERY.as_millis(),
                "samples": peak.samples,
                "first_bytes": peak.first_bytes,
                "peak_bytes": peak.peak_bytes,
                "last_bytes": peak.last_bytes,
                "process_peak_bytes": peak.process_peak_bytes,
                "error": peak.error,
            },
        }),
    ));
    let not_imported = format!("the import ended as {outcome_name}");
    if kind == Kind::Cancelled {
        section.metrics.push(Metric::maybe(
            format!("{label}: rows the cancelled import left"),
            match (&outcome, added) {
                (Ok(Outcome::NothingImported { .. }), Some(added)) => Some(added as f64),
                _ => None,
            },
            "rows",
            Some(ROWS_LEFT_BY_CANCEL),
            &format!("{not_imported}, or the table could not be counted"),
        ));
        section.metrics.push(Metric::maybe(
            format!("{label}: from Cancel to the outcome"),
            cancelled_at.map(|at| elapsed.saturating_sub(at).as_secs_f64()),
            "s",
            None,
            "it was not cancelled",
        ));
    } else {
        section.metrics.push(Metric::maybe(
            format!("{label}: rows missing from the table"),
            imported
                .and(added)
                .map(|added| rows_in_file.saturating_sub(added as usize) as f64),
            "rows",
            Some(ROWS_MISSING),
            &not_imported,
        ));
        section.metrics.push(Metric::maybe(
            format!("{label}: throughput"),
            imported.map(|rows| rows as f64 / seconds.max(0.001)),
            "rows/s",
            None,
            &not_imported,
        ));
        section.metrics.push(Metric::maybe(
            format!("{label}: file read"),
            imported.map(|_| bytes as f64 / MIB / seconds.max(0.001)),
            "MiB/s",
            None,
            &not_imported,
        ));
    }
    let memory_reason = peak
        .error
        .clone()
        .unwrap_or_else(|| "no memory sample was taken".into());
    section.metrics.push(Metric::maybe(
        format!("{label}: peak resident memory during the import"),
        peak.peak_bytes.map(|bytes| bytes as f64 / MIB),
        "MiB",
        None,
        &memory_reason,
    ));
    section.metrics.push(Metric::maybe(
        format!("{label}: growth over the memory before it"),
        growth.map(|bytes| bytes as f64 / MIB),
        "MiB",
        None,
        &memory_reason,
    ));
    section.metrics.push(Metric::maybe(
        format!("{label}: progress first shown"),
        progress_at.map(ms),
        "ms",
        Some(PROGRESS_SHOWN),
        &format!("the import ended in {seconds:.2} s, before its progress showed"),
    ));
    section.metrics.push(Metric::maybe(
        format!("{label}: elapsed time and Cancel first shown"),
        cancel_at.map(ms),
        "ms",
        Some(CANCEL_SHOWN),
        &format!("the import ended in {seconds:.2} s, before 2 s"),
    ));
    numbers.frame_metrics(section, label, OTHER_FRAME_P95, None, Some(STALL));
    run.wait(cx, Duration::from_millis(500)).await;
    Ok(())
}
