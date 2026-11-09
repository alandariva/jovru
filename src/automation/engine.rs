//! The application-root service (specification, sections 5.2–5.6 and 6.6;
//! ADR-0034): the data folder's lock, recovery, admission and the queue,
//! one owner task per run, Stop, and the coordinated shutdown.
use super::chain::{InputProblem, form_values};
use super::definition::{self, ChainProblem, JobBody, JobDefinition, PinnedFile, Problem};
use super::executor::{self, Prepared};
use super::queue::{Admission, Blocked, Decision, Limits};
use super::schedule::{self, Clock};
use super::{
    Cause, ChainRunId, ChainStatus, JobId, JobKind, Phase, Progress, Refusal, RunId, RunStatus,
    SqlJob, UtcMillis, chain_run, jjp, logs, process, program_run, sql_run,
};
use crate::result::DEFAULT_MEMORY_CEILING;
use jykre_core::automation::RunSchedule;
use jykre_core::{CancellationToken, ConnectionId, Driver, DriverResult, ResolvedConnection};
use jykre_storage::{
    AutomationLock, AutomationRepository, ChainRef, InUse, Opened, Orphan, StoredJob, StoredVersion,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

/// How long a stopped SQL run waits for its driver to bring the session
/// back to a known state and close it: the drivers' recovery window (5 s)
/// plus the close's own deadline (5 s), with a margin. Past it the work is
/// dropped, which drops the session.
pub(super) const SETTLE: Duration = Duration::from_secs(11);
/// How often history and logs are pruned while the application runs.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);
/// Program logs are deleted this long after their run finished (ADR-0037).
pub const LOG_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Whether automation can run in this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// The lock, the file and recovery are being taken care of.
    Starting,
    /// Runs can be admitted and jobs saved.
    Ready,
    /// Another Jykre Studio holds the data folder's automation lock:
    /// history can be read, nothing can run or be saved (section 6.6).
    ReadOnly,
    /// Automation is off; the rest of the application works.
    Off(OffReason),
}

/// Why automation is off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OffReason {
    /// `automation.db` was written by a newer version.
    Newer,
    /// The file or the lock could not be opened.
    Unreadable,
}

/// What recovery found when this process took the lock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    /// A damaged `automation.db` was set aside and a new one created: job
    /// definitions were lost.
    pub recreated: bool,
    /// Runs and chain runs marked interrupted.
    pub interrupted: usize,
    /// Programs a previous session left running whose group leader still
    /// has the recorded start time (Unix, section 9.1.6).
    pub orphans: Vec<Orphan>,
}

/// One active or queued run or chain, for the lists and the quit dialog.
#[derive(Debug, Clone)]
pub struct OverviewEntry {
    pub name: String,
    pub kind: JobKind,
    pub job: Option<JobId>,
    pub run: Option<RunId>,
    pub chain: Option<ChainRunId>,
    /// The step, from 0, of a chain's run.
    pub step: Option<usize>,
    pub status: Option<RunStatus>,
    pub chain_status: Option<ChainStatus>,
}

/// A connection the view resolved for a run: its driver, the configuration
/// with the password the user typed or the vault gave, and the profile's
/// revision, recorded with the run (section 6.3).
#[derive(Clone)]
pub struct ConnectionInput {
    pub id: ConnectionId,
    pub driver: Arc<dyn Driver>,
    pub resolved: ResolvedConnection,
    pub revision: i64,
}

/// A run of one job. The view resolves the password and the production
/// confirmation before submitting it (section 4.5); the engine opens no
/// dialog.
pub struct JobRequest {
    /// The stored definition, or the editor's unsaved one (a draft run).
    pub definition: JobDefinition,
    /// The stored job and its revision; `None` for a draft run.
    pub stored: Option<(JobId, i64)>,
    /// What the form holds for each input.
    pub values: HashMap<String, String>,
    /// A SQL Job's connection.
    pub connection: Option<ConnectionInput>,
}

/// One step of a chain's plan, with the version it names.
#[derive(Debug, Clone)]
pub struct PlanStep {
    pub id: String,
    pub job: JobId,
    pub version: i64,
    /// The version's frozen definition.
    pub definition: JobDefinition,
    /// The job's name now, or the version's when the job is gone.
    pub job_name: String,
    /// The job's draft differs from this version.
    pub changed_since: bool,
}

/// A chain checked against the versions it names, before its run is
/// confirmed (section 9.3.4).
#[derive(Debug, Clone)]
pub struct Plan {
    pub chain: JobDefinition,
    pub stored: Option<(JobId, i64)>,
    pub steps: Vec<PlanStep>,
    /// Empty when the chain can run.
    pub problems: Vec<ChainProblem>,
}

impl Plan {
    /// The connections the SQL steps use, in step order, once each.
    pub fn connections(&self) -> Vec<ConnectionId> {
        let mut found = Vec::new();
        for step in &self.steps {
            if let JobBody::Sql(sql) = &step.definition.body
                && let Some(connection) = sql.connection
                && !found.contains(&connection)
            {
                found.push(connection);
            }
        }
        found
    }
}

/// A chain run to start: the confirmed plan, the chain's form and every
/// connection its SQL steps use.
pub struct ChainRequest {
    pub plan: Plan,
    pub values: HashMap<String, String>,
    pub connections: HashMap<ConnectionId, ConnectionInput>,
}

/// Why a job could not be saved, published or deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveError {
    Unavailable,
    Problems(Vec<Problem>),
    Chain(Vec<ChainProblem>),
    /// The job or version is named by these chains.
    InUse(Vec<ChainRef>),
    /// The version is the one the job's schedule runs (ADR-0041).
    Scheduled,
    /// Someone else saved the job meanwhile, or it is gone.
    Conflict,
    Storage(String),
}

/// Why a job could not be published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    Save(SaveError),
    /// A chain is not published; its steps are.
    Chain,
    /// The program's file cannot be read to record its hash; `None` when
    /// the check itself could not run.
    Program(Option<process::Refused>),
}

