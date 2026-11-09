//! The opt-in performance run (`--test-performance`): an isolated window
//! that drives the production views through the same methods as their keys
//! and buttons, and reports each path's numbers against its budget
//! ([Performance](../../../docs/standards/performance.md#how-to-measure)).
//!
//! It loads no connection, workspace, vault or data folder of the user:
//! every file and database it makes lives in a temporary folder removed at
//! the end. What needs no window (the data, the budgets, the report) is
//! `jykre::performance`. Closing the window stops the run at its next
//! update.
mod automation;
mod columns;
mod edit;
mod editor;
mod grid;
mod import;

use super::sql_editor::{GoToLine, SqlEditor};
use super::{probe, result_grid::ResultGrid, ui::*};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::{Theme, ThemeMode, TitleBar};
use gpui_kit::*;
use jykre::performance::budget::{Budget, Metric};
use jykre::performance::report::{Environment, Report, Section};
use jykre::performance::selection::{self, PathId};
use jykre::performance::temp::{self, TempFolder};
use jykre::performance::timings::Timings;
use jykre::performance::{self as metrics, Counters};
use jykre::probe::Summary;
use jykre::tr;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Why a frame or stall metric is missing without the probe.
const PROBE_OFF: &str = "the frame probe is off; run with JYKRE_PROBE=1 to time frames and stalls";

/// What the window shows.
enum Stage {
    Empty,
    Grid(Entity<ResultGrid>),
    Editor {
        editor: Entity<EditorState>,
        prompt: Entity<GoToLine>,
        /// The editor adapter; the prompt holds it weakly.
        _adapter: Entity<SqlEditor>,
    },
    Import(import::Shown),
    View(AnyView),
}

pub(super) struct PerformanceView {
    stage: Stage,
    status: String,
}

/// The window closed: the run stops.
struct Closed;

/// What every phase shares.
struct Run {
    view: WeakEntity<PerformanceView>,
    timings: Timings,
    folder: PathBuf,
}

/// When a phase started, and the process counters then.
struct Clock {
    started: Instant,
    before: Result<Counters, String>,
}

/// What a phase measured besides its own numbers.
struct Numbers {
    seconds: f64,
    cpu_percent: Option<f64>,
    rss_before: Option<u64>,
    rss_after: Option<u64>,
    counter_errors: Vec<String>,
    probe: Option<probe::PhaseSummary>,
    timings: BTreeMap<String, Summary>,
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn summary_json(summary: Option<Summary>) -> Value {
    summary.map_or(Value::Null, |summary| {
        json!({
            "count": summary.count,
            "median_ms": ms(summary.median),
            "p95_ms": ms(summary.p95),
            "max_ms": ms(summary.max),
        })
    })
}

impl Numbers {
    fn frames(&self) -> Option<Summary> {
        self.probe.and_then(|probe| probe.frames)
    }

    fn stalls(&self) -> Option<Summary> {
        self.probe.and_then(|probe| probe.stalls)
    }

    fn timing(&self, operation: &str) -> Option<Summary> {
        self.timings.get(operation).copied()
    }

    /// The phase as the report lists it, with `extra` fields.
    fn to_json(&self, phase: &str, extra: Value) -> Value {
        let mut value = json!({
            "phase": phase,
            "elapsed_seconds": self.seconds,
            "cpu_percent_one_core": self.cpu_percent,
            "rss_before_bytes": self.rss_before,
            "rss_after_bytes": self.rss_after,
            "counter_errors": self.counter_errors,
            "frames": summary_json(self.frames()),
            "stalls": summary_json(self.stalls()),
            "ui_thread": self.timings.iter().map(|(operation, summary)| {
                (operation.clone(), summary_json(Some(*summary)))
            }).collect::<serde_json::Map<_, _>>(),
        });
        if let (Some(object), Value::Object(extra)) = (value.as_object_mut(), extra) {
            object.extend(extra);
        }
        value
    }

