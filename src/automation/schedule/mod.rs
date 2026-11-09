//! Schedules while the application is open (specification, sections
//! 9.5.1–9.5.10; [ADR-0041](../../../docs/adr/0041-schedules-and-grants.md)).
//!
//! GPUI-free. The scheduler is a task of the [`Engine`] that runs only while
//! the engine holds the data folder's lock (`Availability::Ready`). It sleeps
//! until the earliest due time, at most 30 s, then compares the **wall
//! clock** to each enabled schedule's due times:
//!
//! - a due time less than 60 s past is **on time**: the run is submitted as
//!   **Run** would, or the overlap policy applies when the schedule still has
//!   a run queued or active (Q-35);
//! - older ones are **missed**: counted, and handled by the missed-time
//!   policy (Q-34). A catch-up is a **new run**, marked as one, submitted one
//!   at a time; nothing that started is ever run again.
//!
//! Before anything runs, the schedule's grant is checked against what it
//! binds; any difference suspends it (section 9.5.6). A scheduled run opens
//! no dialog: a password the profile does not remember fails it with
//! `Authentication` (section 9.5.7).
pub mod form;
pub mod rule;

use super::definition::{JobBody, JobDefinition, canonical_json};
use super::engine::{ChainRequest, ConnectionInput, Engine, Inner, Plan, Submission};
use super::{Availability, ChainHandle, ChainStatus, Refusal, RunHandle, RunStatus};
use crate::connection_appearance::Environment;
use crate::reports::ParameterSpec;
use jykre_core::automation::{
    Cause, GrantId, JobId, JobKind, MISSED_LIMIT_RANGE, MissedPolicy, OverlapPolicy, RunSchedule,
    RunState, ScheduleId, ScheduleOutcome, UtcMillis,
};
use jykre_core::{ConnectionId, SessionContext};
use jykre_storage::{
    AutomationRepository, Grant, GrantConnection, ScheduleDraft, ScheduleMark, ScheduleRefused,
    StoredSchedule, fingerprint,
};
use rule::{Occurrence, Rule, RuleProblem};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;
use tokio::sync::watch;

/// The longest the scheduler sleeps: a sleeping computer stops the monotonic
/// clock, so the wall clock is read again at least this often.
pub const MAX_SLEEP: Duration = Duration::from_secs(30);
/// How late a due time may still start (Q-39); past it, it is missed.
pub const GRACE: Duration = Duration::from_secs(60);
/// The N the editor proposes for *queue up to N*.
pub const DEFAULT_MISSED_LIMIT: i64 = 3;

// ---------------------------------------------------------------------------
// The clock.

/// The wall clock schedules read. Tests move a manual one instead of
/// sleeping (section 9.5.10).
#[derive(Clone)]
pub enum Clock {
    System,
    Manual(ManualClock),
}

/// A clock a test sets; the scheduler wakes whenever it moves.
#[derive(Clone)]
pub struct ManualClock {
    now: Arc<watch::Sender<UtcMillis>>,
}

impl ManualClock {
    pub fn new(at: UtcMillis) -> Self {
        Self {
            now: Arc::new(watch::Sender::new(at)),
        }
    }

    pub fn now(&self) -> UtcMillis {
        *self.now.borrow()
    }

    /// Moves the clock to `at`, forwards or backwards.
    pub fn set(&self, at: UtcMillis) {
        self.now.send_replace(at);
    }

    pub fn advance(&self, by: Duration) {
        self.now.send_modify(|now| *now = now.plus(by));
    }
}

impl Clock {
    pub fn now(&self) -> UtcMillis {
        match self {
            Clock::System => UtcMillis::now(),
            Clock::Manual(clock) => clock.now(),
        }
    }

    fn moves(&self) -> Option<watch::Receiver<UtcMillis>> {
        match self {
            Clock::System => None,
            Clock::Manual(clock) => Some(clock.now.subscribe()),
        }
    }
}

// ---------------------------------------------------------------------------
// What the application root hands the engine, and what the engine tells it.

/// A connection a schedule may run on, as the application root knows it
/// (section 9.5.7). `input` holds a password only when the profile
/// remembers it, so a schedule never uses one typed for this session.
#[derive(Clone)]
pub struct ScheduleConnection {
    pub input: ConnectionInput,
    pub name: String,
    pub environment: Option<Environment>,
}

/// The stored code of an environment label, as a grant binds it (Q-37).
pub fn environment_code(environment: Option<Environment>) -> &'static str {
    environment.map_or("none", Environment::key)
}

/// A scheduled run or chain run that ended badly, for the in-app indicator
/// and the opt-in notification (Q-40). Names and codes only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadEnd {
    pub schedule: ScheduleId,
    pub job: JobId,
    pub name: String,
    pub due_at: UtcMillis,
    /// The run's cause, when it ran and failed.
    pub cause: Option<Cause>,
    /// Why it never ran (`queue_full`, `queue_timeout`, `input`, …).
    pub refused: Option<&'static str>,
    /// The schedule asked for an operating-system notification.
    pub notify: bool,
}

/// What the scheduler tells the views: a counter of bad ends that only
/// grows, and the latest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduleNews {
    pub bad_ends: u64,
    pub latest: Option<BadEnd>,
}

/// One pass of the scheduler over every schedule, and the wall-clock time
/// it used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulePass {
    pub count: u64,
    pub at: UtcMillis,
}

