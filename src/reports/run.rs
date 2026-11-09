//! How a report runs (ADR-0025): which panels run, with which requests,
//! grouped by where they run (a connection and a database of it), and which
//! session each group uses.
//!
//! Panels of one connection and database share a session and run one after
//! another, in the report's order; different ones run at the same time, at
//! most [`MAX_PARALLEL`] at once. A session only ever runs in its own
//! connection and context, so a panel left at the session default never
//! inherits another panel's database. Nothing here touches GPUI or the
//! network: the report's view starts each group's loop and hands the
//! outcomes back.
use super::chart::{self, ChartProblem, Visual};
use super::params::{self, ParameterError};
use super::{ChartSpec, Panel, PanelId, ParameterSpec};
use crate::backend::SessionRun;
use crate::grouped_tabs::TabStatus;
use crate::result::{DEFAULT_MEMORY_CEILING, ResultData};
use crate::sql::SqlDialect;
use jykre_core::{
    CancellationToken, ConnectionId, DriverError, DriverResult, ErrorCode, ExecutionOptions,
    QueryRequest, Session, SessionContext,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// The most groups (contexts) that run at once in a report.
pub const MAX_PARALLEL: usize = 4;
/// The most idle sessions a report keeps between runs; the least recently
/// used one closes first.
pub const MAX_KEPT: usize = 4;

/// Where a group of panels runs: a connection and the context (database,
/// schema, catalog) its session stays in. Each key has its own session.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub connection: ConnectionId,
    pub context: SessionContext,
}

/// Which panels a run covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunScope {
    All,
    Panels(Vec<PanelId>),
}

impl RunScope {
    pub fn panel(id: PanelId) -> Self {
        Self::Panels(vec![id])
    }

    /// The panels of both scopes.
    pub fn merge(self, other: RunScope) -> RunScope {
        match (self, other) {
            (RunScope::Panels(mut ours), RunScope::Panels(theirs)) => {
                for id in theirs {
                    if !ours.contains(&id) {
                        ours.push(id);
                    }
                }
                RunScope::Panels(ours)
            }
            _ => RunScope::All,
        }
    }

    pub fn includes(&self, id: PanelId) -> bool {
        match self {
            RunScope::All => true,
            RunScope::Panels(ids) => ids.contains(&id),
        }
    }
}

/// Where a panel's run stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PanelPhase {
    #[default]
    Idle,
    /// Waiting for its session or for a free place among the groups.
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl PanelPhase {
    pub fn is_active(self) -> bool {
        matches!(self, PanelPhase::Queued | PanelPhase::Running)
    }
}

/// What a finished run of a panel needs to describe itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobInfo {
    /// The statements as they ran, markers in place.
    pub statements: Vec<String>,
    /// Each parameter the panel uses: its label and the value bound.
    pub bound: Vec<(String, Option<String>)>,
    /// The chart the panel shows, as it was when the run started.
    pub chart: Option<ChartSpec>,
    /// The panel has no chart yet and takes the one the result suggests.
    pub adopt: bool,
}

/// One panel's run, ready for its group's session.
#[derive(Debug)]
pub struct PanelJob {
    pub panel: PanelId,
    /// Tells this run from a later one of the same panel.
    pub run: u64,
    pub requests: Vec<QueryRequest>,
    pub memory_ceiling: usize,
    pub cancel: CancellationToken,
    pub info: JobInfo,
}

/// The panels a run covers, grouped by where they run, in the report's
/// order.
#[derive(Debug, Default)]
pub struct Plan {
    pub groups: Vec<(SessionKey, Vec<PanelJob>)>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|(_, jobs)| jobs.is_empty())
    }

    /// Every statement the plan runs, for the production check.
    pub fn statements(&self) -> Vec<String> {
        self.jobs()
            .flat_map(|job| job.info.statements.iter().cloned())
            .collect()
    }

    pub fn panels(&self) -> Vec<PanelId> {
        self.jobs().map(|job| job.panel).collect()
    }

    /// The statements the plan runs on `connection`, for the production
    /// check of that connection.
    pub fn statements_on(&self, connection: ConnectionId) -> Vec<String> {
        self.groups
            .iter()
            .filter(|(key, _)| key.connection == connection)
            .flat_map(|(_, jobs)| jobs.iter())
            .flat_map(|job| job.info.statements.iter().cloned())
            .collect()
    }

    /// The connections the plan runs on, once each.
    pub fn connections(&self) -> Vec<ConnectionId> {
        let mut found = Vec::new();
        for (key, _) in &self.groups {
            if !found.contains(&key.connection) {
                found.push(key.connection);
            }
        }
        found
    }

    fn jobs(&self) -> impl Iterator<Item = &PanelJob> {
        self.groups.iter().flat_map(|(_, jobs)| jobs.iter())
    }
}

/// The memory a panel's result may take: a report's panels share what one
/// query tab's result may take, so a report never holds more.
pub fn memory_ceiling(panel_count: usize) -> usize {
    DEFAULT_MEMORY_CEILING / panel_count.max(1)
}

