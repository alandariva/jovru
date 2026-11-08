use super::*;
use jykre_core::ErrorCode;
use std::path::PathBuf;

const T0: UtcMillis = UtcMillis(1_800_000_000_000);
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn dir() -> PathBuf {
    std::env::temp_dir().join(format!("jykre-automation-{}", RunId::new()))
}

async fn ready(dir: &Path) -> AutomationRepository {
    let Opened::Ready(repository) = AutomationRepository::open(dir).await.unwrap() else {
        unreachable!("expected a ready repository");
    };
    repository
}

async fn count(repo: &AutomationRepository, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&repo.pool).await.unwrap()
}

fn sql_def(name: &str) -> String {
    format!(r#"{{"schema_version":1,"kind":"sql","name":"{name}"}}"#)
}

fn chain_def(name: &str, steps: &[(JobId, i64)]) -> String {
    let steps: Vec<_> = steps
        .iter()
        .enumerate()
        .map(|(index, (job, version))| {
            serde_json::json!({"id": format!("s{index}"), "job": job.to_string(), "version": version})
        })
        .collect();
    serde_json::json!({"schema_version":1,"kind":"chain","name":name,"steps":steps}).to_string()
}

fn sql_run(job: Option<JobId>, at: UtcMillis) -> NewRun {
    NewRun {
        id: RunId::new(),
        kind: JobKind::Sql,
        job_id: job,
        job_revision: job.map(|_| 1),
        job_version: None,
        chain_run_id: None,
        step_index: None,
        origin: RunOrigin::Manual,
        reads_only: false,
        snapshot_json: r#"{"kind":"sql","inputs":[{"name":"a","kind":"text"}]}"#.to_owned(),
        connection_id: Some(ConnectionId::new()),
        connection_revision: Some(1),
        program_fingerprint: None,
        started_at: at,
    }
}

fn process_run(job: Option<JobId>, at: UtcMillis) -> NewRun {
    NewRun {
        kind: JobKind::Process,
        connection_id: None,
        connection_revision: None,
        program_fingerprint: Some(fingerprint(b"program")),
        ..sql_run(job, at)
    }
}

fn step_of(mut run: NewRun, chain: ChainRunId, index: i64, version: i64) -> NewRun {
    run.chain_run_id = Some(chain);
    run.step_index = Some(index);
    run.job_version = Some(version);
    run.job_revision = None;
    run
}

fn chain_run(job: Option<JobId>, at: UtcMillis) -> NewChainRun {
    NewChainRun {
        id: ChainRunId::new(),
        job_id: job,
        job_revision: job.map(|_| 1),
        origin: RunOrigin::Manual,
        snapshot_json: r#"{"kind":"chain"}"#.to_owned(),
        started_at: at,
    }
}

fn finished(state: RunState, at: UtcMillis) -> RunFinish {
    RunFinish {
        state,
        cause: (state == RunState::Failed).then_some(Cause::Database),
        finished_at: at,
        ..RunFinish::default()
    }
}

fn output(name: &str, value: Option<&str>) -> OutputValue {
    OutputValue {
        name: name.to_owned(),
        kind: ParameterKind::Text,
        value: value.map(str::to_owned),
    }
}

// ----- time and fingerprints -----

#[test]
fn times_round_trip_to_the_millisecond() {
    for millis in [
        0,
        1,
        999,
        86_399_999,
        951_782_400_000,
        951_782_400_001,
        1_700_000_000_123,
        T0.0,
        4_102_444_799_999,
        LAST_MILLIS,
    ] {
        let text = format_time(UtcMillis(millis));
        assert_eq!(text.len(), 24);
        assert_eq!(parse_time(&text), Some(UtcMillis(millis)), "{text}");
    }
    assert_eq!(format_time(UtcMillis(0)), "1970-01-01T00:00:00.000Z");
    assert_eq!(
        format_time(UtcMillis(951_782_400_000)),
        "2000-02-29T00:00:00.000Z"
    );
    assert_eq!(parse_time("2026-13-01T00:00:00.000Z"), None);
    assert_eq!(parse_time("2026-02-30T00:00:00.000Z"), None);
    assert_eq!(parse_time("2026-01-01 00:00:00.000Z"), None);
    assert_eq!(parse_time("garbage"), None);
}

#[tokio::test]
async fn time_text_matches_sqlite_strftime() {
    let dir = dir();
    let repo = ready(&dir).await;
    for millis in [0_i64, 1_700_000_000_123, T0.0 + 7] {
        let theirs: String =
            sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', ?/1000.0, 'unixepoch')")
                .bind(millis)
                .fetch_one(&repo.pool)
                .await
                .unwrap();
        assert_eq!(format_time(UtcMillis(millis)), theirs);
    }
}

#[test]
fn fingerprints_are_sha256() {
    assert_eq!(
        fingerprint(b"abc"),
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let mut streaming = Fingerprinter::new();
    streaming.update(b"a");
    streaming.update(b"bc");
    assert_eq!(streaming.finish(), fingerprint(b"abc"));
}

// ----- migrations and opening -----

#[tokio::test]
async fn a_fresh_file_reaches_the_latest_version() {
    let dir = dir();
    let repo = ready(&dir).await;
    assert_eq!(count(&repo, "PRAGMA user_version").await, LATEST_VERSION);
    for table in ["jobs", "runs", "run_outputs", "job_versions", "chain_runs"] {
        let found: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?")
                .bind(table)
                .fetch_one(&repo.pool)
                .await
                .unwrap();
        assert_eq!(found, 1, "{table}");
    }
    assert!(dir.join(AUTOMATION_FILE).exists());
    // Opening again keeps it.
    repo.pool.close().await;
    ready(&dir).await;
}

const CREATED: &str = "2026-01-01T00:00:00.000Z";

/// Rows valid under migrations 1, 2 and 3 alike: one job and a run in each
/// state.
async fn seed(pool: &SqlitePool, version: i64) -> (String, Vec<(String, &'static str)>) {
    let job = JobId::new().to_string();
    sqlx::query(
        "INSERT INTO jobs (id, kind, definition_json, revision, created_at, updated_at) \
         VALUES (?, 'sql', '{\"kind\":\"sql\"}', 3, ?, '2026-01-02T00:00:00.000Z')",
    )
    .bind(&job)
    .bind(CREATED)
    .execute(pool)
    .await
    .unwrap();
    let mut runs = Vec::new();
    for state in ["running", "succeeded", "failed", "cancelled", "interrupted"] {
        let id = RunId::new().to_string();
        let running = state == "running";
        sqlx::query(
            "INSERT INTO runs (id, job_id, job_revision, origin, state, cause, effect_unknown, \
             reads_only, snapshot_json, fingerprint, connection_id, connection_revision, \
             rows_delivered, truncated, server_code, started_at, finished_at) \
             VALUES (?, ?, 3, 'manual', ?, ?, ?, 0, '{}', 'sha256:x', ?, 2, 10, 1, '42S02', ?, ?)",
        )
        .bind(&id)
        .bind(&job)
        .bind(state)
        .bind((state == "failed").then_some("database"))
        .bind(i64::from(state == "interrupted"))
        .bind(ConnectionId::new().to_string())
        .bind(CREATED)
        .bind((!running).then_some("2026-01-01T00:00:05.250Z"))
        .execute(pool)
        .await
        .unwrap();
        runs.push((id, state));
    }
    if version >= 2 {
        sqlx::query("UPDATE runs SET outputs_stored=1 WHERE id=?")
            .bind(&runs[1].0)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO run_outputs (run_id, position, name, kind, value, is_null, bytes, \
             value_expires_at) VALUES (?, 0, 'total', 'number', '41', 0, 2, '2026-01-08T00:00:05.250Z')",
        )
        .bind(&runs[1].0)
        .execute(pool)
        .await
        .unwrap();
    }
    if version >= 3 {
        sqlx::query(
            "INSERT INTO jobs (id, kind, definition_json, created_at, updated_at) \
             VALUES (?, 'process', '{\"kind\":\"process\"}', ?, ?)",
        )
        .bind(JobId::new().to_string())
        .bind(CREATED)
        .bind(CREATED)
        .execute(pool)
        .await
        .unwrap();
    }
    (job, runs)
}

#[tokio::test]
async fn each_migration_keeps_every_row() {
    for stopped_at in 1..LATEST_VERSION {
        let dir = dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(AUTOMATION_FILE);
        let pool = open_pool_with(&path, true, true).await.unwrap();
        assert_eq!(migrate(&pool, stopped_at).await.unwrap(), Migrated::Current);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("PRAGMA user_version")
                .fetch_one(&pool)
                .await
                .unwrap(),
            stopped_at
        );
        let (job, runs) = seed(&pool, stopped_at).await;
        if stopped_at >= 3 {
            sqlx::query(
                "INSERT INTO runs (id, kind, origin, state, cause, effect_unknown, reads_only, \
                 snapshot_json, fingerprint, exit_code, started_at, finished_at) \
                 VALUES (?, 'process', 'manual', 'failed', 'exit', 0, 0, '{}', 'sha256:y', 3, ?, ?)",
            )
            .bind(RunId::new().to_string())
            .bind(CREATED)
            .bind("2026-01-01T00:00:01.000Z")
            .execute(&pool)
            .await
            .unwrap();
        }
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM runs")
            .fetch_one(&pool)
            .await
            .unwrap();
        pool.close().await;

        let repo = ready(&dir).await;
        assert_eq!(count(&repo, "PRAGMA user_version").await, LATEST_VERSION);
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&repo.pool)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(count(&repo, "SELECT count(*) FROM runs").await, before);
        let stored = repo.job(job.parse().unwrap()).await.unwrap().unwrap();
        assert_eq!(stored.kind, JobKind::Sql);
        assert_eq!(stored.revision, 3);
        assert_eq!(stored.created_at, parse_time(CREATED).unwrap());
        for (id, state) in &runs {
            let run = repo.run(id.parse().unwrap()).await.unwrap().unwrap();
            assert_eq!(run.state.as_str(), *state);
            assert_eq!(run.kind, JobKind::Sql);
            assert_eq!(run.job_id, Some(stored.id));
            assert_eq!(run.job_revision, Some(3));
            assert_eq!(run.rows_delivered, Some(10));
            assert!(run.truncated);
            assert_eq!(run.server_code.as_deref(), Some("42S02"));
            assert_eq!(run.chain_run_id, None);
            assert_eq!(run.exit_code, None);
            assert_eq!(run.effect_unknown, *state == "interrupted");
        }
        let done = repo.run(runs[1].0.parse().unwrap()).await.unwrap().unwrap();
        assert_eq!(done.finished_at, parse_time("2026-01-01T00:00:05.250Z"));
        if stopped_at >= 2 {
            assert_eq!(done.outputs_stored, Some(true));
            let outputs = repo.outputs(done.id).await.unwrap();
            assert_eq!(outputs.len(), 1);
            assert_eq!(outputs[0].value.as_deref(), Some("41"));
            // The key still points at the rebuilt `runs`.
            sqlx::query("DELETE FROM runs WHERE id=?")
                .bind(&runs[1].0)
                .execute(&repo.pool)
                .await
                .unwrap();
            assert_eq!(count(&repo, "SELECT count(*) FROM run_outputs").await, 0);
        } else {
            assert_eq!(done.outputs_stored, None);
        }
        if stopped_at >= 3 {
            let program: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM runs WHERE kind='process' AND exit_code=3",
            )
            .fetch_one(&repo.pool)
            .await
            .unwrap();
            assert_eq!(program, 1);
        }
        let leftovers: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_master WHERE sql LIKE '%jobs_new%' OR sql LIKE '%runs_new%' \
             OR name IN ('jobs_new', 'runs_new')",
        )
        .fetch_one(&repo.pool)
        .await
        .unwrap();
        assert_eq!(leftovers, 0);
        // The migrated file takes new rows of every kind.
        let chain = chain_run(Some(stored.id), T0);
        repo.admit_chain_run(&chain).await.unwrap();
        repo.admit_run(&step_of(sql_run(Some(stored.id), T0), chain.id, 0, 1))
            .await
            .unwrap();
        repo.pool.close().await;
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[tokio::test]
async fn a_newer_file_is_refused_untouched() {
    let dir = dir();
    let repo = ready(&dir).await;
    sqlx::raw_sql("PRAGMA user_version=99")
        .execute(&repo.pool)
        .await
        .unwrap();
    repo.pool.close().await;
    let path = dir.join(AUTOMATION_FILE);
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        AutomationRepository::open(&dir).await.unwrap(),
        Opened::Newer
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let pool = open_pool_with(&path, true, true).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap(),
        99
    );
}

