//! The scheduler with a manual clock (specification, section 9.5.10): time
//! moves without waiting; the tests wait only for the scheduler's passes and
//! for runs to end.
use super::*;
use crate::automation::schedule::rule::Rule;
use crate::automation::schedule::{
    Clock, ManualClock, ScheduleConnection, ScheduleError, missing_defaults,
};
use crate::connection_appearance::Environment;
use jykre_core::automation::{MissedPolicy, OverlapPolicy, ScheduleId, ScheduleOutcome};
use jykre_storage::{ScheduleDraft, StoredSchedule};
use secrecy::SecretString;

const MINUTE: Duration = Duration::from_secs(60);

fn at(text: &str) -> UtcMillis {
    UtcMillis(text.parse::<jiff::Timestamp>().unwrap().as_millisecond())
}

/// 2026-11-09 08:00 UTC.
fn t0() -> UtcMillis {
    at("2026-11-09T08:00:00Z")
}

struct Bench {
    engine: Engine,
    clock: ManualClock,
    connection: ConnectionId,
    driver: Arc<Scripted>,
}

fn scheduled_input(
    driver: &Arc<Scripted>,
    id: ConnectionId,
    revision: i64,
    password: bool,
) -> ConnectionInput {
    ConnectionInput {
        id,
        driver: driver.clone(),
        resolved: ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({}),
            },
            credentials: Credentials {
                password: password.then(|| SecretString::from("synthetic-test-password")),
            },
        },
        revision,
    }
}

impl Bench {
    async fn new(script: Script, limits: Limits) -> Self {
        Self::in_dir(data_dir(), script, limits, ManualClock::new(t0())).await
    }

    async fn in_dir(dir: PathBuf, script: Script, limits: Limits, clock: ManualClock) -> Self {
        let engine = Engine::with_clock(
            Handle::current(),
            dir,
            limits,
            crate::result::DEFAULT_MEMORY_CEILING,
            process::Timings::default(),
            Clock::Manual(clock.clone()),
        );
        engine.started().await;
        let bench = Self {
            engine,
            clock,
            connection: ConnectionId::new(),
            driver: Scripted::new(script),
        };
        bench.connections(1, None, true);
        bench
    }

    /// What the application root would hand over.
    fn connections(&self, revision: i64, environment: Option<Environment>, password: bool) {
        self.engine.set_connections(vec![ScheduleConnection {
            input: scheduled_input(&self.driver, self.connection, revision, password),
            name: "warehouse".into(),
            environment,
        }]);
    }

    /// A published SQL Job on the bench's connection.
    async fn job(&self) -> (JobId, i64) {
        let mut job = sql_job(self.connection, "UPDATE t SET v = 1");
        job.name = "nightly".into();
        saved(&self.engine, &job).await
    }

    /// Every 15 minutes from 08:15, under these policies, consented to.
    async fn schedule(
        &self,
        job: (JobId, i64),
        missed: MissedPolicy,
        limit: Option<i64>,
        overlap: OverlapPolicy,
    ) -> ScheduleId {
        let draft = ScheduleDraft {
            id: ScheduleId::new(),
            job_id: job.0,
            job_version: Some(job.1),
            rule_json: Rule::Interval {
                every_minutes: 15,
                anchor: t0().plus(15 * MINUTE),
            }
            .to_json(),
            time_zone: "Europe/Berlin".into(),
            missed_policy: missed,
            missed_limit: limit,
            overlap_policy: overlap,
            notify: false,
        };
        let saved = self.engine.save_schedule(&draft, None).await.unwrap();
        assert!(!saved.enabled, "nothing runs before consent");
        self.confirm(draft.id).await;
        draft.id
    }

    async fn confirm(&self, schedule: ScheduleId) -> StoredSchedule {
        let consent = self.engine.consent(schedule).await.unwrap();
        self.engine.confirm_schedule(&consent).await.unwrap()
    }