/// A panel to plan, with where it runs as the view resolved it from its
/// connection.
#[derive(Clone, Copy, Debug)]
pub struct PlanPanel<'a> {
    pub panel: &'a Panel,
    pub connection: ConnectionId,
    pub dialect: Option<SqlDialect>,
    /// The connection's row limit.
    pub max_rows: u32,
}

/// The jobs for the panels of `scope` that hold SQL, with the values the
/// form holds. Only the parameters those panels use are checked; every
/// problem is reported once, by name. Each job gets a new run number from
/// `next_run`. `count` is the report's number of panels, which shares the
/// memory ceiling.
pub fn plan(
    panels: &[PlanPanel],
    count: usize,
    scope: &RunScope,
    specs: &[ParameterSpec],
    values: &HashMap<String, String>,
    next_run: &mut u64,
) -> Result<Plan, Vec<ParameterError>> {
    let ceiling = memory_ceiling(count);
    let mut plan = Plan::default();
    let mut errors: Vec<ParameterError> = Vec::new();
    for planned in panels
        .iter()
        .filter(|planned| scope.includes(planned.panel.id))
    {
        let PlanPanel {
            panel,
            connection,
            dialect,
            max_rows,
        } = *planned;
        if panel.sql.trim().is_empty() {
            continue;
        }
        let options = ExecutionOptions {
            max_rows,
            context: panel.context.clone(),
            ..ExecutionOptions::default()
        };
        let requests = match params::requests(&panel.sql, dialect, specs, values, &options) {
            Ok(requests) => requests,
            Err(found) => {
                for error in found {
                    if !errors.iter().any(|known| known.name == error.name) {
                        errors.push(error);
                    }
                }
                continue;
            }
        };
        if requests.is_empty() {
            continue;
        }
        let used = params::names(&panel.sql, dialect);
        let bound = specs
            .iter()
            .filter(|spec| used.contains(&spec.name))
            .map(|spec| {
                let typed = values.get(&spec.name).map_or("", String::as_str);
                (
                    spec.shown_label().to_owned(),
                    params::value(spec, typed).ok().flatten(),
                )
            })
            .collect();
        *next_run += 1;
        let job = PanelJob {
            panel: panel.id,
            run: *next_run,
            info: JobInfo {
                statements: requests.iter().map(|request| request.sql.clone()).collect(),
                bound,
                chart: panel.chart.clone(),
                adopt: panel.chart.is_none(),
            },
            requests,
            memory_ceiling: ceiling,
            cancel: CancellationToken::new(),
        };
        let key = SessionKey {
            connection,
            context: panel.context.clone(),
        };
        match plan.groups.iter_mut().find(|(known, _)| *known == key) {
            Some((_, jobs)) => jobs.push(job),
            None => plan.groups.push((key, vec![job])),
        }
    }
    if errors.is_empty() {
        Ok(plan)
    } else {
        Err(errors)
    }
}

/// How a panel's run ended, ready to show.
#[derive(Debug, Default)]
pub struct PanelOutcome {
    pub phase: PanelPhase,
    /// The last statement's result.
    pub data: Option<Arc<ResultData>>,
    /// The result stopped at the row limit or the memory ceiling.
    pub limited: bool,
    pub elapsed_ms: Option<u128>,
    pub error: Option<DriverError>,
    /// The session had been lost and was opened again.
    pub reopened: bool,
    /// The panel's SQL left a transaction open; its session was closed,
    /// which rolled the transaction back.
    pub transaction_closed: bool,
    /// The result's columns that hold numbers, for the chart's values.
    pub numeric: Vec<String>,
    /// The chart the result suggests.
    pub suggestion: Option<ChartSpec>,
    /// What the panel shows: for its chart, or for the suggestion it adopts.
    pub visual: Option<Result<Visual, ChartProblem>>,
    pub info: JobInfo,
}

/// The session to keep and what the panel shows, from a finished run. It
/// scans the result (number columns, the suggestion, the chart's data) and
/// drops the statements' other results, so it runs off the UI thread.
pub fn settle(
    run: DriverResult<SessionRun>,
    info: JobInfo,
) -> (Option<Box<dyn Session>>, PanelOutcome) {
    let mut outcome = PanelOutcome {
        info,
        ..PanelOutcome::default()
    };
    let (session, batch) = match run {
        Ok(run) => {
            outcome.reopened = run.reopened;
            outcome.transaction_closed = run.transaction_closed;
            (run.session, run.batch)
        }
        Err(error) => (None, Err(error)),
    };
    let failure = match batch {
        Ok(batch) => {
            if let Some(last) = batch.completed.into_iter().last() {
                outcome.limited = last.has_more || last.limit.is_some();
                outcome.elapsed_ms = Some(last.elapsed_ms);
                outcome.numeric = chart::numeric_columns(&last.data);
                outcome.suggestion = chart::suggest(&last.data);
                let shown = match (&outcome.info.chart, outcome.info.adopt) {
                    (Some(spec), _) => Some(spec.clone()),
                    (None, true) => outcome.suggestion.clone(),
                    (None, false) => None,
                };
                outcome.visual = shown.map(|spec| chart::visual(&last.data, &spec));
                outcome.data = Some(Arc::new(last.data));
            }
            batch.error
        }
        Err(error) => Some(error),
    };
    outcome.phase = match &failure {
        None => PanelPhase::Succeeded,
        Some(error) if error.code == ErrorCode::Cancelled => PanelPhase::Cancelled,
        Some(_) => PanelPhase::Failed,
    };
    outcome.error = failure;
    (session, outcome)
}