/// A subscription to one run. Dropping it never stops the run; only
/// [`Engine::stop_run`] does.
#[derive(Debug, Clone)]
pub struct RunHandle {
    pub id: RunId,
    status: watch::Receiver<RunStatus>,
}

impl RunHandle {
    /// The latest status.
    pub fn status(&self) -> RunStatus {
        self.status.borrow().clone()
    }

    /// Waits for the run's end: its outcome, or why it never ran.
    pub async fn finished(&mut self) -> RunStatus {
        let ended = self
            .status
            .wait_for(|status| matches!(status, RunStatus::Finished(_) | RunStatus::Refused(_)))
            .await
            .map(|status| status.clone());
        ended.unwrap_or_else(|_| self.status())
    }

    /// Waits for the next change; `false` once nothing can change.
    pub async fn changed(&mut self) -> bool {
        self.status.changed().await.is_ok()
    }
}

/// A subscription to one chain run.
#[derive(Debug, Clone)]
pub struct ChainHandle {
    pub id: ChainRunId,
    status: watch::Receiver<ChainStatus>,
}

impl ChainHandle {
    pub fn status(&self) -> ChainStatus {
        self.status.borrow().clone()
    }

    pub async fn finished(&mut self) -> ChainStatus {
        let ended = self
            .status
            .wait_for(|status| matches!(status, ChainStatus::Finished(_) | ChainStatus::Refused(_)))
            .await
            .map(|status| status.clone());
        ended.unwrap_or_else(|_| self.status())
    }

    pub async fn changed(&mut self) -> bool {
        self.status.changed().await.is_ok()
    }
}

/// The automation service. Cloning shares it. Runs belong to the engine,
/// not to whoever submitted them: dropping every handle leaves active runs
/// going to their end. [`Engine::shutdown`] stops them all and waits.
#[derive(Clone)]
pub struct Engine {
    pub(super) inner: Arc<Inner>,
}

pub(super) struct Inner {
    pub runtime: Handle,
    pub data_dir: PathBuf,
    pub memory_ceiling: usize,
    pub jjp: jjp::JjpHost,
    pub timings: process::Timings,
    availability: watch::Sender<Availability>,
    changes: watch::Sender<u64>,
    repository: Mutex<Option<AutomationRepository>>,
    lock: Mutex<Option<AutomationLock>>,
    recovery: Mutex<Option<Recovery>>,
    registry: Mutex<Registry>,
    revisions: Mutex<HashMap<ConnectionId, i64>>,
    /// Schedules: the clock, the connections they may use, and what the
    /// scheduler tells the views (ADR-0041).
    pub schedules: schedule::Shared,
    /// One `open` at a time: a second **Try again** waits for the first.
    opening: tokio::sync::Mutex<()>,
    /// One shutdown at a time; the lock is released only after it.
    closing: tokio::sync::Mutex<()>,
}

/// Who to tell when a queued run's place changes.
enum Notify {
    Run(watch::Sender<RunStatus>),
}

struct Registry {
    shutting_down: bool,
    admission: Admission,
    next_ticket: u64,
    waiters: HashMap<u64, oneshot::Sender<()>>,
    queued: HashMap<u64, Notify>,
    runs: Vec<RunEntry>,
    chains: Vec<ChainEntry>,
}

struct RunEntry {
    id: RunId,
    ticket: u64,
    name: String,
    kind: JobKind,
    job: Option<JobId>,
    chain: Option<(ChainRunId, usize)>,
    stop: CancellationToken,
    status: watch::Receiver<RunStatus>,
    task: Option<JoinHandle<()>>,
}

struct ChainEntry {
    id: ChainRunId,
    name: String,
    job: Option<JobId>,
    stop: CancellationToken,
    status: watch::Receiver<ChainStatus>,
    task: Option<JoinHandle<()>>,
}

/// A slot of the admission limits, held by an active run; dropping it
/// starts the runs waiting for it.
pub(super) struct Slot {
    inner: Arc<Inner>,
    ticket: u64,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.inner.release(self.ticket);
    }
}

/// A ticket's place in admission, decided when the run was submitted.
pub(super) enum Ticket {
    Admitted(Slot),
    Queued(u64, oneshot::Receiver<()>),
}

/// How a run's work ended.
pub(super) enum Ended {
    Finished(Arc<super::RunOutcome>),
    Refused(Refusal),
}

/// A SQL run's statement, checked and bound, and its connection.
pub(super) struct SqlWork {
    pub job: SqlJob,
    pub prepared: Prepared,
    pub connection: ConnectionInput,
}

/// What [`Engine::submit_run`] starts.
pub(super) struct Submission {
    pub definition: JobDefinition,
    /// The stored job; `None` for a draft run.
    pub job: Option<JobId>,
    /// The job's revision, for a job run by hand.
    pub revision: Option<i64>,
    /// The published version a schedule runs.
    pub version: Option<i64>,
    pub values: HashMap<String, String>,
    pub connection: Option<ConnectionInput>,
    pub scheduled: Option<RunSchedule>,
}

/// A chain step's run, registered by its chain.
pub(super) struct StepRun {
    pub id: RunId,
    pub name: String,
    pub kind: JobKind,
    pub job: Option<JobId>,
    pub chain: (ChainRunId, usize),
    pub connection: Option<ConnectionId>,
    pub stop: CancellationToken,
    pub sender: watch::Sender<RunStatus>,
}

/// What one run executes once admitted.
pub(super) struct RunSpec {
    pub id: RunId,
    pub definition: JobDefinition,
    /// The stored job; `None` for a draft run.
    pub job: Option<JobId>,
    /// The job's revision at admission, for a job run on its own.
    pub revision: Option<i64>,
    /// The published version a chain step runs.
    pub version: Option<i64>,
    pub chain: Option<(ChainRunId, usize)>,
    /// The schedule that started it, or its chain (ADR-0041).
    pub scheduled: Option<RunSchedule>,
    pub work: Work,
}

pub(super) enum Work {
    Sql(Box<SqlWork>),
    Program {
        /// Each input's checked value, for `hello`.
        values: Vec<(String, Option<String>)>,
    },
}

