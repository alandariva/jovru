//! Admission, one owner task per run, Stop and the deadline (specification,
//! sections 4.8, 5.2–5.6).
use super::executor::{self, Execution, PrepareError, Prepared};
use super::{Cause, Phase, RunId, RunOutcome, RunState, RunStatus, RunSummary, SqlJob};
use crate::result::DEFAULT_MEMORY_CEILING;
use jykre_core::{CancellationToken, ConnectionId, Driver, ResolvedConnection};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How long a stopped run waits for its driver to bring the session back
/// to a known state and close it: the drivers' recovery window (5 s) plus
/// the close's own deadline (5 s), with a margin. Past it the work is
/// dropped, which drops the session.
const SETTLE: Duration = Duration::from_secs(11);

/// Finished runs kept in memory, newest first; older ones are forgotten.
const HISTORY: usize = 256;

/// How many runs may be active at once (section 5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// In the whole application.
    pub global: usize,
    /// Per connection profile. Two profiles that open the same SQLite file
    /// are not serialized.
    pub per_connection: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            global: 2,
            per_connection: 1,
        }
    }
}

/// Why a run was not admitted. Nothing ran and no session opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The job cannot run as written or with these values.
    Invalid(PrepareError),
    /// `limit` runs are already active.
    GlobalLimit { limit: usize },
    /// The connection already has as many active runs as it may.
    ConnectionBusy { connection: ConnectionId },
    /// The engine is shutting down.
    ShuttingDown,
}

/// One run to admit. The view resolves the password and the production
/// confirmation before submitting it (section 4.5); the engine opens no
/// dialog.
pub struct RunRequest {
    pub job: SqlJob,
    pub driver: Arc<dyn Driver>,
    pub connection: ResolvedConnection,
    /// What the form holds for each parameter name.
    pub values: HashMap<String, String>,
}

/// A subscription to one run. Dropping it never stops the run; only
/// [`Engine::stop`] does.
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

    /// Waits for the run's outcome. `None` only if its owner was dropped
    /// without one, which the engine never does while its runtime lives.
    pub async fn finished(&mut self) -> Option<Arc<RunOutcome>> {
        let status = self
            .status
            .wait_for(|status| matches!(status, RunStatus::Finished(_)))
            .await
            .ok()?;
        match &*status {
            RunStatus::Finished(outcome) => Some(outcome.clone()),
            RunStatus::Active { .. } => None,
        }
    }

    /// Waits for the next change of status; `false` once nothing can change
    /// any more.
    pub async fn changed(&mut self) -> bool {
        self.status.changed().await.is_ok()
    }
}

/// The automation service. Cloning shares it. Runs belong to the engine,
/// not to whoever submitted them: dropping every handle, or the engine
/// itself, leaves active runs going to their end. [`Engine::shutdown`]
/// stops them all and waits.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    runtime: Handle,
    limits: Limits,
    memory_ceiling: usize,
    registry: Mutex<Registry>,
}

#[derive(Default)]
struct Registry {
    shutting_down: bool,
    active: Vec<Active>,
    history: VecDeque<RunSummary>,
}

struct Active {
    summary: RunSummary,
    stop: CancellationToken,
    status: watch::Receiver<RunStatus>,
    task: Option<JoinHandle<()>>,
}

impl Engine {
    /// An engine with the default limits, whose runs are tasks of `runtime`
    /// (the application's only runtime).
    pub fn new(runtime: Handle) -> Self {
        Self::with_limits(runtime, Limits::default(), DEFAULT_MEMORY_CEILING)
    }