    /// The phase's frame p95 held to `frame`, and its longest frame and
    /// stall, held to `stall` when given; missing without the probe.
    fn frame_metrics(
        &self,
        section: &mut Section,
        label: &str,
        frame: Budget,
        longest: Option<Budget>,
        stall: Option<Budget>,
    ) {
        let frames = self.frames();
        let reason = PROBE_OFF;
        section.metrics.push(Metric::maybe(
            format!("Frame p95, {label}"),
            frames.map(|frames| ms(frames.p95)),
            "ms",
            Some(frame),
            reason,
        ));
        section.metrics.push(Metric::maybe(
            format!("Longest frame, {label}"),
            frames.map(|frames| ms(frames.max)),
            "ms",
            longest,
            reason,
        ));
        section.metrics.push(Metric::maybe(
            format!("Longest UI-thread stall, {label}"),
            self.stalls().map(|stalls| ms(stalls.max)),
            "ms",
            stall,
            reason,
        ));
    }

    /// `operation`'s longest UI-thread time held to `budget`, and its p95.
    fn timing_metrics(
        &self,
        section: &mut Section,
        label: &str,
        operation: &str,
        budget: Option<Budget>,
    ) {
        let timing = self.timing(operation);
        let reason = format!("no `{operation}` timing was recorded in this phase");
        section.metrics.push(Metric::maybe(
            format!("{label}: longest"),
            timing.map(|timing| ms(timing.max)),
            "ms",
            budget,
            &reason,
        ));
        section.metrics.push(Metric::maybe(
            format!("{label}: p95"),
            timing.map(|timing| ms(timing.p95)),
            "ms",
            None,
            &reason,
        ));
    }
}

impl Run {
    /// Runs `update` on the view with its window.
    fn update<R>(
        &self,
        cx: &mut AsyncWindowContext,
        update: impl FnOnce(&mut PerformanceView, &mut Window, &mut Context<PerformanceView>) -> R,
    ) -> Result<R, Closed> {
        self.view.update_in(cx, update).map_err(|_| Closed)
    }

    /// Runs `update` and says how long it took on the UI thread, the
    /// effects it emitted included.
    fn timed<R>(
        &self,
        cx: &mut AsyncWindowContext,
        update: impl FnOnce(&mut PerformanceView, &mut Window, &mut Context<PerformanceView>) -> R,
    ) -> Result<(R, Duration), Closed> {
        let started = Instant::now();
        let result = self.update(cx, update)?;
        Ok((result, started.elapsed()))
    }

    fn status(&self, cx: &mut AsyncWindowContext, status: String) -> Result<(), Closed> {
        self.update(cx, |view, _, cx| {
            view.status = status;
            cx.notify();
        })
    }

    fn show(&self, cx: &mut AsyncWindowContext, stage: Stage) -> Result<(), Closed> {
        self.update(cx, |view, _, cx| {
            view.stage = stage;
            cx.notify();
        })
    }

    async fn wait(&self, cx: &mut AsyncWindowContext, duration: Duration) {
        cx.background_executor().timer(duration).await;
    }

    /// Waits until `ready` holds, checking every 20 ms, for at most
    /// `limit`. False when it never did.
    async fn until(
        &self,
        cx: &mut AsyncWindowContext,
        limit: Duration,
        mut ready: impl FnMut(&mut PerformanceView, &mut Window, &mut Context<PerformanceView>) -> bool,
    ) -> Result<bool, Closed> {
        let started = Instant::now();
        loop {
            if self.update(cx, &mut ready)? {
                return Ok(true);
            }
            if started.elapsed() > limit {
                return Ok(false);
            }
            self.wait(cx, Duration::from_millis(20)).await;
        }
    }

    /// Starts a phase: the status names it, the counters are read, and the
    /// probe and the timings start over.
    async fn begin(&self, cx: &mut AsyncWindowContext, status: String) -> Result<Clock, Closed> {
        self.status(cx, status)?;
        let before = cx
            .background_executor()
            .spawn(async { metrics::counters() })
            .await;
        probe::begin_phase();
        self.timings.clear();
        Ok(Clock {
            started: Instant::now(),
            before,
        })
    }