impl Engine {
    /// Starts the service for `data_dir` on the application's runtime: it
    /// takes the lock, opens `automation.db`, recovers and prunes in the
    /// background. [`Engine::availability`] says when it is ready.
    pub fn start(runtime: Handle, data_dir: PathBuf) -> Self {
        Self::with_limits(
            runtime,
            data_dir,
            Limits::default(),
            DEFAULT_MEMORY_CEILING,
            process::Timings::default(),
        )
    }

    pub fn with_limits(
        runtime: Handle,
        data_dir: PathBuf,
        limits: Limits,
        memory_ceiling: usize,
        timings: process::Timings,
    ) -> Self {
        Self::with_clock(
            runtime,
            data_dir,
            limits,
            memory_ceiling,
            timings,
            Clock::System,
        )
    }

    /// [`Engine::with_limits`] with the clock schedules read: the system's,
    /// or one a test moves (specification, section 9.5.10).
    pub fn with_clock(
        runtime: Handle,
        data_dir: PathBuf,
        limits: Limits,
        memory_ceiling: usize,
        timings: process::Timings,
        clock: Clock,
    ) -> Self {
        let engine = Self {
            inner: Arc::new(Inner {
                runtime,
                data_dir,
                memory_ceiling,
                jjp: jjp::JjpHost::new(),
                timings,
                availability: watch::Sender::new(Availability::Starting),
                changes: watch::Sender::new(0),
                repository: Mutex::new(None),
                lock: Mutex::new(None),
                recovery: Mutex::new(None),
                registry: Mutex::new(Registry {
                    shutting_down: false,
                    admission: Admission::new(limits),
                    next_ticket: 0,
                    waiters: HashMap::new(),
                    queued: HashMap::new(),
                    runs: Vec::new(),
                    chains: Vec::new(),
                }),
                revisions: Mutex::new(HashMap::new()),
                schedules: schedule::Shared::new(clock),
                opening: tokio::sync::Mutex::new(()),
                closing: tokio::sync::Mutex::new(()),
            }),
        };
        // The background task holds the service weakly, so dropping every
        // `Engine` drops the lock too.
        let starting = Arc::downgrade(&engine.inner);
        drop(engine.inner.runtime.spawn(async move {
            let Some(inner) = starting.upgrade() else {
                return;
            };
            Engine { inner }.open().await;
        }));
        engine
    }

    /// Waits until the engine has left [`Availability::Starting`].
    pub async fn started(&self) -> Availability {
        let mut availability = self.inner.availability.subscribe();
        match availability
            .wait_for(|state| *state != Availability::Starting)
            .await
        {
            Ok(state) => state.clone(),
            Err(_) => self.availability(),
        }
    }

    pub fn availability(&self) -> Availability {
        self.inner.availability.borrow().clone()
    }

    /// Changes of availability.
    pub fn watch_availability(&self) -> watch::Receiver<Availability> {
        self.inner.availability.subscribe()
    }

    /// A counter that grows whenever a run starts, waits, moves in the
    /// queue or ends, or jobs change: views redraw their lists on it,
    /// coalesced per frame (section 5.6).
    pub fn watch_changes(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }

    /// The repository, for reading jobs and history. `None` until started
    /// or when automation is off.
    pub fn repository(&self) -> Option<AutomationRepository> {
        lock(&self.inner.repository).clone()
    }

    /// What recovery found, once; the view shows it at start.
    pub fn take_recovery(&self) -> Option<Recovery> {
        lock(&self.inner.recovery).take()
    }

    /// Where program logs and the lock live.
    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    /// Tries the lock again when another instance held it (**Try again**).
    pub async fn retry(&self) -> Availability {
        if self.availability() == Availability::ReadOnly {
            self.open().await;
        }
        self.availability()
    }

    /// The profile revisions of every connection, from the view whenever
    /// they change: a chain step whose connection changed since its plan was
    /// confirmed does not start (`PlanChanged`).
    pub fn set_connection_revisions(&self, revisions: HashMap<ConnectionId, i64>) {
        *lock(&self.inner.revisions) = revisions;
    }

    pub(super) fn connection_revision(&self, connection: ConnectionId) -> Option<i64> {
        lock(&self.inner.revisions).get(&connection).copied()
    }