#[tokio::test]
async fn a_corrupt_file_is_set_aside_and_recreated() {
    let dir = dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(AUTOMATION_FILE),
        b"this is not a sqlite database, just text padding padding padding padding padding padding padding padding padding padding",
    )
    .unwrap();
    let Opened::Recreated(repo) = AutomationRepository::open(&dir).await.unwrap() else {
        unreachable!("expected a recreated file");
    };
    assert_eq!(repo.jobs().await.unwrap(), vec![]);
    let aside: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("automation.db.corrupt-")
        })
        .collect();
    assert_eq!(aside.len(), 1);
    repo.pool.close().await;
    ready(&dir).await;
}

// ----- the lock -----

#[test]
fn lock_holder_helper() {
    // Run by `a_killed_process_releases_the_lock` in a child process; a
    // plain run of the suite does nothing here.
    let Ok(dir) = std::env::var("JYKRE_TEST_LOCK_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let lock = AutomationLock::try_acquire(&dir).unwrap().unwrap();
    std::fs::write(dir.join("ready"), b"").unwrap();
    std::thread::sleep(Duration::from_secs(120));
    drop(lock);
}

#[test]
fn the_lock_is_exclusive_while_held() {
    let dir = dir();
    let first = AutomationLock::try_acquire(&dir).unwrap().unwrap();
    assert!(AutomationLock::try_acquire(&dir).unwrap().is_none());
    drop(first);
    // The file stays, and means nothing.
    assert!(dir.join(AUTOMATION_LOCK_FILE).exists());
    assert!(AutomationLock::try_acquire(&dir).unwrap().is_some());
}

#[tokio::test]
async fn a_killed_process_releases_the_lock() {
    let dir = dir();
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "automation::tests::lock_holder_helper",
            "--test-threads=1",
        ])
        .env("JYKRE_TEST_LOCK_DIR", &dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..400 {
        if dir.join("ready").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(dir.join("ready").exists(), "the child never took the lock");
    assert!(AutomationLock::try_acquire(&dir).unwrap().is_none());
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(dir.join(AUTOMATION_LOCK_FILE).exists());
    assert!(AutomationLock::try_acquire(&dir).unwrap().is_some());
}

// ----- jobs and versions -----

#[tokio::test]
async fn jobs_keep_revisions_and_refuse_conflicts() {
    let dir = dir();
    let repo = ready(&dir).await;
    let id = JobId::new();
    let job = repo
        .insert_job(id, JobKind::Sql, &sql_def("one"), T0)
        .await
        .unwrap();
    assert_eq!((job.revision, job.created_at, job.updated_at), (1, T0, T0));
    let later = T0.plus(Duration::from_millis(1234));
    let updated = repo
        .update_job(id, &sql_def("renamed"), 1, later)
        .await
        .unwrap();
    assert_eq!(updated.revision, 2);
    assert_eq!(updated.created_at, T0);
    assert_eq!(updated.updated_at, later);
    assert_eq!(updated.definition_json, sql_def("renamed"));
    // Stale revision.
    let stale = repo
        .update_job(id, &sql_def("x"), 1, later)
        .await
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::Validation);
    // Another kind.
    let other = repo
        .update_job(id, r#"{"kind":"process","name":"x"}"#, 2, later)
        .await
        .unwrap_err();
    assert_eq!(other.code, ErrorCode::Validation);
    // Not JSON, and unknown job.
    assert_eq!(
        repo.update_job(id, "{not json", 2, later)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Validation
    );
    assert_eq!(
        repo.insert_job(JobId::new(), JobKind::Sql, "{not json", T0)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Validation
    );
    assert_eq!(
        repo.update_job(JobId::new(), &sql_def("x"), 1, later)
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(repo.job(id).await.unwrap().unwrap().revision, 2);
    repo.insert_job(JobId::new(), JobKind::Process, &sql_def("two"), later)
        .await
        .unwrap();
    assert_eq!(repo.jobs().await.unwrap().len(), 2);
    assert_eq!(repo.job(JobId::new()).await.unwrap(), None);
}

#[tokio::test]
async fn publishing_freezes_versions_and_chains_pin_them() {
    let dir = dir();
    let repo = ready(&dir).await;
    let id = JobId::new();
    repo.insert_job(id, JobKind::Sql, &sql_def("base"), T0)
        .await
        .unwrap();
    let one = repo.publish(id, &sql_def("v1"), T0).await.unwrap();
    let two = repo
        .publish(id, &sql_def("v2"), T0.plus(Duration::from_millis(5)))
        .await
        .unwrap();
    assert_eq!((one.version, two.version), (1, 2));
    assert_eq!(one.fingerprint, fingerprint(sql_def("v1").as_bytes()));
    assert_eq!(two.published_at, T0.plus(Duration::from_millis(5)));
    repo.update_job(id, &sql_def("edited"), 1, T0)
        .await
        .unwrap();
    let versions = repo.versions(id).await.unwrap();
    assert_eq!(versions, vec![two.clone(), one.clone()]);
    assert_eq!(repo.version(id, 1).await.unwrap(), Some(one));
    assert_eq!(repo.version(id, 3).await.unwrap(), None);
    // Unknown jobs and chains cannot be published.
    assert_eq!(
        repo.publish(JobId::new(), "{}", T0).await.unwrap_err().code,
        ErrorCode::NotFound
    );

    let chain = JobId::new();
    repo.insert_job(chain, JobKind::Chain, &chain_def("Nightly", &[(id, 1)]), T0)
        .await
        .unwrap();
    assert_eq!(
        repo.publish(chain, "{}", T0).await.unwrap_err().code,
        ErrorCode::Validation
    );
    let named = vec![ChainRef {
        id: chain,
        name: "Nightly".to_owned(),
    }];
    assert_eq!(repo.chains_using(id, None).await.unwrap(), named);
    assert_eq!(repo.chains_using(id, Some(1)).await.unwrap(), named);
    assert_eq!(repo.chains_using(id, Some(2)).await.unwrap(), vec![]);
    assert_eq!(repo.chains_using(chain, None).await.unwrap(), vec![]);

    assert_eq!(
        repo.delete_version(id, 1).await.unwrap(),
        Err(InUse {
            chains: named.clone()
        })
    );
    assert_eq!(
        repo.delete_job(id).await.unwrap(),
        Err(InUse { chains: named })
    );
    repo.delete_version(id, 2).await.unwrap().unwrap();
    assert_eq!(repo.versions(id).await.unwrap().len(), 1);
    assert_eq!(
        repo.delete_version(id, 2).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    // The next number never reuses a deleted one's slot below the maximum.
    assert_eq!(repo.publish(id, "{}", T0).await.unwrap().version, 2);

    // Once the chain is gone, the job goes with its versions; runs stay.
    let run = sql_run(Some(id), T0);
    repo.admit_run(&run).await.unwrap();
    repo.delete_job(chain).await.unwrap().unwrap();
    repo.delete_job(id).await.unwrap().unwrap();
    assert_eq!(repo.job(id).await.unwrap(), None);
    assert_eq!(count(&repo, "SELECT count(*) FROM job_versions").await, 0);
    assert_eq!(repo.run(run.id).await.unwrap().unwrap().job_id, None);
    assert_eq!(
        repo.delete_job(id).await.unwrap_err().code,
        ErrorCode::NotFound
    );
}

// ----- runs -----

#[tokio::test]
async fn a_run_is_admitted_and_finished_once() {
    let dir = dir();
    let repo = ready(&dir).await;
    let job = JobId::new();
    repo.insert_job(job, JobKind::Sql, &sql_def("j"), T0)
        .await
        .unwrap();
    let new = sql_run(Some(job), T0);
    repo.admit_run(&new).await.unwrap();
    let run = repo.run(new.id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::Running);
    assert_eq!(run.finished_at, None);
    assert_eq!(run.fingerprint, fingerprint(new.snapshot_json.as_bytes()));
    assert_eq!(run.connection_id, new.connection_id);
    assert_eq!(run.outputs_stored, None);
    assert_eq!(
        repo.run_snapshot(new.id).await.unwrap(),
        Some(new.snapshot_json.clone())
    );
    assert!(repo.admit_run(&new).await.is_err(), "same ID twice");

    let end = T0.plus(Duration::from_millis(2500));
    let finish = RunFinish {
        rows_delivered: Some(7),
        rows_discarded: Some(2),
        truncated: true,
        server_code: Some("HY000".to_owned()),
        ..finished(RunState::Succeeded, end)
    };
    repo.finish_run(new.id, &finish).await.unwrap();
    let run = repo.run(new.id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(run.finished_at, Some(end));
    assert_eq!((run.rows_delivered, run.rows_discarded), (Some(7), Some(2)));
    assert!(run.truncated);
    assert_eq!(run.server_code.as_deref(), Some("HY000"));
    // Terminal once.
    assert_eq!(
        repo.finish_run(new.id, &finished(RunState::Cancelled, end))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Validation
    );
    assert_eq!(
        repo.run(new.id).await.unwrap().unwrap().state,
        RunState::Succeeded
    );
    assert_eq!(
        repo.finish_run(RunId::new(), &finish)
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );

    // Failed needs a cause, and only failed has one; running is not terminal.
    let other = sql_run(None, T0);
    repo.admit_run(&other).await.unwrap();
    for bad in [
        RunFinish {
            state: RunState::Failed,
            cause: None,
            ..RunFinish::default()
        },
        RunFinish {
            state: RunState::Succeeded,
            cause: Some(Cause::Timeout),
            ..RunFinish::default()
        },
        RunFinish {
            state: RunState::Running,
            ..RunFinish::default()
        },
    ] {
        assert_eq!(
            repo.finish_run(other.id, &bad).await.unwrap_err().code,
            ErrorCode::Validation
        );
    }
    assert_eq!(
        repo.run(other.id).await.unwrap().unwrap().state,
        RunState::Running
    );
    let failed = RunFinish {
        cause: Some(Cause::Timeout),
        effect_unknown: true,
        ..finished(RunState::Failed, end)
    };
    repo.finish_run(other.id, &failed).await.unwrap();
    let run = repo.run(other.id).await.unwrap().unwrap();
    assert_eq!(run.cause, Some(Cause::Timeout));
    assert!(run.effect_unknown);
}

#[tokio::test]
async fn the_tables_enforce_their_own_rules() {
    let dir = dir();
    let repo = ready(&dir).await;
    // A SQL run needs its connection; a program run must not have one.
    let mut no_connection = sql_run(None, T0);
    no_connection.connection_id = None;
    assert!(repo.admit_run(&no_connection).await.is_err());
    let mut no_revision = sql_run(None, T0);
    no_revision.connection_revision = None;
    assert!(repo.admit_run(&no_revision).await.is_err());
    let mut with_connection = process_run(None, T0);
    with_connection.connection_id = Some(ConnectionId::new());
    with_connection.connection_revision = Some(1);
    assert!(repo.admit_run(&with_connection).await.is_err());
    // A chain has no run row of its own.
    let mut chain_kind = sql_run(None, T0);
    chain_kind.kind = JobKind::Chain;
    assert!(repo.admit_run(&chain_kind).await.is_err());
    // A step names an existing chain run and an existing job.
    assert!(
        repo.admit_run(&step_of(sql_run(None, T0), ChainRunId::new(), 0, 1))
            .await
            .is_err()
    );
    assert!(
        repo.admit_run(&sql_run(Some(JobId::new()), T0))
            .await
            .is_err()
    );

    // Process columns stay NULL for SQL runs.
    let sql = sql_run(None, T0);
    repo.admit_run(&sql).await.unwrap();
    assert!(repo.set_process(sql.id, Some(10), Some("1")).await.is_err());
    assert!(
        sqlx::query("UPDATE runs SET exit_code=1 WHERE id=?")
            .bind(sql.id.to_string())
            .execute(&repo.pool)
            .await
            .is_err()
    );
    assert!(
        repo.finish_run(
            sql.id,
            &RunFinish {
                exit_code: Some(1),
                ..finished(RunState::Succeeded, T0)
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        repo.run(sql.id).await.unwrap().unwrap().state,
        RunState::Running,
        "a refused finish leaves the run as it was"
    );

    // running <=> finished_at NULL; failed <=> cause.
    for bad in [
        "UPDATE runs SET finished_at='2026-01-01T00:00:00.000Z' WHERE id=?",
        "UPDATE runs SET state='succeeded' WHERE id=?",
        "UPDATE runs SET state='failed', finished_at='2026-01-01T00:00:00.000Z' WHERE id=?",
        "UPDATE runs SET cause='timeout' WHERE id=?",
        "UPDATE runs SET state='queued' WHERE id=?",
        "UPDATE runs SET origin='schedule' WHERE id=?",
    ] {
        assert!(
            sqlx::query(bad)
                .bind(sql.id.to_string())
                .execute(&repo.pool)
                .await
                .is_err(),
            "{bad}"
        );
    }
    // The same for chain runs.
    let chain = chain_run(None, T0);
    repo.admit_chain_run(&chain).await.unwrap();
    assert!(
        sqlx::query("UPDATE chain_runs SET state='failed', finished_at='2026-01-01T00:00:00.000Z' WHERE id=?")
            .bind(chain.id.to_string())
            .execute(&repo.pool)
            .await
            .is_err()
    );
    // Programs record their process and exit.
    let program = process_run(None, T0);
    repo.admit_run(&program).await.unwrap();
    repo.set_process(program.id, Some(4242), Some("99887766"))
        .await
        .unwrap();
    repo.finish_run(
        program.id,
        &RunFinish {
            cause: Some(Cause::Exit),
            exit_code: Some(3),
            log_bytes_stdout: Some(10),
            log_bytes_stderr: Some(0),
            log_bytes_jjp: Some(5),
            failure_code: Some("bad_input".to_owned()),
            ..finished(RunState::Failed, T0)
        },
    )
    .await
    .unwrap();
    let run = repo.run(program.id).await.unwrap().unwrap();
    assert_eq!(run.kind, JobKind::Process);
    assert_eq!(
        (
            run.exit_code,
            run.process_group,
            run.process_started.as_deref()
        ),
        (Some(3), Some(4242), Some("99887766"))
    );
    assert_eq!(run.program_fingerprint, Some(fingerprint(b"program")));
    assert_eq!(run.failure_code.as_deref(), Some("bad_input"));
    assert_eq!(run.log_bytes_jjp, Some(5));
    // Not running any more: the process cannot be rewritten.
    assert!(repo.set_process(program.id, None, None).await.is_err());
    // A stored code this build does not know is a read error.
    sqlx::query(
        "UPDATE runs SET state='failed', finished_at=started_at, cause='mystery' WHERE id=?",
    )
    .bind(program.id.to_string())
    .execute(&repo.pool)
    .await
    .unwrap();
    assert!(repo.run(program.id).await.is_err());
}

#[tokio::test]
async fn outputs_are_stored_all_or_none_within_four_kib() {
    let dir = dir();
    let repo = ready(&dir).await;
    let end = T0.plus(Duration::from_millis(900));
    let expires = end.plus(OUTPUT_VALUE_RETENTION);

    // Exactly 4096 bytes (multi-byte characters count as bytes).
    let exact = process_run(None, T0);
    repo.admit_run(&exact).await.unwrap();
    let text = "é".repeat(1024);
    assert_eq!(text.len(), 2048);
    repo.finish_run(
        exact.id,
        &RunFinish {
            outputs: Some(vec![
                output("a", Some(&text)),
                output("b", Some(&text)),
                output("c", None),
            ]),
            ..finished(RunState::Succeeded, end)
        },
    )
    .await
    .unwrap();
    let run = repo.run(exact.id).await.unwrap().unwrap();
    assert_eq!(run.outputs_stored, Some(true));
    let stored = repo.outputs(exact.id).await.unwrap();
    assert_eq!(stored.len(), 3);
    assert_eq!(
        stored.iter().map(|o| o.position).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(stored[0].value.as_deref(), Some(text.as_str()));
    assert_eq!(stored[0].bytes, 2048);
    assert_eq!(stored[0].kind, ParameterKind::Text);
    assert_eq!(stored[0].value_expires_at, expires);
    assert!(!stored[0].expired);
    assert!(stored[2].is_null);
    assert_eq!((stored[2].value.clone(), stored[2].bytes), (None, 0));

    // 4097 bytes: none are stored, the run records it, and it still succeeds.
    let over = process_run(None, T0);
    repo.admit_run(&over).await.unwrap();
    repo.finish_run(
        over.id,
        &RunFinish {
            outputs: Some(vec![
                output("a", Some(&text)),
                output("b", Some(&format!("{text}x"))),
            ]),
            ..finished(RunState::Succeeded, end)
        },
    )
    .await
    .unwrap();
    let run = repo.run(over.id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(run.outputs_stored, Some(false));
    assert_eq!(repo.outputs(over.id).await.unwrap(), vec![]);

    // A job that did not opt in: NULL, and no rows.
    let plain = sql_run(None, T0);
    repo.admit_run(&plain).await.unwrap();
    repo.finish_run(plain.id, &finished(RunState::Succeeded, end))
        .await
        .unwrap();
    assert_eq!(
        repo.run(plain.id).await.unwrap().unwrap().outputs_stored,
        None
    );
    assert_eq!(repo.outputs(plain.id).await.unwrap(), vec![]);

    // Two outputs with one name are refused whole; the run stays running.
    let twice = sql_run(None, T0);
    repo.admit_run(&twice).await.unwrap();
    assert!(
        repo.finish_run(
            twice.id,
            &RunFinish {
                outputs: Some(vec![output("a", Some("1")), output("a", Some("2"))]),
                ..finished(RunState::Succeeded, end)
            },
        )
        .await
        .is_err()
    );
    assert_eq!(
        repo.run(twice.id).await.unwrap().unwrap().state,
        RunState::Running
    );
    assert_eq!(repo.outputs(twice.id).await.unwrap(), vec![]);
    assert_eq!(
        count(&repo, "SELECT count(*) FROM run_outputs").await,
        3,
        "only the first run's rows exist"
    );
}

// ----- recovery -----

#[tokio::test]
async fn recovery_marks_running_rows_and_nothing_else() {
    let dir = dir();
    let repo = ready(&dir).await;
    let end = T0.plus(Duration::from_secs(5));

    let write = sql_run(None, T0);
    let mut read = sql_run(None, T0);
    read.reads_only = true;
    let program = process_run(None, T0);
    let silent_program = process_run(None, T0);
    let done = sql_run(None, T0);
    for run in [&write, &read, &program, &silent_program, &done] {
        repo.admit_run(run).await.unwrap();
    }
    repo.set_process(program.id, Some(777), Some("424242"))
        .await
        .unwrap();
    repo.finish_run(done.id, &finished(RunState::Succeeded, end))
        .await
        .unwrap();

    // Chain X: step 0 done, step 1 (a write) running, step 2 never started.
    let x = chain_run(None, T0);
    repo.admit_chain_run(&x).await.unwrap();
    let x0 = step_of(sql_run(None, T0), x.id, 0, 1);
    let x1 = step_of(sql_run(None, T0), x.id, 1, 1);
    repo.admit_run(&x0).await.unwrap();
    repo.finish_run(x0.id, &finished(RunState::Succeeded, end))
        .await
        .unwrap();
    repo.admit_run(&x1).await.unwrap();
    // Chain Y: its only step reads.
    let y = chain_run(None, T0);
    repo.admit_chain_run(&y).await.unwrap();
    let mut y0 = step_of(sql_run(None, T0), y.id, 0, 1);
    y0.reads_only = true;
    repo.admit_run(&y0).await.unwrap();
    // Chain Z already finished.
    let z = chain_run(None, T0);
    repo.admit_chain_run(&z).await.unwrap();
    repo.finish_chain_run(
        z.id,
        &ChainFinish {
            finished_at: end,
            ..ChainFinish::default()
        },
    )
    .await
    .unwrap();
    let steps_before = count(&repo, "SELECT count(*) FROM runs").await;

    let now = T0.plus(Duration::from_secs(3600));
    let recovered = repo.recover(now).await.unwrap();

    assert_eq!(
        recovered.runs.len(),
        6,
        "write, read, two programs, x1 and y0"
    );
    let record = |id: RunId| recovered.runs.iter().find(|run| run.id == id);
    for (run, unknown) in [
        (&write, true),
        (&read, false),
        (&program, true),
        (&silent_program, true),
        (&x1, true),
        (&y0, false),
    ] {
        let found = record(run.id).unwrap();
        assert_eq!(found.state, RunState::Interrupted);
        assert_eq!(found.effect_unknown, unknown, "{:?}", run.id);
        assert_eq!(found.finished_at, Some(now));
        assert_eq!(found.cause, None);
    }
    assert!(record(done.id).is_none() && record(x0.id).is_none());
    assert_eq!(
        recovered.orphans,
        vec![Orphan {
            run_id: program.id,
            job_id: None,
            process_group: 777,
            process_started: Some("424242".to_owned()),
        }]
    );
    assert_eq!(recovered.chain_runs.len(), 2);
    let chain = |id: ChainRunId| {
        recovered
            .chain_runs
            .iter()
            .find(|chain| chain.id == id)
            .unwrap()
    };
    assert_eq!(chain(x.id).state, RunState::Interrupted);
    assert!(chain(x.id).effect_unknown);
    assert_eq!(chain(y.id).state, RunState::Interrupted);
    assert!(!chain(y.id).effect_unknown);
    assert_eq!(chain(y.id).finished_at, Some(now));

    // Nothing else changed, nothing was added.
    assert_eq!(
        count(&repo, "SELECT count(*) FROM runs").await,
        steps_before
    );
    assert_eq!(repo.chain_steps(x.id).await.unwrap().len(), 2);
    let untouched = repo.run(done.id).await.unwrap().unwrap();
    assert_eq!(
        (untouched.state, untouched.finished_at),
        (RunState::Succeeded, Some(end))
    );
    assert_eq!(
        repo.chain_run(z.id).await.unwrap().unwrap().state,
        RunState::Succeeded
    );
    let steps = repo.chain_steps(x.id).await.unwrap();
    assert_eq!(
        steps.iter().map(|s| s.step_index).collect::<Vec<_>>(),
        vec![Some(0), Some(1)]
    );
    // Recovery is idempotent.
    assert_eq!(repo.recover(now).await.unwrap(), Recovered::default());
    assert_eq!(
        repo.chain_run_snapshot(x.id).await.unwrap(),
        Some(r#"{"kind":"chain"}"#.to_owned())
    );
}

// ----- pruning -----

#[tokio::test]
async fn pruning_expires_values_then_deletes_old_rows() {
    let dir = dir();
    let repo = ready(&dir).await;
    let now = T0;
    let ago = |days: u64| now.minus(DAY * days as u32);
    let with_output = |value: &str| RunFinish {
        outputs: Some(vec![output("answer", Some(value))]),
        ..RunFinish::default()
    };
    let finish = |run: &NewRun, at: UtcMillis, value: &str| {
        let finish = RunFinish {
            finished_at: at,
            ..with_output(value)
        };
        (run.id, finish)
    };

    let recent = process_run(None, ago(3).minus(Duration::from_secs(1)));
    let week_old = process_run(None, ago(8).minus(Duration::from_secs(1)));
    let old = sql_run(None, ago(91).minus(Duration::from_secs(1)));
    let old_program = process_run(None, ago(100).minus(Duration::from_secs(1)));
    let still_running = sql_run(None, ago(200));
    for run in [&recent, &week_old, &old, &old_program, &still_running] {
        repo.admit_run(run).await.unwrap();
    }
    for (run, at) in [
        (&recent, ago(3)),
        (&week_old, ago(8)),
        (&old, ago(91)),
        (&old_program, ago(100)),
    ] {
        let (id, finish) = finish(run, at, "SECRET-OUT");
        repo.finish_run(id, &finish).await.unwrap();
    }

    // A chain 91 days old with a program step, and a recent one.
    let old_chain = chain_run(None, ago(91).minus(Duration::from_secs(5)));
    repo.admit_chain_run(&old_chain).await.unwrap();
    let old_step = step_of(
        process_run(None, ago(91).minus(Duration::from_secs(4))),
        old_chain.id,
        0,
        1,
    );
    repo.admit_run(&old_step).await.unwrap();
    let (id, f) = finish(
        &old_step,
        ago(91).minus(Duration::from_secs(2)),
        "SECRET-OUT",
    );
    repo.finish_run(id, &f).await.unwrap();
    repo.finish_chain_run(
        old_chain.id,
        &ChainFinish {
            finished_at: ago(91),
            ..ChainFinish::default()
        },
    )
    .await
    .unwrap();
    let new_chain = chain_run(None, ago(2).minus(Duration::from_secs(5)));
    repo.admit_chain_run(&new_chain).await.unwrap();
    let new_step = step_of(
        process_run(None, ago(2).minus(Duration::from_secs(4))),
        new_chain.id,
        0,
        1,
    );
    repo.admit_run(&new_step).await.unwrap();
    let (id, f) = finish(&new_step, ago(2), "SECRET-OUT");
    repo.finish_run(id, &f).await.unwrap();
    repo.finish_chain_run(
        new_chain.id,
        &ChainFinish {
            finished_at: ago(2),
            ..ChainFinish::default()
        },
    )
    .await
    .unwrap();

    // More old runs than one batch.
    sqlx::query(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1200) \
         INSERT INTO runs (id, kind, origin, state, reads_only, snapshot_json, fingerprint, \
         connection_id, connection_revision, started_at, finished_at) \
         SELECT 'bulk-' || i, 'sql', 'manual', 'succeeded', 1, '{}', 'sha256:z', 'c', 1, \
         '2020-01-01T00:00:00.000Z', '2020-01-01T00:00:01.000Z' FROM n",
    )
    .execute(&repo.pool)
    .await
    .unwrap();

    let pruned = repo.prune(now).await.unwrap();
    // Values: week_old's, old's, old_program's and old_step's had passed 7
    // days, as had none of the recent ones.
    assert_eq!(pruned.outputs_expired, 4);
    assert_eq!(pruned.chain_runs_deleted, 1);
    assert_eq!(pruned.runs_deleted, 1200 + 2 + 1);
    let mut programs = pruned.program_runs.clone();
    programs.sort_by_key(ToString::to_string);
    let mut expected = vec![old_program.id, old_step.id];
    expected.sort_by_key(ToString::to_string);
    assert_eq!(programs, expected);

    assert!(repo.run(old.id).await.unwrap().is_none());
    assert!(repo.run(old_program.id).await.unwrap().is_none());
    assert!(repo.run(old_step.id).await.unwrap().is_none());
    assert!(repo.chain_run(old_chain.id).await.unwrap().is_none());
    assert_eq!(repo.outputs(old.id).await.unwrap(), vec![]);
    assert!(repo.run(still_running.id).await.unwrap().is_some());
    assert!(repo.run(new_step.id).await.unwrap().is_some());
    assert!(repo.chain_run(new_chain.id).await.unwrap().is_some());

    // Metadata outlives the value; recent values are untouched.
    let kept = repo.outputs(week_old.id).await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].value, None);
    assert!(kept[0].expired);
    assert!(!kept[0].is_null);
    assert_eq!(kept[0].bytes, "SECRET-OUT".len() as i64);
    assert_eq!(kept[0].name, "answer");
    assert_eq!(kept[0].kind, ParameterKind::Text);
    let fresh = repo.outputs(recent.id).await.unwrap();
    assert_eq!(fresh[0].value.as_deref(), Some("SECRET-OUT"));
    assert!(!fresh[0].expired);
    assert_eq!(
        repo.outputs(new_step.id).await.unwrap()[0].value.as_deref(),
        Some("SECRET-OUT")
    );
    assert_eq!(
        repo.run(week_old.id).await.unwrap().unwrap().outputs_stored,
        Some(true)
    );

    // Nothing left to do.
    assert_eq!(repo.prune(now).await.unwrap(), Pruned::default());
    // Later, the week-old run goes too; the exact boundary counts.
    let later = repo.prune(ago(8).plus(RUN_RETENTION)).await.unwrap();
    assert_eq!(later.runs_deleted, 1);
    assert!(repo.run(week_old.id).await.unwrap().is_none());
    assert!(repo.run(recent.id).await.unwrap().is_some());
}

// ----- CA-13 -----

#[tokio::test]
async fn no_input_value_reaches_the_file() {
    // The engine never hands a value to storage except declared outputs
    // (ADR-0035); this checks that storage keeps to what it was given.
    const INPUT: &str = "SENTINEL-INPUT-7f3a9c";
    const OUTPUT: &str = "SENTINEL-OUTPUT-51b2e8";
    let dir = dir();
    let repo = ready(&dir).await;
    let job = JobId::new();
    repo.insert_job(job, JobKind::Sql, &sql_def("report"), T0)
        .await
        .unwrap();
    repo.publish(job, &sql_def("report"), T0).await.unwrap();

    let snapshot = r#"{"kind":"sql","statement":"SELECT * FROM t WHERE a = $1","inputs":[{"name":"a","kind":"text"}]}"#;
    let mut opted_in = process_run(Some(job), T0);
    opted_in.snapshot_json = snapshot.to_owned();
    let mut plain = sql_run(Some(job), T0);
    plain.snapshot_json = snapshot.to_owned();
    let mut failing = sql_run(Some(job), T0);
    failing.snapshot_json = snapshot.to_owned();
    for run in [&opted_in, &plain, &failing] {
        repo.admit_run(run).await.unwrap();
    }
    repo.finish_run(
        opted_in.id,
        &RunFinish {
            outputs: Some(vec![output("answer", Some(OUTPUT))]),
            ..finished(RunState::Succeeded, T0)
        },
    )
    .await
    .unwrap();
    repo.finish_run(
        plain.id,
        &RunFinish {
            rows_delivered: Some(1),
            ..finished(RunState::Succeeded, T0)
        },
    )
    .await
    .unwrap();
    repo.finish_run(
        failing.id,
        &RunFinish {
            cause: Some(Cause::Database),
            server_code: Some("42P01".to_owned()),
            ..finished(RunState::Failed, T0)
        },
    )
    .await
    .unwrap();
    let chain = chain_run(Some(job), T0);
    repo.admit_chain_run(&chain).await.unwrap();
    repo.finish_chain_run(
        chain.id,
        &ChainFinish {
            finished_at: T0,
            ..ChainFinish::default()
        },
    )
    .await
    .unwrap();
    repo.recover(T0).await.unwrap();

    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT m.name, p.name FROM sqlite_master m, pragma_table_info(m.name) p \
         WHERE m.type='table' AND m.name NOT LIKE 'sqlite_%' AND p.type='TEXT'",
    )
    .fetch_all(&repo.pool)
    .await
    .unwrap();
    assert!(columns.len() > 20);
    let mut output_hits = Vec::new();
    for (table, column) in &columns {
        let sql = format!(r#"SELECT count(*) FROM "{table}" WHERE instr("{column}", ?) > 0"#);
        for (sentinel, hits) in [(INPUT, &mut Vec::new()), (OUTPUT, &mut output_hits)] {
            let found: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.clone()))
                .bind(sentinel)
                .fetch_one(&repo.pool)
                .await
                .unwrap();
            if sentinel == INPUT {
                assert_eq!(found, 0, "input value in {table}.{column}");
            } else if found > 0 {
                hits.push((table.clone(), column.clone(), found));
            }
        }
    }
    assert_eq!(
        output_hits,
        vec![("run_outputs".to_owned(), "value".to_owned(), 1)]
    );

    // The raw file, once the log is folded in.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_all(&repo.pool)
        .await
        .unwrap();
    repo.pool.close().await;
    let bytes = std::fs::read(dir.join(AUTOMATION_FILE)).unwrap();
    let contains = |needle: &str| {
        bytes
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    };
    assert!(!contains(INPUT));
    assert!(contains(OUTPUT));
    for sidecar in ["automation.db-wal", "automation.db-shm"] {
        if let Ok(extra) = std::fs::read(dir.join(sidecar)) {
            assert!(
                !extra
                    .windows(INPUT.len())
                    .any(|window| window == INPUT.as_bytes())
            );
        }
    }
}

// ----- history -----

#[tokio::test]
async fn history_merges_runs_and_chains_by_keyset() {
    let dir = dir();
    let repo = ready(&dir).await;
    let (a, b) = (JobId::new(), JobId::new());
    repo.insert_job(a, JobKind::Sql, &sql_def("a"), T0)
        .await
        .unwrap();
    repo.insert_job(b, JobKind::Chain, &chain_def("b", &[(a, 1)]), T0)
        .await
        .unwrap();
    let at = |ms: i64| T0.plus(Duration::from_millis(ms.unsigned_abs()));

    // Ties on started_at are common: the ID breaks them.
    let mut expected = Vec::new();
    for (index, ms) in [0, 10, 10, 10, 20, 30, 30, 40].into_iter().enumerate() {
        if index % 3 == 1 {
            let chain = chain_run(Some(b), at(ms));
            repo.admit_chain_run(&chain).await.unwrap();
            // Its step is not a history entry of its own.
            repo.admit_run(&step_of(sql_run(Some(a), at(ms)), chain.id, 0, 1))
                .await
                .unwrap();
            if index == 4 {
                repo.finish_chain_run(
                    chain.id,
                    &ChainFinish {
                        state: RunState::Cancelled,
                        finished_at: at(ms + 1),
                        ..ChainFinish::default()
                    },
                )
                .await
                .unwrap();
            }
            expected.push((at(ms), chain.id.to_string()));
        } else {
            let run = sql_run(Some(a), at(ms));
            repo.admit_run(&run).await.unwrap();
            if index % 2 == 0 {
                repo.finish_run(run.id, &finished(RunState::Succeeded, at(ms + 1)))
                    .await
                    .unwrap();
            }
            expected.push((at(ms), run.id.to_string()));
        }
    }
    expected.sort();
    expected.reverse();

    let all = repo
        .history(&HistoryFilter {
            limit: 100,
            ..HistoryFilter::default()
        })
        .await
        .unwrap();
    let keys = |entries: &[HistoryEntry]| {
        entries
            .iter()
            .map(|entry| (entry.started_at(), entry.id_text()))
            .collect::<Vec<_>>()
    };
    assert_eq!(keys(&all), expected);
    assert_eq!(all.len(), 8, "steps are not entries");
    assert!(all.iter().any(|e| matches!(e, HistoryEntry::Chain(_))));
    assert!(all.iter().any(|e| matches!(e, HistoryEntry::Run(_))));

    // Pages of 3 cover the same list once.
    let mut paged = Vec::new();
    let mut before = None;
    loop {
        let page = repo
            .history(&HistoryFilter {
                before: before.clone(),
                limit: 3,
                ..HistoryFilter::default()
            })
            .await
            .unwrap();
        let Some(last) = page.last() else { break };
        before = Some((last.started_at(), last.id_text()));
        paged.extend(page);
    }
    assert_eq!(keys(&paged), expected);

    // Filters.
    let of_chain = repo
        .history(&HistoryFilter {
            job: Some(b),
            limit: 100,
            ..HistoryFilter::default()
        })
        .await
        .unwrap();
    assert!(!of_chain.is_empty());
    assert!(of_chain.iter().all(|e| matches!(e, HistoryEntry::Chain(_))));
    let succeeded = repo
        .history(&HistoryFilter {
            state: Some(RunState::Succeeded),
            limit: 100,
            ..HistoryFilter::default()
        })
        .await
        .unwrap();
    assert!(!succeeded.is_empty());
    assert!(succeeded.iter().all(|entry| match entry {
        HistoryEntry::Run(run) => run.state == RunState::Succeeded,
        HistoryEntry::Chain(_) => false,
    }));
    let cancelled = repo
        .history(&HistoryFilter {
            state: Some(RunState::Cancelled),
            job: Some(b),
            limit: 100,
            ..HistoryFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(cancelled.len(), 1);
    let none = repo
        .history(&HistoryFilter {
            job: Some(JobId::new()),
            ..HistoryFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(none, vec![]);
}

// ----- programs and logs -----

#[tokio::test]
async fn programs_remember_their_versions_and_logs_are_forgotten() {
    let dir = dir();
    let repo = ready(&dir).await;
    let job = JobId::new();
    repo.insert_job(job, JobKind::Process, r#"{"kind":"process"}"#, T0)
        .await
        .unwrap();
    assert!(!repo.program_has_run(job, None, None).await.unwrap());
    assert!(!repo.program_has_run(job, Some(1), None).await.unwrap());

    let own = process_run(Some(job), T0);
    repo.admit_run(&own).await.unwrap();
    assert!(repo.program_has_run(job, Some(1), None).await.unwrap());
    assert!(!repo.program_has_run(job, Some(2), None).await.unwrap());
    assert!(!repo.program_has_run(job, None, Some(1)).await.unwrap());
    assert!(repo.program_has_run(job, None, None).await.unwrap());

    let chain = chain_run(None, T0);
    repo.admit_chain_run(&chain).await.unwrap();
    repo.admit_run(&step_of(process_run(Some(job), T0), chain.id, 0, 3))
        .await
        .unwrap();
    assert!(repo.program_has_run(job, None, Some(3)).await.unwrap());
    assert!(!repo.program_has_run(job, None, Some(4)).await.unwrap());
    // A step is not the job run on its own.
    assert!(!repo.program_has_run(job, Some(3), None).await.unwrap());
    // SQL runs do not count.
    let other = JobId::new();
    repo.insert_job(other, JobKind::Sql, &sql_def("s"), T0)
        .await
        .unwrap();
    repo.admit_run(&sql_run(Some(other), T0)).await.unwrap();
    assert!(!repo.program_has_run(other, None, None).await.unwrap());

    repo.finish_run(
        own.id,
        &RunFinish {
            log_bytes_stdout: Some(100),
            log_bytes_stderr: Some(5),
            log_bytes_jjp: Some(7),
            ..finished(RunState::Succeeded, T0)
        },
    )
    .await
    .unwrap();
    let run = repo.run(own.id).await.unwrap().unwrap();
    assert_eq!(run.log_bytes_stdout, Some(100));
    repo.forget_logs(own.id).await.unwrap();
    let run = repo.run(own.id).await.unwrap().unwrap();
    assert_eq!(
        (
            run.log_bytes_stdout,
            run.log_bytes_stderr,
            run.log_bytes_jjp
        ),
        (None, None, None)
    );
    assert_eq!(run.state, RunState::Succeeded);
}

#[tokio::test]
async fn deleted_values_are_overwritten_in_the_file() {
    // ADR-0035: an expired value must not linger in free pages.
    let dir = dir();
    let repo = ready(&dir).await;
    let on: i64 = sqlx::query_scalar("PRAGMA secure_delete")
        .fetch_one(&repo.pool)
        .await
        .unwrap();
    assert_eq!(on, 1);
}

#[tokio::test]
async fn a_read_only_open_never_creates_migrates_or_writes() {
    let dir = dir();
    std::fs::create_dir_all(&dir).unwrap();
    // No file: nothing is created.
    assert!(
        AutomationRepository::open_read_only(&dir)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!dir.join(AUTOMATION_FILE).exists());
    // A file at an older schema is left as it is.
    let pool = open_pool_with(&dir.join(AUTOMATION_FILE), true, true)
        .await
        .unwrap();
    migrate(&pool, 2).await.unwrap();
    pool.close().await;
    assert!(
        AutomationRepository::open_read_only(&dir)
            .await
            .unwrap()
            .is_none()
    );
    let pool = open_pool_with(&dir.join(AUTOMATION_FILE), true, true)
        .await
        .unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(version, 2);
    pool.close().await;
    // At this version's schema it reads, and refuses writes.
    let repo = ready(&dir).await;
    repo.insert_job(JobId::new(), JobKind::Sql, "{}", T0)
        .await
        .unwrap();
    let Some(Opened::Ready(reader)) = AutomationRepository::open_read_only(&dir).await.unwrap()
    else {
        unreachable!("a current file opens");
    };
    assert_eq!(reader.jobs().await.unwrap().len(), 1);
    assert!(
        reader
            .insert_job(JobId::new(), JobKind::Sql, "{}", T0)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_version_number_that_runs_still_name_is_never_given_again() {
    let dir = dir();
    let repo = ready(&dir).await;
    let id = JobId::new();
    repo.insert_job(id, JobKind::Sql, &sql_def("job"), T0)
        .await
        .unwrap();
    repo.publish(id, &sql_def("v1"), T0).await.unwrap();
    repo.publish(id, &sql_def("v2"), T0).await.unwrap();
    // A chain ran version 2, which is then deleted.
    let chain = chain_run(None, T0);
    repo.admit_chain_run(&chain).await.unwrap();
    repo.admit_run(&step_of(process_run(Some(id), T0), chain.id, 0, 2))
        .await
        .unwrap();
    repo.delete_version(id, 2).await.unwrap().unwrap();
    let next = repo.publish(id, &sql_def("v3"), T0).await.unwrap();
    assert_eq!(next.version, 3);
    // Debug never shows a stored value.
    let value = OutputValue {
        name: "n".into(),
        kind: jykre_core::ParameterKind::Text,
        value: Some("secret-value-1".into()),
    };
    assert!(!format!("{value:?}").contains("secret-value-1"));
}
