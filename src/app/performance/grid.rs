//! The result grid's synthetic sizes: 1,000 and 100,000 rows of five
//! columns, then 1,000 rows of 400 columns with three frozen (A14), each
//! idle, selecting, selecting near the last row, scrolling and panning
//! sideways.
use super::*;
use jykre::performance::budget::GRID_FRAME_P95;

/// Ticks of each phase, 40 ms apart.
pub(super) const PHASES: [(&str, usize); 5] = [
    ("idle", 75),
    ("selection", 250),
    ("selection_bottom", 250),
    ("scroll", 125),
    ("pan", 125),
];
pub(super) const TICK: Duration = Duration::from_millis(40);

pub(super) async fn run(
    run: &Run,
    cx: &mut AsyncWindowContext,
    section: &mut Section,
) -> Result<(), Closed> {
    let grid = run.update(cx, |view, _, cx| {
        let grid = cx.new(|cx| ResultGrid::new(Arc::default(), false, cx));
        view.stage = Stage::Grid(grid.clone());
        cx.notify();
        grid
    })?;
    for (rows, columns, frozen) in [(1000, 5, 0), (100_000, 5, 0), (1000, 400, 3)] {
        let data = cx
            .background_executor()
            .spawn(async move {
                if columns == 5 {
                    metrics::fixture(rows)
                } else {
                    metrics::wide_fixture(rows, columns)
                }
            })
            .await;
        run.update(cx, |_, _, cx| {
            grid.update(cx, |grid, cx| {
                grid.set_page(Arc::new(data), false, 0, None, cx);
                grid.benchmark_freeze(frozen, cx);
            });
        })?;
        run.wait(cx, Duration::from_secs(2)).await;
        let size = format!("{rows} × {columns} ({frozen} frozen)");
        for (phase, steps) in PHASES {
            run.update(cx, |_, _, cx| {
                grid.update(cx, |grid, cx| {
                    grid.benchmark_begin(phase == "selection_bottom", cx)
                });
            })?;
            run.status(cx, phase_status(PathId::Grid, phase, &size))?;
            run.wait(cx, Duration::from_millis(300)).await;
            let clock = run
                .begin(cx, phase_status(PathId::Grid, phase, &size))
                .await?;
            for tick in 0..steps {
                run.update(cx, |_, _, cx| {
                    grid.update(cx, |grid, cx| grid.benchmark_step(phase, tick, cx));
                })?;
                run.wait(cx, TICK).await;
            }
            let numbers = run.end(cx, clock).await;
            section.phases.push(numbers.to_json(
                phase,
                json!({"rows": rows, "columns": columns, "frozen_columns": frozen, "scheduled_steps": steps}),
            ));
            if phase != "idle" {
                let label = format!("{phase}, {size}");
                let frames = numbers.frames();
                section.metrics.push(Metric::maybe(
                    format!("Frame p95, {label}"),
                    frames.map(|frames| ms(frames.p95)),
                    "ms",
                    Some(GRID_FRAME_P95),
                    PROBE_OFF,
                ));
                section.metrics.push(Metric::maybe(
                    format!("Longest UI-thread stall, {label}"),
                    numbers.stalls().map(|stalls| ms(stalls.max)),
                    "ms",
                    None,
                    PROBE_OFF,
                ));
            }
        }
    }
    section.notes.push(
        "Selection, scrolling and panning call the grid's own keyboard and scroll methods every 40 ms; frames are the probe's.".into(),
    );
    Ok(())
}
