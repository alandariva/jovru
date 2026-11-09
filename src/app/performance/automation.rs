//! The Automation interface (A6) over an engine of the run's own: its
//! `automation.db` in the temporary folder, written through the production
//! storage, and SQL Jobs on a synthetic SQLite database. It measures what
//! the specification's section 8.2 budgets: bursts of run statuses
//! published to the job tabs, a large result published to a job's grid,
//! history pages over 10,000 runs, and the global **Runs** list.
use super::*;
use crate::app::automation::benchmark::{Bench, BenchJob, List};
use crate::app::report_panel::ReportSource;
use jykre::automation::{Availability, Engine, RunState, UtcMillis};
use jykre::drivers::{ConnectionId, DriverError, DriverKind, ErrorCode, builtin_registry};
use jykre::performance::budget::{
    AUTOMATION_FRAME_P95, AUTOMATION_STALL, FIRST_FRAME_AFTER_RESULT, PUBLISH_RESULT,
    STATE_PUBLICATION,
};
use jykre::performance::data;
use jykre::reports::ConnectionRef;

/// Rows of the table the jobs read, and of the large result.
const ROWS: usize = 100_000;
/// Small jobs run together in a burst, half on each connection: two run
/// and eight wait, the engine's limits.
const BURST_JOBS: usize = 10;
const BURSTS: usize = 5;
/// Runs written to the history before it is read.
const HISTORY_RUNS: usize = 10_000;
/// Older pages read in the job's history and in the global list.
const JOB_PAGES: usize = 40;
const GLOBAL_PAGES: usize = 10;
/// How long a burst or a run may take before the run gives up on it.
const RUN_LIMIT: Duration = Duration::from_secs(120);

fn failed(error: DriverError) -> String {
    error.message
}

pub(super) async fn run(
    run: &Run,
    cx: &mut AsyncWindowContext,
    section: &mut Section,
) -> Result<(), Closed> {
    match prepare(run, cx, section).await? {
        Ok((engine, jobs, big, sources)) => {
            let bench = run.update(cx, |view, window, cx| {
                let bench = Bench::new(engine, jobs, big, sources, window, cx);
                view.stage = Stage::View(bench.job_view());
                cx.notify();
                bench
            })?;
            let measured = measure(run, cx, section, &bench).await;
            // Nothing holds the engine but the bench: closing it lets the
            // temporary folder go.
            let engine = bench.engine();
            run.update(cx, |view, _, cx| {
                view.stage = Stage::Empty;
                cx.notify();
            })?;
            drop(bench);
            let _ = jykre::backend::run(async move {
                engine.shutdown().await;
                Ok(())
            })
            .await;
            measured?;
        }
        Err(reason) => {
            section
                .notes
                .push(format!("Automation could not be prepared: {reason}"));
            for (name, budget) in [
                ("Run status publication, longest", STATE_PUBLICATION),
                ("Large result published, longest", PUBLISH_RESULT),
                ("History page shown, longest", STATE_PUBLICATION),
            ] {
                section
                    .metrics
                    .push(Metric::missing(name, "ms", Some(budget), reason.clone()));
            }
        }
    }
    section.notes.push(
        "Jobs run against a synthetic SQLite database; PostgreSQL, MySQL and Databricks connections are not measured here.".into(),
    );
    Ok(())
}

type Prepared = (Engine, Vec<BenchJob>, BenchJob, Vec<ReportSource>);