/// The scheduler's share of the engine.
pub(crate) struct Shared {
    pub clock: Clock,
    /// `None` until the application root has said which connections exist:
    /// before that no grant can be checked, so nothing runs.
    connections: Mutex<Option<HashMap<ConnectionId, ScheduleConnection>>>,
    wake: watch::Sender<u64>,
    news: watch::Sender<ScheduleNews>,
    passes: watch::Sender<SchedulePass>,
}

impl Shared {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            connections: Mutex::new(None),
            wake: watch::Sender::new(0),
            news: watch::Sender::new(ScheduleNews::default()),
            passes: watch::Sender::new(SchedulePass {
                count: 0,
                at: UtcMillis(0),
            }),
        }
    }

    /// Makes the scheduler look again now.
    pub fn wake(&self) {
        self.wake
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    fn connections(&self) -> Option<HashMap<ConnectionId, ScheduleConnection>> {
        lock(&self.connections).clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Errors and consent.

/// Why a schedule could not be saved, confirmed, turned off or deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    Unavailable,
    Rule(Vec<RuleProblem>),
    UnknownZone,
    /// *Queue up to N* without an N from 2 to 24, or an N with another
    /// policy.
    MissedLimit,
    /// The job, the version or the chain is gone, or is not one a schedule
    /// can run (a draft, a chain's version).
    Missing,
    /// Required inputs without a default (Q-36), by name.
    Inputs(Vec<String>),
    /// The chain does not match the versions it names.
    Chain,
    /// A connection the target needs is not available.
    NoConnection,
    /// Something changed since it was read or shown: read it again.
    Changed,
    /// The job already has a schedule (Q-39).
    Taken,
    Storage(String),
}

/// One step of what a consent covers: a job's version, or one of a chain's
/// steps.
#[derive(Debug, Clone)]
pub struct ConsentStep {
    /// The step's ID in a chain; empty for a job's version.
    pub step: String,
    pub job: JobId,
    pub job_name: String,
    pub version: i64,
    pub definition: JobDefinition,
}

/// A connection the consent binds, as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentConnection {
    pub id: ConnectionId,
    pub name: String,
    pub revision: i64,
    pub environment: Option<Environment>,
    /// A scheduled run on it fails with `Authentication` when it does not.
    pub remembers_password: bool,
    /// Where each step on it runs.
    pub contexts: Vec<SessionContext>,
}

/// Everything the consent dialog shows, and the grant confirming it records
/// (section 9.5.6). Built by [`Engine::consent`].
#[derive(Debug, Clone)]
pub struct Consent {
    pub schedule: StoredSchedule,
    pub rule: Rule,
    pub zone: String,
    /// The next three due times.
    pub next: Vec<Occurrence>,
    pub kind: JobKind,
    pub name: String,
    pub steps: Vec<ConsentStep>,
    pub connections: Vec<ConsentConnection>,
    /// The values the runs use: the version's defaults, or the chain's
    /// (Q-36).
    pub inputs: Vec<(String, Option<String>)>,
    /// A connection is labelled Production: this consent is its production
    /// confirmation.
    pub production: bool,
    /// Why it is suspended, when confirming again.
    pub suspended: Option<String>,
    /// The connections whose revision or label differ from the previous
    /// grant, by name, when confirming again.
    pub changed: Vec<String>,
    grant: Grant,
}

impl Consent {
    pub fn grant(&self) -> &Grant {
        &self.grant
    }
}

/// The required inputs without a default, which keep a job or chain from
/// being scheduled (Q-36).
pub fn missing_defaults(specs: &[ParameterSpec]) -> Vec<String> {
    specs
        .iter()
        .filter(|spec| {
            spec.required
                && spec
                    .default
                    .as_deref()
                    .is_none_or(|text| text.trim().is_empty())
        })
        .map(|spec| spec.name.clone())
        .collect()
}

/// The values a scheduled run's form would hold: each input's default.
fn defaults(specs: &[ParameterSpec]) -> HashMap<String, String> {
    specs
        .iter()
        .map(|spec| (spec.name.clone(), spec.default.clone().unwrap_or_default()))
        .collect()
}

/// What a scheduled run executes, read and checked against its grant.
struct Target {
    kind: JobKind,
    name: String,
    job: JobId,
    steps: Vec<ConsentStep>,
    /// A chain's plan.
    plan: Option<Plan>,
    job_revision: Option<i64>,
    fingerprint: String,
    /// Each connection the target uses, with the step contexts on it.
    connections: Vec<(ConnectionId, Vec<SessionContext>)>,
    inputs: Vec<ParameterSpec>,
}

impl Target {
    fn definition(&self) -> Option<&JobDefinition> {
        match &self.plan {
            Some(plan) => Some(&plan.chain),
            None => self.steps.first().map(|step| &step.definition),
        }
    }
}

/// The SQL connection of a job's definition and where it runs.
fn sql_connection(definition: &JobDefinition) -> Option<(ConnectionId, SessionContext)> {
    match &definition.body {
        JobBody::Sql(sql) => sql
            .connection
            .map(|connection| (connection, sql.context.clone())),
        _ => None,
    }
}

impl Engine {
    // -----------------------------------------------------------------------
    // What views call.

    /// The clock schedules read.
    pub fn clock(&self) -> Clock {
        self.inner.schedules.clock.clone()
    }