    /// Moves the clock and waits for a pass that saw the new time.
    async fn set(&self, time: UtcMillis) {
        let mut passes = self.engine.watch_schedule_passes();
        let before = passes.borrow_and_update().count;
        self.clock.set(time);
        let waited = tokio::time::timeout(
            Duration::from_secs(10),
            passes.wait_for(|pass| pass.count > before && pass.at == time),
        )
        .await;
        assert!(matches!(waited, Ok(Ok(_))), "no pass at {time:?}");
    }

    async fn stored(&self, schedule: ScheduleId) -> StoredSchedule {
        self.engine
            .repository()
            .unwrap()
            .schedule(schedule)
            .await
            .unwrap()
            .unwrap()
    }

    /// The runs of the job, oldest first.
    async fn runs(&self, job: JobId) -> Vec<RunRecord> {
        let mut runs: Vec<RunRecord> = self
            .engine
            .repository()
            .unwrap()
            .history(&HistoryFilter {
                job: Some(job),
                ..HistoryFilter::default()
            })
            .await
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry {
                HistoryEntry::Run(run) => Some(*run),
                HistoryEntry::Chain(_) => None,
            })
            .collect();
        runs.sort_by_key(|run| run.due_at);
        runs
    }

    async fn wait_runs(&self, job: JobId, count: usize) -> Vec<RunRecord> {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let runs = self.runs(job).await;
            if runs.len() >= count {
                return runs;
            }
            assert!(Instant::now() < until, "{} runs, not {count}", runs.len());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn wait_outcome(&self, schedule: ScheduleId, outcome: ScheduleOutcome) -> StoredSchedule {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let stored = self.stored(schedule).await;
            if stored.last_outcome == Some(outcome) {
                return stored;
            }
            assert!(
                Instant::now() < until,
                "outcome {:?}, not {outcome:?}",
                stored.last_outcome
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn wait_suspended(&self, schedule: ScheduleId) -> StoredSchedule {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let stored = self.stored(schedule).await;
            if stored.suspended.is_some() {
                return stored;
            }
            assert!(Instant::now() < until, "not suspended");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The schedule's run that is queued or active.
    fn active(&self) -> Vec<OverviewEntry> {
        self.engine.overview()
    }

    async fn idle(&self) {
        eventually(Duration::from_secs(10), || self.engine.active() == 0).await;
    }
}

// ---------------------------------------------------------------------------
// On time, history and the clock.

#[tokio::test]
async fn an_on_time_due_time_runs_once_recorded_as_scheduled() {
    let bench = Bench::new(Script::Rows(Vec::new(), Vec::new()), Limits::default()).await;
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::Skip)
        .await;
    // Nothing before its time.
    bench.set(t0().plus(14 * MINUTE)).await;
    assert!(bench.runs(job.0).await.is_empty());
    let due = t0().plus(15 * MINUTE);
    bench.set(due.plus(Duration::from_secs(10))).await;
    let runs = bench.wait_runs(job.0, 1).await;
    let run = &runs[0];
    assert_eq!(run.origin, RunOrigin::Schedule);
    assert_eq!((run.schedule_id, run.due_at), (Some(schedule), Some(due)));
    assert!(!run.catch_up);
    assert_eq!(run.job_version, Some(job.1));
    bench.idle().await;
    assert_eq!(bench.runs(job.0).await[0].state, RunState::Succeeded);
    let stored = bench.stored(schedule).await;
    assert_eq!(stored.last_outcome, Some(ScheduleOutcome::Started));
    assert_eq!(
        (stored.last_due_at, stored.last_run_id),
        (Some(due), Some(run.id))
    );
    assert_eq!(stored.missed_count, 0);

    // A wall clock set back repeats nothing.
    bench.set(t0().plus(5 * MINUTE)).await;
    bench.set(due.plus(Duration::from_secs(20))).await;
    assert_eq!(bench.runs(job.0).await.len(), 1);
    // A manual confirmation creates no grant, and turning off revokes.
    bench.engine.disable_schedule(schedule).await.unwrap();
    assert!(
        bench
            .engine
            .repository()
            .unwrap()
            .grant(schedule)
            .await
            .unwrap()
            .is_none()
    );
    bench.set(t0().plus(30 * MINUTE)).await;
    assert_eq!(
        bench.runs(job.0).await.len(),
        1,
        "an off schedule runs nothing"
    );
}

#[tokio::test]
async fn a_jump_past_the_grace_is_missed_and_the_next_time_is_ahead() {
    let bench = Bench::new(Script::Rows(Vec::new(), Vec::new()), Limits::default()).await;
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::Skip)
        .await;
    // Closed (or asleep) over 08:15, 08:30 and 08:45; awake at 08:46:30.
    bench
        .set(t0().plus(46 * MINUTE + Duration::from_secs(30)))
        .await;
    let stored = bench.wait_outcome(schedule, ScheduleOutcome::Missed).await;
    assert_eq!(stored.missed_count, 3);
    assert_eq!(stored.missed_last_at, Some(t0().plus(45 * MINUTE)));
    assert!(
        bench.runs(job.0).await.is_empty(),
        "ignore runs none of them"
    );
    // The next one runs on time.
    bench.set(t0().plus(60 * MINUTE)).await;
    let runs = bench.wait_runs(job.0, 1).await;
    assert_eq!(runs[0].due_at, Some(t0().plus(60 * MINUTE)));
    assert!(!runs[0].catch_up);
}

// ---------------------------------------------------------------------------
// Missed-time policies (Q-34).

#[tokio::test]
async fn run_once_submits_one_catch_up_for_the_latest_missed_time() {
    let bench = Bench::new(Script::Rows(Vec::new(), Vec::new()), Limits::default()).await;
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::RunOnce, None, OverlapPolicy::Skip)
        .await;
    bench.set(t0().plus(50 * MINUTE)).await;
    let runs = bench.wait_runs(job.0, 1).await;
    bench.idle().await;
    let runs_after = bench.runs(job.0).await;
    assert_eq!(runs_after.len(), 1, "one run for all of them");
    assert_eq!(runs[0].due_at, Some(t0().plus(45 * MINUTE)));
    assert!(runs[0].catch_up);
    assert_eq!(runs[0].origin, RunOrigin::Schedule);
    let stored = bench.stored(schedule).await;
    assert_eq!(stored.missed_count, 3);
    assert_eq!(stored.last_outcome, Some(ScheduleOutcome::CaughtUp));
}