    async fn open(&self) {
        let _opening = self.inner.opening.lock().await;
        if matches!(
            self.availability(),
            Availability::Ready | Availability::Off(_)
        ) || self.inner.registry().shutting_down
        {
            return;
        }
        let dir = self.inner.data_dir.clone();
        let acquired = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || {
                std::fs::create_dir_all(&dir).ok();
                AutomationLock::try_acquire(&dir)
            })
            .await
        };
        let lock_taken = match acquired {
            Ok(Ok(Some(lock_file))) => {
                *lock(&self.inner.lock) = Some(lock_file);
                true
            }
            Ok(Ok(None)) => false,
            Ok(Err(_)) | Err(_) => {
                tracing::warn!("automation lock unavailable");
                self.set_availability(Availability::Off(OffReason::Unreadable));
                return;
            }
        };
        if !lock_taken {
            // The file belongs to the lock holder: read it as it is, never
            // create, migrate or set it aside (ADR-0034, decision 4).
            match AutomationRepository::open_read_only(&dir).await {
                Ok(Some(Opened::Newer)) => {
                    self.set_availability(Availability::Off(OffReason::Newer));
                }
                Ok(Some(Opened::Ready(repository) | Opened::Recreated(repository))) => {
                    *lock(&self.inner.repository) = Some(repository);
                    self.set_availability(Availability::ReadOnly);
                }
                Ok(None) | Err(_) => {
                    *lock(&self.inner.repository) = None;
                    self.set_availability(Availability::ReadOnly);
                }
            }
            tracing::info!("automation is read-only: another instance holds its lock");
            return;
        }
        let opened = AutomationRepository::open(&dir).await;
        let (repository, recreated) = match opened {
            Ok(Opened::Ready(repository)) => (repository, false),
            Ok(Opened::Recreated(repository)) => (repository, true),
            Ok(Opened::Newer) => {
                *lock(&self.inner.lock) = None;
                self.set_availability(Availability::Off(OffReason::Newer));
                return;
            }
            Err(error) => {
                tracing::warn!(error_code = ?error.code, "automation history unavailable");
                *lock(&self.inner.lock) = None;
                self.set_availability(Availability::Off(OffReason::Unreadable));
                return;
            }
        };
        *lock(&self.inner.repository) = Some(repository.clone());
        let mut recovery = Recovery {
            recreated,
            ..Recovery::default()
        };
        match repository.recover(UtcMillis::now()).await {
            Ok(recovered) => {
                recovery.interrupted = recovered.runs.len() + recovered.chain_runs.len();
                recovery.orphans = recovered
                    .orphans
                    .into_iter()
                    .filter(|orphan| {
                        orphan.process_started.as_deref().is_some_and(|started| {
                            process::orphan_alive(orphan.process_group, started)
                        })
                    })
                    .collect();
            }
            Err(error) => {
                tracing::warn!(error_code = ?error.code, "automation recovery failed");
                *lock(&self.inner.lock) = None;
                self.set_availability(Availability::Off(OffReason::Unreadable));
                return;
            }
        }
        if recovery.interrupted > 0 || recovery.recreated {
            tracing::info!(
                interrupted = recovery.interrupted,
                orphans = recovery.orphans.len(),
                recreated = recovery.recreated,
                "automation recovered"
            );
        }
        *lock(&self.inner.recovery) = Some(recovery);
        self.set_availability(Availability::Ready);
        // Pruning runs while this process holds the lock, however it got
        // it (at start or through **Try again**).
        let pruning = Arc::downgrade(&self.inner);
        drop(
            self.inner
                .runtime
                .spawn(async move { Engine::prune_loop(pruning).await }),
        );
        // Schedules run only in the lock holder, from now on (section 9.5.4).
        schedule::start(&self.inner);
    }

    /// Ends the programs a previous session left running, each only while
    /// its group leader still has the recorded start time.
    pub async fn end_orphans(&self, orphans: Vec<Orphan>) {
        drop(
            tokio::task::spawn_blocking(move || {
                for orphan in orphans {
                    // Ended only while its leader still has the recorded
                    // start time: a reused ID is never touched.
                    let Some(started) = orphan.process_started.as_deref() else {
                        continue;
                    };
                    if process::end_group(orphan.process_group, started).is_err() {
                        tracing::warn!(run = %orphan.run_id, "could not end a program left running");
                    }
                }
            })
            .await,
        );
    }

    fn set_availability(&self, state: Availability) {
        self.inner.availability.send_replace(state);
        self.inner.changed();
    }

    async fn prune_loop(inner: std::sync::Weak<Inner>) {
        loop {
            let Some(engine) = inner.upgrade().map(|inner| Engine { inner }) else {
                return;
            };
            if engine.availability() != Availability::Ready
                || lock(&engine.inner.registry).shutting_down
            {
                return;
            }
            engine.prune(UtcMillis::now()).await;
            drop(engine);
            tokio::time::sleep(PRUNE_EVERY).await;
        }
    }

    /// Prunes history (ADR-0034, ADR-0035) and program logs older than 7
    /// days, or whose run is gone (ADR-0037). Never blocks admission.
    pub async fn prune(&self, now: UtcMillis) {
        let Some(repository) = self.repository() else {
            return;
        };
        if let Err(error) = repository.prune(now).await {
            tracing::warn!(error_code = ?error.code, "automation pruning failed");
        }
        let present = logs::log_runs(&self.inner.data_dir)
            .await
            .unwrap_or_default();
        let active: Vec<RunId> = lock(&self.inner.registry)
            .runs
            .iter()
            .map(|entry| entry.id)
            .collect();
        for run in present {
            if active.contains(&run) {
                continue;
            }
            let expired = match repository.run(run).await {
                Ok(Some(record)) => record
                    .finished_at
                    .is_some_and(|finished| finished <= now.minus(LOG_RETENTION)),
                Ok(None) => true,
                Err(_) => false,
            };
            if expired {
                self.delete_logs(run).await;
            }
        }
    }

    /// **Delete logs**: removes a run's log folder now (ADR-0037).
    pub async fn delete_logs(&self, run: RunId) {
        if logs::delete_logs(&self.inner.data_dir, run).await.is_err() {
            tracing::warn!(run = %run, "could not delete a run's logs");
        }
        if let Some(repository) = self.repository()
            && let Err(error) = repository.forget_logs(run).await
        {
            tracing::warn!(error_code = ?error.code, "could not record deleted logs");
        }
    }

    // -----------------------------------------------------------------------
    // Jobs.

    fn writable(&self) -> Result<AutomationRepository, SaveError> {
        if self.availability() != Availability::Ready || self.inner.registry().shutting_down {
            return Err(SaveError::Unavailable);
        }
        self.repository().ok_or(SaveError::Unavailable)
    }

    /// Saves `definition` as a new job (`expected_revision` `None`) or over
    /// the stored one at that revision. Checks it first; a chain is checked
    /// against the versions it names.
    pub async fn save_job(
        &self,
        definition: &JobDefinition,
        expected_revision: Option<i64>,
    ) -> Result<StoredJob, SaveError> {
        let repository = self.writable()?;
        let problems = definition::problems(definition);
        if !problems.is_empty() {
            return Err(SaveError::Problems(problems));
        }
        if let JobBody::Chain(chain) = &definition.body {
            let versions = self.versions_named(&repository, chain).await?;
            let problems =
                definition::chain_problems(chain, |job, version| versions.get(&(job, version)));
            if !problems.is_empty() {
                return Err(SaveError::Chain(problems));
            }
        }
        let now = UtcMillis::now();
        let saved = match expected_revision {
            None => {
                repository
                    .insert_job(definition.id, definition.kind(), &definition.to_json(), now)
                    .await
            }
            Some(revision) => {
                repository
                    .update_job(definition.id, &definition.to_json(), revision, now)
                    .await
            }
        };
        let saved = saved.map_err(|error| match error.code {
            jykre_core::ErrorCode::Validation => SaveError::Conflict,
            _ => SaveError::Storage(error.message),
        })?;
        self.inner.changed();
        Ok(saved)
    }

    async fn versions_named(
        &self,
        repository: &AutomationRepository,
        chain: &definition::ChainBody,
    ) -> Result<HashMap<(JobId, i64), JobDefinition>, SaveError> {
        let mut versions = HashMap::new();
        for step in &chain.steps {
            if versions.contains_key(&(step.job, step.version)) {
                continue;
            }
            let stored = repository
                .version(step.job, step.version)
                .await
                .map_err(|error| SaveError::Storage(error.message))?;
            if let Some(stored) = stored
                && let Ok(definition) = JobDefinition::from_json(&stored.definition_json)
            {
                versions.insert((step.job, step.version), definition);
            }
        }
        Ok(versions)
    }

    /// Deletes a job with its versions and its schedule (whose grant goes
    /// with it), refused while a chain names any of its versions.
    pub async fn delete_job(&self, job: JobId) -> Result<(), SaveError> {
        let repository = self.writable()?;
        match repository.delete_job(job).await {
            Ok(Ok(())) => {
                self.inner.changed();
                Ok(())
            }
            Ok(Err(InUse { chains, .. })) => Err(SaveError::InUse(chains)),
            Err(error) => Err(SaveError::Storage(error.message)),
        }
    }

    /// Deletes a published version, refused while a chain or the job's
    /// schedule names it.
    pub async fn delete_version(&self, job: JobId, version: i64) -> Result<(), SaveError> {
        let repository = self.writable()?;
        match repository.delete_version(job, version).await {
            Ok(Ok(())) => {
                self.inner.changed();
                Ok(())
            }
            Ok(Err(InUse { chains, schedule })) => {
                Err(if chains.is_empty() && schedule.is_some() {
                    SaveError::Scheduled
                } else {
                    SaveError::InUse(chains)
                })
            }
            Err(error) => Err(SaveError::Storage(error.message)),
        }
    }

    /// Publishes the stored job's current definition as its next version
    /// (section 9.3.1). A program's version pins its entrypoint's path and
    /// SHA-256 (Q-16).
    pub async fn publish(&self, job: JobId) -> Result<StoredVersion, PublishError> {
        let repository = self.writable().map_err(PublishError::Save)?;
        let stored = repository
            .job(job)
            .await
            .map_err(|error| PublishError::Save(SaveError::Storage(error.message)))?
            .ok_or(PublishError::Save(SaveError::Conflict))?;
        let mut definition = JobDefinition::from_json(&stored.definition_json)
            .map_err(|_| PublishError::Save(SaveError::Conflict))?;
        let problems = definition::problems(&definition);
        if !problems.is_empty() {
            return Err(PublishError::Save(SaveError::Problems(problems)));
        }
        if let JobBody::Process(body) = &mut definition.body {
            body.pinned = Some(pin(body.clone()).await?);
        } else if definition.kind() == JobKind::Chain {
            return Err(PublishError::Chain);
        }
        let version = repository
            .publish(job, &definition.to_json(), UtcMillis::now())
            .await
            .map_err(|error| PublishError::Save(SaveError::Storage(error.message)))?;
        self.inner.changed();
        Ok(version)
    }

    /// Whether a program already ran at this revision (on its own) or
    /// version (in a chain): the trusted-code warning shows on the first run
    /// of each (Q-10).
    pub async fn program_has_run(
        &self,
        job: JobId,
        revision: Option<i64>,
        version: Option<i64>,
    ) -> bool {
        match self.repository() {
            Some(repository) => repository
                .program_has_run(job, revision, version)
                .await
                .unwrap_or(false),
            None => false,
        }
    }

    // -----------------------------------------------------------------------
    // Runs.

    /// Checks the job and its values and asks for a slot. A run that finds
    /// the limits full waits in the queue; one that cannot even wait is
    /// refused at once, with the reason.
    pub fn submit(&self, request: JobRequest) -> Result<RunHandle, Refusal> {
        let JobRequest {
            definition,
            stored,
            values,
            connection,
        } = request;
        self.submit_run(Submission {
            definition,
            job: stored.map(|(job, _)| job),
            revision: stored.map(|(_, revision)| revision),
            version: None,
            values,
            connection,
            scheduled: None,
        })
    }

    /// [`Engine::submit`] for a job, its draft or one of its published
    /// versions, by hand or by a schedule.
    pub(super) fn submit_run(&self, submission: Submission) -> Result<RunHandle, Refusal> {
        if self.availability() != Availability::Ready {
            return Err(Refusal::Unavailable);
        }
        let Submission {
            definition,
            job,
            revision,
            version,
            values,
            connection,
            scheduled,
        } = submission;
        let (work, connection_id) = match &definition.body {
            JobBody::Sql(body) => {
                let connection = connection.ok_or(Refusal::NoConnection)?;
                if body.connection != Some(connection.id) {
                    return Err(Refusal::NoConnection);
                }
                let job = SqlJob::from_body(body, connection.id);
                let prepared =
                    executor::prepare(&job, connection.driver.descriptor().dialect, &values)
                        .map_err(Refusal::Invalid)?;
                let id = connection.id;
                (
                    Work::Sql(Box::new(SqlWork {
                        job,
                        prepared,
                        connection,
                    })),
                    Some(id),
                )
            }
            JobBody::Process(_) => {
                let values = form_values(definition.inputs(), &values).map_err(Refusal::Inputs)?;
                (Work::Program { values }, None)
            }
            JobBody::Chain(_) => return Err(Refusal::Missing),
        };
        drop(values);
        let id = RunId::new();
        let name = definition.name.clone();
        let kind = definition.kind();
        let spec = RunSpec {
            id,
            definition,
            job,
            revision,
            version,
            chain: None,
            scheduled,
            work,
        };
        let stop = CancellationToken::new();
        let (sender, status) = watch::channel(RunStatus::Active(Progress::new(Phase::Admitted)));
        let mut registry = self.inner.registry();
        if registry.shutting_down {
            return Err(Refusal::ShuttingDown);
        }
        let ticket =
            self.inner
                .request(&mut registry, connection_id, Notify::Run(sender.clone()))?;
        let ticket_number = match &ticket {
            Ticket::Admitted(slot) => slot.ticket,
            Ticket::Queued(number, _) => *number,
        };
        let owner = self.clone();
        let stop_owner = stop.clone();
        // Spawned under the lock, so the owner finds its entry when it ends.
        let task = self.inner.runtime.spawn(async move {
            let ended = match owner.wait(ticket, &stop_owner, &sender).await {
                Ok(slot) => {
                    let ended = owner.execute(spec, &stop_owner, &sender).await;
                    drop(slot);
                    ended
                }
                Err(refusal) => Ended::Refused(refusal),
            };
            owner.inner.forget_run(id);
            match ended {
                Ended::Finished(outcome) => {
                    sender.send_replace(RunStatus::Finished(outcome));
                }
                Ended::Refused(refusal) => {
                    sender.send_replace(RunStatus::Refused(refusal));
                }
            }
            owner.inner.changed();
        });
        registry.runs.push(RunEntry {
            id,
            ticket: ticket_number,
            name,
            kind,
            job,
            chain: None,
            stop,
            status: status.clone(),
            task: Some(task),
        });
        drop(registry);
        self.inner.changed();
        tracing::info!(run = %id, kind = %kind, "automation run submitted");
        Ok(RunHandle { id, status })
    }

    /// Waits for a queued ticket's slot, until Stop, the queue's limit or
    /// the shutdown.
    pub(super) async fn wait(
        &self,
        ticket: Ticket,
        stop: &CancellationToken,
        status: &watch::Sender<RunStatus>,
    ) -> Result<Slot, Refusal> {
        let (number, mut admitted) = match ticket {
            Ticket::Admitted(slot) => return Ok(slot),
            Ticket::Queued(number, admitted) => (number, admitted),
        };
        let wait = self.inner.registry().admission.limits().queue_wait;
        let leave = tokio::time::sleep(wait);
        tokio::pin!(leave);
        let outcome = tokio::select! { biased;
            admitted = &mut admitted => match admitted {
                Ok(()) => Ok(()),
                Err(_) => Err(Refusal::ShuttingDown),
            },
            () = stop.cancelled() => Err(Refusal::Withdrawn),
            () = &mut leave => Err(Refusal::QueueTimeout),
        };
        let mut registry = self.inner.registry();
        registry.queued.remove(&number);
        match outcome {
            Ok(()) => {
                drop(registry);
                status.send_replace(RunStatus::Active(Progress::new(Phase::Admitted)));
                Ok(Slot {
                    inner: self.inner.clone(),
                    ticket: number,
                })
            }
            Err(refusal) => {
                registry.admission.withdraw(number);
                registry.waiters.remove(&number);
                // Admitted in the same instant only if it holds a slot now;
                // a queue emptied by the shutdown holds none.
                let admitted = registry.admission.is_active(number);
                self.inner.refresh_queue(&mut registry);
                drop(registry);
                if !admitted {
                    self.inner.changed();
                    return Err(refusal);
                }
                // Admitted in the same instant.
                let slot = Slot {
                    inner: self.inner.clone(),
                    ticket: number,
                };
                if refusal == Refusal::QueueTimeout {
                    status.send_replace(RunStatus::Active(Progress::new(Phase::Admitted)));
                    Ok(slot)
                } else {
                    drop(slot);
                    Err(refusal)
                }
            }
        }
    }

    /// Runs an admitted run to its end: durable admission, execution,
    /// arbitration, the terminal write, in that order.
    pub(super) async fn execute(
        &self,
        spec: RunSpec,
        stop: &CancellationToken,
        status: &watch::Sender<RunStatus>,
    ) -> Ended {
        let Some(repository) = self.repository() else {
            return Ended::Refused(Refusal::Unavailable);
        };
        match spec.work {
            Work::Sql(_) => sql_run::run(self, &repository, spec, stop, status).await,
            Work::Program { .. } => program_run::run(self, &repository, spec, stop, status).await,
        }
    }

    /// Registers a chain step's run so it shows in the lists and Stop
    /// reaches it, and asks for its slot.
    pub(super) fn submit_step(&self, step: StepRun) -> Result<Ticket, Refusal> {
        let mut registry = self.inner.registry();
        if registry.shutting_down {
            return Err(Refusal::ShuttingDown);
        }
        let ticket = self.inner.request(
            &mut registry,
            step.connection,
            Notify::Run(step.sender.clone()),
        )?;
        let number = match &ticket {
            Ticket::Admitted(slot) => slot.ticket,
            Ticket::Queued(number, _) => *number,
        };
        registry.runs.push(RunEntry {
            id: step.id,
            ticket: number,
            name: step.name,
            kind: step.kind,
            job: step.job,
            chain: Some(step.chain),
            stop: step.stop,
            status: step.sender.subscribe(),
            task: None,
        });
        drop(registry);
        self.inner.changed();
        Ok(ticket)
    }

    pub(super) fn forget_step(&self, id: RunId) {
        self.inner.forget_run(id);
        self.inner.changed();
    }

    /// Reads the versions a chain names and checks it against them, for the
    /// view to ask the passwords and the plan's confirmation.
    pub async fn plan(
        &self,
        chain: JobDefinition,
        stored: Option<(JobId, i64)>,
    ) -> DriverResult<Plan> {
        let repository = self
            .repository()
            .ok_or_else(|| jykre_core::DriverError::validation("Automation is not available."))?;
        let JobBody::Chain(body) = &chain.body else {
            return Err(jykre_core::DriverError::validation("Not a chain."));
        };
        let mut steps = Vec::new();
        let mut versions = HashMap::new();
        for step in &body.steps {
            let Some(version) = repository.version(step.job, step.version).await? else {
                continue;
            };
            let Ok(definition) = JobDefinition::from_json(&version.definition_json) else {
                continue;
            };
            let current = repository.job(step.job).await?;
            let current_definition = current
                .as_ref()
                .and_then(|job| JobDefinition::from_json(&job.definition_json).ok());
            let changed_since = current_definition.as_ref().is_some_and(|current| {
                let mut published = definition.clone();
                if let JobBody::Process(process) = &mut published.body {
                    process.pinned = None;
                }
                !current.same_definition(&published)
            });
            steps.push(PlanStep {
                id: step.id.clone(),
                job: step.job,
                version: step.version,
                job_name: current_definition
                    .map(|current| current.name)
                    .unwrap_or_else(|| definition.name.clone()),
                definition: definition.clone(),
                changed_since,
            });
            versions.insert((step.job, step.version), definition);
        }
        let mut problems = definition::problems(&chain)
            .into_iter()
            .filter_map(|problem| match problem {
                Problem::StepCount(_) | Problem::StepId(_) | Problem::StepVersion(_) => {
                    Some(ChainProblem::UnknownVersion {
                        step: String::new(),
                    })
                }
                _ => None,
            })
            .take(1)
            .collect::<Vec<_>>();
        problems.extend(definition::chain_problems(body, |job, version| {
            versions.get(&(job, version))
        }));
        Ok(Plan {
            chain,
            stored,
            steps,
            problems,
        })
    }

    /// Starts a confirmed chain plan. Exactly the confirmed plan runs.
    pub fn submit_chain(&self, request: ChainRequest) -> Result<ChainHandle, Refusal> {
        self.submit_chain_with(request, None)
    }

    /// [`Engine::submit_chain`], by hand or by a schedule.
    pub(super) fn submit_chain_with(
        &self,
        request: ChainRequest,
        scheduled: Option<RunSchedule>,
    ) -> Result<ChainHandle, Refusal> {
        if self.availability() != Availability::Ready {
            return Err(Refusal::Unavailable);
        }
        if !request.plan.problems.is_empty() {
            return Err(Refusal::Chain(request.plan.problems.clone()));
        }
        let JobBody::Chain(body) = &request.plan.chain.body else {
            return Err(Refusal::Missing);
        };
        if body.steps.len() != request.plan.steps.len() {
            return Err(Refusal::Missing);
        }
        let values = form_values(&body.inputs, &request.values).map_err(Refusal::Inputs)?;
        for connection in request.plan.connections() {
            if !request.connections.contains_key(&connection) {
                return Err(Refusal::NoConnection);
            }
        }
        let id = ChainRunId::new();
        let stop = CancellationToken::new();
        let (sender, status) = watch::channel(ChainStatus::Running { step: 0, run: None });
        let mut registry = self.inner.registry();
        if registry.shutting_down {
            return Err(Refusal::ShuttingDown);
        }
        let name = request.plan.chain.name.clone();
        let job = request.plan.stored.map(|(job, _)| job);
        let owner = self.clone();
        let chain_stop = stop.clone();
        let task = self.inner.runtime.spawn(async move {
            let ended = chain_run::run(
                &owner,
                chain_run::Started {
                    id,
                    request,
                    values,
                    scheduled,
                },
                &chain_stop,
                &sender,
            )
            .await;
            owner.inner.forget_chain(id);
            sender.send_replace(ended);
            owner.inner.changed();
        });
        registry.chains.push(ChainEntry {
            id,
            name,
            job,
            stop,
            status: status.clone(),
            task: Some(task),
        });
        drop(registry);
        self.inner.changed();
        tracing::info!(chain_run = %id, "automation chain submitted");
        Ok(ChainHandle { id, status })
    }

    /// Stops an active or queued run at once; `false` when it is neither.
    pub fn stop_run(&self, id: RunId) -> bool {
        let registry = self.inner.registry();
        match registry.runs.iter().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.stop.cancel();
                true
            }
            None => false,
        }
    }

    /// Stops a chain run and its active step.
    pub fn stop_chain(&self, id: ChainRunId) -> bool {
        let registry = self.inner.registry();
        match registry.chains.iter().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.stop.cancel();
                true
            }
            None => false,
        }
    }

    /// A new subscription to an active or queued run.
    pub fn subscribe(&self, id: RunId) -> Option<RunHandle> {
        let registry = self.inner.registry();
        registry
            .runs
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| RunHandle {
                id,
                status: entry.status.clone(),
            })
    }

    /// A new subscription to an active chain run.
    pub fn subscribe_chain(&self, id: ChainRunId) -> Option<ChainHandle> {
        let registry = self.inner.registry();
        registry
            .chains
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| ChainHandle {
                id,
                status: entry.status.clone(),
            })
    }

    /// Active and queued runs and chains, in the order they were submitted.
    pub fn overview(&self) -> Vec<OverviewEntry> {
        let registry = self.inner.registry();
        let mut entries: Vec<OverviewEntry> = registry
            .chains
            .iter()
            .map(|entry| OverviewEntry {
                name: entry.name.clone(),
                kind: JobKind::Chain,
                job: entry.job,
                run: None,
                chain: Some(entry.id),
                step: None,
                status: None,
                chain_status: Some(entry.status.borrow().clone()),
            })
            .collect();
        entries.extend(registry.runs.iter().map(|entry| OverviewEntry {
            name: entry.name.clone(),
            kind: entry.kind,
            job: entry.job,
            run: Some(entry.id),
            chain: entry.chain.map(|(chain, _)| chain),
            step: entry.chain.map(|(_, step)| step),
            status: Some(entry.status.borrow().clone()),
            chain_status: None,
        }));
        entries
    }

    /// The names of the runs that hold a limit, for "which run holds it".
    pub fn holders(&self, blocked: &Blocked) -> Vec<String> {
        let tickets = match blocked {
            Blocked::Global { holders, .. } | Blocked::Connection { holders, .. } => holders,
        };
        let registry = self.inner.registry();
        tickets
            .iter()
            .filter_map(|ticket| {
                registry
                    .runs
                    .iter()
                    .find(|entry| entry.ticket == *ticket)
                    .map(|entry| entry.name.clone())
            })
            .collect()
    }

    /// Runs and chains that are active or queued.
    pub fn active(&self) -> usize {
        let registry = self.inner.registry();
        registry.chains.len()
            + registry
                .runs
                .iter()
                .filter(|entry| entry.chain.is_none())
                .count()
    }

    /// Refuses new runs, empties the queue, stops the active runs and waits
    /// until each has closed its session or its program and written its
    /// terminal state (section 5.2).
    pub async fn shutdown(&self) {
        // A second call waits for the first, so the lock is never released
        // while runs are still stopping.
        let _closing = self.inner.closing.lock().await;
        let tasks: Vec<JoinHandle<()>> = {
            let mut registry = self.inner.registry();
            registry.shutting_down = true;
            // The scheduler stops first: nothing new starts while closing.
            self.inner.schedules.wake();
            for ticket in registry.admission.drain_queue() {
                registry.waiters.remove(&ticket);
            }
            registry.queued.clear();
            for chain in &registry.chains {
                chain.stop.cancel();
            }
            let mut tasks: Vec<JoinHandle<()>> = registry
                .chains
                .iter_mut()
                .filter_map(|entry| entry.task.take())
                .collect();
            for entry in registry.runs.iter_mut() {
                entry.stop.cancel();
                if let Some(task) = entry.task.take() {
                    tasks.push(task);
                }
            }
            tasks
        };
        for task in tasks {
            if let Err(error) = task.await {
                // Owners do not panic; the panic hook recorded one if so.
                tracing::error!(panicked = error.is_panic(), "automation run task failed");
            }
        }
        // Nothing runs any more: another instance may take over.
        *lock(&self.inner.lock) = None;
    }
}