    async fn end(&self, cx: &mut AsyncWindowContext, clock: Clock) -> Numbers {
        let seconds = clock.started.elapsed().as_secs_f64();
        let probe = probe::end_phase();
        let timings = self.timings.take();
        let after = cx
            .background_executor()
            .spawn(async { metrics::counters() })
            .await;
        let before = clock.before;
        let cpu_percent = before
            .as_ref()
            .ok()
            .zip(after.as_ref().ok())
            .and_then(|(before, after)| metrics::cpu_percent(*before, *after, seconds));
        let counter_errors = before
            .as_ref()
            .err()
            .into_iter()
            .chain(after.as_ref().err())
            .cloned()
            .collect();
        Numbers {
            seconds,
            cpu_percent,
            rss_before: before.ok().map(|counters| counters.rss_bytes),
            rss_after: after.ok().map(|counters| counters.rss_bytes),
            counter_errors,
            probe,
            timings,
        }
    }
}

impl PerformanceView {
    fn new(
        paths: Result<Vec<PathId>, String>,
        timings: Timings,
        probe: bool,
        leftovers: Rc<RefCell<Vec<PathBuf>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let paths = match paths {
            Ok(paths) => paths,
            Err(error) => {
                return Self {
                    stage: Stage::Empty,
                    status: error,
                };
            }
        };
        cx.spawn_in(window, async move |view, cx| {
            let started = Instant::now();
            let folder = match TempFolder::create() {
                Ok(folder) => folder,
                Err(error) => {
                    let _ = view.update(cx, |view, cx| {
                        view.status = error;
                        cx.notify();
                    });
                    return;
                }
            };
            leftovers.borrow_mut().push(folder.path().to_path_buf());
            let run = Run {
                view,
                timings,
                folder: folder.path().to_path_buf(),
            };
            let mut environment = Environment::current(probe);
            let Ok(window) = run.update(cx, |_, window, _| {
                let size = window.viewport_size();
                (
                    f32::from(size.width),
                    f32::from(size.height),
                    window.scale_factor(),
                )
            }) else {
                return;
            };
            environment.window = Some(window);
            let mut sections = Vec::new();
            for path in paths {
                let mut section = Section::new(path);
                let done = match path {
                    PathId::Grid => grid::run(&run, cx, &mut section).await,
                    PathId::Editor => editor::run(&run, cx, &mut section).await,
                    PathId::Edit => edit::run(&run, cx, &mut section).await,
                    PathId::Columns => columns::run(&run, cx, &mut section).await,
                    PathId::Import => import::run(&run, cx, &mut section).await,
                    PathId::Automation => automation::run(&run, cx, &mut section).await,
                };
                if done.is_err() || run.show(cx, Stage::Empty).is_err() {
                    return;
                }
                sections.push(section);
            }
            let _ = run.status(cx, "Writing the report…".into());
            let removed = cx
                .background_executor()
                .spawn(async move { folder.remove() })
                .await;
            let mut cleanup = Vec::new();
            match removed {
                Ok(()) => leftovers.borrow_mut().retain(|path| *path != run.folder),
                Err(error) => cleanup.push(error),
            }
            let report = Report {
                environment,
                sections,
                started: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |now| now.as_secs())
                    .saturating_sub(started.elapsed().as_secs()),
                elapsed_seconds: started.elapsed().as_secs_f64(),
                cleanup,
            };
            let saved = cx
                .background_executor()
                .spawn(async move {
                    let stamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(|e| e.to_string())?
                        .as_nanos();
                    let dir = std::env::current_dir().map_err(|e| e.to_string())?;
                    let stem = metrics::report::file_stem(stamp, std::process::id());
                    metrics::report::write(&dir, &stem, &report)
                })
                .await;
            let _ = run.update(cx, |view, _, cx| {
                view.status = match saved {
                    Ok((path, _)) => tr!("Performance report: {0}", path.display()),
                    Err(error) => tr!("Could not save performance report: {0}", error),
                };
                tracing::info!(status = %view.status, "performance test finished");
                cx.notify();
            });
        })
        .detach();
        Self {
            stage: Stage::Empty,
            status: tr!("Running synthetic performance test…").into(),
        }
    }

