//! The Automation interface as the performance run (`--test-performance`,
//! A6) drives it: the production job tabs, their **Runs** lists and the
//! global **Runs** list over an engine of the run's own, in a temporary
//! folder. It plays the host's part `AppView` plays in the application:
//! it hands the views their jobs and connections, and forwards the engine's
//! changes, coalesced to one read per frame. Nothing here runs at start-up.
use super::history::HistoryList;
use super::job_view::{Inner, JobView, Setup};
use super::runs_view::RunsView;
use super::{FRAME, JobItem, JobSources, Jobs};
use crate::app::report_panel::ReportSource;
use gpui_kit::*;
use jykre::automation::definition::JobDefinition;
use jykre::automation::{Engine, RunState};
use jykre::drivers::{ConnectionId, SqlDialect};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

/// A saved job and its stored revision.
pub(in crate::app) struct BenchJob {
    pub definition: JobDefinition,
    pub revision: i64,
}

/// Which history list a step drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum List {
    /// The first job's **Runs** tab.
    Job,
    /// The global **Runs** list.
    Global,
}

pub(in crate::app) struct Bench {
    engine: Engine,
    jobs: Vec<Entity<JobView>>,
    big: Entity<JobView>,
    runs: Entity<RunsView>,
    _changes: Task<()>,
}

impl Bench {
    /// A tab for each of `jobs` and for `big`, and the global list, over
    /// `engine` and the connections of `sources`.
    pub(in crate::app) fn new<V: 'static>(
        engine: Engine,
        jobs: Vec<BenchJob>,
        big: BenchJob,
        sources: Vec<ReportSource>,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Self {
        let items: Jobs = Rc::new(
            jobs.iter()
                .chain(std::iter::once(&big))
                .map(|job| JobItem {
                    id: job.definition.id,
                    kind: job.definition.kind(),
                    name: job.definition.name.clone(),
                    revision: job.revision,
                    definition: job.definition.clone(),
                    schedule: None,
                })
                .collect(),
        );
        let revisions: HashMap<ConnectionId, i64> = sources
            .iter()
            .filter_map(|source| source.connection.id)
            .map(|id| (id, 1))
            .collect();
        engine.set_connection_revisions(revisions.clone());
        let connections: Rc<HashMap<ConnectionId, (String, Option<SqlDialect>)>> = Rc::new(
            sources
                .iter()
                .filter_map(|source| {
                    Some((
                        source.connection.id?,
                        (source.connection.name.clone(), source.dialect),
                    ))
                })
                .collect(),
        );
        let job_sources = JobSources {
            sources: Rc::new(sources),
            revisions: Rc::new(revisions),
        };
        let view = |job: BenchJob, window: &mut Window, cx: &mut Context<V>| {
            let setup = Setup {
                engine: engine.clone(),
                definition: job.definition,
                revision: Some(job.revision),
                sources: job_sources.clone(),
                catalogs: Arc::default(),
                jobs: items.clone(),
                availability: engine.availability(),
                connections: connections.clone(),
            };
            cx.new(|cx| JobView::new(setup, window, cx))
        };
        let views: Vec<Entity<JobView>> =
            jobs.into_iter().map(|job| view(job, window, cx)).collect();
        let big = view(big, window, cx);
        let runs =
            cx.new(|cx| RunsView::new(engine.clone(), items.clone(), connections.clone(), cx));
        // The engine's changes reach every view at most once a frame, as
        // `AppView::automation_changed` does.
        let mut changes = engine.watch_changes();
        let watched: Vec<WeakEntity<JobView>> = views
            .iter()
            .chain(std::iter::once(&big))
            .map(Entity::downgrade)
            .collect();
        let watched_runs = runs.downgrade();
        let task = cx.spawn(async move |_, cx| {
            while changes.changed().await.is_ok() {
                cx.background_executor().timer(FRAME).await;
                changes.borrow_and_update();
                let alive = cx.update(|cx| {
                    let _timer = jykre::logging::UiTimer::start("automation changes");
                    for view in &watched {
                        let _ = view.update(cx, |view, cx| view.engine_changed(cx));
                    }
                    watched_runs
                        .update(cx, |view, cx| view.engine_changed(cx))
                        .is_ok()
                });
                if !alive {
                    break;
                }
            }
        });
        Self {
            engine,
            jobs: views,
            big,
            runs,
            _changes: task,
        }
    }