impl Inner {
    /// The registry, even after a panic elsewhere left the lock poisoned:
    /// every update leaves it consistent.
    fn registry(&self) -> MutexGuard<'_, Registry> {
        lock(&self.registry)
    }

    pub(super) fn shutting_down(&self) -> bool {
        self.registry().shutting_down
    }

    pub(super) fn changed(&self) {
        self.changes
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    /// Asks admission for a slot for a new ticket.
    fn request(
        self: &Arc<Self>,
        registry: &mut Registry,
        connection: Option<ConnectionId>,
        notify: Notify,
    ) -> Result<Ticket, Refusal> {
        registry.next_ticket += 1;
        let ticket = registry.next_ticket;
        match registry
            .admission
            .request(ticket, connection, Instant::now())
        {
            Decision::Admit => Ok(Ticket::Admitted(Slot {
                inner: self.clone(),
                ticket,
            })),
            Decision::Queued { .. } => {
                let (sender, receiver) = oneshot::channel();
                registry.waiters.insert(ticket, sender);
                registry.queued.insert(ticket, notify);
                self.refresh_queue(registry);
                Ok(Ticket::Queued(ticket, receiver))
            }
            Decision::Refused(full) => Err(Refusal::QueueFull(full)),
        }
    }

    /// Ends `ticket`'s slot and starts the waiting runs that can start.
    fn release(&self, ticket: u64) {
        let mut registry = self.registry();
        let mut started = registry.admission.release(ticket);
        while let Some(next) = started.pop() {
            registry.queued.remove(&next);
            let delivered = registry
                .waiters
                .remove(&next)
                .is_some_and(|waiter| waiter.send(()).is_ok());
            if !delivered {
                // Its waiter is gone: give the slot back.
                started.extend(registry.admission.release(next));
            }
        }
        self.refresh_queue(&mut registry);
        drop(registry);
        self.changed();
    }

    /// Tells every queued run its place and what holds it.
    fn refresh_queue(&self, registry: &mut Registry) {
        for (ticket, notify) in &registry.queued {
            let Some((position, leaves_at)) = registry.admission.position(*ticket) else {
                continue;
            };
            let Some(blocked) = registry.admission.blocked_by(*ticket) else {
                continue;
            };
            match notify {
                Notify::Run(sender) => {
                    sender.send_replace(RunStatus::Queued {
                        position,
                        leaves_at,
                        blocked,
                    });
                }
            }
        }
    }

    fn forget_run(&self, id: RunId) {
        self.registry().runs.retain(|entry| entry.id != id);
    }

    fn forget_chain(&self, id: ChainRunId) {
        self.registry().chains.retain(|entry| entry.id != id);
    }
}

