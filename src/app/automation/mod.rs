//! The Automation section of the rail and its tabs (specification, section
//! 8.1; Q-20…Q-22): the engine as an application-root service
//! ([ADR-0034](../../../docs/adr/0034-automation-database-service-and-lock.md)),
//! the list of jobs and chains in the sidebar, a tab per job with its
//! **Editor**, **Run and result** and **History**, and the global **Runs**
//! list.
//!
//! `AppView` creates one [`Engine`] at start. Runs belong to the engine,
//! not to a tab: closing a job's tab never stops its run. Closing the
//! window with runs active asks first, stops them and waits
//! ([`AppView::automation_may_close`]). The engine's changes arrive through
//! a counter that is read at most once per frame.
//!
//! The job tabs and the Runs tab share the Automation tab group, as report
//! tabs share the Reports group; it offers no new tab and takes no other tab.
pub(super) mod benchmark;
mod history;
mod job_view;
mod logic;
mod program_bits;
mod runs_view;
mod schedule_words;
mod words;

use super::report_panel::{ReportSource, Sources};
use super::*;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use history::HistoryEvent;
use job_view::{JobEvent, JobView};
use jykre::automation::definition::JobDefinition;
use jykre::automation::schedule::{ScheduleConnection, ScheduleNews};
use jykre::automation::{
    Availability, ConnectionInput, Engine, JobId, JobKind, OffReason, Recovery,
};
use runs_view::RunsView;
use std::rc::Rc;
use std::time::Duration;

/// How often at most the views read the engine's changes: once a frame.
const FRAME: Duration = Duration::from_millis(16);

/// A stored job as the sidebar and the chain editor list it.
#[derive(Clone)]
pub(super) struct JobItem {
    pub id: JobId,
    pub kind: JobKind,
    pub name: String,
    pub revision: i64,
    pub definition: JobDefinition,
    /// Its schedule, as the list shows it.
    pub schedule: Option<ScheduleLine>,
}

/// A job's schedule in the sidebar: its next run in its zone, or its state.
#[derive(Clone)]
pub(super) struct ScheduleLine {
    /// Few words: `Next 09:00 CET`, `Suspended`, `Off`.
    pub short: String,
    /// The sentence for the accessible label.
    pub full: String,
    pub suspended: bool,
    /// Its IANA zone, for the due times of its runs.
    pub zone: String,
}

/// The stored jobs, sorted by name.
pub(super) type Jobs = Rc<Vec<JobItem>>;

/// The connections a job may run on, with their profile revisions.
#[derive(Clone, Default)]
pub(super) struct JobSources {
    pub sources: Sources,
    pub revisions: Rc<HashMap<ConnectionId, i64>>,
}

/// What an Automation tab shows.
enum TabView {
    Job(Entity<JobView>),
    Runs(Entity<RunsView>),
}

pub(super) struct AutomationTab {
    view: TabView,
    group: TabGroupId,
    _subscriptions: Vec<Subscription>,
}

/// The engine and what the Automation section shows of it.
#[derive(Default)]
pub(super) struct Host {
    engine: Option<Engine>,
    availability: Option<Availability>,
    jobs: Jobs,
    jobs_loading: bool,
    jobs_generation: u64,
    scroll: UniformListScrollHandle,
    tabs: HashMap<TabId, AutomationTab>,
    /// Shown once in the section: what recovery found at start.
    notice: Option<Feedback>,
    retrying: bool,
    /// Runs are being stopped before the window closes.
    stopping: Option<usize>,
    /// The user agreed to stop the active runs and they stopped: the window
    /// closes without asking again.
    pub(super) quit_settled: bool,
    /// Scheduled runs that ended badly (Q-40), and how many the user has
    /// seen: the rail and the status bar show the difference.
    news: ScheduleNews,
    news_seen: u64,
}

impl Host {
    pub(super) fn tab_groups(&self) -> impl Iterator<Item = TabGroupId> + '_ {
        self.tabs.values().map(|tab| tab.group)
    }

    pub(super) fn has_tabs(&self) -> bool {
        !self.tabs.is_empty()
    }

    pub(super) fn owns(&self, tab: TabId) -> bool {
        self.tabs.contains_key(&tab)
    }

    pub(super) fn forget(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
    }

    /// While runs are being stopped for the window to close.
    pub(super) fn stopping(&self) -> Option<usize> {
        self.stopping
    }
}