/// The panels waiting and running, grouped by where they run, and the phase of
/// every panel that ran.
#[derive(Debug, Default)]
pub struct Scheduler {
    /// Groups waiting or running, in the order they were asked for.
    groups: Vec<(SessionKey, VecDeque<PanelJob>)>,
    /// The groups whose loop runs.
    running: HashSet<SessionKey>,
    phases: HashMap<PanelId, PanelPhase>,
    /// The run of each panel handed to a group, how to stop it and where it
    /// runs.
    current: HashMap<PanelId, (u64, CancellationToken, SessionKey)>,
}

impl Scheduler {
    /// Queues the plan's jobs; a panel already waiting or running keeps its
    /// run, and its new job is dropped. Jobs for a context already queued or
    /// running join that group. Returns the panels queued.
    pub fn enqueue(&mut self, plan: Plan) -> Vec<PanelId> {
        let mut queued = Vec::new();
        for (context, jobs) in plan.groups {
            for job in jobs {
                if self.phase(job.panel).is_active() {
                    continue;
                }
                self.phases.insert(job.panel, PanelPhase::Queued);
                queued.push(job.panel);
                match self.groups.iter_mut().find(|(known, _)| *known == context) {
                    Some((_, waiting)) => waiting.push_back(job),
                    None => self.groups.push((context.clone(), VecDeque::from([job]))),
                }
            }
        }
        queued
    }

    /// The groups to start now: those waiting, in order, while fewer than
    /// [`MAX_PARALLEL`] run.
    pub fn start(&mut self) -> Vec<SessionKey> {
        let mut started = Vec::new();
        for (context, jobs) in &self.groups {
            if self.running.len() >= MAX_PARALLEL {
                break;
            }
            if !jobs.is_empty() && !self.running.contains(context) {
                self.running.insert(context.clone());
                started.push(context.clone());
            }
        }
        started
    }

    /// The next job of the group running at `context`; `None` ends the
    /// group, which then no longer counts as running.
    pub fn next(&mut self, context: &SessionKey) -> Option<PanelJob> {
        let index = self.groups.iter().position(|(known, _)| known == context);
        let job = index.and_then(|index| self.groups[index].1.pop_front());
        match job {
            Some(job) => {
                self.phases.insert(job.panel, PanelPhase::Running);
                self.current
                    .insert(job.panel, (job.run, job.cancel.clone(), context.clone()));
                Some(job)
            }
            None => {
                if let Some(index) = index {
                    self.groups.remove(index);
                }
                self.running.remove(context);
                None
            }
        }
    }

    /// Records how the run `run` of `panel` ended. `false` when that run is
    /// no longer the panel's (it was stopped, removed or reset meanwhile):
    /// its outcome is dropped.
    pub fn finish(&mut self, panel: PanelId, run: u64, phase: PanelPhase) -> bool {
        match self.current.get(&panel) {
            Some((current, _, _)) if *current == run => {
                self.current.remove(&panel);
                self.phases.insert(panel, phase);
                true
            }
            _ => false,
        }
    }

    /// Stops `panel`: a waiting job is dropped and a running one cancelled
    /// (it reports back as cancelled).
    pub fn cancel_panel(&mut self, panel: PanelId) {
        let mut dropped = false;
        for (_, jobs) in &mut self.groups {
            let before = jobs.len();
            jobs.retain(|job| job.panel != panel);
            dropped |= jobs.len() != before;
        }
        if dropped {
            self.phases.insert(panel, PanelPhase::Cancelled);
        }
        if let Some((_, cancel, _)) = self.current.get(&panel) {
            cancel.cancel();
        }
    }

    /// Stops every panel.
    pub fn cancel_all(&mut self) {
        let panels: Vec<PanelId> = self
            .groups
            .iter()
            .flat_map(|(_, jobs)| jobs.iter().map(|job| job.panel))
            .chain(self.current.keys().copied())
            .collect();
        for panel in panels {
            self.cancel_panel(panel);
        }
    }

    /// Marks `panel` as failed without running it (its connection cannot
    /// run), unless it waits or runs.
    pub fn mark_failed(&mut self, panel: PanelId) {
        if !self.phase(panel).is_active() {
            self.phases.insert(panel, PanelPhase::Failed);
        }
    }