/// The entrypoint a version of `body` pins: its resolved path and SHA-256
/// now (Q-16).
async fn pin(mut body: definition::ProcessBody) -> Result<PinnedFile, PublishError> {
    body.pinned = None;
    let prepared = tokio::task::spawn_blocking(move || {
        let prepared = process::prepare(&body, std::env::vars_os());
        (body, prepared)
    })
    .await;
    let (body, prepared) = prepared.map_err(|_| PublishError::Program(None))?;
    match prepared.map_err(|refused| PublishError::Program(Some(refused)))? {
        process::Prepared::Ready(launch) => match launch.fingerprint {
            Some(sha256) => Ok(PinnedFile {
                path: launch.entrypoint.to_string_lossy().into_owned(),
                sha256,
            }),
            None => Err(PublishError::Program(Some(
                process::Refused::EntrypointUnreadable {
                    path: launch.entrypoint,
                },
            ))),
        },
        process::Prepared::CannotStart { .. } => Err(PublishError::Program(Some(
            process::Refused::EntrypointUnreadable {
                path: PathBuf::from(body.entrypoint.unwrap_or(body.program)),
            },
        ))),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The cause a refusal gives a chain step that could not run.
pub(super) fn chain_cause(refusal: &Refusal) -> Cause {
    match refusal {
        Refusal::QueueTimeout => Cause::QueueTimeout,
        Refusal::QueueFull(_) => Cause::QueueFull,
        Refusal::Inputs(_) | Refusal::Invalid(_) => Cause::Input,
        // The version's file changed since the plan was confirmed.
        Refusal::Program(process::Refused::ProgramChanged { .. }) => Cause::PlanChanged,
        Refusal::Program(process::Refused::EntrypointUnreadable { .. }) => Cause::NotFound,
        Refusal::Program(_) => Cause::Validation,
        Refusal::NoConnection => Cause::Connection,
        _ => Cause::Internal,
    }
}

impl From<&InputProblem> for Refusal {
    fn from(problem: &InputProblem) -> Self {
        Refusal::Inputs(vec![problem.clone()])
    }
}