    /// The connections schedules may use, from the application root
    /// whenever they load or change: their profile revisions, environment
    /// labels and, when remembered, passwords. The scheduler checks every
    /// grant against them at once (section 9.5.6).
    pub fn set_connections(&self, connections: Vec<ScheduleConnection>) {
        let revisions: HashMap<ConnectionId, i64> = connections
            .iter()
            .map(|connection| (connection.input.id, connection.input.revision))
            .collect();
        self.set_connection_revisions(revisions);
        *lock(&self.inner.schedules.connections) = Some(
            connections
                .into_iter()
                .map(|connection| (connection.input.id, connection))
                .collect(),
        );
        self.inner.schedules.wake();
    }

    /// Bad ends of scheduled runs, for the indicator and notifications.
    pub fn watch_schedule_news(&self) -> watch::Receiver<ScheduleNews> {
        self.inner.schedules.news.subscribe()
    }

    /// The scheduler's passes: each looked at every schedule once, at the
    /// time it gives.
    pub fn watch_schedule_passes(&self) -> watch::Receiver<SchedulePass> {
        self.inner.schedules.passes.subscribe()
    }

    /// Saves a schedule (`expected_revision` `None` for a new one), always
    /// off until consent is given. Checks the rule, the zone, the policies
    /// and that the target can run on its defaults alone.
    pub async fn save_schedule(
        &self,
        draft: &ScheduleDraft,
        expected_revision: Option<i64>,
    ) -> Result<StoredSchedule, ScheduleError> {
        let repository = self.schedules_writable()?;
        let rule = Rule::from_json(&draft.rule_json)
            .map_err(|problem| ScheduleError::Rule(vec![problem]))?;
        let problems = rule.problems();
        if !problems.is_empty() {
            return Err(ScheduleError::Rule(problems));
        }
        if rule::time_zone(&draft.time_zone).is_none() {
            return Err(ScheduleError::UnknownZone);
        }
        let limit_fits = match (draft.missed_policy, draft.missed_limit) {
            (MissedPolicy::Queue, Some(limit)) => MISSED_LIMIT_RANGE.contains(&limit),
            (MissedPolicy::Queue, None) => false,
            (_, limit) => limit.is_none(),
        };
        if !limit_fits {
            return Err(ScheduleError::MissedLimit);
        }
        let target = self.read_target(&repository, draft).await?;
        let missing = missing_defaults(&target.inputs);
        if !missing.is_empty() {
            return Err(ScheduleError::Inputs(missing));
        }
        let saved = repository
            .save_schedule(draft, expected_revision, self.clock().now())
            .await
            .map_err(|error| ScheduleError::Storage(error.message))?
            .map_err(|refused| match refused {
                ScheduleRefused::Changed => ScheduleError::Changed,
                ScheduleRefused::Taken => ScheduleError::Taken,
            })?;
        self.schedules_changed();
        Ok(saved)
    }