    pub fn with_limits(runtime: Handle, limits: Limits, memory_ceiling: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                runtime,
                limits,
                memory_ceiling,
                registry: Mutex::new(Registry::default()),
            }),
        }
    }

    /// Checks the job, admits it within the limits and starts its owner.
    /// Refuses at once, with the reason, instead of queueing.
    pub fn submit(&self, request: RunRequest) -> Result<RunHandle, Refusal> {
        let RunRequest {
            job,
            driver,
            connection: input,
            values,
        } = request;
        let prepared = executor::prepare(&job, driver.descriptor().dialect, &values)
            .map_err(Refusal::Invalid)?;
        drop(values);
        let mut registry = self.inner.registry();
        if registry.shutting_down {
            return Err(Refusal::ShuttingDown);
        }
        if registry.active.len() >= self.inner.limits.global {
            return Err(Refusal::GlobalLimit {
                limit: self.inner.limits.global,
            });
        }
        let on_connection = registry
            .active
            .iter()
            .filter(|active| active.summary.connection == job.connection)
            .count();
        if on_connection >= self.inner.limits.per_connection {
            return Err(Refusal::ConnectionBusy {
                connection: job.connection,
            });
        }
        let id = RunId::new();
        let started_at = SystemTime::now();
        let (sender, status) = watch::channel(RunStatus::Active {
            phase: Phase::Admitted,
            rows: 0,
        });
        let stop = CancellationToken::new();
        let owner = Owner {
            inner: self.inner.clone(),
            id,
            connection: job.connection,
            prepared,
            driver,
            input,
            stop: stop.clone(),
            status: sender,
            started_at,
        };
        // Spawned under the lock, so the owner finds its entry when it ends.
        let task = self.inner.runtime.spawn(owner.run());
        registry.active.push(Active {
            summary: RunSummary {
                id,
                connection: job.connection,
                state: RunState::Running,
                cause: None,
                effect_unknown: false,
                truncated: false,
                rows_delivered: 0,
                rows_affected: None,
                server_code: None,
                started_at,
                elapsed: None,
            },
            stop,
            status: status.clone(),
            task: Some(task),
        });
        tracing::info!(run = %id, connection = %job.connection, "automation run admitted");
        Ok(RunHandle { id, status })
    }

    /// Stops an active run at once; `false` when it is not active.
    pub fn stop(&self, id: RunId) -> bool {
        let registry = self.inner.registry();
        match registry
            .active
            .iter()
            .find(|active| active.summary.id == id)
        {
            Some(active) => {
                active.stop.cancel();
                true
            }
            None => false,
        }
    }

    /// A new subscription to an active run.
    pub fn subscribe(&self, id: RunId) -> Option<RunHandle> {
        let registry = self.inner.registry();
        registry
            .active
            .iter()
            .find(|active| active.summary.id == id)
            .map(|active| RunHandle {
                id,
                status: active.status.clone(),
            })
    }

    /// Active runs, then finished ones, newest first.
    pub fn runs(&self) -> Vec<RunSummary> {
        let registry = self.inner.registry();
        registry
            .active
            .iter()
            .rev()
            .map(|active| active.summary.clone())
            .chain(registry.history.iter().cloned())
            .collect()
    }

    pub fn active(&self) -> usize {
        self.inner.registry().active.len()
    }

    /// Refuses new runs, stops the active ones and waits until each has
    /// closed its session and published its outcome (section 5.2).
    pub async fn shutdown(&self) {
        let tasks: Vec<JoinHandle<()>> = {
            let mut registry = self.inner.registry();
            registry.shutting_down = true;
            registry
                .active
                .iter_mut()
                .filter_map(|active| {
                    active.stop.cancel();
                    active.task.take()
                })
                .collect()
        };
        for task in tasks {
            if let Err(error) = task.await {
                // Owners do not panic; the panic hook recorded one if so.
                tracing::error!(panicked = error.is_panic(), "automation run task failed");
            }
        }
    }
}

impl Inner {
    /// The registry, even after a panic elsewhere left the lock poisoned:
    /// every update leaves it consistent, so the state is still good.
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What came first, in the owner's eyes.
enum Decision {
    Stopped,
    Deadline,
}

/// The one task that executes a run and the only writer of its outcome.
struct Owner {
    inner: Arc<Inner>,
    id: RunId,
    connection: ConnectionId,
    prepared: Prepared,
    driver: Arc<dyn Driver>,
    input: ResolvedConnection,
    stop: CancellationToken,
    status: watch::Sender<RunStatus>,
    started_at: SystemTime,
}

impl Owner {
    async fn run(self) {
        let started = Instant::now();
        let deadline = started + self.prepared.timeout;
        let work = CancellationToken::new();
        let outcome = self.execute(started, deadline, work).await;
        self.finish(outcome);
    }