#[tokio::test]
async fn queue_up_to_n_catches_up_the_most_recent_one_at_a_time() {
    let release = Arc::new(Notify::new());
    let bench = Bench::new(Script::WaitsFor(release.clone()), Limits::default()).await;
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::Queue, Some(2), OverlapPolicy::Skip)
        .await;
    // 08:15, 08:30 and 08:45 missed: the two most recent are caught up.
    bench.set(t0().plus(50 * MINUTE)).await;
    let runs = bench.wait_runs(job.0, 1).await;
    assert_eq!(runs[0].due_at, Some(t0().plus(30 * MINUTE)), "oldest first");
    assert!(runs[0].catch_up);
    // One at a time: the second waits for the first to end.
    bench.set(t0().plus(51 * MINUTE)).await;
    assert_eq!(bench.runs(job.0).await.len(), 1);
    // A normal due time meanwhile follows the overlap policy: skipped.
    bench.set(t0().plus(60 * MINUTE)).await;
    bench
        .wait_outcome(schedule, ScheduleOutcome::SkippedRunning)
        .await;
    assert_eq!(bench.runs(job.0).await.len(), 1);
    release.notify_one();
    let runs = bench.wait_runs(job.0, 2).await;
    assert_eq!(runs[1].due_at, Some(t0().plus(45 * MINUTE)));
    assert!(runs[1].catch_up);
    release.notify_one();
    bench.idle().await;
    bench.set(t0().plus(61 * MINUTE)).await;
    assert_eq!(bench.runs(job.0).await.len(), 2, "08:15 was only counted");
    assert_eq!(bench.stored(schedule).await.missed_count, 3);
}