/// The database, the engine, the jobs and a long history.
async fn prepare(
    run: &Run,
    cx: &mut AsyncWindowContext,
    section: &mut Section,
) -> Result<Result<Prepared, String>, Closed> {
    run.status(
        cx,
        phase_status(PathId::Automation, "preparing", "database and engine"),
    )?;
    let folder = run.folder.join("automation");
    let database = run.folder.join("jobs.sqlite");
    let filled = {
        let database = database.clone();
        jykre::backend::run(async move {
            data::sqlite_run(
                &database,
                &[data::JOBS_TABLE_SQL, &data::jobs_rows_sql(ROWS)],
            )
            .await
        })
        .await
    };
    if let Err(error) = filled {
        return Ok(Err(failed(error)));
    }
    let runtime = match jykre::backend::runtime_handle() {
        Ok(runtime) => runtime,
        Err(error) => return Ok(Err(failed(error))),
    };
    let engine = Engine::start(runtime, folder);
    let started = {
        let engine = engine.clone();
        jykre::backend::run(async move { Ok(engine.started().await) }).await
    };
    if !matches!(started, Ok(Availability::Ready)) {
        return Ok(Err(format!("the engine did not start: {started:?}")));
    }
    let Some(repository) = engine.repository() else {
        return Ok(Err("the engine has no repository".into()));
    };
    // Two connections to the same file: one active run on each.
    let connections = [ConnectionId::new(), ConnectionId::new()];
    let mut definitions: Vec<_> = (0..BURST_JOBS)
        .map(|index| {
            data::sql_job(
                &format!("Synthetic job {:02}", index + 1),
                connections[index % 2],
                "SELECT * FROM big WHERE id <= 200",
                1_000,
            )
        })
        .collect();
    definitions.push(data::sql_job(
        "Synthetic large result",
        connections[0],
        "SELECT * FROM big",
        u32::try_from(ROWS).unwrap_or(u32::MAX),
    ));
    let saved = {
        let engine = engine.clone();
        jykre::backend::run(async move {
            let mut saved = Vec::new();
            for definition in definitions {
                let stored = engine.save_job(&definition, None).await.map_err(|_| {
                    DriverError::new(ErrorCode::Internal, "A synthetic job could not be saved")
                })?;
                saved.push(BenchJob {
                    definition,
                    revision: stored.revision,
                });
            }
            Ok(saved)
        })
        .await
    };
    let mut jobs = match saved {
        Ok(jobs) => jobs,
        Err(error) => return Ok(Err(failed(error))),
    };
    let Some(big) = jobs.pop() else {
        return Ok(Err("no job was saved".into()));
    };
    let Some(first) = jobs.first() else {
        return Ok(Err("no job was saved".into()));
    };
    run.status(
        cx,
        phase_status(
            PathId::Automation,
            "preparing",
            &format!("writing {HISTORY_RUNS} runs through the repository"),
        ),
    )?;
    let seeded = Instant::now();
    let seed = {
        let job = first.definition.clone();
        let revision = first.revision;
        let connection = connections[0];
        jykre::backend::run(async move {
            data::seed_history(
                &repository,
                &job,
                revision,
                connection,
                HISTORY_RUNS,
                UtcMillis::now(),
            )
            .await
        })
        .await
    };
    if let Err(error) = seed {
        return Ok(Err(failed(error)));
    }
    section.notes.push(format!(
        "{HISTORY_RUNS} finished runs of the first job written through AutomationRepository in {:.1} s; {BURST_JOBS} jobs reading 200 rows and one reading {ROWS}.",
        seeded.elapsed().as_secs_f64()
    ));
    let dialect = builtin_registry()
        .and_then(|registry| registry.get(DriverKind::Sqlite))
        .map(|driver| driver.descriptor().dialect)
        .ok();
    let sources = connections
        .iter()
        .enumerate()
        .map(|(index, id)| ReportSource {
            connection: ConnectionRef {
                id: Some(*id),
                name: format!("Synthetic SQLite {}", index + 1),
            },
            input: Some(data::sqlite(&database)),
            row_limit: jykre::result::DEFAULT_ROW_LIMIT,
            environment: None,
            dialect,
            choices: Vec::new(),
            names: None,
        })
        .collect();
    Ok(Ok((engine, jobs, big, sources)))
}

/// Waits until nothing runs or waits, and then a frame more.
async fn settle(run: &Run, cx: &mut AsyncWindowContext, bench: &Bench) -> Result<bool, Closed> {
    // The tabs' statuses arrive a frame after the engine's.
    run.wait(cx, Duration::from_millis(100)).await;
    let idle = run.until(cx, RUN_LIMIT, |_, _, cx| bench.idle(cx)).await?;
    run.wait(cx, Duration::from_millis(300)).await;
    Ok(idle)
}