    /// Stops every panel of `connection`, waiting or running, as when the
    /// connection's settings change or it is removed.
    pub fn cancel_connection(&mut self, connection: ConnectionId) {
        let waiting: Vec<PanelId> = self
            .groups
            .iter()
            .filter(|(key, _)| key.connection == connection)
            .flat_map(|(_, jobs)| jobs.iter().map(|job| job.panel))
            .collect();
        for panel in waiting {
            self.cancel_panel(panel);
        }
        let running: Vec<PanelId> = self
            .current
            .iter()
            .filter(|(_, (_, _, key))| key.connection == connection)
            .map(|(panel, _)| *panel)
            .collect();
        for panel in running {
            self.cancel_panel(panel);
        }
    }

    /// Forgets `panel`: it stops, and a run still under way is dropped when
    /// it reports back.
    pub fn remove_panel(&mut self, panel: PanelId) {
        self.cancel_panel(panel);
        self.current.remove(&panel);
        self.phases.remove(&panel);
    }

    /// Stops everything and forgets every run, as when the connection
    /// changes: the loops still under way find no next job, and their
    /// outcomes are dropped.
    pub fn reset(&mut self) {
        self.cancel_all();
        for phase in self.phases.values_mut() {
            if phase.is_active() {
                *phase = PanelPhase::Cancelled;
            }
        }
        self.groups.clear();
        self.running.clear();
        self.current.clear();
    }

    pub fn phase(&self, panel: PanelId) -> PanelPhase {
        self.phases.get(&panel).copied().unwrap_or_default()
    }

    pub fn is_busy(&self) -> bool {
        self.phases.values().any(|phase| phase.is_active())
    }

    /// The report's status for the tab bar: running while any panel runs or
    /// waits, then failed, cancelled or succeeded by the worst last run.
    pub fn status(&self) -> TabStatus {
        let has = |wanted: &[PanelPhase]| self.phases.values().any(|phase| wanted.contains(phase));
        if has(&[PanelPhase::Queued, PanelPhase::Running]) {
            TabStatus::Running
        } else if has(&[PanelPhase::Failed]) {
            TabStatus::Failed
        } else if has(&[PanelPhase::Cancelled]) {
            TabStatus::Cancelled
        } else if has(&[PanelPhase::Succeeded]) {
            TabStatus::Succeeded
        } else {
            TabStatus::Idle
        }
    }
}

/// A report's sessions, one per connection and context, each lent to one
/// group at a time.
#[derive(Debug, Default)]
pub struct Sessions {
    slots: HashMap<SessionKey, Slot>,
    next_lease: u64,
    clock: u64,
}

#[derive(Debug, Default)]
struct Slot {
    session: Option<Box<dyn Session>>,
    /// The group the session is lent to.
    lease: Option<u64>,
    /// When the session was last given back, for closing the oldest first.
    used: u64,
}

impl Sessions {
    /// Lends the session of `context` (`None` when there is none yet: the
    /// group opens one) under a new lease.
    pub fn take(&mut self, context: &SessionKey) -> (Option<Box<dyn Session>>, u64) {
        self.next_lease += 1;
        let lease = self.next_lease;
        let slot = self.slots.entry(context.clone()).or_default();
        slot.lease = Some(lease);
        (slot.session.take(), lease)
    }

    /// Whether `lease` still holds the session of `context`.
    pub fn is_current(&self, context: &SessionKey, lease: u64) -> bool {
        self.slots
            .get(context)
            .is_some_and(|slot| slot.lease == Some(lease))
    }

    /// Takes back the session lent under `lease`, and returns the sessions
    /// to close: this one when the lease is no longer current, and the
    /// least recently used idle ones beyond [`MAX_KEPT`].
    pub fn give_back(
        &mut self,
        context: &SessionKey,
        lease: u64,
        session: Option<Box<dyn Session>>,
    ) -> Vec<Box<dyn Session>> {
        let mut closing = Vec::new();
        if !self.is_current(context, lease) {
            closing.extend(session);
            return closing;
        }
        self.clock += 1;
        if let Some(slot) = self.slots.get_mut(context) {
            slot.lease = None;
            slot.session = session;
            slot.used = self.clock;
        }
        loop {
            let idle: Vec<(&SessionKey, u64)> = self
                .slots
                .iter()
                .filter(|(_, slot)| slot.lease.is_none() && slot.session.is_some())
                .map(|(context, slot)| (context, slot.used))
                .collect();
            if idle.len() <= MAX_KEPT {
                break;
            }
            let Some(oldest) = idle
                .into_iter()
                .min_by_key(|(_, used)| *used)
                .map(|(context, _)| context.clone())
            else {
                break;
            };
            if let Some(slot) = self.slots.remove(&oldest) {
                closing.extend(slot.session);
            }
        }
        self.slots
            .retain(|_, slot| slot.lease.is_some() || slot.session.is_some());
        closing
    }