// ---------------------------------------------------------------------------
// Overlap policies (Q-35).

#[tokio::test]
async fn overlap_skip_records_and_never_starts_a_second_run() {
    let bench = Bench::new(Script::Blocks, Limits::default()).await;
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::Skip)
        .await;
    bench.set(t0().plus(15 * MINUTE)).await;
    let runs = bench.wait_runs(job.0, 1).await;
    bench.set(t0().plus(30 * MINUTE)).await;
    let stored = bench
        .wait_outcome(schedule, ScheduleOutcome::SkippedRunning)
        .await;
    assert_eq!(stored.last_due_at, Some(t0().plus(30 * MINUTE)));
    assert_eq!(bench.runs(job.0).await.len(), 1);
    assert_eq!(bench.active().len(), 1);
    bench.engine.stop_run(runs[0].id);
    bench.idle().await;
}

#[tokio::test]
async fn overlap_queue_one_waits_for_the_run_and_coalesces_the_rest() {
    let release = Arc::new(Notify::new());
    let bench = Bench::new(Script::WaitsFor(release.clone()), Limits::default()).await;
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::QueueOne)
        .await;
    bench.set(t0().plus(15 * MINUTE)).await;
    bench.wait_runs(job.0, 1).await;
    bench.set(t0().plus(30 * MINUTE)).await;
    bench.wait_outcome(schedule, ScheduleOutcome::Waiting).await;
    bench.set(t0().plus(45 * MINUTE)).await;
    let coalesced = bench
        .wait_outcome(schedule, ScheduleOutcome::SkippedRunning)
        .await;
    assert_eq!(coalesced.last_reason.as_deref(), Some("coalesced"));
    assert_eq!(bench.runs(job.0).await.len(), 1, "at most one waits");
    release.notify_one();
    let runs = bench.wait_runs(job.0, 2).await;
    assert_eq!(runs[1].due_at, Some(t0().plus(30 * MINUTE)));
    assert!(!runs[1].catch_up);
    release.notify_one();
    bench.idle().await;
    assert_eq!(bench.runs(job.0).await.len(), 2);
}

#[tokio::test]
async fn overlap_concurrent_runs_beside_under_the_connection_limit() {
    let bench = Bench::new(Script::Blocks, Limits::default()).await;
    let job = bench.job().await;
    bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::Concurrent)
        .await;
    bench.set(t0().plus(15 * MINUTE)).await;
    bench.wait_runs(job.0, 1).await;
    bench.set(t0().plus(30 * MINUTE)).await;
    // Submitted at once, it waits in the queue: one run per connection.
    eventually(Duration::from_secs(10), || {
        bench
            .active()
            .iter()
            .any(|entry| matches!(entry.status, Some(RunStatus::Queued { .. })))
    })
    .await;
    assert_eq!(bench.active().len(), 2);
    for entry in bench.active() {
        bench.engine.stop_run(entry.run.unwrap());
    }
    bench.idle().await;
}

// ---------------------------------------------------------------------------
// Queue, limits, consent and dialogs.

#[tokio::test]
async fn a_full_queue_records_refused_without_a_run_row() {
    let limits = Limits {
        global: 1,
        queue_global: 0,
        ..Limits::default()
    };
    let bench = Bench::new(Script::Blocks, limits).await;
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::Skip)
        .await;
    let mut news = bench.engine.watch_schedule_news();
    // A manual run holds the only slot.
    let mut manual = bench
        .engine
        .submit(scripted_run(
            &bench.driver,
            sql_job(ConnectionId::new(), "SELECT 1"),
        ))
        .unwrap();
    bench.set(t0().plus(15 * MINUTE)).await;
    let stored = bench.wait_outcome(schedule, ScheduleOutcome::Refused).await;
    assert_eq!(stored.last_reason.as_deref(), Some("queue_full"));
    assert!(bench.runs(job.0).await.is_empty(), "no run row");
    let latest = news.borrow_and_update().latest.clone().unwrap();
    assert_eq!(
        (latest.schedule, latest.refused),
        (schedule, Some("queue_full"))
    );
    bench.engine.stop_run(manual.id);
    manual.finished().await;
}