    pub(in crate::app) fn engine(&self) -> Engine {
        self.engine.clone()
    }

    /// The first job's tab.
    pub(in crate::app) fn job_view(&self) -> AnyView {
        self.jobs
            .first()
            .map_or_else(|| self.big.clone().into(), |job| job.clone().into())
    }

    /// The tab of the job with the large result.
    pub(in crate::app) fn big_view(&self) -> AnyView {
        self.big.clone().into()
    }

    pub(in crate::app) fn runs_view(&self) -> AnyView {
        self.runs.clone().into()
    }

    /// **Run** in every small job's tab at once: a burst of statuses.
    pub(in crate::app) fn run_jobs(&self, cx: &mut App) {
        for job in &self.jobs {
            job.update(cx, |job, cx| job.run(cx));
        }
    }

    /// **Run** in the large job's tab.
    pub(in crate::app) fn run_big(&self, cx: &mut App) {
        self.big.update(cx, |job, cx| job.run(cx));
    }

    /// Nothing is queued or active, in the engine or in the tabs.
    pub(in crate::app) fn idle(&self, cx: &App) -> bool {
        self.engine.overview().is_empty()
            && self
                .jobs
                .iter()
                .chain(std::iter::once(&self.big))
                .all(|job| !job.read(cx).benchmark_running().0)
    }

    /// The large job's result is in its grid.
    pub(in crate::app) fn big_has_result(&self, cx: &App) -> bool {
        self.big.read(cx).benchmark_running().1
    }

    /// Shows the first job's **Runs** tab, or its **Inputs** with the run's
    /// outcome and result.
    pub(in crate::app) fn show_job_runs(&self, runs: bool, window: &mut Window, cx: &mut App) {
        let inner = if runs { Inner::Runs } else { Inner::Inputs };
        for job in self.jobs.iter().chain(std::iter::once(&self.big)) {
            job.update(cx, |job, cx| job.benchmark_show(inner, window, cx));
        }
    }

    fn history(&self, list: List, cx: &App) -> Entity<HistoryList> {
        match (list, self.jobs.first()) {
            (List::Job, Some(job)) => job.read(cx).benchmark_history(),
            (List::Job, None) => self.big.read(cx).benchmark_history(),
            (List::Global, _) => self.runs.read(cx).benchmark_history(),
        }
    }

    /// **Load older runs** in `list`.
    pub(in crate::app) fn older(&self, list: List, cx: &mut App) {
        self.history(list, cx)
            .update(cx, |history, cx| history.benchmark_older(cx));
    }

    /// Whether `list` is reading a page, its entries and whether there are
    /// more.
    pub(in crate::app) fn list_state(&self, list: List, cx: &App) -> (bool, usize, bool) {
        self.history(list, cx).read(cx).benchmark_state()
    }

    pub(in crate::app) fn scroll(&self, list: List, index: usize, cx: &mut App) {
        self.history(list, cx)
            .update(cx, |history, cx| history.benchmark_scroll(index, cx));
    }

    pub(in crate::app) fn choose(&self, list: List, index: usize, cx: &mut App) {
        self.history(list, cx)
            .update(cx, |history, cx| history.benchmark_choose(index, cx));
    }

    /// Filters `list` by state, as its **State** menu does.
    pub(in crate::app) fn filter(&self, list: List, state: Option<RunState>, cx: &mut App) {
        self.history(list, cx)
            .update(cx, |history, cx| history.set_state(state, cx));
    }
}