    /// Returns the idle sessions of contexts no panel uses any more, to
    /// close.
    pub fn retain_used(&mut self, used: &[SessionKey]) -> Vec<Box<dyn Session>> {
        let unused: Vec<SessionKey> = self
            .slots
            .iter()
            .filter(|(context, slot)| slot.lease.is_none() && !used.contains(context))
            .map(|(context, _)| context.clone())
            .collect();
        unused
            .into_iter()
            .filter_map(|context| self.slots.remove(&context))
            .filter_map(|slot| slot.session)
            .collect()
    }

    /// Returns the idle sessions of `connection`, to close, and voids the
    /// leases of its sessions lent meanwhile, which close when given back.
    pub fn close_connection(&mut self, connection: ConnectionId) -> Vec<Box<dyn Session>> {
        let keys: Vec<SessionKey> = self
            .slots
            .keys()
            .filter(|key| key.connection == connection)
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|key| self.slots.remove(&key))
            .filter_map(|slot| slot.session)
            .collect()
    }

    /// Returns every idle session, to close, and voids every lease: a
    /// session lent meanwhile closes when its group gives it back.
    pub fn clear(&mut self) -> Vec<Box<dyn Session>> {
        self.slots
            .drain()
            .filter_map(|(_, slot)| slot.session)
            .collect()
    }

    /// How many sessions are kept or lent.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{QueryBatch, QueryResult};
    use crate::reports::{ChartKind, Panel};
    use crate::result::ResultColumn;
    use jykre_core::{DriverOutcome, ParameterKind, TransactionStatus};

    fn context(schema: Option<&str>) -> SessionContext {
        SessionContext {
            catalog: None,
            schema: schema.map(str::to_owned),
        }
    }

    /// The test's one connection, unless a test names another.
    fn home() -> ConnectionId {
        static HOME: std::sync::OnceLock<ConnectionId> = std::sync::OnceLock::new();
        *HOME.get_or_init(ConnectionId::new)
    }

    /// Where a panel on the test's connection runs in `schema`.
    fn key(schema: Option<&str>) -> SessionKey {
        SessionKey {
            connection: home(),
            context: context(schema),
        }
    }

    fn panel(id: u64, sql: &str, schema: Option<&str>) -> Panel {
        Panel::new(
            PanelId(id),
            crate::reports::ConnectionRef::default(),
            sql,
            context(schema),
        )
    }

    fn on_home(panels: &[Panel]) -> Vec<PlanPanel<'_>> {
        panels
            .iter()
            .map(|panel| PlanPanel {
                panel,
                connection: home(),
                dialect: None,
                max_rows: 100,
            })
            .collect()
    }

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn plan_of(panels: &[Panel], scope: &RunScope, next_run: &mut u64) -> Plan {
        plan(
            &on_home(panels),
            panels.len(),
            scope,
            &[],
            &HashMap::new(),
            next_run,
        )
        .unwrap()
    }

    #[derive(Debug)]
    struct Idle;

    #[jykre_core::async_trait]
    impl Session for Idle {
        async fn execute(
            &mut self,
            _: &QueryRequest,
            _: &mut dyn jykre_core::RowSink,
            _: CancellationToken,
        ) -> DriverResult<DriverOutcome> {
            Ok(DriverOutcome::default())
        }
        async fn transaction(
            &mut self,
            _: CancellationToken,
        ) -> DriverResult<Option<TransactionStatus>> {
            Ok(None)
        }
        async fn close(self: Box<Self>) -> DriverResult<()> {
            Ok(())
        }
    }

    fn session() -> Option<Box<dyn Session>> {
        Some(Box::new(Idle))
    }

    #[test]
    fn panels_are_grouped_by_context_in_the_report_order() {
        let panels = [
            panel(1, "SELECT 1", None),
            panel(2, "SELECT 2", Some("shop")),
            panel(3, "  ", None),
            panel(4, "SELECT 4; SELECT 5", None),
        ];
        let mut next_run = 0;
        let plan = plan_of(&panels, &RunScope::All, &mut next_run);
        let groups: Vec<(Option<&str>, Vec<PanelId>)> = plan
            .groups
            .iter()
            .map(|(key, jobs)| {
                (
                    key.context.schema.as_deref(),
                    jobs.iter().map(|job| job.panel).collect(),
                )
            })
            .collect();
        assert_eq!(
            groups,
            [
                (None, vec![PanelId(1), PanelId(4)]),
                (Some("shop"), vec![PanelId(2)]),
            ]
        );
        assert_eq!(next_run, 3);
        assert_eq!(plan.statements().len(), 4);
        let shop = &plan.groups[1].1[0];
        assert_eq!(shop.requests[0].options.context, context(Some("shop")));
        assert_eq!(shop.requests[0].options.max_rows, 100);
        assert_eq!(shop.memory_ceiling, DEFAULT_MEMORY_CEILING / 4);
        assert!(shop.info.adopt);
        let one = plan_of(&panels, &RunScope::panel(PanelId(2)), &mut next_run);
        assert_eq!(one.panels(), [PanelId(2)]);
    }

    #[test]
    fn the_same_database_of_two_connections_runs_in_two_groups() {
        let other = ConnectionId::new();
        let panels = [
            panel(1, "SELECT 1", Some("shop")),
            panel(2, "SELECT 2", Some("shop")),
            panel(3, "DELETE FROM t", None),
        ];
        let planned = vec![
            PlanPanel {
                panel: &panels[0],
                connection: home(),
                dialect: None,
                max_rows: 100,
            },
            PlanPanel {
                panel: &panels[1],
                connection: other,
                dialect: None,
                max_rows: 7,
            },
            PlanPanel {
                panel: &panels[2],
                connection: other,
                dialect: None,
                max_rows: 7,
            },
        ];
        let mut next_run = 0;
        let plan = plan(
            &planned,
            3,
            &RunScope::All,
            &[],
            &HashMap::new(),
            &mut next_run,
        )
        .unwrap();
        let keys: Vec<&SessionKey> = plan.groups.iter().map(|(key, _)| key).collect();
        assert_eq!(keys.len(), 3);
        assert_eq!(*keys[0], key(Some("shop")));
        assert_eq!(keys[1].connection, other);
        assert_eq!(plan.groups[1].1[0].requests[0].options.max_rows, 7);
        assert_eq!(plan.connections(), [home(), other]);
        assert_eq!(plan.statements_on(other), ["SELECT 2", "DELETE FROM t"]);
        assert_eq!(plan.statements_on(home()), ["SELECT 1"]);
        // Stopping one connection leaves the other's panels.
        let mut scheduler = Scheduler::default();
        scheduler.enqueue(plan);
        let started = scheduler.start();
        assert_eq!(started.len(), 3);
        let running = scheduler.next(&started[1]).unwrap();
        scheduler.cancel_connection(other);
        assert!(running.cancel.is_cancelled());
        assert_eq!(scheduler.phase(PanelId(3)), PanelPhase::Cancelled);
        assert_eq!(scheduler.phase(PanelId(1)), PanelPhase::Queued);
        // A panel that could not start fails; one under way keeps its phase.
        scheduler.mark_failed(PanelId(9));
        assert_eq!(scheduler.phase(PanelId(9)), PanelPhase::Failed);
        scheduler.mark_failed(PanelId(1));
        assert_eq!(scheduler.phase(PanelId(1)), PanelPhase::Queued);
        let mut sessions = Sessions::default();
        for key in &started {
            let (_, lease) = sessions.take(key);
            assert!(sessions.give_back(key, lease, session()).is_empty());
        }
        let (lent, lease) = sessions.take(&started[1]);
        assert_eq!(sessions.close_connection(other).len(), 1);
        assert_eq!(
            sessions.give_back(&started[1], lease, lent).len(),
            1,
            "a session lent meanwhile closes when it comes back"
        );
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn a_plan_checks_only_the_parameters_its_panels_use_and_reports_each_once() {
        let specs = [
            crate::reports::ParameterSpec {
                kind: ParameterKind::Date,
                ..crate::reports::ParameterSpec::new("day")
            },
            crate::reports::ParameterSpec::new("region"),
        ];
        let panels = [
            panel(1, "SELECT :day", None),
            panel(2, "SELECT :day, :region", Some("shop")),
        ];
        let mut next_run = 0;
        let only_first = plan(
            &on_home(&panels),
            panels.len(),
            &RunScope::panel(PanelId(1)),
            &specs,
            &values(&[("day", "2024-01-31")]),
            &mut next_run,
        )
        .unwrap();
        assert_eq!(
            only_first.groups[0].1[0].info.bound,
            [("day".to_owned(), Some("2024-01-31".to_owned()))]
        );
        let errors = plan(
            &on_home(&panels),
            panels.len(),
            &RunScope::All,
            &specs,
            &values(&[("day", "soon")]),
            &mut next_run,
        )
        .unwrap_err();
        let names: Vec<_> = errors.iter().map(|error| error.name.as_str()).collect();
        assert_eq!(names, ["day", "region"]);
    }

    #[test]
    fn scope_merging_keeps_every_panel_asked_for() {
        let a = RunScope::panel(PanelId(1));
        assert_eq!(
            a.clone().merge(RunScope::panel(PanelId(2))),
            RunScope::Panels(vec![PanelId(1), PanelId(2)])
        );
        assert_eq!(a.clone().merge(RunScope::All), RunScope::All);
        assert!(RunScope::All.includes(PanelId(9)));
        assert!(!a.includes(PanelId(2)));
    }

    #[test]
    fn at_most_four_contexts_run_at_once_and_a_context_runs_its_panels_in_turn() {
        let panels: Vec<Panel> = (1..=6)
            .map(|id| {
                let schema = format!("s{id}");
                panel(id, "SELECT 1", Some(&schema))
            })
            .chain([panel(7, "SELECT 7", Some("s1"))])
            .collect();
        let mut next_run = 0;
        let mut scheduler = Scheduler::default();
        let queued = scheduler.enqueue(plan_of(&panels, &RunScope::All, &mut next_run));
        assert_eq!(queued.len(), 7);
        assert_eq!(scheduler.status(), TabStatus::Running);
        let started = scheduler.start();
        assert_eq!(started.len(), MAX_PARALLEL);
        assert!(scheduler.start().is_empty(), "no place is free");
        let s1 = key(Some("s1"));
        let first = scheduler.next(&s1).unwrap();
        assert_eq!(first.panel, PanelId(1));
        assert_eq!(scheduler.phase(PanelId(1)), PanelPhase::Running);
        assert_eq!(scheduler.phase(PanelId(7)), PanelPhase::Queued);
        // A panel already waiting or running is not queued twice.
        assert!(
            scheduler
                .enqueue(plan_of(
                    &panels,
                    &RunScope::panel(PanelId(7)),
                    &mut next_run
                ))
                .is_empty()
        );
        assert!(scheduler.finish(PanelId(1), first.run, PanelPhase::Succeeded));
        let second = scheduler.next(&s1).unwrap();
        assert_eq!(second.panel, PanelId(7));
        assert!(scheduler.finish(PanelId(7), second.run, PanelPhase::Failed));
        assert!(scheduler.next(&s1).is_none(), "the group ends");
        // Its place goes to a waiting context.
        assert_eq!(scheduler.start(), [key(Some("s5"))]);
        assert_eq!(scheduler.status(), TabStatus::Running);
    }

    #[test]
    fn a_new_job_for_a_running_context_joins_its_group() {
        let panels = [panel(1, "SELECT 1", None), panel(2, "SELECT 2", None)];
        let mut next_run = 0;
        let mut scheduler = Scheduler::default();
        scheduler.enqueue(plan_of(
            &panels,
            &RunScope::panel(PanelId(1)),
            &mut next_run,
        ));
        let none = key(None);
        assert_eq!(scheduler.start(), std::slice::from_ref(&none));
        let first = scheduler.next(&none).unwrap();
        scheduler.enqueue(plan_of(
            &panels,
            &RunScope::panel(PanelId(2)),
            &mut next_run,
        ));
        assert!(scheduler.start().is_empty(), "the running group takes it");
        assert!(scheduler.finish(first.panel, first.run, PanelPhase::Succeeded));
        assert_eq!(scheduler.next(&none).map(|job| job.panel), Some(PanelId(2)));
    }

    #[test]
    fn stopping_drops_waiting_jobs_cancels_running_ones_and_voids_stale_runs() {
        let panels = [
            panel(1, "SELECT 1", None),
            panel(2, "SELECT 2", None),
            panel(3, "SELECT 3", Some("x")),
        ];
        let mut next_run = 0;
        let mut scheduler = Scheduler::default();
        scheduler.enqueue(plan_of(&panels, &RunScope::All, &mut next_run));
        scheduler.start();
        let running = scheduler.next(&key(None)).unwrap();
        scheduler.cancel_panel(PanelId(2));
        assert_eq!(scheduler.phase(PanelId(2)), PanelPhase::Cancelled);
        scheduler.cancel_panel(PanelId(1));
        assert!(running.cancel.is_cancelled());
        assert!(scheduler.finish(PanelId(1), running.run, PanelPhase::Cancelled));
        assert!(scheduler.next(&key(None)).is_none());
        // A removed panel's run is dropped when it reports back.
        let other = scheduler.next(&key(Some("x"))).unwrap();
        scheduler.remove_panel(PanelId(3));
        assert!(other.cancel.is_cancelled());
        assert!(!scheduler.finish(PanelId(3), other.run, PanelPhase::Succeeded));
        assert_eq!(scheduler.phase(PanelId(3)), PanelPhase::Idle);
        assert_eq!(scheduler.status(), TabStatus::Cancelled);
        // A reset voids everything under way.
        scheduler.enqueue(plan_of(&panels, &RunScope::All, &mut next_run));
        scheduler.start();
        let job = scheduler.next(&key(None)).unwrap();
        scheduler.reset();
        assert!(job.cancel.is_cancelled());
        assert!(!scheduler.finish(job.panel, job.run, PanelPhase::Succeeded));
        assert!(scheduler.next(&key(None)).is_none());
        assert!(!scheduler.is_busy());
    }

    #[test]
    fn sessions_are_lent_under_leases_and_the_oldest_idle_ones_close() {
        let mut sessions = Sessions::default();
        let contexts: Vec<SessionKey> = (0..6).map(|n| key(Some(&format!("s{n}")))).collect();
        let (none, lease) = sessions.take(&contexts[0]);
        assert!(none.is_none());
        assert!(sessions.is_current(&contexts[0], lease));
        assert!(
            sessions
                .give_back(&contexts[0], lease, session())
                .is_empty()
        );
        let (kept, lease) = sessions.take(&contexts[0]);
        assert!(kept.is_some(), "the session is lent again");
        // A void lease closes what comes back.
        let closed = sessions.clear();
        assert!(closed.is_empty(), "the only session is lent");
        assert_eq!(sessions.give_back(&contexts[0], lease, kept).len(), 1);
        assert!(sessions.is_empty());
        for (n, context) in contexts.iter().enumerate() {
            let (_, lease) = sessions.take(context);
            let closing = sessions.give_back(context, lease, session());
            assert_eq!(closing.len(), usize::from(n >= MAX_KEPT));
        }
        assert_eq!(sessions.len(), MAX_KEPT);
        // The first contexts were the least recently used.
        assert!(sessions.take(&contexts[0]).0.is_none());
        assert!(sessions.take(&contexts[5]).0.is_some());
        let closing = sessions.retain_used(&[contexts[5].clone()]);
        assert_eq!(closing.len(), 3, "idle sessions no panel uses close");
    }

    #[test]
    fn a_panel_that_left_a_transaction_says_its_session_was_closed() {
        let info = JobInfo::default();
        let (_, outcome) = settle(finished(vec![QueryResult::default()], None), info.clone());
        assert!(!outcome.transaction_closed);
        let closed = finished(vec![QueryResult::default()], None).map(|run| SessionRun {
            session: None,
            transaction_closed: true,
            ..run
        });
        let (session, outcome) = settle(closed, info);
        assert!(session.is_none() && outcome.transaction_closed);
        assert_eq!(outcome.phase, PanelPhase::Succeeded);
    }

    fn data(columns: &[(&str, &str)], rows: &[Vec<Option<&str>>]) -> ResultData {
        ResultData::from_rows(
            columns
                .iter()
                .map(|(name, kind)| ResultColumn::new(*name, *kind))
                .collect(),
            rows.iter().map(|row| row.iter().copied()),
        )
    }

    fn finished(results: Vec<QueryResult>, error: Option<DriverError>) -> DriverResult<SessionRun> {
        Ok(SessionRun {
            batch: Ok(QueryBatch {
                completed: results,
                error,
            }),
            session: session(),
            reopened: false,
            ended: false,
            transaction_lost: false,
            transaction_closed: false,
            context: None,
            transaction: None,
            encryption: None,
        })
    }

    #[test]
    fn a_settled_run_keeps_the_last_result_and_what_it_shows() {
        let first = QueryResult::default();
        let last = QueryResult {
            data: data(
                &[("month", "text"), ("total", "numeric")],
                &[vec![Some("Jan"), Some("10.5")]],
            ),
            has_more: true,
            elapsed_ms: 7,
            ..QueryResult::default()
        };
        let info = JobInfo {
            adopt: true,
            ..JobInfo::default()
        };
        let (session, outcome) = settle(finished(vec![first, last], None), info);
        assert!(session.is_some());
        assert_eq!(outcome.phase, PanelPhase::Succeeded);
        assert!(outcome.limited);
        assert_eq!(outcome.elapsed_ms, Some(7));
        assert_eq!(outcome.numeric, ["total"]);
        assert_eq!(
            outcome.suggestion.as_ref().map(|s| s.kind),
            Some(ChartKind::Bar)
        );
        assert!(matches!(outcome.visual, Some(Ok(Visual::Chart(_)))));
        assert_eq!(outcome.data.as_ref().map(|data| data.row_count()), Some(1));
        // A panel with a chart of its own shows it, even a broken one.
        let info = JobInfo {
            chart: Some(ChartSpec::new(ChartKind::Number, "", vec!["gone".into()])),
            ..JobInfo::default()
        };
        let last = QueryResult {
            data: data(&[("n", "int4")], &[vec![Some("1")]]),
            ..QueryResult::default()
        };
        let (_, outcome) = settle(finished(vec![last], None), info);
        assert_eq!(
            outcome.visual,
            Some(Err(ChartProblem::MissingColumn("gone".into())))
        );
        // Without a chart and nothing to adopt, nothing is drawn.
        let (_, outcome) = settle(
            finished(vec![QueryResult::default()], None),
            JobInfo::default(),
        );
        assert!(outcome.visual.is_none());
    }

    #[test]
    fn a_failed_or_stopped_run_says_so() {
        let (session, outcome) = settle(
            finished(vec![], Some(DriverError::new(ErrorCode::Database, "boom"))),
            JobInfo::default(),
        );
        assert!(session.is_some(), "a failed statement keeps the session");
        assert_eq!(outcome.phase, PanelPhase::Failed);
        assert!(outcome.data.is_none());
        let (_, outcome) = settle(
            finished(vec![], Some(DriverError::cancelled())),
            JobInfo::default(),
        );
        assert_eq!(outcome.phase, PanelPhase::Cancelled);
        let (session, outcome) = settle(
            Err(DriverError::validation("no such database")),
            JobInfo::default(),
        );
        assert!(session.is_none());
        assert_eq!(outcome.phase, PanelPhase::Failed);
    }
}