#[tokio::test]
async fn a_scheduled_run_behind_a_manual_one_waits_and_turning_off_withdraws_it() {
    let limits = Limits {
        global: 1,
        ..Limits::default()
    };
    let bench = Bench::new(Script::Blocks, limits).await;
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::Skip)
        .await;
    let mut manual = bench
        .engine
        .submit(scripted_run(
            &bench.driver,
            sql_job(ConnectionId::new(), "SELECT 1"),
        ))
        .unwrap();
    bench.set(t0().plus(15 * MINUTE)).await;
    eventually(Duration::from_secs(10), || {
        bench
            .active()
            .iter()
            .any(|entry| matches!(entry.status, Some(RunStatus::Queued { .. })))
    })
    .await;
    // Revoking removes it from the queue, as Stop does: it never ran.
    bench.engine.disable_schedule(schedule).await.unwrap();
    eventually(Duration::from_secs(10), || bench.active().len() == 1).await;
    assert!(bench.runs(job.0).await.is_empty());
    bench.engine.stop_run(manual.id);
    manual.finished().await;
}

#[tokio::test]
async fn a_password_not_remembered_fails_with_authentication_and_sends_nothing() {
    let bench = Bench::new(Script::Rows(Vec::new(), Vec::new()), Limits::default()).await;
    bench.connections(1, None, false);
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::Skip)
        .await;
    let consent = bench.engine.consent(schedule).await.unwrap();
    assert!(
        !consent.connections[0].remembers_password,
        "the dialog warns"
    );
    bench.set(t0().plus(15 * MINUTE)).await;
    bench.wait_runs(job.0, 1).await;
    bench.idle().await;
    let run = &bench.runs(job.0).await[0];
    assert_eq!(
        (run.state, run.cause, run.effect_unknown),
        (RunState::Failed, Some(Cause::Authentication), false)
    );
    assert_eq!(
        bench.driver.opened.load(Ordering::SeqCst),
        0,
        "nothing sent"
    );
}

#[tokio::test]
async fn each_binding_suspends_and_confirm_again_restores() {
    let bench = Bench::new(Script::Rows(Vec::new(), Vec::new()), Limits::default()).await;
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::Skip)
        .await;
    let mut minutes = 15;
    for (change, reason) in [
        ("revision", "connection_changed"),
        ("environment", "environment_changed"),
        ("deleted", "connection_deleted"),
    ] {
        match change {
            "revision" => bench.connections(2, None, true),
            "environment" => bench.connections(2, Some(Environment::Production), true),
            _ => bench.engine.set_connections(Vec::new()),
        }
        bench.set(t0().plus(minutes * MINUTE)).await;
        let stored = bench.wait_suspended(schedule).await;
        assert_eq!(stored.suspended.as_deref(), Some(reason));
        assert!(bench.runs(job.0).await.is_empty(), "{change}: nothing runs");
        if change == "deleted" {
            assert_eq!(
                bench.engine.consent(schedule).await.unwrap_err(),
                ScheduleError::NoConnection
            );
            bench.connections(2, Some(Environment::Production), true);
        }
        // Confirm again: what changed is named, and it runs again.
        let consent = bench.engine.consent(schedule).await.unwrap();
        assert_eq!(consent.suspended.as_deref(), Some(reason));
        if change != "deleted" {
            assert_eq!(consent.changed, vec!["warehouse".to_owned()]);
        }
        let restored = bench.engine.confirm_schedule(&consent).await.unwrap();
        assert!(restored.enabled && restored.suspended.is_none());
        minutes += 15;
    }
    let consent = bench.engine.consent(schedule).await.unwrap();
    assert!(
        consent.production,
        "the consent is the production confirmation"
    );
    // A consent shown before a change cannot be confirmed after it.
    bench.connections(3, Some(Environment::Production), true);
    assert_eq!(
        bench.engine.confirm_schedule(&consent).await.unwrap_err(),
        ScheduleError::Changed
    );
    // That change suspended it too, until confirmed again.
    let stored = bench.wait_suspended(schedule).await;
    assert_eq!(stored.suspended.as_deref(), Some("connection_changed"));
    bench.confirm(schedule).await;
    bench.set(t0().plus(minutes * MINUTE)).await;
    let runs = bench.wait_runs(job.0, 1).await;
    assert_eq!(runs[0].due_at, Some(t0().plus(minutes * MINUTE)));
}