/// Reads older pages of `list` until `pages` were read or none is left,
/// scrolling to the last entry and choosing one every five pages.
async fn page_through(
    run: &Run,
    cx: &mut AsyncWindowContext,
    bench: &Bench,
    list: List,
    pages: usize,
    phase: &str,
) -> Result<usize, Closed> {
    let mut read = 0;
    for page in 0..pages {
        let (_, entries, more) = run.update(cx, |_, _, cx| bench.list_state(list, cx))?;
        if !more {
            break;
        }
        run.update(cx, |view, _, cx| {
            view.status = phase_status(
                PathId::Automation,
                phase,
                &format!("page {} of {pages}, {entries} runs shown", page + 1),
            );
            bench.older(list, cx);
        })?;
        run.until(cx, RUN_LIMIT, |_, _, cx| !bench.list_state(list, cx).0)
            .await?;
        read += 1;
        let (_, entries, _) = run.update(cx, |_, _, cx| bench.list_state(list, cx))?;
        run.update(cx, |_, _, cx| {
            bench.scroll(list, entries.saturating_sub(1), cx);
            if page % 5 == 0 {
                bench.choose(list, entries.saturating_sub(10), cx);
            }
        })?;
        run.wait(cx, Duration::from_millis(80)).await;
    }
    // Back to the top, a screen at a time.
    let (_, entries, _) = run.update(cx, |_, _, cx| bench.list_state(list, cx))?;
    let mut index = entries;
    while index > 0 {
        index = index.saturating_sub(12);
        run.update(cx, |_, _, cx| bench.scroll(list, index, cx))?;
        run.wait(cx, Duration::from_millis(40)).await;
    }
    Ok(read)
}

