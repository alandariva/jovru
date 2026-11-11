//! Editing in the grid (A10): 100,000 synthetic rows made editable with
//! the description a driver would give of their table, so the production
//! `grid_edit` session holds the pending edits. Nothing is written: the
//! run measures adding edits (values typed, NULLs, inserted and deleted
//! rows), each grid view rebuilt after one, and selecting and scrolling
//! with the edits shown.
use super::grid::TICK;
use super::*;
use crate::app::result_grid::Editing;
use jykre::grid_edit::EditSession;
use jykre::performance::budget::{GRID_FRAME_P95, GRID_VIEW_REBUILD, OTHER_FRAME_P95};
use jykre::performance::data;
use jykre::probe::Samples;
use jykre::result_tabs::{Placement, ResultTabs};

const ROWS: usize = 100_000;
const EDITS: usize = 400;

pub(super) async fn run(
    run: &Run,
    cx: &mut AsyncWindowContext,
    section: &mut Section,
) -> Result<(), Closed> {
    run.status(cx, phase_status(PathId::Edit, "preparing", "100,000 rows"))?;
    let data = Arc::new(
        cx.background_executor()
            .spawn(async { metrics::fixture(ROWS) })
            .await,
    );
    // A result tab's identity, as a query tab's run gives one.
    let Some(id) = ResultTabs::new(0)
        .begin(&["SELECT * FROM synthetic".to_owned()], Placement::Hidden)
        .first()
        .copied()
    else {
        section.metrics.push(Metric::missing(
            "Grid view rebuilt after an edit, longest",
            "ms",
            Some(GRID_VIEW_REBUILD),
            "no result tab could be made",
        ));
        return Ok(());
    };
    let grid = run.update(cx, |view, window, cx| {
        let grid = cx.new(|cx| ResultGrid::new(Arc::default(), false, cx));
        grid.update(cx, |grid, cx| {
            grid.set_page(data.clone(), false, 0, Some((id, 0)), cx);
            let session = EditSession::new(
                data::editable_target(data.columns()),
                vec![Some(false); data.column_count()],
            );
            grid.set_editing(
                id,
                Some(Editing::Session(std::sync::Arc::new(session))),
                window,
                cx,
            );
        });
        view.stage = Stage::Grid(grid.clone());
        cx.notify();
        grid
    })?;
    run.wait(cx, Duration::from_secs(1)).await;

    let clock = run
        .begin(cx, phase_status(PathId::Edit, "edit", "starting"))
        .await?;
    let mut edits = Samples::default();
    for tick in 0..EDITS {
        let ((), took) = run.timed(cx, |view, window, cx| {
            grid.update(cx, |grid, cx| grid.benchmark_edit(tick, window, cx));
            if tick % 20 == 0 {
                view.status = phase_status(PathId::Edit, "edit", &format!("{tick} of {EDITS}"));
                cx.notify();
            }
        })?;
        edits.push(took);
        run.wait(cx, TICK).await;
    }
    let numbers = run.end(cx, clock).await;
    let pending = run.update(cx, |_, _, cx| grid.read(cx).pending_edits(None))?;
    let edits = edits.take_summary();
    section.phases.push(numbers.to_json(
        "edit",
        json!({"rows": ROWS, "edits": EDITS, "pending_rows": pending, "edit_ui_thread": summary_json(edits)}),
    ));
    numbers.timing_metrics(
        section,
        "Grid view rebuilt after an edit",
        "grid view",
        Some(GRID_VIEW_REBUILD),
    );
    section.metrics.push(Metric::maybe(
        "An edit's UI-thread time (the rebuild included), p95",
        edits.map(|edits| ms(edits.p95)),
        "ms",
        None,
        "no edit was made",
    ));
    section.metrics.push(Metric::info(
        "Rows with pending edits",
        pending as f64,
        "rows",
    ));
    numbers.frame_metrics(section, "editing", OTHER_FRAME_P95, None, None);

    for (phase, steps) in [("selection", 250), ("scroll", 125)] {
        run.update(cx, |_, _, cx| {
            grid.update(cx, |grid, cx| grid.benchmark_begin(false, cx));
        })?;
        run.wait(cx, Duration::from_millis(300)).await;
        let name = format!("{phase}_with_edits");
        let clock = run
            .begin(cx, phase_status(PathId::Edit, &name, "100,000 rows"))
            .await?;
        for tick in 0..steps {
            run.update(cx, |_, _, cx| {
                grid.update(cx, |grid, cx| grid.benchmark_step(phase, tick, cx));
            })?;
            run.wait(cx, TICK).await;
        }
        let numbers = run.end(cx, clock).await;
        section
            .phases
            .push(numbers.to_json(&name, json!({"rows": ROWS, "scheduled_steps": steps})));
        numbers.frame_metrics(
            section,
            &format!("{phase} with pending edits"),
            GRID_FRAME_P95,
            None,
            None,
        );
    }
    section.notes.push(format!(
        "{EDITS} edits by turns (a value typed into a cell, NULL over a block, a row inserted and given a value, a row marked for deletion) over {ROWS} rows, without writing; the rebuild budget is the grid view's, applied to rebuilds after an edit."
    ));
    Ok(())
}