#[tokio::test]
async fn a_bad_zone_rule_limit_or_input_is_refused_when_saved() {
    let bench = Bench::new(Script::Rows(Vec::new(), Vec::new()), Limits::default()).await;
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::Skip)
        .await;
    let stored = bench.stored(schedule).await;
    let mut draft = stored.draft.clone();
    draft.id = ScheduleId::new();
    draft.time_zone = "Mars/Olympus_Mons".into();
    assert_eq!(
        bench.engine.save_schedule(&draft, None).await.unwrap_err(),
        ScheduleError::UnknownZone
    );
    draft.time_zone = "UTC".into();
    draft.rule_json = Rule::Interval {
        every_minutes: 0,
        anchor: t0(),
    }
    .to_json();
    assert!(matches!(
        bench.engine.save_schedule(&draft, None).await.unwrap_err(),
        ScheduleError::Rule(_)
    ));
    draft.rule_json = stored.draft.rule_json.clone();
    draft.missed_policy = MissedPolicy::Queue;
    draft.missed_limit = Some(25);
    assert_eq!(
        bench.engine.save_schedule(&draft, None).await.unwrap_err(),
        ScheduleError::MissedLimit
    );
    // A required input without a default cannot be scheduled (Q-36).
    let mut needs = sql_job(bench.connection, "SELECT :day");
    body(&mut needs).parameters = vec![ParameterSpec::new("day")];
    assert_eq!(missing_defaults(needs.inputs()), vec!["day".to_owned()]);
    let needs = saved(&bench.engine, &needs).await;
    let mut draft = stored.draft.clone();
    draft.id = ScheduleId::new();
    draft.job_id = needs.0;
    draft.job_version = Some(needs.1);
    assert_eq!(
        bench.engine.save_schedule(&draft, None).await.unwrap_err(),
        ScheduleError::Inputs(vec!["day".to_owned()])
    );
    // One schedule per job.
    let mut second = stored.draft.clone();
    second.id = ScheduleId::new();
    assert_eq!(
        bench.engine.save_schedule(&second, None).await.unwrap_err(),
        ScheduleError::Taken
    );
}

#[tokio::test]
async fn a_zone_the_database_no_longer_knows_suspends_the_schedule() {
    let bench = Bench::new(Script::Rows(Vec::new(), Vec::new()), Limits::default()).await;
    let job = bench.job().await;
    let schedule = bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::Skip)
        .await;
    let consent = bench.engine.consent(schedule).await.unwrap();
    // Written behind the engine's back, as a newer zone database without
    // the name would leave the schedule: the engine refuses to save it.
    let repository = bench.engine.repository().unwrap();
    let stored = bench.stored(schedule).await;
    let mut gone = stored.draft.clone();
    gone.time_zone = "Mars/Olympus_Mons".into();
    let changed = repository
        .save_schedule(&gone, Some(stored.revision), t0())
        .await
        .unwrap()
        .unwrap();
    let mut grant = consent.grant().clone();
    grant.schedule_revision = changed.revision;
    repository
        .confirm_schedule(&grant, t0())
        .await
        .unwrap()
        .unwrap();
    bench.set(t0().plus(15 * MINUTE)).await;
    let stored = bench.wait_suspended(schedule).await;
    assert_eq!(stored.suspended.as_deref(), Some("time_zone_unknown"));
    assert!(bench.runs(job.0).await.is_empty());
}