async fn measure(
    run: &Run,
    cx: &mut AsyncWindowContext,
    section: &mut Section,
    bench: &Bench,
) -> Result<(), Closed> {
    // Wait for the tabs' first reads (versions, history).
    run.wait(cx, Duration::from_secs(2)).await;

    // Bursts of statuses in the job tabs.
    run.update(cx, |_, window, cx| bench.show_job_runs(false, window, cx))?;
    let clock = run
        .begin(cx, phase_status(PathId::Automation, "burst", "starting"))
        .await?;
    let mut settled = 0;
    for burst in 0..BURSTS {
        run.update(cx, |view, _, cx| {
            view.status = phase_status(
                PathId::Automation,
                "burst",
                &format!("{} of {BURSTS}: {BURST_JOBS} runs at once", burst + 1),
            );
            bench.run_jobs(cx);
            cx.notify();
        })?;
        if settle(run, cx, bench).await? {
            settled += 1;
        }
    }
    let numbers = run.end(cx, clock).await;
    section.phases.push(numbers.to_json(
        "burst",
        json!({"bursts": BURSTS, "runs_per_burst": BURST_JOBS, "bursts_settled": settled}),
    ));
    numbers.timing_metrics(
        section,
        "Run status published to a job tab",
        "automation run status",
        Some(STATE_PUBLICATION),
    );
    numbers.timing_metrics(
        section,
        "Engine changes forwarded to the tabs and the Runs list",
        "automation changes",
        Some(STATE_PUBLICATION),
    );
    numbers.timing_metrics(
        section,
        "A job's 200-row result published",
        "automation result",
        Some(PUBLISH_RESULT),
    );
    numbers.frame_metrics(
        section,
        "run status bursts",
        AUTOMATION_FRAME_P95,
        None,
        Some(AUTOMATION_STALL),
    );

    // A large result published to a job's grid.
    run.show(cx, Stage::View(bench.big_view()))?;
    run.wait(cx, Duration::from_millis(500)).await;
    let clock = run
        .begin(
            cx,
            phase_status(PathId::Automation, "result", "100,000 rows"),
        )
        .await?;
    run.update(cx, |_, _, cx| bench.run_big(cx))?;
    let arrived = run
        .until(cx, RUN_LIMIT, |_, _, cx| {
            bench.idle(cx) && bench.big_has_result(cx)
        })
        .await?;
    run.wait(cx, Duration::from_secs(1)).await;
    let numbers = run.end(cx, clock).await;
    section
        .phases
        .push(numbers.to_json("result", json!({"rows": ROWS, "arrived": arrived})));
    numbers.timing_metrics(
        section,
        "The 100,000-row result published",
        "automation result",
        Some(PUBLISH_RESULT),
    );
    numbers.frame_metrics(
        section,
        "a 100,000-row result arriving",
        AUTOMATION_FRAME_P95,
        Some(FIRST_FRAME_AFTER_RESULT),
        Some(AUTOMATION_STALL),
    );

    // The job's history: pages over 10,000 runs.
    run.update(cx, |view, window, cx| {
        bench.show_job_runs(true, window, cx);
        view.stage = Stage::View(bench.job_view());
        cx.notify();
    })?;
    run.until(cx, RUN_LIMIT, |_, _, cx| {
        let (loading, entries, _) = bench.list_state(List::Job, cx);
        !loading && entries > 0
    })
    .await?;
    let clock = run
        .begin(cx, phase_status(PathId::Automation, "history", "starting"))
        .await?;
    let pages = page_through(run, cx, bench, List::Job, JOB_PAGES, "history").await?;
    let numbers = run.end(cx, clock).await;
    let shown = run.update(cx, |_, _, cx| bench.list_state(List::Job, cx).1)?;
    section.phases.push(numbers.to_json(
        "history",
        json!({"runs_stored": HISTORY_RUNS, "pages_read": pages, "entries_shown": shown}),
    ));
    numbers.timing_metrics(
        section,
        "History page shown",
        "automation history page",
        Some(STATE_PUBLICATION),
    );
    numbers.frame_metrics(
        section,
        "a job's history",
        AUTOMATION_FRAME_P95,
        None,
        Some(AUTOMATION_STALL),
    );

    // The global Runs list: filters, pages and a burst while it shows.
    run.show(cx, Stage::View(bench.runs_view()))?;
    run.until(cx, RUN_LIMIT, |_, _, cx| {
        let (loading, entries, _) = bench.list_state(List::Global, cx);
        !loading && entries > 0
    })
    .await?;
    let clock = run
        .begin(cx, phase_status(PathId::Automation, "runs", "starting"))
        .await?;
    for state in [Some(RunState::Failed), Some(RunState::Cancelled), None] {
        run.update(cx, |view, _, cx| {
            view.status = phase_status(
                PathId::Automation,
                "runs",
                &format!("filter: {}", state.map_or("all", RunState::as_str)),
            );
            bench.filter(List::Global, state, cx);
            cx.notify();
        })?;
        run.until(cx, RUN_LIMIT, |_, _, cx| {
            !bench.list_state(List::Global, cx).0
        })
        .await?;
        run.wait(cx, Duration::from_millis(300)).await;
    }
    let pages = page_through(run, cx, bench, List::Global, GLOBAL_PAGES, "runs").await?;
    run.update(cx, |_, _, cx| bench.run_jobs(cx))?;
    let burst = settle(run, cx, bench).await?;
    let numbers = run.end(cx, clock).await;
    section.phases.push(numbers.to_json(
        "runs",
        json!({"filters": 3, "pages_read": pages, "burst_settled": burst}),
    ));
    numbers.timing_metrics(
        section,
        "Runs list: history page shown",
        "automation history page",
        Some(STATE_PUBLICATION),
    );
    numbers.timing_metrics(
        section,
        "Runs list: engine changes forwarded",
        "automation changes",
        Some(STATE_PUBLICATION),
    );
    numbers.frame_metrics(
        section,
        "the global Runs list",
        AUTOMATION_FRAME_P95,
        None,
        Some(AUTOMATION_STALL),
    );
    Ok(())
}