    /// Runs the executor until it ends or the first of Stop and the
    /// deadline, and decides the outcome.
    async fn execute(
        &self,
        started: Instant,
        deadline: Instant,
        work: CancellationToken,
    ) -> RunOutcome {
        let execution = executor::execute(
            self.driver.clone(),
            self.input.clone(),
            &self.prepared,
            self.inner.memory_ceiling,
            work.clone(),
            deadline,
            &self.status,
        );
        tokio::pin!(execution);
        // The first event observed decides; the others are ignored. A
        // completion already confirmed wins over a Stop or a deadline that
        // became ready in the same instant, since it is what happened.
        let decided = tokio::select! { biased;
            done = &mut execution => Err(done),
            () = self.stop.cancelled() => Ok(Decision::Stopped),
            () = tokio::time::sleep_until(deadline.into()) => Ok(Decision::Deadline),
        };
        match decided {
            Err(done) => self.completed(done, started),
            Ok(decision) => {
                let sent_before = matches!(
                    &*self.status.borrow(),
                    RunStatus::Active {
                        phase: Phase::Executing | Phase::Completing,
                        ..
                    }
                );
                self.status.send_replace(RunStatus::Active {
                    phase: Phase::Cancelling,
                    rows: 0,
                });
                work.cancel();
                let settled = tokio::time::timeout(SETTLE, &mut execution).await;
                if settled.is_err() {
                    tracing::warn!(run = %self.id, "automation run did not settle in time; its session was dropped");
                }
                self.stopped(decision, settled.ok(), sent_before, started)
            }
        }
    }

    /// The executor ended before any Stop or deadline.
    fn completed(&self, done: Execution, started: Instant) -> RunOutcome {
        let cause = done.error.as_ref().map(Cause::of);
        let state = if done.confirmed {
            RunState::Succeeded
        } else {
            RunState::Failed
        };
        let effect_unknown = state == RunState::Failed
            && done.sent
            && !self.prepared.reads_only()
            && cause.is_none_or(Cause::leaves_effect_unknown);
        self.outcome(
            state,
            if state == RunState::Failed {
                Some(cause.unwrap_or(Cause::Internal))
            } else {
                None
            },
            effect_unknown,
            Some(done),
            started,
        )
    }

    /// Stop or the deadline came first. Whether a write finished meanwhile
    /// is not something the run claims: once sent, its effect is unknown.
    fn stopped(
        &self,
        decision: Decision,
        done: Option<Execution>,
        sent_before: bool,
        started: Instant,
    ) -> RunOutcome {
        let sent = done.as_ref().map_or(sent_before, |done| done.sent);
        let effect_unknown = sent && !self.prepared.reads_only();
        let (state, cause) = match decision {
            Decision::Stopped => (RunState::Cancelled, None),
            Decision::Deadline => (RunState::Failed, Some(Cause::Timeout)),
        };
        let done = done.map(|mut done| {
            // The driver's error only says it was interrupted, which the
            // state already says.
            done.error = None;
            done
        });
        self.outcome(state, cause, effect_unknown, done, started)
    }

    fn outcome(
        &self,
        state: RunState,
        cause: Option<Cause>,
        effect_unknown: bool,
        done: Option<Execution>,
        started: Instant,
    ) -> RunOutcome {
        let done = done.unwrap_or_default();
        RunOutcome {
            id: self.id,
            state,
            cause,
            error: done.error,
            effect_unknown,
            truncated: done.truncated,
            rows_discarded: done.rows_discarded,
            rows_affected: done.rows_affected,
            data: done.data,
            transaction_rolled_back: done.transaction_rolled_back,
            started_at: self.started_at,
            elapsed: started.elapsed(),
            completion: done.completion,
        }
    }

    /// Records the outcome, then publishes it: whoever sees it finds the
    /// run in the history already.
    fn finish(self, outcome: RunOutcome) {
        let summary = RunSummary::finished(self.connection, &outcome);
        tracing::info!(
            run = %self.id,
            state = ?summary.state,
            cause = ?summary.cause,
            effect_unknown = summary.effect_unknown,
            truncated = summary.truncated,
            rows = summary.rows_delivered,
            discarded = outcome.rows_discarded,
            elapsed_ms = outcome.elapsed.as_millis() as u64,
            "automation run finished"
        );
        {
            let mut registry = self.inner.registry();
            registry
                .active
                .retain(|active| active.summary.id != self.id);
            registry.history.push_front(summary);
            registry.history.truncate(HISTORY);
        }
        self.status
            .send_replace(RunStatus::Finished(Arc::new(outcome)));
    }
}