impl AppView {
    /// Starts the engine on the application's runtime. Without a runtime
    /// automation stays off and the rest of the application works.
    pub(super) fn start_automation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let runtime = match backend::runtime_handle() {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::warn!(error_code = ?error.code, "automation unavailable: no runtime");
                self.automation.availability = Some(Availability::Off(OffReason::Unreadable));
                return;
            }
        };
        let engine = Engine::start(runtime, connection_store::data_dir());
        self.automation.engine = Some(engine.clone());
        self.automation.availability = Some(engine.availability());
        self.sync_automation(cx);
        // Availability: read-only, off, or ready; the recovery result is
        // shown once, when it is ready.
        let mut availability = engine.watch_availability();
        cx.spawn_in(window, async move |weak, cx| {
            loop {
                let state = availability.borrow_and_update().clone();
                let alive = weak
                    .update_in(cx, |this, window, cx| {
                        this.automation_available(state, window, cx)
                    })
                    .is_ok();
                if !alive || availability.changed().await.is_err() {
                    break;
                }
            }
        })
        .detach();
        // Scheduled runs that ended badly: the indicator, and a notification
        // for the schedules that asked for one (Q-40).
        let mut news = engine.watch_schedule_news();
        cx.spawn(async move |weak, cx| {
            while news.changed().await.is_ok() {
                let latest = news.borrow_and_update().clone();
                if weak
                    .update(cx, |this, cx| this.schedule_news(latest, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        // Runs starting, waiting, ending and jobs saved: coalesced to one
        // read per frame.
        let mut changes = engine.watch_changes();
        cx.spawn(async move |weak, cx| {
            while changes.changed().await.is_ok() {
                cx.background_executor().timer(FRAME).await;
                changes.borrow_and_update();
                if weak
                    .update(cx, |this, cx| this.automation_changed(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn automation_available(
        &mut self,
        state: Availability,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ready = state == Availability::Ready;
        self.automation.notice = logic::notice_after(self.automation.notice.take(), &state, false);
        self.automation.availability = Some(state.clone());
        if ready || state == Availability::ReadOnly {
            self.reload_jobs(cx);
        }
        for view in self.job_views() {
            let state = state.clone();
            view.update(cx, |view, cx| view.set_availability(state, cx));
        }
        if ready
            && let Some(recovery) = self
                .automation
                .engine
                .as_ref()
                .and_then(Engine::take_recovery)
        {
            self.show_recovery(recovery, window, cx);
        }
        cx.notify();
    }

    /// What recovery found, once: a damaged file set aside, runs
    /// interrupted, and (Unix) programs left running, which the user may end.
    fn show_recovery(&mut self, recovery: Recovery, window: &mut Window, cx: &mut Context<Self>) {
        let mut parts = Vec::new();
        if recovery.recreated {
            parts.push(
                tr!(
                    "automation.db was damaged: it was set aside and a new one created, so the job definitions it held were lost."
                )
                .to_owned(),
            );
        }
        if recovery.interrupted > 0 {
            parts.push(words::interrupted_runs(recovery.interrupted));
        }
        if !parts.is_empty() {
            let tone = if recovery.recreated {
                Tone::Warning
            } else {
                Tone::Info
            };
            self.automation.notice = Some(Feedback::new(tone, parts.join(" ")));
        }
        if recovery.recreated {
            self.app_notice = Some(Feedback::new(
                Tone::Warning,
                tr!(
                    "automation.db was damaged and set aside; a new one was created and its job definitions were lost."
                ),
            ));
        }
        if recovery.orphans.is_empty() {
            return;
        }
        let Some(engine) = self.automation.engine.clone() else {
            return;
        };
        let count = recovery.orphans.len();
        let orphans = recovery.orphans;
        confirm(
            window,
            cx,
            Confirmation {
                title: if count == 1 {
                    tr!("A program from the last session is still running").to_owned()
                } else {
                    tr!("{0} programs from the last session are still running", count)
                },
                description: tr!(
                    "Jykre Studio closed while they ran, and their process groups are still alive. Ending them kills each group; leaving them lets them finish on their own. Their runs are marked interrupted either way."
                )
                .to_owned(),
                ok: tr!("End them"),
                cancel: tr!("Leave them"),
                focus: ConfirmFocus::Dialog,
            },
            move |_, cx| {
                let engine = engine.clone();
                let orphans = orphans.clone();
                cx.spawn(async move |_| {
                    let _ = backend::run(async move {
                        engine.end_orphans(orphans).await;
                        Ok(())
                    })
                    .await;
                })
                .detach();
            },
        );
    }

    /// Something changed in the engine: the list of jobs, the open views'
    /// histories and the global list read it again.
    fn automation_changed(&mut self, cx: &mut Context<Self>) {
        let _timer = jykre::logging::UiTimer::start("automation changes");
        self.reload_jobs(cx);
        for tab in self.automation.tabs.values() {
            match &tab.view {
                TabView::Job(view) => view.update(cx, |view, cx| view.engine_changed(cx)),
                TabView::Runs(view) => view.update(cx, |view, cx| view.engine_changed(cx)),
            }
        }
    }

    /// Reads the stored jobs off the UI thread, sorted by name.
    fn reload_jobs(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.automation.engine.as_ref().and_then(Engine::repository) else {
            return;
        };
        self.automation.jobs_generation += 1;
        let generation = self.automation.jobs_generation;
        self.automation.jobs_loading = true;
        cx.spawn(async move |weak, cx| {
            let read = backend::run(async move {
                let stored = repository.jobs().await?;
                let schedules = repository.schedules().await?;
                let mut jobs: Vec<JobItem> = stored
                    .into_iter()
                    .filter_map(|job| {
                        let definition = JobDefinition::from_json(&job.definition_json).ok()?;
                        Some(JobItem {
                            id: job.id,
                            kind: job.kind,
                            name: definition.name.clone(),
                            revision: job.revision,
                            definition,
                            schedule: schedules
                                .iter()
                                .find(|schedule| schedule.draft.job_id == job.id)
                                .map(schedule_line),
                        })
                    })
                    .collect();
                jobs.sort_by_cached_key(|job| job.name.to_lowercase());
                Ok(jobs)
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                if this.automation.jobs_generation != generation {
                    return;
                }
                this.automation.jobs_loading = false;
                match read {
                    Ok(jobs) => {
                        this.automation.jobs = Rc::new(jobs);
                        let jobs = this.automation.jobs.clone();
                        for tab in this.automation.tabs.values() {
                            match &tab.view {
                                TabView::Job(view) => {
                                    let jobs = jobs.clone();
                                    view.update(cx, |view, cx| view.set_jobs(jobs, cx));
                                }
                                TabView::Runs(view) => {
                                    let jobs = jobs.clone();
                                    view.update(cx, |view, cx| view.set_jobs(jobs, cx));
                                }
                            }
                        }
                    }
                    Err(error) => {
                        this.automation.notice = Some(Feedback::new(
                            Tone::Error,
                            tr!("Could not read the jobs: {0}", error),
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The connections jobs may run on and their profile revisions, as the
    /// host knows them now: handed to the engine (`PlanChanged`) and to
    /// every job tab. Called whenever connections load or change.
    pub(super) fn sync_automation(&self, cx: &mut Context<Self>) {
        let revisions: HashMap<ConnectionId, i64> = self
            .connections
            .iter()
            .filter_map(|connection| connection.profile.as_ref())
            .map(|profile| (profile.id, profile.revision))
            .collect();
        if let Some(engine) = &self.automation.engine {
            // Until the saved connections are read, the engine has none to
            // check grants against, rather than an empty list that would
            // suspend every schedule as if its connections were deleted.
            if let Some(connections) =
                known_connections(self.connections_loading, || self.schedule_connections())
            {
                engine.set_connections(connections);
            }
            // Every profile's revision, those that cannot run now included.
            engine.set_connection_revisions(revisions.clone());
        }
        if self.automation.tabs.is_empty() {
            return;
        }
        let sources = JobSources {
            sources: self.report_sources(),
            revisions: Rc::new(revisions),
        };
        for view in self.job_views() {
            let sources = sources.clone();
            view.update(cx, |view, cx| view.set_sources(sources, cx));
        }
        let connections = self.connection_names();
        for tab in self.automation.tabs.values() {
            if let TabView::Runs(view) = &tab.view {
                let connections = connections.clone();
                view.update(cx, |view, cx| view.set_connections(connections, cx));
            }
        }
    }

    /// The connections schedules may use (section 9.5.7): each profile with
    /// its revision and environment label, and its password only when the
    /// profile remembers it, so a schedule never uses one typed for this
    /// session.
    fn schedule_connections(&self) -> Vec<ScheduleConnection> {
        self.connections
            .iter()
            .filter_map(|connection| {
                let profile = connection.profile.as_ref()?;
                let mut resolved = connection.input.clone()?;
                if !profile.remember_password {
                    resolved.credentials.password = None;
                }
                let driver = builtin_registry()
                    .and_then(|registry| registry.get(resolved.config.driver))
                    .ok()?;
                Some(ScheduleConnection {
                    input: ConnectionInput {
                        id: profile.id,
                        driver,
                        resolved,
                        revision: profile.revision,
                    },
                    name: connection.name.clone(),
                    environment: connection.appearance.environment,
                })
            })
            .collect()
    }

    /// A scheduled run ended badly: counted for the indicator, and told by
    /// the operating system when its schedule asked (Q-40). Only the job's
    /// name, the outcome and the cause; never a value or a server message.
    fn schedule_news(&mut self, news: ScheduleNews, cx: &mut Context<Self>) {
        let new = news.bad_ends > self.automation.news.bad_ends;
        if new && let Some(end) = news.latest.as_ref().filter(|end| end.notify) {
            let (title, body) = schedule_words::bad_end(end, self.job_kind(end.job));
            cx.show_system_notification(SystemNotification {
                tag: format!("jykre-schedule-{}", end.schedule).into(),
                title: title.into(),
                body: body.into(),
                actions: Vec::new(),
            });
        }
        self.automation.news = news;
        cx.notify();
    }

    /// The kind of stored job `job`, when the list holds it.
    fn job_kind(&self, job: JobId) -> Option<JobKind> {
        self.automation
            .jobs
            .iter()
            .find(|item| item.id == job)
            .map(|item| item.kind)
    }

    /// How many scheduled runs ended badly since the user last looked.
    fn automation_unseen(&self) -> u64 {
        self.automation
            .news
            .bad_ends
            .saturating_sub(self.automation.news_seen)
    }

    /// The user looked at Automation: the indicator clears.
    fn automation_seen(&mut self) {
        self.automation.news_seen = self.automation.news.bad_ends;
    }

    /// The rail's mark on Automation: the count of scheduled runs that ended
    /// badly since the section was last opened.
    pub(super) fn automation_rail_badge(&self) -> Option<Div> {
        let unseen = self.automation_unseen();
        (unseen > 0).then(|| {
            div()
                .absolute()
                .top(px(-2.))
                .right(px(-2.))
                .min_w(px(16.))
                .h(px(16.))
                .px_1()
                .rounded(px(8.))
                .bg(rgb(colors().red))
                .text_color(rgb(colors().bg))
                .text_size(px(10.))
                .flex()
                .items_center()
                .justify_center()
                .child(if unseen > 99 {
                    "99+".to_owned()
                } else {
                    unseen.to_string()
                })
        })
    }

    /// The status bar names the latest scheduled run that ended badly while
    /// the user has not looked.
    pub(super) fn automation_news_line(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if self.automation_unseen() == 0 {
            return None;
        }
        let end = self.automation.news.latest.as_ref()?;
        let (title, body) = schedule_words::bad_end(end, self.job_kind(end.job));
        let job = end.job;
        Some(
            row()
                .id("automation-news")
                .role(Role::Status)
                .aria_label(format!("{title}: {body}"))
                .focusable()
                .tab_index(0)
                .focus_visible(|s| s.bg(rgb(colors().hover)))
                .gap_1()
                .px_1()
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(colors().hover)))
                .child(icon(I::TriangleAlert, colors().red))
                .child(label(format!("{title}: {body}")))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.automation_seen();
                    this.open_job(job, window, cx);
                    cx.notify();
                })),
        )
    }

    /// Each connection's name and dialect, for history lines.
    fn connection_names(&self) -> Rc<HashMap<ConnectionId, (String, Option<SqlDialect>)>> {
        let sources = self.report_sources();
        Rc::new(
            sources
                .iter()
                .filter_map(|source| {
                    Some((
                        source.connection.id?,
                        (source.connection.name.clone(), source.dialect),
                    ))
                })
                .collect(),
        )
    }

    fn job_views(&self) -> Vec<Entity<JobView>> {
        self.automation
            .tabs
            .values()
            .filter_map(|tab| match &tab.view {
                TabView::Job(view) => Some(view.clone()),
                TabView::Runs(_) => None,
            })
            .collect()
    }

    /// The tab holding a job view, if any.
    fn automation_tab_of(&self, entity: EntityId) -> Option<TabId> {
        self.automation
            .tabs
            .iter()
            .find(|(_, tab)| match &tab.view {
                TabView::Job(view) => view.entity_id() == entity,
                TabView::Runs(view) => view.entity_id() == entity,
            })
            .map(|(id, _)| *id)
    }

    /// Puts the keyboard in an Automation tab, when `tab` is one.
    pub(super) fn focus_automation_tab(
        &self,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.automation.tabs.get(&tab).map(|tab| &tab.view) {
            Some(TabView::Job(view)) => {
                view.update(cx, |view, cx| view.focus(window, cx));
                true
            }
            Some(TabView::Runs(view)) => {
                view.update(cx, |view, cx| view.focus(window, cx));
                true
            }
            None => false,
        }
    }

    /// `Ctrl F2` on a job's tab stops its run, as **Stop** does.
    pub(super) fn stop_automation_tab(&self, tab: TabId, cx: &mut Context<Self>) -> bool {
        match self.automation.tabs.get(&tab).map(|tab| &tab.view) {
            Some(TabView::Job(view)) => {
                view.update(cx, |view, cx| view.stop(cx));
                true
            }
            Some(TabView::Runs(_)) => true,
            None => false,
        }
    }

    /// **New SQL Job**, **New program** or **New chain**: a draft in a tab
    /// of its own, stored only when saved.
    fn new_job(&mut self, kind: JobKind, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.automation.jobs.len() + 1;
        let definition = match kind {
            JobKind::Sql => {
                // The only saved connection is the job's; with several the
                // user chooses, rather than run on whichever comes first.
                let mut saved = self
                    .connections
                    .iter()
                    .filter_map(|connection| connection.profile.as_ref().map(|profile| profile.id));
                let connection = logic::only(&mut saved);
                JobDefinition::new_sql(tr!("SQL Job {0}", count), connection)
            }
            JobKind::Process => JobDefinition::new_process(tr!("Program {0}", count)),
            JobKind::Chain => JobDefinition::new_chain(tr!("Chain {0}", count)),
        };
        self.open_job_tab(definition, None, window, cx);
    }

    /// Opens a stored job: its tab when one is open, otherwise a new one.
    pub(super) fn open_job(
        &mut self,
        id: JobId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<JobView>> {
        let open = self
            .automation
            .tabs
            .iter()
            .find_map(|(tab, open)| match &open.view {
                TabView::Job(view) if view.read(cx).job_id() == id => Some((*tab, view.clone())),
                _ => None,
            });
        if let Some((tab, view)) = open {
            self.show_tab(tab, window, cx);
            return Some(view);
        }
        let job = self
            .automation
            .jobs
            .iter()
            .find(|job| job.id == id)?
            .clone();
        self.open_job_tab(job.definition, Some(job.revision), window, cx)
    }

    fn open_job_tab(
        &mut self,
        definition: JobDefinition,
        revision: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<JobView>> {
        let engine = self.automation.engine.clone()?;
        let title = if definition.name.trim().is_empty() {
            tr!("Untitled job").to_owned()
        } else {
            definition.name.clone()
        };
        let open = self.automation.tab_groups().next();
        let group = self.kind_group(open, tr!("Automation"), cx);
        let sources = JobSources {
            sources: self.report_sources(),
            revisions: Rc::new(
                self.connections
                    .iter()
                    .filter_map(|connection| connection.profile.as_ref())
                    .map(|profile| (profile.id, profile.revision))
                    .collect(),
            ),
        };
        let setup = job_view::Setup {
            engine,
            definition,
            revision,
            sources,
            catalogs: self.sql_catalogs.clone(),
            jobs: self.automation.jobs.clone(),
            availability: self
                .automation
                .availability
                .clone()
                .unwrap_or(Availability::Starting),
            connections: self.connection_names(),
        };
        let view = cx.new(|cx| JobView::new(setup, window, cx));
        let events = cx.subscribe_in(&view, window, |this, view, event: &JobEvent, window, cx| {
            this.job_event(view, event, window, cx)
        });
        let status = cx.observe(&view, |this, view, cx| {
            let (status, title) = {
                let view = view.read(cx);
                (view.tab_status(), view.title())
            };
            if let Some(tab) = this.automation_tab_of(view.entity_id()) {
                let _ = this.tabs.update(cx, |tabs, cx| {
                    let _ = tabs.set_tab_status(tab, status, cx);
                    tabs.set_tab_title(tab, title, cx)
                });
            }
        });
        let Ok(tab) = self
            .tabs
            .update(cx, |tabs, cx| tabs.add_tab(group, title, view.clone(), cx))
        else {
            tracing::error!("could not add a job tab: its group does not exist");
            return None;
        };
        self.automation.tabs.insert(
            tab,
            AutomationTab {
                view: TabView::Job(view.clone()),
                group,
                _subscriptions: vec![events, status],
            },
        );
        view.update(cx, |view, cx| view.focus(window, cx));
        cx.notify();
        Some(view)
    }

    /// The global **Runs** list: every run and chain run, in one tab.
    fn open_runs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.automation_seen();
        let open = self
            .automation
            .tabs
            .iter()
            .find_map(|(tab, open)| match &open.view {
                TabView::Runs(_) => Some(*tab),
                TabView::Job(_) => None,
            });
        if let Some(tab) = open {
            self.show_tab(tab, window, cx);
            return;
        }
        let Some(engine) = self.automation.engine.clone() else {
            return;
        };
        let title = tr!("Runs").to_owned();
        let open = self.automation.tab_groups().next();
        let group = self.kind_group(open, tr!("Automation"), cx);
        let jobs = self.automation.jobs.clone();
        let connections = self.connection_names();
        let view = cx.new(|cx| RunsView::new(engine, jobs, connections, cx));
        let events = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &HistoryEvent, window, cx| this.history_event(event, window, cx),
        );
        let Ok(tab) = self
            .tabs
            .update(cx, |tabs, cx| tabs.add_tab(group, title, view.clone(), cx))
        else {
            return;
        };
        self.automation.tabs.insert(
            tab,
            AutomationTab {
                view: TabView::Runs(view.clone()),
                group,
                _subscriptions: vec![events],
            },
        );
        view.update(cx, |view, cx| view.focus(window, cx));
        cx.notify();
    }

    fn job_event(
        &mut self,
        view: &Entity<JobView>,
        event: &JobEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            JobEvent::PasswordRequired(connection) => {
                let index = self.connections.iter().position(|candidate| {
                    candidate
                        .profile
                        .as_ref()
                        .is_some_and(|profile| profile.id == *connection)
                });
                let Some(index) = index else {
                    view.update(cx, |view, cx| view.cancel_pending(cx));
                    return;
                };
                let resume = view.clone();
                let cancel = view.clone();
                self.ask_password(
                    index,
                    Box::new(move |_, _, cx| resume.update(cx, |view, cx| view.resume_pending(cx))),
                    Box::new(move |_, cx| cancel.update(cx, |view, cx| view.cancel_pending(cx))),
                    cx,
                );
            }
            JobEvent::Confirm {
                title,
                description,
                ok,
                what,
            } => {
                let view = view.downgrade();
                let what = *what;
                confirm(
                    window,
                    cx,
                    Confirmation {
                        title: title.clone(),
                        description: description.clone(),
                        ok,
                        cancel: tr!("Cancel"),
                        focus: ConfirmFocus::Dialog,
                    },
                    move |window, cx| {
                        let _ = view.update(cx, |view, cx| view.confirmed(what, window, cx));
                    },
                );
            }
            JobEvent::History(event) => self.history_event(event, window, cx),
            JobEvent::Duplicate(definition) => {
                self.open_job_tab((**definition).clone(), None, window, cx);
            }
            JobEvent::Deleted => {
                if let Some(tab) = self.automation_tab_of(view.entity_id()) {
                    self.tabs.update(cx, |tabs, cx| {
                        let _ = tabs.close_tab(tab, cx);
                        tabs.focus_active(window, cx);
                    });
                }
            }
        }
        cx.notify();
    }

    fn history_event(&mut self, event: &HistoryEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            HistoryEvent::OpenJob {
                job,
                values,
                missing,
            } => {
                let Some(view) = self.open_job(*job, window, cx) else {
                    self.app_notice = Some(Feedback::new(
                        Tone::Warning,
                        tr!("That step's job no longer exists."),
                    ));
                    cx.notify();
                    return;
                };
                let values = values.clone();
                let missing = missing.clone();
                view.update(cx, |view, cx| view.prefill(values, missing, window, cx));
            }
        }
    }

    /// Closing a job's tab asks first when its definition is not saved.
    /// Its runs keep going: they belong to the engine.
    pub(super) fn request_automation_close(
        &mut self,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unsaved = match self.automation.tabs.get(&tab).map(|tab| &tab.view) {
            Some(TabView::Job(view)) => view.read(cx).has_unsaved_changes(),
            _ => false,
        };
        let tabs = self.tabs.downgrade();
        let close = move |window: &mut Window, cx: &mut App| {
            let _ = tabs.update(cx, |tabs, cx| {
                let _ = tabs.close_tab(tab, cx);
                tabs.focus_active(window, cx);
            });
        };
        if !unsaved {
            close(window, cx);
            return;
        }
        confirm(
            window,
            cx,
            Confirmation {
                title: tr!("Close the job without saving it?").to_owned(),
                description: tr!(
                    "Its changes are not saved, so closing the tab loses them. A run it started keeps going."
                )
                .to_owned(),
                ok: tr!("Close without saving"),
                cancel: tr!("Keep the tab"),
                focus: ConfirmFocus::Dialog,
            },
            move |window, cx| close(window, cx),
        );
    }

    /// The last question before the window closes: with runs active it
    /// names them and says they will be stopped and the queue emptied; on
    /// confirmation it stops them, waits for the engine's shutdown, then
    /// closes. `true` when nothing is active.
    pub(super) fn automation_may_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.automation.quit_settled {
            return true;
        }
        if self.automation.stopping.is_some() {
            return false;
        }
        let Some(engine) = self.automation.engine.clone() else {
            return true;
        };
        let active = engine.active();
        if active == 0 {
            return true;
        }
        let names: Vec<String> = engine
            .overview()
            .into_iter()
            .filter(|entry| entry.step.is_none())
            .map(|entry| entry.name)
            .collect();
        let (title, description) = words::quit(&names);
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            Confirmation {
                title,
                description,
                ok: tr!("Stop the runs and close"),
                cancel: tr!("Keep open"),
                focus: ConfirmFocus::Dialog,
            },
            move |window, cx| {
                let engine = engine.clone();
                let _ = view.update(cx, |this, cx| {
                    this.automation.stopping = Some(active);
                    this.mcp_mark_shutdown();
                    let endpoint = this.mcp_endpoint();
                    cx.notify();
                    cx.spawn_in(window, async move |weak, cx| {
                        let _ = backend::run(async move {
                            engine.shutdown().await;
                            if let Some(endpoint) = endpoint {
                                endpoint.shutdown().await;
                            }
                            Ok(())
                        })
                        .await;
                        let _ = weak.update_in(cx, |this, window, cx| {
                            this.automation.stopping = None;
                            this.automation.quit_settled = true;
                            this.mcp_mark_closed();
                            this.closing = true;
                            window.remove_window();
                            cx.notify();
                        });
                    })
                    .detach();
                });
            },
        );
        false
    }

    // -----------------------------------------------------------------------
    // The sidebar section.

    /// The rail's Automation section: its state, **Runs**, and the jobs.
    pub(super) fn automation_list(&self, s: Div, cx: &mut Context<Self>) -> Div {
        let state = self.automation.availability.clone();
        let blocked = matches!(&state, None | Some(Availability::Off(_)));
        s.child(label(tr!("Jobs and chains, run by hand")))
            .when_some(self.automation_state(cx), |s, state| s.child(state))
            .when_some(self.automation.notice.as_ref(), |s, notice| {
                s.child(
                    row()
                        .id("automation-notice")
                        .role(if notice.tone == Tone::Error {
                            Role::Alert
                        } else {
                            Role::Status
                        })
                        .p_2()
                        .items_start()
                        .rounded(px(6.))
                        .bg(rgb(notice.tone.background()))
                        .child(feedback_line(notice).flex_1().text_size(px(12.)))
                        .child(
                            icon_button("dismiss-automation-notice", I::X, tr!("Close notice"))
                                .size(px(24.))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.automation.notice = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when(!blocked, |s| {
                s.child(
                    row()
                        .id("automation-runs")
                        .role(Role::Button)
                        .aria_label(tr!("Runs: every run and chain run"))
                        .focusable()
                        .tab_index(0)
                        .focus_visible(|s| s.bg(rgb(colors().hover)))
                        .h(px(36.))
                        .px_2()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(colors().hover)))
                        .child(icon(I::ListChecks, colors().mint))
                        .child(div().flex_1().child(tr!("Runs")))
                        .when_some(
                            self.automation
                                .engine
                                .as_ref()
                                .map(Engine::active)
                                .filter(|active| *active > 0),
                            |s, active| s.child(badge(tr!("{0} active", active), colors().mint)),
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.open_runs(window, cx))),
                )
            })
            .map(|s| {
                if blocked {
                    s
                } else if self.automation.jobs.is_empty() {
                    s.child(
                        div()
                            .p_3()
                            .text_size(px(14.))
                            .text_color(rgb(colors().muted))
                            .child(if self.automation.jobs_loading {
                                tr!("Reading the jobs…")
                            } else if self.automation.availability
                                == Some(Availability::ReadOnly)
                            {
                                // The holder may not have migrated the file yet.
                                tr!("No jobs can be read here until the other window lets automation go.")
                            } else {
                                tr!(
                                    "Create a SQL Job, a program or a chain with the + button. A job runs by hand with the values its form asks for; a chain runs published versions of jobs in order."
                                )
                            }),
                    )
                } else {
                    s.child(
                        div()
                            .id("automation-jobs")
                            .role(Role::List)
                            .aria_label(tr!("Jobs and chains"))
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(
                                uniform_list(
                                    "automation-job-entries",
                                    self.automation.jobs.len(),
                                    cx.processor(move |this, range: Range<usize>, _, cx| {
                                        range
                                            .filter_map(|index| this.automation_entry(index, cx))
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .size_full()
                                .track_scroll(&self.automation.scroll),
                            )
                            .vertical_scrollbar(&self.automation.scroll),
                    )
                }
            })
    }

    /// What the section says while automation is not ready: starting,
    /// read-only (with **Try again**) or off.
    fn automation_state(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let (tone, text, retry) = match self.automation.availability.as_ref()? {
            Availability::Ready => return None,
            Availability::Starting => (Tone::Info, tr!("Starting automation…").to_owned(), false),
            Availability::ReadOnly => (
                Tone::Warning,
                tr!(
                    "Read-only: another Jykre Studio window is running automation on this data folder. Its jobs and history can be read here; nothing can run or be saved until it closes."
                )
                .to_owned(),
                true,
            ),
            Availability::Off(OffReason::Newer) => (
                Tone::Warning,
                tr!(
                    "Automation is off: automation.db was written by a newer version of Jykre Studio. It is left as it is; the rest of the application works."
                )
                .to_owned(),
                false,
            ),
            Availability::Off(OffReason::Unreadable) => (
                Tone::Error,
                tr!(
                    "Automation is off: its file or its lock in the data folder could not be opened. The rest of the application works."
                )
                .to_owned(),
                false,
            ),
        };
        Some(
            column()
                .id("automation-state")
                .role(if tone == Tone::Error {
                    Role::Alert
                } else {
                    Role::Status
                })
                .p_2()
                .gap_2()
                .rounded(px(6.))
                .bg(rgb(tone.background()))
                .child(feedback_line(&Feedback::new(tone, text)).text_size(px(12.)))
                .when(retry, |s| {
                    s.child(
                        button(
                            "automation-retry",
                            if self.automation.retrying {
                                I::LoaderCircle
                            } else {
                                I::RefreshCw
                            },
                            tr!("Try again"),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.retry_automation(cx))),
                    )
                }),
        )
    }

    /// **Try again**: takes the lock when the other instance let it go.
    fn retry_automation(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.automation.engine.clone() else {
            return;
        };
        if self.automation.retrying {
            return;
        }
        self.automation.retrying = true;
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let state = backend::run(async move { Ok(engine.retry().await) }).await;
            let _ = weak.update(cx, |this, cx| {
                this.automation.retrying = false;
                if let Ok(state) = state {
                    this.automation.notice =
                        logic::notice_after(this.automation.notice.take(), &state, true);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Opening the section tries the lock again when another window held
    /// it (ADR-0034).
    pub(super) fn automation_shown(&mut self, cx: &mut Context<Self>) {
        self.automation_seen();
        if self.automation.availability == Some(Availability::ReadOnly) {
            self.retry_automation(cx);
        }
    }

    fn automation_entry(&self, index: usize, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let job = self.automation.jobs.get(index)?;
        let id = job.id;
        let symbol = kind_icon(job.kind);
        Some(
            row()
                .id(("automation-entry", index))
                .role(Role::ListItem)
                .aria_label(match &job.schedule {
                    Some(schedule) => tr!(
                        "Open {0}: {1}. {2}",
                        words::kind_name(job.kind),
                        job.name,
                        schedule.full
                    ),
                    None => tr!("Open {0}: {1}", words::kind_name(job.kind), job.name),
                })
                .focusable()
                .tab_index(0)
                .focus_visible(|s| s.bg(rgb(colors().hover)))
                .w_full()
                .h(px(34.))
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(colors().hover)))
                .child(icon(symbol, colors().mint))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(14.))
                        .child(job.name.clone()),
                )
                .map(|s| match &job.schedule {
                    Some(schedule) => s
                        .child(icon(
                            I::Clock,
                            if schedule.suspended {
                                colors().amber
                            } else {
                                colors().faint
                            },
                        ))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(if schedule.suspended {
                                    colors().amber
                                } else {
                                    colors().faint
                                }))
                                .child(schedule.short.clone()),
                        ),
                    None => s.child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(colors().faint))
                            .child(words::kind_name(job.kind)),
                    ),
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_job(id, window, cx);
                })),
        )
    }

    /// The section's **+**: a new SQL Job, program or chain.
    pub(super) fn automation_add_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let ready = self.automation.availability == Some(Availability::Ready);
        Button::new("automation-add")
            .ghost()
            .small()
            .icon(I::Plus)
            .dropdown_caret(true)
            .accessibility_label(tr!("New job"))
            .tooltip(tr!("New SQL Job, program or chain"))
            .dropdown_menu(move |mut menu, _, _| {
                if !ready {
                    return menu.label(tr!("Automation is not available here."));
                }
                for (kind, text) in [
                    (JobKind::Sql, tr!("New SQL Job")),
                    (JobKind::Process, tr!("New program")),
                    (JobKind::Chain, tr!("New chain")),
                ] {
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(text).icon(kind_icon(kind)).on_click(
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.new_job(kind, window, cx));
                        },
                    ));
                }
                menu
            })
    }
}

/// What the sidebar says of a schedule, computed when the jobs are read.
fn schedule_line(schedule: &jykre_storage::StoredSchedule) -> ScheduleLine {
    use jykre::automation::schedule::rule::{self, Rule};
    let zone = rule::time_zone(&schedule.draft.time_zone);
    let (short, full) = if !schedule.enabled {
        (
            tr!("Schedule off").to_owned(),
            tr!("Its schedule is off.").to_owned(),
        )
    } else if let Some(reason) = &schedule.suspended {
        (
            tr!("Suspended").to_owned(),
            tr!(
                "Its schedule is suspended: {0}.",
                schedule_words::suspended_reason(reason)
            ),
        )
    } else {
        let now = jykre::automation::UtcMillis::now();
        let next = zone
            .as_ref()
            .zip(Rule::from_json(&schedule.draft.rule_json).ok())
            .and_then(|(zone, rule)| {
                let after = schedule.last_due_at.map_or(now, |last| last.max(now));
                rule.next_after(zone, after).map(|at| (at, zone))
            });
        match next {
            Some((at, zone)) => {
                let local = rule::local(at, zone);
                (
                    local.map_or_else(String::new, |local| {
                        tr!(
                            "Next {0} {1}",
                            format!("{:02}:{:02}", local.hour, local.minute),
                            local.abbreviation
                        )
                    }),
                    tr!("Next scheduled run: {0}.", schedule_words::when(at, zone)),
                )
            }
            None => (
                tr!("Scheduled").to_owned(),
                tr!("It is scheduled.").to_owned(),
            ),
        }
    };
    ScheduleLine {
        short,
        full,
        suspended: schedule.suspended.is_some(),
        zone: schedule.draft.time_zone.clone(),
    }
}

/// A job's kind as an icon.
pub(super) fn kind_icon(kind: JobKind) -> I {
    match kind {
        JobKind::Sql => I::FileCode,
        JobKind::Process => I::SquareTerminal,
        JobKind::Chain => I::Workflow,
    }
}

/// Reads one step of a definition's text field as a whole number.
pub(super) fn whole(text: &str) -> Option<u64> {
    text.trim().replace(['_', ','], "").parse().ok()
}

/// The `ReportSource` of connection `id`, among `sources`.
pub(super) fn source_of(sources: &[ReportSource], id: ConnectionId) -> Option<&ReportSource> {
    sources
        .iter()
        .find(|source| source.connection.id == Some(id))
}

/// The connections to hand the scheduler: none while the saved ones are
/// still being read (the scheduler then waits), else the list as it is,
/// empty included, so a profile that was deleted still suspends its
/// schedules.
fn known_connections<T>(loading: bool, list: impl FnOnce() -> Vec<T>) -> Option<Vec<T>> {
    (!loading).then(list)
}

#[cfg(test)]
mod tests {
    use super::known_connections;

    #[test]
    fn the_scheduler_gets_no_connections_while_they_load() {
        crate::app::source_text();
        assert_eq!(known_connections(true, || vec![1, 2]), None);
        let mut asked = false;
        assert_eq!(
            known_connections::<i32>(true, || {
                asked = true;
                Vec::new()
            }),
            None
        );
        assert!(!asked, "the list is not built while connections load");
    }

    #[test]
    fn once_loaded_the_list_is_handed_whole_even_when_empty() {
        crate::app::source_text();
        assert_eq!(known_connections(false, || vec![1, 2]), Some(vec![1, 2]));
        // Every profile deleted: an empty list, so their schedules suspend.
        assert_eq!(known_connections::<i32>(false, Vec::new), Some(Vec::new()));
    }
}