    /// **Cancel import**, from the progress line's button or the run.
    fn cancel_import(&mut self, cx: &mut Context<Self>) {
        if let Stage::Import(shown) = &self.stage {
            shown.cancel.cancel();
            cx.notify();
        }
    }

    fn import_running(&self) -> bool {
        matches!(&self.stage, Stage::Import(shown) if shown.outcome.is_none())
    }
}

impl Render for PerformanceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // With `JYKRE_PROBE=1` the frame probe times this window's frames.
        probe::frame_started();
        probe::rendered("PerformanceView");
        let body = match &mut self.stage {
            Stage::Empty => div().into_any_element(),
            Stage::Grid(grid) => grid.clone().into_any_element(),
            Stage::Editor { editor, prompt, .. } => div()
                .relative()
                .size_full()
                .child(Editor::new(editor).h_full())
                .child(prompt.clone())
                .into_any_element(),
            Stage::View(view) => view.clone().into_any_element(),
            Stage::Import(shown) => {
                shown.mark();
                let status = match &shown.outcome {
                    Some(outcome) => div().child(outcome.clone()).into_any_element(),
                    None => super::import::running_status(
                        shown.started,
                        *shown.progress.borrow(),
                        Self::cancel_import,
                        cx,
                    ),
                };
                column()
                    .p_6()
                    .gap_3()
                    .child(div().text_size(px(15.)).child(shown.title.clone()))
                    .child(status)
                    .into_any_element()
            }
        };
        column()
            .size_full()
            .bg(rgb(colors().bg))
            .text_color(rgb(colors().text))
            .child(div().flex_1().min_h_0().child(body))
            .child(div().p_3().child(self.status.clone()))
            .child(probe::frame_end())
    }
}

/// Opens the performance window for `paths` (the switch's list, or why it
/// could not be read). Never called at a normal start.
pub fn run(paths: Result<Vec<PathId>, String>) {
    application().with_assets(assets::AllAssets).run(|cx| {
        init(cx);
        super::fonts::resolve(cx);
        let probe = probe::start(cx);
        // UiTimer timings, in memory only: no log file is written.
        let timings = Timings::default();
        timings.install();
        Theme::change(ThemeMode::Dark, None, cx);
        // Folders a closed window left behind are removed on the way out.
        let leftovers: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
        let on_quit = leftovers.clone();
        cx.on_app_quit(move |cx| {
            let folders = std::mem::take(&mut *on_quit.borrow_mut());
            let executor = cx.background_executor().clone();
            async move {
                for folder in folders {
                    let _ = executor
                        .spawn(async move { temp::remove(&folder, 8, Duration::from_millis(250)) })
                        .await;
                }
            }
        })
        .detach();
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
            cx.new(|cx| PerformanceView::new(paths, timings, probe, leftovers, window, cx))
        })
        .is_err()
        {
            cx.quit();
        }
    });
}

/// The status line's text for a phase of `path`.
fn phase_status(path: PathId, phase: &str, detail: &str) -> String {
    if detail.is_empty() {
        format!("{} · {phase}", path.title())
    } else {
        format!("{} · {phase} · {detail}", path.title())
    }
}

/// The switch's argument, read from the command line: `None` for a normal
/// start.
pub fn requested() -> Option<Result<Vec<PathId>, String>> {
    std::env::args_os().find_map(|argument| argument.to_str().and_then(selection::from_argument))
}

#[cfg(test)]
mod tests {
    use super::{PathId, phase_status};

    #[test]
    fn the_status_names_the_path_and_the_phase() {
        crate::app::source_text();
        assert_eq!(
            phase_status(PathId::Editor, "go_to_line", "12 of 60"),
            "Go to a line (A9) · go_to_line · 12 of 60"
        );
        assert_eq!(
            phase_status(PathId::Grid, "pan", ""),
            "Result grid (A14) · pan"
        );
    }
}