    /// What confirming `schedule` would consent to, as things are now.
    pub async fn consent(&self, schedule: ScheduleId) -> Result<Consent, ScheduleError> {
        let repository = self.schedules_writable()?;
        let stored = repository
            .schedule(schedule)
            .await
            .map_err(|error| ScheduleError::Storage(error.message))?
            .ok_or(ScheduleError::Missing)?;
        let rule = Rule::from_json(&stored.draft.rule_json)
            .map_err(|problem| ScheduleError::Rule(vec![problem]))?;
        let zone = rule::time_zone(&stored.draft.time_zone).ok_or(ScheduleError::UnknownZone)?;
        let target = self.read_target(&repository, &stored.draft).await?;
        let known = self
            .inner
            .schedules
            .connections()
            .ok_or(ScheduleError::NoConnection)?;
        let mut connections = Vec::new();
        for (id, contexts) in &target.connections {
            let connection = known.get(id).ok_or(ScheduleError::NoConnection)?;
            connections.push(ConsentConnection {
                id: *id,
                name: connection.name.clone(),
                revision: connection.input.revision,
                environment: connection.environment,
                remembers_password: !crate::connection_store::needs_password(
                    &connection.input.resolved,
                ),
                contexts: contexts.clone(),
            });
        }
        let production = connections
            .iter()
            .any(|connection| connection.environment == Some(Environment::Production));
        let now = self.clock().now();
        let grant = Grant {
            id: GrantId::new(),
            schedule_id: schedule,
            schedule_revision: stored.revision,
            target_fingerprint: target.fingerprint.clone(),
            job_revision: target.job_revision,
            production,
            connections: connections
                .iter()
                .map(|connection| GrantConnection {
                    connection_id: connection.id,
                    connection_revision: connection.revision,
                    environment: environment_code(connection.environment).to_owned(),
                })
                .collect(),
            granted_at: now,
        };
        let previous = repository
            .grant(schedule)
            .await
            .map_err(|error| ScheduleError::Storage(error.message))?;
        let changed = previous
            .map(|previous| {
                connections
                    .iter()
                    .filter(|connection| {
                        !previous.connections.iter().any(|bound| {
                            bound.connection_id == connection.id
                                && bound.connection_revision == connection.revision
                                && bound.environment == environment_code(connection.environment)
                        })
                    })
                    .map(|connection| connection.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let inputs = target
            .inputs
            .iter()
            .map(|spec| {
                (
                    spec.name.clone(),
                    spec.default.clone().filter(|text| !text.is_empty()),
                )
            })
            .collect();
        Ok(Consent {
            next: rule.next_times(&zone, now, 3),
            zone: stored.draft.time_zone.clone(),
            suspended: stored.suspended.clone(),
            schedule: stored,
            rule,
            kind: target.kind,
            name: target.name,
            steps: target.steps,
            connections,
            inputs,
            production,
            changed,
            grant,
        })
    }

    /// Records the consent the user confirmed: the grant of exactly what
    /// was shown. Refused with [`ScheduleError::Changed`] when anything it
    /// binds changed meanwhile. A manual confirmation never comes here
    /// (P-15).
    pub async fn confirm_schedule(
        &self,
        consent: &Consent,
    ) -> Result<StoredSchedule, ScheduleError> {
        let repository = self.schedules_writable()?;
        let now = self.consent(consent.schedule.draft.id).await?;
        let same = |grant: &Grant| {
            (
                grant.schedule_revision,
                grant.target_fingerprint.clone(),
                grant.job_revision,
                grant.production,
                grant.connections.clone(),
            )
        };
        if same(&now.grant) != same(&consent.grant) {
            return Err(ScheduleError::Changed);
        }
        let mut grant = consent.grant.clone();
        grant.granted_at = self.clock().now();
        let saved = repository
            .confirm_schedule(&grant, grant.granted_at)
            .await
            .map_err(|error| ScheduleError::Storage(error.message))?
            .map_err(|_| ScheduleError::Changed)?;
        tracing::info!(schedule = %saved.draft.id, "schedule confirmed");
        self.schedules_changed();
        Ok(saved)
    }

    /// Turns a schedule off and revokes its grant (Q-38). A queued run of it
    /// leaves the queue; an active one is not stopped.
    pub async fn disable_schedule(&self, schedule: ScheduleId) -> Result<(), ScheduleError> {
        let repository = self.schedules_writable()?;
        repository
            .disable_schedule(schedule, self.clock().now())
            .await
            .map_err(|error| ScheduleError::Storage(error.message))?;
        self.schedules_changed();
        Ok(())
    }

    /// Deletes a schedule and its grant; its runs stay in the history.
    pub async fn delete_schedule(&self, schedule: ScheduleId) -> Result<(), ScheduleError> {
        let repository = self.schedules_writable()?;
        repository
            .delete_schedule(schedule)
            .await
            .map_err(|error| ScheduleError::Storage(error.message))?;
        self.schedules_changed();
        Ok(())
    }

    fn schedules_writable(&self) -> Result<AutomationRepository, ScheduleError> {
        if self.availability() != Availability::Ready {
            return Err(ScheduleError::Unavailable);
        }
        self.repository().ok_or(ScheduleError::Unavailable)
    }

    fn schedules_changed(&self) {
        self.inner.schedules.wake();
        self.inner.changed();
    }

    /// The target `draft` names, read from storage, with its fingerprint.
    async fn read_target(
        &self,
        repository: &AutomationRepository,
        draft: &ScheduleDraft,
    ) -> Result<Target, ScheduleError> {
        let storage = |error: jykre_core::DriverError| ScheduleError::Storage(error.message);
        let job = repository
            .job(draft.job_id)
            .await
            .map_err(storage)?
            .ok_or(ScheduleError::Missing)?;
        let current = JobDefinition::from_json(&job.definition_json).ok();
        match (job.kind, draft.job_version) {
            (JobKind::Chain, None) => {
                let chain = current.ok_or(ScheduleError::Missing)?;
                let plan = self
                    .plan(chain.clone(), Some((job.id, job.revision)))
                    .await
                    .map_err(storage)?;
                if !plan.problems.is_empty() {
                    return Err(ScheduleError::Chain);
                }
                let mut fingerprints = Vec::new();
                for step in &plan.steps {
                    let version = repository
                        .version(step.job, step.version)
                        .await
                        .map_err(storage)?
                        .ok_or(ScheduleError::Chain)?;
                    fingerprints.push(json!({
                        "id": step.id,
                        "job": step.job.to_string(),
                        "version": step.version,
                        "fingerprint": version.fingerprint,
                    }));
                }
                let fingerprint = fingerprint(
                    canonical_json(&json!({
                        "chain": chain.to_value(),
                        "revision": job.revision,
                        "steps": fingerprints,
                    }))
                    .as_bytes(),
                );
                let mut connections: Vec<(ConnectionId, Vec<SessionContext>)> = Vec::new();
                for step in &plan.steps {
                    if let Some((id, context)) = sql_connection(&step.definition) {
                        match connections.iter_mut().find(|(known, _)| *known == id) {
                            Some((_, contexts)) => contexts.push(context),
                            None => connections.push((id, vec![context])),
                        }
                    }
                }
                let inputs = chain.inputs().to_vec();
                Ok(Target {
                    kind: JobKind::Chain,
                    name: chain.name.clone(),
                    job: job.id,
                    steps: plan
                        .steps
                        .iter()
                        .map(|step| ConsentStep {
                            step: step.id.clone(),
                            job: step.job,
                            job_name: step.job_name.clone(),
                            version: step.version,
                            definition: step.definition.clone(),
                        })
                        .collect(),
                    plan: Some(plan),
                    job_revision: Some(job.revision),
                    fingerprint,
                    connections,
                    inputs,
                })
            }
            (JobKind::Sql | JobKind::Process, Some(number)) => {
                let version = repository
                    .version(job.id, number)
                    .await
                    .map_err(storage)?
                    .ok_or(ScheduleError::Missing)?;
                let definition = JobDefinition::from_json(&version.definition_json)
                    .map_err(|_| ScheduleError::Missing)?;
                let name = current
                    .map(|current| current.name)
                    .unwrap_or_else(|| definition.name.clone());
                let connections = sql_connection(&definition)
                    .map(|(id, context)| vec![(id, vec![context])])
                    .unwrap_or_default();
                if matches!(definition.body, JobBody::Sql(_)) && connections.is_empty() {
                    return Err(ScheduleError::NoConnection);
                }
                Ok(Target {
                    kind: job.kind,
                    name: name.clone(),
                    job: job.id,
                    inputs: definition.inputs().to_vec(),
                    steps: vec![ConsentStep {
                        step: String::new(),
                        job: job.id,
                        job_name: name,
                        version: number,
                        definition,
                    }],
                    plan: None,
                    job_revision: None,
                    fingerprint: version.fingerprint,
                    connections,
                })
            }
            _ => Err(ScheduleError::Missing),
        }
    }
}

// ---------------------------------------------------------------------------
// The scheduler.

/// A run or chain run a schedule submitted.
struct Tracked {
    due_at: UtcMillis,
    handle: Handle,
    /// The schedule names it as its last run: its row exists.
    linked: bool,
}

enum Handle {
    Run(RunHandle),
    Chain(ChainHandle),
}

impl Handle {
    /// Queued or active.
    fn live(&self) -> bool {
        match self {
            Handle::Run(handle) => {
                matches!(
                    handle.status(),
                    RunStatus::Queued { .. } | RunStatus::Active(_)
                )
            }
            Handle::Chain(handle) => matches!(handle.status(), ChainStatus::Running { .. }),
        }
    }
}

/// What the scheduler holds of one schedule, in memory only.
#[derive(Default)]
struct Slot {
    runs: Vec<Tracked>,
    /// Missed times waiting to be caught up, oldest first.
    backlog: VecDeque<UtcMillis>,
    /// The due time *queue one* holds until the schedule's runs end.
    waiting: Option<UtcMillis>,
    /// The latest due time this pass loop took (submitted, held, skipped or
    /// missed): never taken again while the application runs, even when its
    /// mark could not be written.
    handled: Option<UtcMillis>,
}

impl Slot {
    fn live(&self) -> bool {
        self.runs.iter().any(|run| run.handle.live())
    }
}

/// Starts the scheduler of an engine that just took the lock.
pub(super) fn start(inner: &Arc<Inner>) {
    let weak = Arc::downgrade(inner);
    drop(inner.runtime.spawn(async move { scheduler(weak).await }));
}

async fn scheduler(inner: Weak<Inner>) {
    let (mut wake, mut changes, mut moves) = {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        (
            inner.schedules.wake.subscribe(),
            Engine {
                inner: inner.clone(),
            }
            .watch_changes(),
            inner.schedules.clock.moves(),
        )
    };
    let mut slots: HashMap<ScheduleId, Slot> = HashMap::new();
    tracing::info!("scheduler started");
    loop {
        let Some(engine) = inner.upgrade().map(|inner| Engine { inner }) else {
            return;
        };
        if engine.availability() != Availability::Ready || engine.shutting_down() {
            tracing::info!("scheduler stopped");
            return;
        }
        wake.borrow_and_update();
        changes.borrow_and_update();
        if let Some(moves) = moves.as_mut() {
            moves.borrow_and_update();
        }
        let now = engine.clock().now();
        let next = engine.pass(&mut slots, now).await;
        engine.inner.schedules.passes.send_modify(|pass| {
            pass.count = pass.count.wrapping_add(1);
            pass.at = now;
        });
        let wait = match (&engine.inner.schedules.clock, next) {
            (Clock::System, Some(next)) => next.since(UtcMillis::now()).min(MAX_SLEEP),
            (Clock::System, None) => MAX_SLEEP,
            // A manual clock wakes the scheduler when it moves.
            (Clock::Manual(_), _) => Duration::from_secs(3600),
        };
        drop(engine);
        let moved = async {
            match moves.as_mut() {
                Some(moves) => {
                    if moves.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait.max(Duration::from_millis(1))) => {}
            changed = wake.changed() => if changed.is_err() { return; },
            changed = changes.changed() => if changed.is_err() { return; },
            () = moved => {}
        }
    }
}

/// What one schedule's submissions share within a pass.
#[derive(Clone, Copy)]
struct Due<'a> {
    repository: &'a AutomationRepository,
    schedule: &'a StoredSchedule,
    target: &'a Target,
    connections: &'a HashMap<ConnectionId, ScheduleConnection>,
}

/// Why a schedule cannot run now.
enum Blocked {
    /// What the grant binds changed: suspend with this reason.
    Suspend(&'static str),
    /// It cannot be read now; try again at the next pass.
    Unreadable,
}

impl Engine {
    fn shutting_down(&self) -> bool {
        self.inner.shutting_down()
    }

    /// One look at every schedule; returns the earliest next due time.
    async fn pass(
        &self,
        slots: &mut HashMap<ScheduleId, Slot>,
        now: UtcMillis,
    ) -> Option<UtcMillis> {
        let repository = self.repository()?;
        let connections = self.inner.schedules.connections()?;
        let schedules = match repository.schedules().await {
            Ok(schedules) => schedules,
            Err(error) => {
                tracing::warn!(error_code = ?error.code, "schedules unreadable");
                return None;
            }
        };
        slots.retain(|id, slot| {
            let kept = schedules.iter().any(|schedule| schedule.draft.id == *id);
            if !kept {
                withdraw(self, slot);
            }
            kept
        });
        let mut wrote = false;
        let mut earliest: Option<UtcMillis> = None;
        for schedule in &schedules {
            if self.shutting_down() {
                break;
            }
            let slot = slots.entry(schedule.draft.id).or_default();
            let (written, next) = self
                .pass_one(&repository, schedule, slot, &connections, now)
                .await;
            wrote |= written;
            if let Some(next) = next {
                earliest = Some(earliest.map_or(next, |earliest| earliest.min(next)));
            }
        }
        if wrote {
            self.inner.changed();
        }
        earliest
    }

    async fn pass_one(
        &self,
        repository: &AutomationRepository,
        schedule: &StoredSchedule,
        slot: &mut Slot,
        connections: &HashMap<ConnectionId, ScheduleConnection>,
        now: UtcMillis,
    ) -> (bool, Option<UtcMillis>) {
        let id = schedule.draft.id;
        let mut wrote = self.settle(repository, schedule, slot).await;
        if !schedule.enabled || schedule.suspended.is_some() {
            wrote |= withdraw(self, slot);
            return (wrote, None);
        }
        let (target, grant, rule, zone) = match self.check(repository, schedule, connections).await
        {
            Ok(checked) => checked,
            Err(Blocked::Suspend(reason)) => {
                self.suspend(repository, schedule, slot, reason).await;
                return (true, None);
            }
            Err(Blocked::Unreadable) => return (wrote, None),
        };
        let due = Due {
            repository,
            schedule,
            target: &target,
            connections,
        };
        // Never a time before the consent, nor one already handled: a wall
        // clock set back repeats nothing. Handled is what the schedule's mark
        // says, what this loop took, and what a run was admitted for: a mark
        // that could not be written (or a crash before it) never lets a
        // time run twice.
        let admitted = match repository.latest_admitted_due(id).await {
            Ok(admitted) => admitted,
            Err(error) => {
                tracing::warn!(schedule = %id, error_code = ?error.code, "schedule's runs unreadable");
                return (wrote, None);
            }
        };
        let floor = [schedule.last_due_at, slot.handled, admitted]
            .into_iter()
            .flatten()
            .fold(grant.granted_at, UtcMillis::max);
        let limit = match schedule.draft.missed_policy {
            MissedPolicy::Queue => usize::try_from(schedule.draft.missed_limit.unwrap_or(1))
                .unwrap_or(1)
                .max(1),
            _ => 1,
        };
        let times = rule.due_in(&zone, floor, now, limit + 1);
        let latest = times.latest.last().copied();
        let on_time = latest.filter(|latest| now.since(*latest) < GRACE);
        let missed_count = times.count - u64::from(on_time.is_some());
        let mut missed: Vec<UtcMillis> = times.latest.clone();
        if on_time.is_some() {
            missed.pop();
        }
        if missed_count > 0
            && let Some(last_missed) = missed.last().copied()
        {
            let mut mark = ScheduleMark::new(last_missed, ScheduleOutcome::Missed);
            mark.missed = i64::try_from(missed_count).unwrap_or(i64::MAX);
            mark.missed_last_at = Some(last_missed);
            self.mark(repository, id, &mark).await;
            slot.handled = slot.handled.max(Some(last_missed));
            wrote = true;
            tracing::info!(schedule = %id, missed = missed_count, "schedule missed times");
            match schedule.draft.missed_policy {
                MissedPolicy::Ignore => {}
                MissedPolicy::RunOnce => {
                    // One run for all of them, due at the latest.
                    slot.backlog.clear();
                    slot.backlog.push_back(last_missed);
                }
                MissedPolicy::Queue => {
                    let skip = missed.len().saturating_sub(limit);
                    slot.backlog.extend(missed.iter().skip(skip).copied());
                    while slot.backlog.len() > limit {
                        slot.backlog.pop_front();
                    }
                }
            }
        }
        if let Some(due_at) = on_time {
            wrote = true;
            slot.handled = slot.handled.max(Some(due_at));
            if slot.live() {
                match schedule.draft.overlap_policy {
                    OverlapPolicy::Skip => {
                        self.mark(
                            repository,
                            id,
                            &ScheduleMark::new(due_at, ScheduleOutcome::SkippedRunning),
                        )
                        .await;
                    }
                    OverlapPolicy::QueueOne => {
                        if slot.waiting.is_none() {
                            slot.waiting = Some(due_at);
                            self.mark(
                                repository,
                                id,
                                &ScheduleMark::new(due_at, ScheduleOutcome::Waiting),
                            )
                            .await;
                        } else {
                            let mut mark =
                                ScheduleMark::new(due_at, ScheduleOutcome::SkippedRunning);
                            mark.reason = Some("coalesced".to_owned());
                            self.mark(repository, id, &mark).await;
                        }
                    }
                    OverlapPolicy::Concurrent => {
                        self.submit_due(due, slot, due_at, false).await;
                    }
                }
            } else {
                self.submit_due(due, slot, due_at, false).await;
            }
        }
        // Then what waits: the due time held by *queue one*, then the
        // catch-ups, one at a time, each only once the schedule is idle.
        if !slot.live() {
            if let Some(due_at) = slot.waiting.take() {
                self.submit_due(due, slot, due_at, false).await;
                wrote = true;
            } else if let Some(due_at) = slot.backlog.pop_front() {
                self.submit_due(due, slot, due_at, true).await;
                wrote = true;
            }
        }
        (wrote, rule.next_after(&zone, now.max(floor)))
    }

    /// Reads what a schedule runs and checks it against its grant.
    async fn check(
        &self,
        repository: &AutomationRepository,
        schedule: &StoredSchedule,
        connections: &HashMap<ConnectionId, ScheduleConnection>,
    ) -> Result<(Target, Grant, Rule, jiff::tz::TimeZone), Blocked> {
        let zone = rule::time_zone(&schedule.draft.time_zone)
            .ok_or(Blocked::Suspend("time_zone_unknown"))?;
        let rule = Rule::from_json(&schedule.draft.rule_json).map_err(|_| Blocked::Unreadable)?;
        let grant = repository
            .grant(schedule.draft.id)
            .await
            .map_err(|_| Blocked::Unreadable)?
            .ok_or(Blocked::Suspend("target_changed"))?;
        if grant.schedule_revision != schedule.revision {
            return Err(Blocked::Suspend("target_changed"));
        }
        let target = match self.read_target(repository, &schedule.draft).await {
            Ok(target) => target,
            Err(ScheduleError::Storage(_)) => return Err(Blocked::Unreadable),
            Err(_) => {
                return Err(Blocked::Suspend(if schedule.draft.job_version.is_none() {
                    "chain_changed"
                } else {
                    "target_changed"
                }));
            }
        };
        if target.job_revision != grant.job_revision {
            return Err(Blocked::Suspend("chain_changed"));
        }
        if target.fingerprint != grant.target_fingerprint {
            return Err(Blocked::Suspend("target_changed"));
        }
        for (id, _) in &target.connections {
            if !grant
                .connections
                .iter()
                .any(|bound| bound.connection_id == *id)
            {
                return Err(Blocked::Suspend("target_changed"));
            }
        }
        for bound in &grant.connections {
            let Some(connection) = connections.get(&bound.connection_id) else {
                return Err(Blocked::Suspend("connection_deleted"));
            };
            if connection.input.revision != bound.connection_revision {
                return Err(Blocked::Suspend("connection_changed"));
            }
            if environment_code(connection.environment) != bound.environment {
                return Err(Blocked::Suspend("environment_changed"));
            }
        }
        Ok((target, grant, rule, zone))
    }

    async fn suspend(
        &self,
        repository: &AutomationRepository,
        schedule: &StoredSchedule,
        slot: &mut Slot,
        reason: &str,
    ) {
        withdraw(self, slot);
        match repository
            .suspend_schedule(schedule.draft.id, reason, self.clock().now())
            .await
        {
            Ok(_) => tracing::info!(schedule = %schedule.draft.id, reason, "schedule suspended"),
            Err(error) => {
                tracing::warn!(error_code = ?error.code, "schedule suspension not recorded");
            }
        }
    }

    async fn mark(
        &self,
        repository: &AutomationRepository,
        schedule: ScheduleId,
        mark: &ScheduleMark,
    ) {
        if let Err(error) = repository.mark_schedule(schedule, mark).await {
            tracing::warn!(schedule = %schedule, error_code = ?error.code, "schedule outcome not recorded");
        }
    }

    /// Submits the run of one due time, as **Run** would, and records it.
    async fn submit_due(&self, due: Due<'_>, slot: &mut Slot, due_at: UtcMillis, catch_up: bool) {
        let Due {
            repository,
            schedule,
            target,
            connections,
        } = due;
        let id = schedule.draft.id;
        let scheduled = RunSchedule {
            schedule: id,
            due_at,
            catch_up,
        };
        let outcome = if catch_up {
            ScheduleOutcome::CaughtUp
        } else {
            ScheduleOutcome::Started
        };
        let submitted = match &target.plan {
            Some(plan) => {
                let mut inputs = HashMap::new();
                for connection in plan.connections() {
                    if let Some(known) = connections.get(&connection) {
                        inputs.insert(connection, known.input.clone());
                    }
                }
                self.submit_chain_with(
                    ChainRequest {
                        plan: plan.clone(),
                        values: defaults(&target.inputs),
                        connections: inputs,
                    },
                    Some(scheduled),
                )
                .map(Handle::Chain)
            }
            None => match target.definition() {
                Some(definition) => self
                    .submit_run(Submission {
                        definition: definition.clone(),
                        job: Some(target.job),
                        revision: None,
                        version: schedule.draft.job_version,
                        values: defaults(&target.inputs),
                        connection: sql_connection(definition)
                            .and_then(|(connection, _)| connections.get(&connection))
                            .map(|known| known.input.clone()),
                        scheduled: Some(scheduled),
                    })
                    .map(Handle::Run),
                None => Err(Refusal::Missing),
            },
        };
        match submitted {
            Ok(handle) => {
                let mut mark = ScheduleMark::new(due_at, outcome);
                match &handle {
                    Handle::Run(run) => mark.run_id = Some(run.id),
                    Handle::Chain(chain) => mark.chain_run_id = Some(chain.id),
                }
                self.mark(repository, id, &mark).await;
                tracing::info!(schedule = %id, catch_up, "scheduled run submitted");
                slot.runs.push(Tracked {
                    due_at,
                    handle,
                    linked: false,
                });
            }
            Err(refusal) => {
                let reason = refusal_code(&refusal);
                let mut mark = ScheduleMark::new(due_at, ScheduleOutcome::Refused);
                mark.reason = Some(reason.to_owned());
                self.mark(repository, id, &mark).await;
                tracing::info!(schedule = %id, reason, "scheduled run refused");
                self.bad_end(schedule, &target.name, due_at, None, Some(reason));
            }
        }
    }

    /// Forgets the runs that ended, recording what the history alone would
    /// not show: a run refused after waiting, a program whose file changed.
    async fn settle(
        &self,
        repository: &AutomationRepository,
        schedule: &StoredSchedule,
        slot: &mut Slot,
    ) -> bool {
        let mut wrote = false;
        // The run's owner writes its row: once it exists, the schedule names
        // it (the latest submitted last).
        for tracked in slot.runs.iter_mut().filter(|tracked| !tracked.linked) {
            let (run, chain_run) = match &tracked.handle {
                Handle::Run(handle) => (Some(handle.id), None),
                Handle::Chain(handle) => (None, Some(handle.id)),
            };
            match repository
                .link_schedule_run(schedule.draft.id, run, chain_run)
                .await
            {
                Ok(linked) => {
                    tracked.linked = linked;
                    wrote |= linked;
                }
                Err(error) => {
                    tracing::warn!(error_code = ?error.code, "schedule's run not linked");
                }
            }
        }
        let mut ended = Vec::new();
        slot.runs.retain(|tracked| {
            if tracked.handle.live() {
                true
            } else {
                ended.push((tracked.due_at, end_of(&tracked.handle)));
                false
            }
        });
        if ended.is_empty() {
            return wrote;
        }
        let name = self.job_name(repository, schedule.draft.job_id).await;
        for (due_at, end) in ended {
            match end {
                End::Succeeded | End::Cancelled | End::Withdrawn => {}
                End::Failed(cause) => self.bad_end(schedule, &name, due_at, Some(cause), None),
                End::ProgramChanged => {
                    self.suspend(repository, schedule, slot, "program_changed")
                        .await;
                    wrote = true;
                }
                End::Refused(reason) => {
                    let mut mark = ScheduleMark::new(due_at, ScheduleOutcome::Refused);
                    mark.reason = Some(reason.to_owned());
                    self.mark(repository, schedule.draft.id, &mark).await;
                    self.bad_end(schedule, &name, due_at, None, Some(reason));
                    wrote = true;
                }
            }
        }
        wrote
    }

    async fn job_name(&self, repository: &AutomationRepository, job: JobId) -> String {
        repository
            .job(job)
            .await
            .ok()
            .flatten()
            .and_then(|job| JobDefinition::from_json(&job.definition_json).ok())
            .map(|definition| definition.name)
            .unwrap_or_default()
    }

    fn bad_end(
        &self,
        schedule: &StoredSchedule,
        name: &str,
        due_at: UtcMillis,
        cause: Option<Cause>,
        refused: Option<&'static str>,
    ) {
        let end = BadEnd {
            schedule: schedule.draft.id,
            job: schedule.draft.job_id,
            name: name.to_owned(),
            due_at,
            cause,
            refused,
            notify: schedule.draft.notify,
        };
        self.inner.schedules.news.send_modify(|news| {
            news.bad_ends = news.bad_ends.wrapping_add(1);
            news.latest = Some(end);
        });
    }
}

/// How a tracked run ended, as the scheduler cares.
enum End {
    Succeeded,
    Cancelled,
    Failed(Cause),
    /// Stop, a suspension or quitting took it out of the queue.
    Withdrawn,
    /// The version's program file changed (Q-16): its grant no longer holds.
    ProgramChanged,
    Refused(&'static str),
}

fn end_of(handle: &Handle) -> End {
    let state = |state: RunState, cause: Option<Cause>| match state {
        RunState::Succeeded => End::Succeeded,
        RunState::Failed => End::Failed(cause.unwrap_or(Cause::Internal)),
        _ => End::Cancelled,
    };
    let refused = |refusal: &Refusal| match refusal {
        Refusal::Withdrawn | Refusal::ShuttingDown => End::Withdrawn,
        Refusal::Program(super::process::Refused::ProgramChanged { .. }) => End::ProgramChanged,
        other => End::Refused(refusal_code(other)),
    };
    match handle {
        Handle::Run(handle) => match handle.status() {
            RunStatus::Finished(outcome) => state(outcome.state, outcome.cause),
            RunStatus::Refused(refusal) => refused(&refusal),
            _ => End::Cancelled,
        },
        Handle::Chain(handle) => match handle.status() {
            ChainStatus::Finished(outcome) => state(outcome.state, outcome.cause),
            ChainStatus::Refused(refusal) => refused(&refusal),
            ChainStatus::Running { .. } => End::Cancelled,
        },
    }
}

/// A stable code for why a scheduled run did not run; never a message.
pub fn refusal_code(refusal: &Refusal) -> &'static str {
    match refusal {
        Refusal::QueueFull(_) => "queue_full",
        Refusal::QueueTimeout => "queue_timeout",
        Refusal::Inputs(_) | Refusal::Invalid(_) => "input",
        Refusal::NoConnection => "connection",
        Refusal::Unavailable | Refusal::ShuttingDown => "unavailable",
        Refusal::History => "history",
        Refusal::Program(_) => "program",
        Refusal::Chain(_) | Refusal::Missing => "target",
        Refusal::Withdrawn => "withdrawn",
    }
}

/// Takes a schedule's queued runs out of the queue, as **Stop** does, and
/// drops its catch-ups and its waiting due time. An active run goes on.
fn withdraw(engine: &Engine, slot: &mut Slot) -> bool {
    let mut changed = !slot.backlog.is_empty() || slot.waiting.is_some();
    slot.backlog.clear();
    slot.waiting = None;
    for tracked in &slot.runs {
        if let Handle::Run(handle) = &tracked.handle
            && matches!(handle.status(), RunStatus::Queued { .. })
        {
            changed |= engine.stop_run(handle.id);
        }
    }
    changed
}
