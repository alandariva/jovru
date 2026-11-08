//! Opt-in, isolated performance window using the production ResultGrid.
use super::{probe, result_grid::ResultGrid, ui::*};
use gpui_kit::component::{Theme, ThemeMode, TitleBar};
use gpui_kit::*;
use jykre::{performance as metrics, tr};
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct PerformanceView {
    grid: Entity<ResultGrid>,
    status: String,
}

impl PerformanceView {
    fn new(cx: &mut Context<Self>) -> Self {
        let grid = cx.new(|cx| ResultGrid::new(Arc::default(), false, cx));
        cx.spawn(async move |weak, cx| {
            let mut results = Vec::new();
            // The last size is wide, with three frozen columns, for panning
            // sideways (`pan`) past columns that are not built until shown.
            for (rows, columns, frozen) in [(1000, 5, 0), (100_000, 5, 0), (1000, 400, 3)] {
                let data = cx.background_executor().spawn(async move {
                    if columns == 5 { metrics::fixture(rows) } else { metrics::wide_fixture(rows, columns) }
                }).await;
                if weak.update(cx, |view, cx| {
                    view.grid.update(cx, |grid, cx| {
                        grid.set_page(Arc::new(data), false, 0, None, cx);
                        grid.benchmark_freeze(frozen, cx);
                    });
                    cx.notify();
                }).is_err() { return; }
                cx.background_executor().timer(Duration::from_secs(2)).await;
                for (phase, steps) in [("idle", 75), ("selection", 250), ("selection_bottom", 250), ("scroll", 125), ("pan", 125)] {
                    if weak.update(cx, |view, cx| {
                        view.status = format!("{rows} rows × {columns} columns ({frozen} frozen) · {phase}");
                        view.grid.update(cx, |grid, cx| grid.benchmark_begin(phase == "selection_bottom", cx));
                        cx.notify();
                    }).is_err() { return; }
                    cx.background_executor().timer(Duration::from_millis(300)).await;
                    let before = cx.background_executor().spawn(async { metrics::counters() }).await;
                    let started = Instant::now();
                    for tick in 0..steps {
                        if weak.update(cx, |view, cx| {
                            view.grid.update(cx, |grid, cx| grid.benchmark_step(phase, tick, cx));
                        }).is_err() { return; }
                        cx.background_executor().timer(Duration::from_millis(40)).await;
                    }
                    let after = cx.background_executor().spawn(async { metrics::counters() }).await;
                    let seconds = started.elapsed().as_secs_f64();
                    let cpu = before.as_ref().ok().zip(after.as_ref().ok()).and_then(|(a,b)| metrics::cpu_percent(*a,*b,seconds));
                    let errors: Vec<_> = before.as_ref().err().into_iter().chain(after.as_ref().err()).cloned().collect();
                    results.push(json!({"rows":rows,"columns":columns,"frozen_columns":frozen,"phase":phase,"elapsed_seconds":seconds,"scheduled_steps":steps,"cpu_percent_one_core":cpu,"rss_before_bytes":before.ok().map(|v|v.rss_bytes),"rss_after_bytes":after.ok().map(|v|v.rss_bytes),"counter_errors":errors}));
                }
            }
            let report = json!({"version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"debug_build":cfg!(debug_assertions),"logical_cpus":std::thread::available_parallelism().ok().map(usize::from),"measurement":"process CPU (100% = one logical CPU), endpoint resident memory; elapsed phase wall time, not frame latency","input":"in-process production selection/navigation handlers, 40 ms timer; not OS input injection","results":results});
            let saved = cx.background_executor().spawn(async move {
                let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
                let path = std::env::current_dir().map_err(|e|e.to_string())?.join(format!("jykre-performance-{stamp}-{}.json",std::process::id()));
                metrics::write_report(&path, &report)?;
                Ok::<_,String>(path)
            }).await;
            let _ = weak.update(cx, |view, cx| {
                view.status = match saved {
                    Ok(path) => tr!("Performance report: {0}", path.display()),
                    Err(error) => tr!("Could not save performance report: {0}", error),
                };
                tracing::info!(status = %view.status, "performance test finished");
                cx.notify();
            });
        }).detach();
        Self {
            grid,
            status: tr!("Running synthetic performance test…").into(),
        }
    }
}

impl Render for PerformanceView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // With `JYKRE_PROBE=1` the frame probe times this window's frames.
        probe::frame_started();
        probe::rendered("PerformanceView");
        column()
            .size_full()
            .bg(rgb(colors().bg))
            .text_color(rgb(colors().text))
            .child(div().flex_1().min_h_0().child(self.grid.clone()))
            .child(div().p_3().child(self.status.clone()))
            .child(probe::frame_end())
    }
}

pub fn run() {
    application().with_assets(assets::AllAssets).run(|cx| {
        init(cx);
        super::fonts::resolve(cx);
        probe::start(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1480.), px(920.)),
                cx,
            ))),
            ..TitleBar::window_options()
        };
        if open_window(options, cx, |window, cx| {
            window.set_window_title("Jykre Studio · Performance test");
            cx.new(PerformanceView::new)
        })
        .is_err()
        {
            cx.quit();
        }
    });
}