// ---------------------------------------------------------------------------
// Chains, instances and quitting.

#[tokio::test]
async fn a_scheduled_chain_records_its_schedule_on_itself_and_its_steps_and_suspends_on_edit() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let clock = ManualClock::new(t0());
    let engine = Engine::with_clock(
        Handle::current(),
        data_dir(),
        Limits::default(),
        crate::result::DEFAULT_MEMORY_CEILING,
        process::Timings::default(),
        Clock::Manual(clock.clone()),
    );
    engine.started().await;
    engine.set_connections(vec![ScheduleConnection {
        input: file.connection(),
        name: "file".into(),
        environment: None,
    }]);
    let mut chain = lookup_then_insert(&engine, &file).await;
    if let JobBody::Chain(body) = &mut chain.body {
        body.inputs[0].default = Some("a".into());
    }
    let stored = engine.save_job(&chain, None).await.unwrap();
    let schedule = ScheduleDraft {
        id: ScheduleId::new(),
        job_id: stored.id,
        job_version: None,
        rule_json: Rule::Interval {
            every_minutes: 60,
            anchor: t0().plus(MINUTE),
        }
        .to_json(),
        time_zone: "UTC".into(),
        missed_policy: MissedPolicy::Ignore,
        missed_limit: None,
        overlap_policy: OverlapPolicy::Skip,
        notify: true,
    };
    engine.save_schedule(&schedule, None).await.unwrap();
    let consent = engine.consent(schedule.id).await.unwrap();
    assert_eq!(consent.steps.len(), 2);
    assert_eq!(
        consent.inputs,
        vec![("key".to_owned(), Some("a".to_owned()))]
    );
    engine.confirm_schedule(&consent).await.unwrap();
    let mut passes = engine.watch_schedule_passes();
    let before = passes.borrow_and_update().count;
    clock.set(t0().plus(MINUTE));
    passes
        .wait_for(|pass| pass.count > before && pass.at == t0().plus(MINUTE))
        .await
        .unwrap();
    eventually(Duration::from_secs(10), || engine.active() == 0).await;
    let history = engine
        .repository()
        .unwrap()
        .history(&HistoryFilter::default())
        .await
        .unwrap();
    let chain_run = history
        .iter()
        .find_map(|entry| match entry {
            HistoryEntry::Chain(chain) if chain.job_id == Some(stored.id) => Some(chain.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(chain_run.state, RunState::Succeeded);
    assert_eq!(chain_run.origin, RunOrigin::Schedule);
    assert_eq!(
        (chain_run.schedule_id, chain_run.due_at),
        (Some(schedule.id), Some(t0().plus(MINUTE)))
    );
    let steps = engine
        .repository()
        .unwrap()
        .chain_steps(chain_run.id)
        .await
        .unwrap();
    assert_eq!(steps.len(), 2);
    for step in &steps {
        assert_eq!(step.origin, RunOrigin::Schedule);
        assert_eq!(step.schedule_id, Some(schedule.id));
        assert_eq!(step.due_at, Some(t0().plus(MINUTE)));
    }
    // An edit of the chain is a new revision: suspended.
    let mut edited = chain.clone();
    edited.description = "edited".into();
    engine
        .save_job(&edited, Some(stored.revision))
        .await
        .unwrap();
    let before = passes.borrow_and_update().count;
    clock.set(t0().plus(2 * MINUTE));
    passes
        .wait_for(|pass| pass.count > before && pass.at == t0().plus(2 * MINUTE))
        .await
        .unwrap();
    let suspended = engine
        .repository()
        .unwrap()
        .schedule(schedule.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(suspended.suspended.as_deref(), Some("chain_changed"));
}

#[tokio::test]
async fn a_second_instance_runs_nothing_and_taking_the_lock_applies_the_policy() {
    let dir = data_dir();
    let first = Bench::in_dir(
        dir.clone(),
        Script::Rows(Vec::new(), Vec::new()),
        Limits::default(),
        ManualClock::new(t0()),
    )
    .await;
    let job = first.job().await;
    let schedule = first
        .schedule(job, MissedPolicy::RunOnce, None, OverlapPolicy::Skip)
        .await;
    let clock = ManualClock::new(t0());
    let second = Engine::with_clock(
        Handle::current(),
        dir,
        Limits::default(),
        crate::result::DEFAULT_MEMORY_CEILING,
        process::Timings::default(),
        Clock::Manual(clock.clone()),
    );
    assert_eq!(second.started().await, Availability::ReadOnly);
    second.set_connections(vec![ScheduleConnection {
        input: scheduled_input(&first.driver, first.connection, 1, true),
        name: "warehouse".into(),
        environment: None,
    }]);
    assert_eq!(
        second
            .save_schedule(&first.stored(schedule).await.draft, Some(1))
            .await,
        Err(ScheduleError::Unavailable)
    );
    // The first quits before 08:15: its scheduler stops first.
    first.engine.shutdown().await;
    first.clock.set(t0().plus(20 * MINUTE));
    clock.set(t0().plus(35 * MINUTE));
    assert!(first.runs(job.0).await.is_empty(), "the second ran nothing");
    // Taking the lock over: 08:15 and 08:30 were missed; run once.
    assert_eq!(second.retry().await, Availability::Ready);
    let until = Instant::now() + Duration::from_secs(10);
    let runs = loop {
        let runs = first.runs(job.0).await;
        if !runs.is_empty() {
            break runs;
        }
        assert!(Instant::now() < until, "no catch-up after taking the lock");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(runs[0].due_at, Some(t0().plus(30 * MINUTE)));
    assert!(runs[0].catch_up);
    eventually(Duration::from_secs(10), || second.active() == 0).await;
    second.shutdown().await;
}

#[tokio::test]
async fn quitting_stops_the_scheduler_before_the_runs() {
    let bench = Bench::new(Script::Blocks, Limits::default()).await;
    let job = bench.job().await;
    bench
        .schedule(job, MissedPolicy::RunOnce, None, OverlapPolicy::Concurrent)
        .await;
    bench.set(t0().plus(15 * MINUTE)).await;
    bench.wait_runs(job.0, 1).await;
    bench.engine.shutdown().await;
    bench.clock.set(t0().plus(90 * MINUTE));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let runs = bench.runs(job.0).await;
    assert_eq!(runs.len(), 1, "nothing new starts while closing");
    assert_eq!(runs[0].state, RunState::Cancelled);
}

#[tokio::test]
async fn a_program_whose_file_changed_suspends_its_schedule() {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let entrypoint = dir.join("entrypoint.txt");
    std::fs::write(&entrypoint, b"first").unwrap();
    let bench = Bench::in_dir(
        dir,
        Script::Rows(Vec::new(), Vec::new()),
        Limits::default(),
        ManualClock::new(t0()),
    )
    .await;
    let mut job = program_job("print:scheduled", Protocol::None, &[]);
    program_body(&mut job).entrypoint = Some(entrypoint.to_string_lossy().into_owned());
    let job = saved(&bench.engine, &job).await;
    let schedule = bench
        .schedule(job, MissedPolicy::Ignore, None, OverlapPolicy::Skip)
        .await;
    std::fs::write(&entrypoint, b"changed").unwrap();
    bench.set(t0().plus(15 * MINUTE)).await;
    let stored = bench.wait_suspended(schedule).await;
    assert_eq!(stored.suspended.as_deref(), Some("program_changed"));
    assert!(
        bench.runs(job.0).await.is_empty(),
        "refused before admission"
    );
}
