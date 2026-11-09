//! `automation.db`: job definitions, published versions and run history
//! ([ADR-0034](../../../docs/adr/0034-automation-database-service-and-lock.md),
//! [ADR-0035](../../../docs/adr/0035-persisted-declared-outputs.md),
//! [ADR-0036](../../../docs/adr/0036-published-versions-and-chain-runs.md),
//! [ADR-0038](../../../docs/adr/0038-external-program-runs.md)).
//!
//! Only this module opens the file. Nothing is written here that the
//! specification forbids: no input value, cell, scalar or server message. The
//! one exception is the declared, opt-in, small and short-lived outputs of
//! ADR-0035, stored in `run_outputs`.
use crate::{error, open_pool_with, sql_error};
use jykre_core::{
    ConnectionId, DriverError, DriverResult, ErrorCode, ParameterKind,
    automation::{
        Cause, ChainRunId, ChainRunRecord, GrantId, JobId, JobKind, MissedPolicy, OverlapPolicy,
        RunId, RunOrigin, RunOutput, RunRecord, RunSchedule, RunState, ScheduleId, ScheduleOutcome,
        UtcMillis, parameter_kind_str, parse_parameter_kind,
    },
};
use sha2::{Digest, Sha256};
use sqlx::{
    ConnectOptions, Row, SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow},
};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions, TryLockError},
    path::Path,
    time::Duration,
};

pub const AUTOMATION_FILE: &str = "automation.db";
pub const AUTOMATION_LOCK_FILE: &str = "automation.lock";
/// Run rows (and chain runs) are pruned this long after they finished.
pub const RUN_RETENTION: Duration = Duration::from_secs(90 * 24 * 60 * 60);
/// Output values are emptied this long after their run finished.
pub const OUTPUT_VALUE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// The most UTF-8 bytes of output values one run may store.
pub const OUTPUT_QUOTA_BYTES: usize = 4096;
/// Rows touched by one pruning transaction.
const PRUNE_BATCH: usize = 500;

// ---------------------------------------------------------------------------
// The data-folder lock (ADR-0034, decision 4)
// ---------------------------------------------------------------------------

/// An exclusive operating-system lock on `automation.lock`, held for as long
/// as the value lives. The system releases it when the process ends however
/// it ends; the file left on disk means nothing.
#[derive(Debug)]
pub struct AutomationLock {
    _file: File,
}

impl AutomationLock {
    /// `Ok(None)` when another holder (a process, or an earlier call in this
    /// one) has the lock.
    pub fn try_acquire(dir: &Path) -> DriverResult<Option<AutomationLock>> {
        std::fs::create_dir_all(dir).map_err(|_| error("Could not open the automation lock."))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(AUTOMATION_LOCK_FILE))
            .map_err(|_| error("Could not open the automation lock."))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(AutomationLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(_)) => Err(error("Could not take the automation lock.")),
        }
    }
}

// ---------------------------------------------------------------------------
// Fingerprints
// ---------------------------------------------------------------------------

/// Streaming SHA-256 for files too large to hold in memory.
#[derive(Debug, Default)]
pub struct Fingerprinter(Sha256);

impl Fingerprinter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// `sha256:` followed by 64 lowercase hex digits.
    pub fn finish(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(71);
        text.push_str("sha256:");
        for byte in self.0.finalize() {
            text.push(char::from(HEX[usize::from(byte >> 4)]));
            text.push(char::from(HEX[usize::from(byte & 15)]));
        }
        text
    }
}

/// `sha256:` + 64 lowercase hex digits of `text`.
pub fn fingerprint(text: &[u8]) -> String {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.update(text);
    fingerprinter.finish()
}

// ---------------------------------------------------------------------------
// Timestamps: UTC text `YYYY-MM-DDTHH:MM:SS.mmmZ`, exact to the millisecond
// ---------------------------------------------------------------------------

const MILLIS_PER_DAY: i64 = 86_400_000;
/// 9999-12-31T23:59:59.999Z, the last instant the text form can hold.
const LAST_MILLIS: i64 = 253_402_300_799_999;

fn format_time(time: UtcMillis) -> String {
    let millis = time.0.clamp(0, LAST_MILLIS);
    let days = millis.div_euclid(MILLIS_PER_DAY);
    let of_day = millis.rem_euclid(MILLIS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3_600_000,
        of_day / 60_000 % 60,
        of_day / 1000 % 60,
        of_day % 1000
    )
}

fn parse_time(text: &str) -> Option<UtcMillis> {
    let bytes = text.as_bytes();
    if bytes.len() != 24
        || [4, 7, 10, 13, 16, 19, 23]
            .iter()
            .zip(*b"--T::.Z")
            .any(|(at, expected)| bytes[*at] != expected)
    {
        return None;
    }
    let number = |from: usize, to: usize| -> Option<i64> {
        let digits = text.get(from..to)?;
        if digits.bytes().all(|byte| byte.is_ascii_digit()) {
            digits.parse().ok()
        } else {
            None
        }
    };
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second, milli) = (
        number(11, 13)?,
        number(14, 16)?,
        number(17, 19)?,
        number(20, 23)?,
    );
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let millis = days * MILLIS_PER_DAY + hour * 3_600_000 + minute * 60_000 + second * 1000 + milli;
    (format_time(UtcMillis(millis)) == text).then_some(UtcMillis(millis))
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (y, m, d).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ---------------------------------------------------------------------------
// Migrations (forward-only, `PRAGMA user_version`)
// ---------------------------------------------------------------------------

/// The newest schema version this build writes.
pub(crate) const LATEST_VERSION: i64 = 5;

/// ADR-0034, migration 1.
const STEP_1: &str = "
CREATE TABLE jobs (
  id              TEXT PRIMARY KEY,
  kind            TEXT NOT NULL CHECK (kind IN ('sql')),
  definition_json TEXT NOT NULL,
  revision        INTEGER NOT NULL DEFAULT 1,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);

CREATE TABLE runs (
  id                  TEXT PRIMARY KEY,
  job_id              TEXT REFERENCES jobs(id) ON DELETE SET NULL,
  job_revision        INTEGER,
  origin              TEXT NOT NULL CHECK (origin IN ('manual')),
  state               TEXT NOT NULL CHECK (state IN
                        ('running', 'succeeded', 'failed', 'cancelled', 'interrupted')),
  cause               TEXT,
  effect_unknown      INTEGER NOT NULL DEFAULT 0 CHECK (effect_unknown IN (0, 1)),
  reads_only          INTEGER NOT NULL CHECK (reads_only IN (0, 1)),
  snapshot_json       TEXT NOT NULL,
  fingerprint         TEXT NOT NULL,
  connection_id       TEXT NOT NULL,
  connection_revision INTEGER NOT NULL,
  rows_delivered      INTEGER,
  rows_discarded      INTEGER,
  rows_affected       INTEGER,
  truncated           INTEGER NOT NULL DEFAULT 0 CHECK (truncated IN (0, 1)),
  server_code         TEXT,
  started_at          TEXT NOT NULL,
  finished_at         TEXT,
  CHECK ((state = 'running') = (finished_at IS NULL)),
  CHECK ((state = 'failed') = (cause IS NOT NULL))
);

CREATE INDEX runs_by_job   ON runs(job_id, started_at DESC);
CREATE INDEX runs_by_start ON runs(started_at);
CREATE INDEX runs_open     ON runs(state) WHERE state = 'running';
";

/// ADR-0035, migration 2.
const STEP_2: &str = "
CREATE TABLE run_outputs (
  run_id           TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  position         INTEGER NOT NULL,
  name             TEXT NOT NULL,
  kind             TEXT NOT NULL,
  value            TEXT,
  is_null          INTEGER NOT NULL CHECK (is_null IN (0, 1)),
  bytes            INTEGER NOT NULL,
  value_expires_at TEXT NOT NULL,
  expired          INTEGER NOT NULL DEFAULT 0 CHECK (expired IN (0, 1)),
  PRIMARY KEY (run_id, position),
  UNIQUE (run_id, name),
  CHECK (NOT (is_null = 1 AND value IS NOT NULL)),
  CHECK (NOT (expired = 1 AND value IS NOT NULL))
);

CREATE INDEX run_outputs_expiring ON run_outputs(value_expires_at) WHERE expired = 0;

ALTER TABLE runs ADD COLUMN outputs_stored INTEGER;
";

/// ADR-0038, migration 3: `jobs` and `runs` are rebuilt (new table, copy,
/// drop, rename). `foreign_key_check` runs before the commit.
const STEP_3: &str = "
CREATE TABLE jobs_new (
  id              TEXT PRIMARY KEY,
  kind            TEXT NOT NULL CHECK (kind IN ('sql', 'process')),
  definition_json TEXT NOT NULL,
  revision        INTEGER NOT NULL DEFAULT 1,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);
INSERT INTO jobs_new (id, kind, definition_json, revision, created_at, updated_at)
  SELECT id, kind, definition_json, revision, created_at, updated_at FROM jobs;

CREATE TABLE runs_new (
  id                  TEXT PRIMARY KEY,
  kind                TEXT NOT NULL DEFAULT 'sql' CHECK (kind IN ('sql', 'process')),
  job_id              TEXT REFERENCES jobs(id) ON DELETE SET NULL,
  job_revision        INTEGER,
  origin              TEXT NOT NULL CHECK (origin IN ('manual')),
  state               TEXT NOT NULL CHECK (state IN
                        ('running', 'succeeded', 'failed', 'cancelled', 'interrupted')),
  cause               TEXT,
  effect_unknown      INTEGER NOT NULL DEFAULT 0 CHECK (effect_unknown IN (0, 1)),
  reads_only          INTEGER NOT NULL CHECK (reads_only IN (0, 1)),
  snapshot_json       TEXT NOT NULL,
  fingerprint         TEXT NOT NULL,
  connection_id       TEXT,
  connection_revision INTEGER,
  rows_delivered      INTEGER,
  rows_discarded      INTEGER,
  rows_affected       INTEGER,
  truncated           INTEGER NOT NULL DEFAULT 0 CHECK (truncated IN (0, 1)),
  server_code         TEXT,
  exit_code           INTEGER,
  exit_signal         INTEGER,
  process_group       INTEGER,
  process_started     TEXT,
  program_fingerprint TEXT,
  failure_code        TEXT,
  log_bytes_stdout    INTEGER,
  log_bytes_stderr    INTEGER,
  log_bytes_jjp       INTEGER,
  outputs_stored      INTEGER,
  started_at          TEXT NOT NULL,
  finished_at         TEXT,
  CHECK ((state = 'running') = (finished_at IS NULL)),
  CHECK ((state = 'failed') = (cause IS NOT NULL)),
  CHECK ((kind = 'sql') = (connection_id IS NOT NULL)),
  CHECK ((kind = 'sql') = (connection_revision IS NOT NULL)),
  CHECK (kind = 'process' OR (exit_code IS NULL AND exit_signal IS NULL
         AND process_group IS NULL AND program_fingerprint IS NULL))
);
INSERT INTO runs_new (id, kind, job_id, job_revision, origin, state, cause, effect_unknown,
    reads_only, snapshot_json, fingerprint, connection_id, connection_revision,
    rows_delivered, rows_discarded, rows_affected, truncated, server_code,
    outputs_stored, started_at, finished_at)
  SELECT id, 'sql', job_id, job_revision, origin, state, cause, effect_unknown,
    reads_only, snapshot_json, fingerprint, connection_id, connection_revision,
    rows_delivered, rows_discarded, rows_affected, truncated, server_code,
    outputs_stored, started_at, finished_at FROM runs;

DROP TABLE runs;
DROP TABLE jobs;
ALTER TABLE jobs_new RENAME TO jobs;
ALTER TABLE runs_new RENAME TO runs;

CREATE INDEX runs_by_job   ON runs(job_id, started_at DESC);
CREATE INDEX runs_by_start ON runs(started_at);
CREATE INDEX runs_open     ON runs(state) WHERE state = 'running';
";

/// ADR-0036, migration 4: `jobs` is rebuilt for the kind `chain`; versions,
/// chain runs and the chain columns of `runs` arrive.
const STEP_4: &str = "
CREATE TABLE jobs_new (
  id              TEXT PRIMARY KEY,
  kind            TEXT NOT NULL CHECK (kind IN ('sql', 'process', 'chain')),
  definition_json TEXT NOT NULL,
  revision        INTEGER NOT NULL DEFAULT 1,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);
INSERT INTO jobs_new (id, kind, definition_json, revision, created_at, updated_at)
  SELECT id, kind, definition_json, revision, created_at, updated_at FROM jobs;
DROP TABLE jobs;
ALTER TABLE jobs_new RENAME TO jobs;

CREATE TABLE job_versions (
  job_id          TEXT NOT NULL REFERENCES jobs(id) ON DELETE RESTRICT,
  version         INTEGER NOT NULL CHECK (version >= 1),
  definition_json TEXT NOT NULL,
  fingerprint     TEXT NOT NULL,
  published_at    TEXT NOT NULL,
  PRIMARY KEY (job_id, version)
);

CREATE TABLE chain_runs (
  id             TEXT PRIMARY KEY,
  job_id         TEXT REFERENCES jobs(id) ON DELETE SET NULL,
  job_revision   INTEGER,
  origin         TEXT NOT NULL CHECK (origin IN ('manual')),
  state          TEXT NOT NULL CHECK (state IN
                   ('running', 'succeeded', 'failed', 'cancelled', 'interrupted')),
  cause          TEXT,
  failed_step    TEXT,
  effect_unknown INTEGER NOT NULL DEFAULT 0 CHECK (effect_unknown IN (0, 1)),
  snapshot_json  TEXT NOT NULL,
  fingerprint    TEXT NOT NULL,
  started_at     TEXT NOT NULL,
  finished_at    TEXT,
  CHECK ((state = 'running') = (finished_at IS NULL)),
  CHECK ((state = 'failed') = (cause IS NOT NULL))
);

ALTER TABLE runs ADD COLUMN chain_run_id TEXT REFERENCES chain_runs(id) ON DELETE CASCADE;
ALTER TABLE runs ADD COLUMN step_index   INTEGER;
ALTER TABLE runs ADD COLUMN job_version  INTEGER;

CREATE INDEX runs_by_chain       ON runs(chain_run_id, step_index) WHERE chain_run_id IS NOT NULL;
CREATE INDEX chain_runs_by_job   ON chain_runs(job_id, started_at DESC);
CREATE INDEX chain_runs_by_start ON chain_runs(started_at);
CREATE INDEX chain_runs_open     ON chain_runs(state) WHERE state = 'running';
";

/// ADR-0041, migration 5: schedules and their grants; `runs` and
/// `chain_runs` are rebuilt so the origin takes `schedule`, with the
/// schedule, the due time and the catch-up marker. `schedules` comes first:
/// the rebuilt tables reference it.
const STEP_5: &str = "
CREATE TABLE schedules (
  id                TEXT PRIMARY KEY,
  job_id            TEXT NOT NULL UNIQUE REFERENCES jobs(id) ON DELETE CASCADE,
  job_version       INTEGER,
  rule_json         TEXT NOT NULL,
  time_zone         TEXT NOT NULL,
  missed_policy     TEXT NOT NULL DEFAULT 'ignore' CHECK (missed_policy IN
                      ('ignore', 'run_once', 'queue')),
  missed_limit      INTEGER CHECK (missed_limit BETWEEN 2 AND 24),
  overlap_policy    TEXT NOT NULL DEFAULT 'skip' CHECK (overlap_policy IN
                      ('skip', 'queue_one', 'concurrent')),
  notify            INTEGER NOT NULL DEFAULT 0 CHECK (notify IN (0, 1)),
  revision          INTEGER NOT NULL DEFAULT 1,
  enabled           INTEGER NOT NULL CHECK (enabled IN (0, 1)),
  suspended         TEXT,
  last_due_at       TEXT,
  last_outcome      TEXT CHECK (last_outcome IN
                      ('started', 'caught_up', 'waiting', 'skipped_running',
                       'missed', 'refused', 'suspended')),
  last_reason       TEXT,
  last_run_id       TEXT REFERENCES runs(id) ON DELETE SET NULL,
  last_chain_run_id TEXT REFERENCES chain_runs(id) ON DELETE SET NULL,
  missed_count      INTEGER NOT NULL DEFAULT 0 CHECK (missed_count >= 0),
  missed_last_at    TEXT,
  created_at        TEXT NOT NULL,
  updated_at        TEXT NOT NULL,
  FOREIGN KEY (job_id, job_version) REFERENCES job_versions(job_id, version) ON DELETE RESTRICT,
  CHECK (enabled = 1 OR suspended IS NULL),
  CHECK ((missed_policy = 'queue') = (missed_limit IS NOT NULL))
);

CREATE TABLE grants (
  id                 TEXT PRIMARY KEY,
  kind               TEXT NOT NULL CHECK (kind IN ('schedule')),
  schedule_id        TEXT NOT NULL UNIQUE REFERENCES schedules(id) ON DELETE CASCADE,
  schedule_revision  INTEGER NOT NULL,
  target_fingerprint TEXT NOT NULL,
  job_revision       INTEGER,
  production         INTEGER NOT NULL CHECK (production IN (0, 1)),
  granted_at         TEXT NOT NULL
);

CREATE TABLE grant_connections (
  grant_id            TEXT NOT NULL REFERENCES grants(id) ON DELETE CASCADE,
  connection_id       TEXT NOT NULL,
  connection_revision INTEGER NOT NULL,
  environment         TEXT NOT NULL CHECK (environment IN
                        ('none', 'development', 'staging', 'production')),
  PRIMARY KEY (grant_id, connection_id)
);

CREATE INDEX grant_connections_by_connection ON grant_connections(connection_id);

CREATE TABLE runs_new (
  id                  TEXT PRIMARY KEY,
  kind                TEXT NOT NULL DEFAULT 'sql' CHECK (kind IN ('sql', 'process')),
  job_id              TEXT REFERENCES jobs(id) ON DELETE SET NULL,
  job_revision        INTEGER,
  origin              TEXT NOT NULL CHECK (origin IN ('manual', 'schedule')),
  state               TEXT NOT NULL CHECK (state IN
                        ('running', 'succeeded', 'failed', 'cancelled', 'interrupted')),
  cause               TEXT,
  effect_unknown      INTEGER NOT NULL DEFAULT 0 CHECK (effect_unknown IN (0, 1)),
  reads_only          INTEGER NOT NULL CHECK (reads_only IN (0, 1)),
  snapshot_json       TEXT NOT NULL,
  fingerprint         TEXT NOT NULL,
  connection_id       TEXT,
  connection_revision INTEGER,
  rows_delivered      INTEGER,
  rows_discarded      INTEGER,
  rows_affected       INTEGER,
  truncated           INTEGER NOT NULL DEFAULT 0 CHECK (truncated IN (0, 1)),
  server_code         TEXT,
  exit_code           INTEGER,
  exit_signal         INTEGER,
  process_group       INTEGER,
  process_started     TEXT,
  program_fingerprint TEXT,
  failure_code        TEXT,
  log_bytes_stdout    INTEGER,
  log_bytes_stderr    INTEGER,
  log_bytes_jjp       INTEGER,
  outputs_stored      INTEGER,
  started_at          TEXT NOT NULL,
  finished_at         TEXT,
  chain_run_id        TEXT REFERENCES chain_runs(id) ON DELETE CASCADE,
  step_index          INTEGER,
  job_version         INTEGER,
  schedule_id         TEXT REFERENCES schedules(id) ON DELETE SET NULL,
  due_at              TEXT,
  catch_up            INTEGER NOT NULL DEFAULT 0 CHECK (catch_up IN (0, 1)),
  CHECK ((state = 'running') = (finished_at IS NULL)),
  CHECK ((state = 'failed') = (cause IS NOT NULL)),
  CHECK ((kind = 'sql') = (connection_id IS NOT NULL)),
  CHECK ((kind = 'sql') = (connection_revision IS NOT NULL)),
  CHECK (kind = 'process' OR (exit_code IS NULL AND exit_signal IS NULL
         AND process_group IS NULL AND program_fingerprint IS NULL)),
  CHECK ((origin = 'schedule') = (due_at IS NOT NULL)),
  CHECK (origin = 'schedule' OR schedule_id IS NULL),
  CHECK (catch_up = 0 OR origin = 'schedule')
);
INSERT INTO runs_new (id, kind, job_id, job_revision, origin, state, cause, effect_unknown,
    reads_only, snapshot_json, fingerprint, connection_id, connection_revision,
    rows_delivered, rows_discarded, rows_affected, truncated, server_code, exit_code,
    exit_signal, process_group, process_started, program_fingerprint, failure_code,
    log_bytes_stdout, log_bytes_stderr, log_bytes_jjp, outputs_stored, started_at,
    finished_at, chain_run_id, step_index, job_version)
  SELECT id, kind, job_id, job_revision, origin, state, cause, effect_unknown,
    reads_only, snapshot_json, fingerprint, connection_id, connection_revision,
    rows_delivered, rows_discarded, rows_affected, truncated, server_code, exit_code,
    exit_signal, process_group, process_started, program_fingerprint, failure_code,
    log_bytes_stdout, log_bytes_stderr, log_bytes_jjp, outputs_stored, started_at,
    finished_at, chain_run_id, step_index, job_version FROM runs;

CREATE TABLE chain_runs_new (
  id             TEXT PRIMARY KEY,
  job_id         TEXT REFERENCES jobs(id) ON DELETE SET NULL,
  job_revision   INTEGER,
  origin         TEXT NOT NULL CHECK (origin IN ('manual', 'schedule')),
  state          TEXT NOT NULL CHECK (state IN
                   ('running', 'succeeded', 'failed', 'cancelled', 'interrupted')),
  cause          TEXT,
  failed_step    TEXT,
  effect_unknown INTEGER NOT NULL DEFAULT 0 CHECK (effect_unknown IN (0, 1)),
  snapshot_json  TEXT NOT NULL,
  fingerprint    TEXT NOT NULL,
  started_at     TEXT NOT NULL,
  finished_at    TEXT,
  schedule_id    TEXT REFERENCES schedules(id) ON DELETE SET NULL,
  due_at         TEXT,
  catch_up       INTEGER NOT NULL DEFAULT 0 CHECK (catch_up IN (0, 1)),
  CHECK ((state = 'running') = (finished_at IS NULL)),
  CHECK ((state = 'failed') = (cause IS NOT NULL)),
  CHECK ((origin = 'schedule') = (due_at IS NOT NULL)),
  CHECK (origin = 'schedule' OR schedule_id IS NULL),
  CHECK (catch_up = 0 OR origin = 'schedule')
);
INSERT INTO chain_runs_new (id, job_id, job_revision, origin, state, cause, failed_step,
    effect_unknown, snapshot_json, fingerprint, started_at, finished_at)
  SELECT id, job_id, job_revision, origin, state, cause, failed_step,
    effect_unknown, snapshot_json, fingerprint, started_at, finished_at FROM chain_runs;

DROP TABLE chain_runs;
ALTER TABLE chain_runs_new RENAME TO chain_runs;
DROP TABLE runs;
ALTER TABLE runs_new RENAME TO runs;

CREATE INDEX runs_by_job            ON runs(job_id, started_at DESC);
CREATE INDEX runs_by_start          ON runs(started_at);
CREATE INDEX runs_open              ON runs(state) WHERE state = 'running';
CREATE INDEX runs_by_chain          ON runs(chain_run_id, step_index) WHERE chain_run_id IS NOT NULL;
CREATE INDEX runs_by_schedule       ON runs(schedule_id, started_at DESC) WHERE schedule_id IS NOT NULL;
CREATE INDEX chain_runs_by_job      ON chain_runs(job_id, started_at DESC);
CREATE INDEX chain_runs_by_start    ON chain_runs(started_at);
CREATE INDEX chain_runs_open        ON chain_runs(state) WHERE state = 'running';
CREATE INDEX chain_runs_by_schedule ON chain_runs(schedule_id, started_at DESC) WHERE schedule_id IS NOT NULL;
";

const STEPS: [&str; 5] = [STEP_1, STEP_2, STEP_3, STEP_4, STEP_5];

/// Whether a file could be brought to the requested version.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Migrated {
    Current,
    /// The file's `user_version` is above what this build knows.
    Newer,
}

/// Brings the file to `target`: each missing step runs in one transaction on
/// a connection of its own with foreign keys off (SQLite's procedure for
/// changing a table), checks the keys before the commit and ends by setting
/// `user_version`. A file above `LATEST_VERSION` is refused untouched.
pub(crate) async fn migrate(pool: &SqlitePool, target: i64) -> DriverResult<Migrated> {
    let mut version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .map_err(sql_error)?;
    if version > LATEST_VERSION {
        return Ok(Migrated::Newer);
    }
    if version >= target {
        return Ok(Migrated::Current);
    }
    let mut connection = pool.acquire().await.map_err(sql_error)?.detach();
    sqlx::raw_sql("PRAGMA foreign_keys=OFF")
        .execute(&mut connection)
        .await
        .map_err(sql_error)?;
    while version < target {
        let step = usize::try_from(version)
            .ok()
            .and_then(|index| STEPS.get(index))
            .ok_or_else(|| error("Unknown automation schema version."))?;
        let mut tx = sqlx::Connection::begin(&mut connection)
            .await
            .map_err(sql_error)?;
        sqlx::raw_sql(*step)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        let broken = sqlx::query("PRAGMA foreign_key_check")
            .fetch_optional(&mut *tx)
            .await
            .map_err(sql_error)?;
        if broken.is_some() {
            return Err(error("The automation file holds a broken reference."));
        }
        let next = version + 1;
        let pragma: &'static str = match next {
            1 => "PRAGMA user_version=1",
            2 => "PRAGMA user_version=2",
            3 => "PRAGMA user_version=3",
            4 => "PRAGMA user_version=4",
            _ => "PRAGMA user_version=5",
        };
        sqlx::raw_sql(pragma)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)?;
        version = next;
    }
    sqlx::Connection::close(connection)
        .await
        .map_err(sql_error)?;
    Ok(Migrated::Current)
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// What opening `automation.db` produced.
#[derive(Debug)]
pub enum Opened {
    Ready(AutomationRepository),
    /// A corrupt file was set aside as `automation.db.corrupt-<millis>` and
    /// a new one created: the job definitions in it are lost.
    Recreated(AutomationRepository),
    /// The file comes from a newer version: automation stays off and the
    /// file is untouched.
    Newer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredJob {
    pub id: JobId,
    pub kind: JobKind,
    pub definition_json: String,
    pub revision: i64,
    pub created_at: UtcMillis,
    pub updated_at: UtcMillis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVersion {
    pub job_id: JobId,
    pub version: i64,
    pub definition_json: String,
    pub fingerprint: String,
    pub published_at: UtcMillis,
}

/// A chain that names a job or a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainRef {
    pub id: JobId,
    pub name: String,
}

/// Why a job or a version cannot be deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InUse {
    pub chains: Vec<ChainRef>,
    /// The job's schedule runs this version (ADR-0041); deleting the job
    /// deletes its schedule instead.
    pub schedule: Option<ScheduleId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRun {
    pub id: RunId,
    /// `Sql` or `Process`.
    pub kind: JobKind,
    pub job_id: Option<JobId>,
    pub job_revision: Option<i64>,
    pub job_version: Option<i64>,
    pub chain_run_id: Option<ChainRunId>,
    pub step_index: Option<i64>,
    pub origin: RunOrigin,
    pub reads_only: bool,
    /// The snapshot, without values; its fingerprint is stored with it.
    pub snapshot_json: String,
    pub connection_id: Option<ConnectionId>,
    pub connection_revision: Option<i64>,
    pub program_fingerprint: Option<String>,
    /// The schedule, due time and catch-up marker of a scheduled run, or of
    /// a scheduled chain's step (ADR-0041); `None` for a manual run.
    pub scheduled: Option<RunSchedule>,
    pub started_at: UtcMillis,
}

/// One declared output to store (ADR-0035). `value` is `None` for NULL.
/// `Debug` never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct OutputValue {
    pub name: String,
    pub kind: ParameterKind,
    pub value: Option<String>,
}

impl std::fmt::Debug for OutputValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputValue")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("null", &self.value.is_none())
            .field("bytes", &self.value.as_deref().map_or(0, str::len))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFinish {
    /// Terminal: never `Running`.
    pub state: RunState,
    /// Required with `Failed`, refused otherwise.
    pub cause: Option<Cause>,
    pub effect_unknown: bool,
    pub rows_delivered: Option<i64>,
    pub rows_discarded: Option<i64>,
    pub rows_affected: Option<i64>,
    pub truncated: bool,
    pub server_code: Option<String>,
    pub exit_code: Option<i64>,
    pub exit_signal: Option<i64>,
    pub failure_code: Option<String>,
    pub log_bytes_stdout: Option<i64>,
    pub log_bytes_stderr: Option<i64>,
    pub log_bytes_jjp: Option<i64>,
    /// `Some` when the job opted in to persisting outputs.
    pub outputs: Option<Vec<OutputValue>>,
    pub finished_at: UtcMillis,
}

impl Default for RunFinish {
    /// A succeeded run finished at the epoch: callers set what differs.
    fn default() -> Self {
        Self {
            state: RunState::Succeeded,
            cause: None,
            effect_unknown: false,
            rows_delivered: None,
            rows_discarded: None,
            rows_affected: None,
            truncated: false,
            server_code: None,
            exit_code: None,
            exit_signal: None,
            failure_code: None,
            log_bytes_stdout: None,
            log_bytes_stderr: None,
            log_bytes_jjp: None,
            outputs: None,
            finished_at: UtcMillis(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChainRun {
    pub id: ChainRunId,
    pub job_id: Option<JobId>,
    pub job_revision: Option<i64>,
    pub origin: RunOrigin,
    pub snapshot_json: String,
    /// As [`NewRun::scheduled`].
    pub scheduled: Option<RunSchedule>,
    pub started_at: UtcMillis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainFinish {
    pub state: RunState,
    pub cause: Option<Cause>,
    pub failed_step: Option<String>,
    pub effect_unknown: bool,
    pub finished_at: UtcMillis,
}

impl Default for ChainFinish {
    fn default() -> Self {
        Self {
            state: RunState::Succeeded,
            cause: None,
            failed_step: None,
            effect_unknown: false,
            finished_at: UtcMillis(0),
        }
    }
}

/// A program run recovery found with a recorded process group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub run_id: RunId,
    pub job_id: Option<JobId>,
    pub process_group: i64,
    pub process_started: Option<String>,
}

/// What recovery changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Recovered {
    pub runs: Vec<RunRecord>,
    pub chain_runs: Vec<ChainRunRecord>,
    pub orphans: Vec<Orphan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryFilter {
    pub job: Option<JobId>,
    pub state: Option<RunState>,
    /// Keyset cursor: the `started_at` and the ID (as text) of the last entry
    /// of the previous page.
    pub before: Option<(UtcMillis, String)>,
    pub limit: u32,
}

impl Default for HistoryFilter {
    fn default() -> Self {
        Self {
            job: None,
            state: None,
            before: None,
            limit: 50,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryEntry {
    /// Boxed: a run record is much larger than a chain run's.
    Run(Box<RunRecord>),
    Chain(ChainRunRecord),
}

impl HistoryEntry {
    pub fn started_at(&self) -> UtcMillis {
        match self {
            Self::Run(run) => run.started_at,
            Self::Chain(chain) => chain.started_at,
        }
    }

    /// The ID as text, the second half of the keyset cursor.
    pub fn id_text(&self) -> String {
        match self {
            Self::Run(run) => run.id.to_string(),
            Self::Chain(chain) => chain.id.to_string(),
        }
    }
}

/// What a pruning pass removed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pruned {
    /// Output values emptied (7 days).
    pub outputs_expired: u64,
    /// Runs deleted, including the steps of deleted chain runs (90 days).
    pub runs_deleted: u64,
    pub chain_runs_deleted: u64,
    /// The deleted program runs, whose log folders the caller removes.
    pub program_runs: Vec<RunId>,
}

/// What the user edits of a schedule (ADR-0041). The rule, the zone and the
/// policies are what the consent covers; `notify` is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleDraft {
    pub id: ScheduleId,
    pub job_id: JobId,
    /// The published version it runs; `None` for a chain.
    pub job_version: Option<i64>,
    /// Canonical JSON of the rule (specification, section 9.5.2).
    pub rule_json: String,
    /// An IANA name.
    pub time_zone: String,
    pub missed_policy: MissedPolicy,
    /// N of [`MissedPolicy::Queue`], within `MISSED_LIMIT_RANGE`; `None`
    /// with any other policy.
    pub missed_limit: Option<i64>,
    pub overlap_policy: OverlapPolicy,
    /// An operating-system notification when a run ends badly (Q-40).
    pub notify: bool,
}

/// A schedule as stored, with its bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSchedule {
    pub draft: ScheduleDraft,
    /// Grows when the rule, the zone or a policy changes.
    pub revision: i64,
    pub enabled: bool,
    /// The suspension's reason code; `None` when not suspended.
    pub suspended: Option<String>,
    /// The latest due time handled, whatever its outcome.
    pub last_due_at: Option<UtcMillis>,
    pub last_outcome: Option<ScheduleOutcome>,
    pub last_reason: Option<String>,
    pub last_run_id: Option<RunId>,
    pub last_chain_run_id: Option<ChainRunId>,
    pub missed_count: i64,
    pub missed_last_at: Option<UtcMillis>,
    pub created_at: UtcMillis,
    pub updated_at: UtcMillis,
}

/// One connection a grant binds: its profile revision and environment label
/// (`none`, `development`, `staging` or `production`) when consent was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantConnection {
    pub connection_id: ConnectionId,
    pub connection_revision: i64,
    pub environment: String,
}

/// The consent of one schedule (specification, section 9.5.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub id: GrantId,
    pub schedule_id: ScheduleId,
    /// The schedule's revision consented to.
    pub schedule_revision: i64,
    /// The version's fingerprint, or the chain plan's.
    pub target_fingerprint: String,
    /// A chain's revision; `None` for a version.
    pub job_revision: Option<i64>,
    /// A production confirmation was part of it.
    pub production: bool,
    pub connections: Vec<GrantConnection>,
    pub granted_at: UtcMillis,
}

/// What a schedule's due time came to, for its bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleMark {
    pub due_at: UtcMillis,
    pub outcome: ScheduleOutcome,
    /// A stable code (`queue_full`, `coalesced`, …); never a message.
    pub reason: Option<String>,
    pub run_id: Option<RunId>,
    pub chain_run_id: Option<ChainRunId>,
    /// Missed times to add to the count, and the latest of them.
    pub missed: i64,
    pub missed_last_at: Option<UtcMillis>,
}

impl ScheduleMark {
    pub fn new(due_at: UtcMillis, outcome: ScheduleOutcome) -> Self {
        Self {
            due_at,
            outcome,
            reason: None,
            run_id: None,
            chain_run_id: None,
            missed: 0,
            missed_last_at: None,
        }
    }
}

/// Why a schedule could not be saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleRefused {
    /// The schedule changed since it was read, or is gone.
    Changed,
    /// The job already has another schedule.
    Taken,
}

// ---------------------------------------------------------------------------
// Row mapping
// ---------------------------------------------------------------------------

macro_rules! run_columns {
    () => {
        "id, kind, job_id, job_revision, job_version, chain_run_id, step_index, origin, state, \
         cause, effect_unknown, reads_only, fingerprint, connection_id, connection_revision, \
         rows_delivered, rows_discarded, rows_affected, truncated, server_code, exit_code, \
         exit_signal, process_group, process_started, program_fingerprint, failure_code, \
         log_bytes_stdout, log_bytes_stderr, log_bytes_jjp, outputs_stored, schedule_id, \
         due_at, catch_up, started_at, finished_at"
    };
}

macro_rules! chain_columns {
    () => {
        "id, job_id, job_revision, origin, state, cause, failed_step, effect_unknown, \
         fingerprint, schedule_id, due_at, catch_up, started_at, finished_at"
    };
}

macro_rules! schedule_columns {
    () => {
        "id, job_id, job_version, rule_json, time_zone, missed_policy, missed_limit, \
         overlap_policy, notify, revision, enabled, suspended, last_due_at, last_outcome, \
         last_reason, last_run_id, last_chain_run_id, missed_count, missed_last_at, created_at, \
         updated_at"
    };
}

fn corrupt() -> DriverError {
    error("The automation file holds data this version cannot read.")
}

fn get<'r, T>(row: &'r SqliteRow, column: &str) -> DriverResult<T>
where
    T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
{
    row.try_get(column).map_err(sql_error)
}

fn time_of(row: &SqliteRow, column: &str) -> DriverResult<UtcMillis> {
    parse_time(&get::<String>(row, column)?).ok_or_else(corrupt)
}

fn optional_time_of(row: &SqliteRow, column: &str) -> DriverResult<Option<UtcMillis>> {
    get::<Option<String>>(row, column)?
        .map(|text| parse_time(&text).ok_or_else(corrupt))
        .transpose()
}

fn parsed<T: std::str::FromStr>(text: &str) -> DriverResult<T> {
    text.parse().map_err(|_| corrupt())
}

fn optional_parsed<T: std::str::FromStr>(row: &SqliteRow, column: &str) -> DriverResult<Option<T>> {
    get::<Option<String>>(row, column)?
        .map(|text| parsed(&text))
        .transpose()
}

fn cause_of(row: &SqliteRow) -> DriverResult<Option<Cause>> {
    get::<Option<String>>(row, "cause")?
        .map(|code| Cause::parse(&code).ok_or_else(corrupt))
        .transpose()
}

fn state_of(row: &SqliteRow) -> DriverResult<RunState> {
    RunState::parse(&get::<String>(row, "state")?).ok_or_else(corrupt)
}

fn origin_of(row: &SqliteRow) -> DriverResult<RunOrigin> {
    RunOrigin::parse(&get::<String>(row, "origin")?).ok_or_else(corrupt)
}

fn run_record(row: &SqliteRow) -> DriverResult<RunRecord> {
    Ok(RunRecord {
        id: parsed(&get::<String>(row, "id")?)?,
        kind: JobKind::parse(&get::<String>(row, "kind")?).ok_or_else(corrupt)?,
        job_id: optional_parsed(row, "job_id")?,
        job_revision: get(row, "job_revision")?,
        job_version: get(row, "job_version")?,
        chain_run_id: optional_parsed(row, "chain_run_id")?,
        step_index: get(row, "step_index")?,
        origin: origin_of(row)?,
        state: state_of(row)?,
        cause: cause_of(row)?,
        effect_unknown: get::<i64>(row, "effect_unknown")? != 0,
        reads_only: get::<i64>(row, "reads_only")? != 0,
        fingerprint: get(row, "fingerprint")?,
        connection_id: optional_parsed(row, "connection_id")?,
        connection_revision: get(row, "connection_revision")?,
        rows_delivered: get(row, "rows_delivered")?,
        rows_discarded: get(row, "rows_discarded")?,
        rows_affected: get(row, "rows_affected")?,
        truncated: get::<i64>(row, "truncated")? != 0,
        server_code: get(row, "server_code")?,
        exit_code: get(row, "exit_code")?,
        exit_signal: get(row, "exit_signal")?,
        process_group: get(row, "process_group")?,
        process_started: get(row, "process_started")?,
        program_fingerprint: get(row, "program_fingerprint")?,
        failure_code: get(row, "failure_code")?,
        log_bytes_stdout: get(row, "log_bytes_stdout")?,
        log_bytes_stderr: get(row, "log_bytes_stderr")?,
        log_bytes_jjp: get(row, "log_bytes_jjp")?,
        outputs_stored: get::<Option<i64>>(row, "outputs_stored")?.map(|stored| stored != 0),
        schedule_id: optional_parsed(row, "schedule_id")?,
        due_at: optional_time_of(row, "due_at")?,
        catch_up: get::<i64>(row, "catch_up")? != 0,
        started_at: time_of(row, "started_at")?,
        finished_at: optional_time_of(row, "finished_at")?,
    })
}

fn chain_record(row: &SqliteRow) -> DriverResult<ChainRunRecord> {
    Ok(ChainRunRecord {
        id: parsed(&get::<String>(row, "id")?)?,
        job_id: optional_parsed(row, "job_id")?,
        job_revision: get(row, "job_revision")?,
        origin: origin_of(row)?,
        state: state_of(row)?,
        cause: cause_of(row)?,
        failed_step: get(row, "failed_step")?,
        effect_unknown: get::<i64>(row, "effect_unknown")? != 0,
        fingerprint: get(row, "fingerprint")?,
        schedule_id: optional_parsed(row, "schedule_id")?,
        due_at: optional_time_of(row, "due_at")?,
        catch_up: get::<i64>(row, "catch_up")? != 0,
        started_at: time_of(row, "started_at")?,
        finished_at: optional_time_of(row, "finished_at")?,
    })
}

fn schedule_record(row: &SqliteRow) -> DriverResult<StoredSchedule> {
    Ok(StoredSchedule {
        draft: ScheduleDraft {
            id: parsed(&get::<String>(row, "id")?)?,
            job_id: parsed(&get::<String>(row, "job_id")?)?,
            job_version: get(row, "job_version")?,
            rule_json: get(row, "rule_json")?,
            time_zone: get(row, "time_zone")?,
            missed_policy: MissedPolicy::parse(&get::<String>(row, "missed_policy")?)
                .ok_or_else(corrupt)?,
            missed_limit: get(row, "missed_limit")?,
            overlap_policy: OverlapPolicy::parse(&get::<String>(row, "overlap_policy")?)
                .ok_or_else(corrupt)?,
            notify: get::<i64>(row, "notify")? != 0,
        },
        revision: get(row, "revision")?,
        enabled: get::<i64>(row, "enabled")? != 0,
        suspended: get(row, "suspended")?,
        last_due_at: optional_time_of(row, "last_due_at")?,
        last_outcome: get::<Option<String>>(row, "last_outcome")?
            .map(|code| ScheduleOutcome::parse(&code).ok_or_else(corrupt))
            .transpose()?,
        last_reason: get(row, "last_reason")?,
        last_run_id: optional_parsed(row, "last_run_id")?,
        last_chain_run_id: optional_parsed(row, "last_chain_run_id")?,
        missed_count: get(row, "missed_count")?,
        missed_last_at: optional_time_of(row, "missed_last_at")?,
        created_at: time_of(row, "created_at")?,
        updated_at: time_of(row, "updated_at")?,
    })
}

fn job_record(row: &SqliteRow) -> DriverResult<StoredJob> {
    Ok(StoredJob {
        id: parsed(&get::<String>(row, "id")?)?,
        kind: JobKind::parse(&get::<String>(row, "kind")?).ok_or_else(corrupt)?,
        definition_json: get(row, "definition_json")?,
        revision: get(row, "revision")?,
        created_at: time_of(row, "created_at")?,
        updated_at: time_of(row, "updated_at")?,
    })
}

fn version_record(row: &SqliteRow) -> DriverResult<StoredVersion> {
    Ok(StoredVersion {
        job_id: parsed(&get::<String>(row, "job_id")?)?,
        version: get(row, "version")?,
        definition_json: get(row, "definition_json")?,
        fingerprint: get(row, "fingerprint")?,
        published_at: time_of(row, "published_at")?,
    })
}

fn output_record(row: &SqliteRow) -> DriverResult<RunOutput> {
    Ok(RunOutput {
        position: get(row, "position")?,
        name: get(row, "name")?,
        kind: parse_parameter_kind(&get::<String>(row, "kind")?).ok_or_else(corrupt)?,
        value: get(row, "value")?,
        is_null: get::<i64>(row, "is_null")? != 0,
        bytes: get(row, "bytes")?,
        value_expires_at: time_of(row, "value_expires_at")?,
        expired: get::<i64>(row, "expired")? != 0,
    })
}

/// Definitions are canonical JSON; `json_each` in a query would fail on
/// anything else, so it never reaches the file.
fn check_json(text: &str) -> DriverResult<()> {
    serde_json::from_str::<serde_json::Value>(text)
        .map(|_| ())
        .map_err(|_| DriverError::validation("The definition is not valid JSON."))
}

fn ids_json<T: ToString>(ids: &[T]) -> DriverResult<String> {
    serde_json::to_string(&ids.iter().map(ToString::to_string).collect::<Vec<_>>())
        .map_err(crate::json_error)
}

fn flag(value: bool) -> i64 {
    i64::from(value)
}

// ---------------------------------------------------------------------------
// The repository
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AutomationRepository {
    pool: SqlitePool,
}

/// Whether a file that failed to open is confirmed corrupt (not a database,
/// or failing `quick_check`); permission and I/O errors are not.
async fn confirmed_corrupt(path: &Path) -> bool {
    let is_corrupt_code = |error: &sqlx::Error| {
        error
            .as_database_error()
            .and_then(|db| db.code())
            .is_some_and(|code| code == "26" || code == "11")
    };
    match connect_plain(path).await {
        Ok(mut connection) => {
            let result = sqlx::query_scalar::<_, String>("PRAGMA quick_check")
                .fetch_one(&mut connection)
                .await;
            let _ = sqlx::Connection::close(connection).await;
            match result {
                Ok(value) => value != "ok",
                Err(error) => is_corrupt_code(&error),
            }
        }
        Err(error) => is_corrupt_code(&error),
    }
}

async fn connect_plain(path: &Path) -> Result<SqliteConnection, sqlx::Error> {
    SqliteConnectOptions::new()
        .filename(path)
        .disable_statement_logging()
        .connect()
        .await
}

impl AutomationRepository {
    /// Opens (creating, migrating or recovering) `dir/automation.db`. The
    /// caller holds the [`AutomationLock`] first.
    ///
    /// A confirmed corrupt file is set aside and a new one created
    /// ([`Opened::Recreated`]). A marker, `automation.db.recovering`, is
    /// written before anything moves and removed once the new file opened,
    /// so a recovery cut short by a failure or a crash (the file already set
    /// aside, the new one not yet made) is finished at the next open and
    /// still reported as recreated, never as a ready, empty file.
    pub async fn open(dir: &Path) -> DriverResult<Opened> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|_| error("Could not create the data folder."))?;
        let path = dir.join(AUTOMATION_FILE);
        let marker = dir.join(format!("{AUTOMATION_FILE}.recovering"));
        let mut recreated = tokio::fs::try_exists(&marker)
            .await
            .map_err(|_| error("Could not verify the automation file."))?;
        let pool = match open_pool_with(&path, true, true).await {
            Ok(pool) => pool,
            Err(_) => {
                if !confirmed_corrupt(&path).await {
                    return Err(error("Could not open the automation file."));
                }
                tokio::fs::write(&marker, b"")
                    .await
                    .map_err(|_| error("Could not preserve the corrupted file."))?;
                recreated = true;
                let stamp = UtcMillis::now().0;
                // The journal files first: left beside a new file, SQLite
                // would read them as its own. The file itself goes last.
                for suffix in ["-wal", "-shm", ""] {
                    let from = dir.join(format!("{AUTOMATION_FILE}{suffix}"));
                    if tokio::fs::try_exists(&from)
                        .await
                        .map_err(|_| error("Could not verify the automation file."))?
                    {
                        tokio::fs::rename(
                            &from,
                            dir.join(format!("{AUTOMATION_FILE}.corrupt-{stamp}{suffix}")),
                        )
                        .await
                        .map_err(|_| error("Could not preserve the corrupted file."))?;
                    }
                }
                open_pool_with(&path, true, true).await?
            }
        };
        if migrate(&pool, LATEST_VERSION).await? == Migrated::Newer {
            pool.close().await;
            return Ok(Opened::Newer);
        }
        if recreated {
            // Left behind, the next open says again that the jobs were
            // lost, which is still true of this file.
            let _ = tokio::fs::remove_file(&marker).await;
        }
        let repository = Self { pool };
        Ok(if recreated {
            Opened::Recreated(repository)
        } else {
            Opened::Ready(repository)
        })
    }

    /// Opens `dir/automation.db` for reading only, for an instance that does
    /// not hold the [`AutomationLock`]: it never creates, migrates or sets
    /// aside the file, which belongs to the lock holder (ADR-0034, decision
    /// 4). `Ok(None)` when there is no file yet, or it is not yet at this
    /// version's schema (the holder migrates it); `Newer` when a newer
    /// version wrote it.
    pub async fn open_read_only(dir: &Path) -> DriverResult<Option<Opened>> {
        let path = dir.join(AUTOMATION_FILE);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(None);
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .read_only(true)
                    .create_if_missing(false)
                    .busy_timeout(std::time::Duration::from_secs(5))
                    .disable_statement_logging(),
            )
            .await
            .map_err(sql_error)?;
        // Read only after the same check as the lock holder's open; a file
        // that fails it is left where it is, for the holder to set aside.
        let checked = sqlx::query_scalar::<_, String>("PRAGMA quick_check")
            .fetch_one(&pool)
            .await;
        if !matches!(checked, Ok(ref value) if value == "ok") {
            pool.close().await;
            return Err(error("Invalid or corrupted SQLite file."));
        }
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .map_err(sql_error)?;
        if version > LATEST_VERSION {
            pool.close().await;
            return Ok(Some(Opened::Newer));
        }
        if version < LATEST_VERSION {
            pool.close().await;
            return Ok(None);
        }
        Ok(Some(Opened::Ready(Self { pool })))
    }

    /// A write transaction that takes the write lock at once, so a read
    /// followed by a write cannot fail on a stale snapshot.
    async fn write(&self) -> DriverResult<sqlx::Transaction<'static, sqlx::Sqlite>> {
        self.pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(sql_error)
    }

    // ----- jobs -----

    pub async fn jobs(&self) -> DriverResult<Vec<StoredJob>> {
        sqlx::query(
            "SELECT id, kind, definition_json, revision, created_at, updated_at \
             FROM jobs ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(sql_error)?
        .iter()
        .map(job_record)
        .collect()
    }

    pub async fn job(&self, id: JobId) -> DriverResult<Option<StoredJob>> {
        sqlx::query(
            "SELECT id, kind, definition_json, revision, created_at, updated_at \
             FROM jobs WHERE id=?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(sql_error)?
        .as_ref()
        .map(job_record)
        .transpose()
    }

    pub async fn insert_job(
        &self,
        id: JobId,
        kind: JobKind,
        definition_json: &str,
        now: UtcMillis,
    ) -> DriverResult<StoredJob> {
        check_json(definition_json)?;
        let stamp = format_time(now);
        sqlx::query(
            "INSERT INTO jobs (id, kind, definition_json, revision, created_at, updated_at) \
             VALUES (?, ?, ?, 1, ?, ?)",
        )
        .bind(id.to_string())
        .bind(kind.as_str())
        .bind(definition_json)
        .bind(&stamp)
        .bind(&stamp)
        .execute(&self.pool)
        .await
        .map_err(sql_error)?;
        self.job(id)
            .await?
            .ok_or_else(|| error("Could not read the job back."))
    }

    /// Saves a new definition and raises the revision by one. Refused when
    /// the stored revision is not `expected_revision` (the job changed since
    /// it was read) or when the definition names another kind.
    pub async fn update_job(
        &self,
        id: JobId,
        definition_json: &str,
        expected_revision: i64,
        now: UtcMillis,
    ) -> DriverResult<StoredJob> {
        check_json(definition_json)?;
        let mut tx = self.write().await?;
        let row = sqlx::query("SELECT kind, revision FROM jobs WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(sql_error)?
            .ok_or_else(|| DriverError::new(ErrorCode::NotFound, "Job not found."))?;
        let kind: String = get(&row, "kind")?;
        let revision: i64 = get(&row, "revision")?;
        if revision != expected_revision {
            return Err(DriverError::validation(
                "The job changed since it was read.",
            ));
        }
        let named: Option<String> = sqlx::query_scalar("SELECT json_extract(?, '$.kind')")
            .bind(definition_json)
            .fetch_one(&mut *tx)
            .await
            .map_err(sql_error)?;
        if named.is_some_and(|named| named != kind) {
            return Err(DriverError::validation("A job cannot change its kind."));
        }
        sqlx::query(
            "UPDATE jobs SET definition_json=?, revision=revision+1, updated_at=? WHERE id=?",
        )
        .bind(definition_json)
        .bind(format_time(now))
        .bind(id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)?;
        self.job(id)
            .await?
            .ok_or_else(|| error("Could not read the job back."))
    }

    /// Deletes a job, its versions and its schedule with the schedule's
    /// grant, in one transaction, unless a chain names any of its versions.
    /// Runs keep their rows with no job and no schedule.
    pub async fn delete_job(&self, id: JobId) -> DriverResult<Result<(), InUse>> {
        let mut tx = self.write().await?;
        let chains = chains_using_in(&mut tx, id, None).await?;
        if !chains.is_empty() {
            return Ok(Err(InUse {
                chains,
                schedule: None,
            }));
        }
        // First: the schedule's key on its version would refuse the
        // versions' deletion.
        sqlx::query("DELETE FROM schedules WHERE job_id=?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        sqlx::query("DELETE FROM job_versions WHERE job_id=?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        let deleted = sqlx::query("DELETE FROM jobs WHERE id=?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?
            .rows_affected();
        if deleted == 0 {
            return Err(DriverError::new(ErrorCode::NotFound, "Job not found."));
        }
        tx.commit().await.map_err(sql_error)?;
        Ok(Ok(()))
    }

    // ----- versions -----

    /// Copies the job's definition (as given) into the next version, from 1.
    /// Chains are not published.
    pub async fn publish(
        &self,
        job: JobId,
        definition_json: &str,
        now: UtcMillis,
    ) -> DriverResult<StoredVersion> {
        check_json(definition_json)?;
        let mut tx = self.write().await?;
        let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM jobs WHERE id=?")
            .bind(job.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(sql_error)?;
        match kind.as_deref() {
            None => {
                return Err(DriverError::new(ErrorCode::NotFound, "Job not found."));
            }
            Some("chain") => {
                return Err(DriverError::validation("A chain cannot be published."));
            }
            Some(_) => {}
        }
        // A number is never given twice while anything still names it: past
        // the versions kept and the step runs that ran a deleted one, so its
        // history and the first-run warning (Q-10) never mix two definitions.
        let version: i64 = sqlx::query_scalar(
            "SELECT MAX(COALESCE((SELECT MAX(version) FROM job_versions WHERE job_id=?1), 0), \
             COALESCE((SELECT MAX(job_version) FROM runs WHERE job_id=?1), 0)) + 1",
        )
        .bind(job.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(sql_error)?;
        sqlx::query(
            "INSERT INTO job_versions (job_id, version, definition_json, fingerprint, \
             published_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(job.to_string())
        .bind(version)
        .bind(definition_json)
        .bind(fingerprint(definition_json.as_bytes()))
        .bind(format_time(now))
        .execute(&mut *tx)
        .await
        .map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)?;
        self.version(job, version)
            .await?
            .ok_or_else(|| error("Could not read the version back."))
    }

    /// Newest first.
    pub async fn versions(&self, job: JobId) -> DriverResult<Vec<StoredVersion>> {
        sqlx::query(
            "SELECT job_id, version, definition_json, fingerprint, published_at \
             FROM job_versions WHERE job_id=? ORDER BY version DESC",
        )
        .bind(job.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(sql_error)?
        .iter()
        .map(version_record)
        .collect()
    }

    pub async fn version(&self, job: JobId, version: i64) -> DriverResult<Option<StoredVersion>> {
        sqlx::query(
            "SELECT job_id, version, definition_json, fingerprint, published_at \
             FROM job_versions WHERE job_id=? AND version=?",
        )
        .bind(job.to_string())
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(sql_error)?
        .as_ref()
        .map(version_record)
        .transpose()
    }

    /// Refused, naming the chains or the schedule, while any chain or the
    /// job's schedule uses the version.
    pub async fn delete_version(
        &self,
        job: JobId,
        version: i64,
    ) -> DriverResult<Result<(), InUse>> {
        let mut tx = self.write().await?;
        let chains = chains_using_in(&mut tx, job, Some(version)).await?;
        let schedule: Option<String> =
            sqlx::query_scalar("SELECT id FROM schedules WHERE job_id=? AND job_version=?")
                .bind(job.to_string())
                .bind(version)
                .fetch_optional(&mut *tx)
                .await
                .map_err(sql_error)?;
        let schedule = schedule.map(|id| parsed::<ScheduleId>(&id)).transpose()?;
        if !chains.is_empty() || schedule.is_some() {
            return Ok(Err(InUse { chains, schedule }));
        }
        let deleted = sqlx::query("DELETE FROM job_versions WHERE job_id=? AND version=?")
            .bind(job.to_string())
            .bind(version)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?
            .rows_affected();
        if deleted == 0 {
            return Err(DriverError::new(ErrorCode::NotFound, "Version not found."));
        }
        tx.commit().await.map_err(sql_error)?;
        Ok(Ok(()))
    }

    /// The chains whose steps name `job` (and `version`, when given).
    pub async fn chains_using(
        &self,
        job: JobId,
        version: Option<i64>,
    ) -> DriverResult<Vec<ChainRef>> {
        let mut connection = self.pool.acquire().await.map_err(sql_error)?;
        chains_using_in(&mut connection, job, version).await
    }

    // ----- schedules and grants (ADR-0041) -----

    /// Every schedule, oldest first.
    pub async fn schedules(&self) -> DriverResult<Vec<StoredSchedule>> {
        sqlx::query(concat!(
            "SELECT ",
            schedule_columns!(),
            " FROM schedules ORDER BY created_at, id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(sql_error)?
        .iter()
        .map(schedule_record)
        .collect()
    }

    pub async fn schedule(&self, id: ScheduleId) -> DriverResult<Option<StoredSchedule>> {
        sqlx::query(concat!(
            "SELECT ",
            schedule_columns!(),
            " FROM schedules WHERE id=?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(sql_error)?
        .as_ref()
        .map(schedule_record)
        .transpose()
    }

    /// The schedule of a job or chain, if it has one.
    pub async fn schedule_of_job(&self, job: JobId) -> DriverResult<Option<StoredSchedule>> {
        sqlx::query(concat!(
            "SELECT ",
            schedule_columns!(),
            " FROM schedules WHERE job_id=?"
        ))
        .bind(job.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(sql_error)?
        .as_ref()
        .map(schedule_record)
        .transpose()
    }

    /// Saves a new schedule (`expected_revision` `None`), always off until
    /// consent is given, or changes one at that revision. A change to what
    /// the consent covers (the version, the rule, the zone, a policy or N)
    /// raises the revision, turns it off and deletes its grant: it asks
    /// again. A change of `notify` alone keeps both.
    pub async fn save_schedule(
        &self,
        draft: &ScheduleDraft,
        expected_revision: Option<i64>,
        now: UtcMillis,
    ) -> DriverResult<Result<StoredSchedule, ScheduleRefused>> {
        check_json(&draft.rule_json)?;
        let stamp = format_time(now);
        let mut tx = self.write().await?;
        match expected_revision {
            None => {
                let taken: Option<String> =
                    sqlx::query_scalar("SELECT id FROM schedules WHERE job_id=?")
                        .bind(draft.job_id.to_string())
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(sql_error)?;
                if taken.is_some() {
                    return Ok(Err(ScheduleRefused::Taken));
                }
                sqlx::query(
                    "INSERT INTO schedules (id, job_id, job_version, rule_json, time_zone, \
                     missed_policy, missed_limit, overlap_policy, notify, revision, enabled, \
                     created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 0, ?, ?)",
                )
                .bind(draft.id.to_string())
                .bind(draft.job_id.to_string())
                .bind(draft.job_version)
                .bind(&draft.rule_json)
                .bind(&draft.time_zone)
                .bind(draft.missed_policy.as_str())
                .bind(draft.missed_limit)
                .bind(draft.overlap_policy.as_str())
                .bind(flag(draft.notify))
                .bind(&stamp)
                .bind(&stamp)
                .execute(&mut *tx)
                .await
                .map_err(sql_error)?;
            }
            Some(revision) => {
                let stored = sqlx::query(concat!(
                    "SELECT ",
                    schedule_columns!(),
                    " FROM schedules WHERE id=?"
                ))
                .bind(draft.id.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(sql_error)?
                .as_ref()
                .map(schedule_record)
                .transpose()?;
                let Some(stored) = stored.filter(|stored| {
                    stored.revision == revision && stored.draft.job_id == draft.job_id
                }) else {
                    return Ok(Err(ScheduleRefused::Changed));
                };
                let consented = |draft: &ScheduleDraft| {
                    (
                        draft.job_version,
                        draft.rule_json.clone(),
                        draft.time_zone.clone(),
                        draft.missed_policy,
                        draft.missed_limit,
                        draft.overlap_policy,
                    )
                };
                if consented(&stored.draft) == consented(draft) {
                    sqlx::query("UPDATE schedules SET notify=?, updated_at=? WHERE id=?")
                        .bind(flag(draft.notify))
                        .bind(&stamp)
                        .bind(draft.id.to_string())
                        .execute(&mut *tx)
                        .await
                        .map_err(sql_error)?;
                } else {
                    sqlx::query(
                        "UPDATE schedules SET job_version=?, rule_json=?, time_zone=?, \
                         missed_policy=?, missed_limit=?, overlap_policy=?, notify=?, \
                         revision=revision+1, enabled=0, suspended=NULL, updated_at=? WHERE id=?",
                    )
                    .bind(draft.job_version)
                    .bind(&draft.rule_json)
                    .bind(&draft.time_zone)
                    .bind(draft.missed_policy.as_str())
                    .bind(draft.missed_limit)
                    .bind(draft.overlap_policy.as_str())
                    .bind(flag(draft.notify))
                    .bind(&stamp)
                    .bind(draft.id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(sql_error)?;
                    sqlx::query("DELETE FROM grants WHERE schedule_id=?")
                        .bind(draft.id.to_string())
                        .execute(&mut *tx)
                        .await
                        .map_err(sql_error)?;
                }
            }
        }
        tx.commit().await.map_err(sql_error)?;
        let saved = self
            .schedule(draft.id)
            .await?
            .ok_or_else(|| error("Could not read the schedule back."))?;
        Ok(Ok(saved))
    }

    /// Records consent: replaces the schedule's grant with `grant` and turns
    /// it on, no longer suspended. Refused when the schedule is not at the
    /// revision the grant names.
    pub async fn confirm_schedule(
        &self,
        grant: &Grant,
        now: UtcMillis,
    ) -> DriverResult<Result<StoredSchedule, ScheduleRefused>> {
        let mut tx = self.write().await?;
        let revision: Option<i64> = sqlx::query_scalar("SELECT revision FROM schedules WHERE id=?")
            .bind(grant.schedule_id.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(sql_error)?;
        if revision != Some(grant.schedule_revision) {
            return Ok(Err(ScheduleRefused::Changed));
        }
        sqlx::query("DELETE FROM grants WHERE schedule_id=?")
            .bind(grant.schedule_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        sqlx::query(
            "INSERT INTO grants (id, kind, schedule_id, schedule_revision, target_fingerprint, \
             job_revision, production, granted_at) VALUES (?, 'schedule', ?, ?, ?, ?, ?, ?)",
        )
        .bind(grant.id.to_string())
        .bind(grant.schedule_id.to_string())
        .bind(grant.schedule_revision)
        .bind(&grant.target_fingerprint)
        .bind(grant.job_revision)
        .bind(flag(grant.production))
        .bind(format_time(grant.granted_at))
        .execute(&mut *tx)
        .await
        .map_err(sql_error)?;
        for connection in &grant.connections {
            sqlx::query(
                "INSERT INTO grant_connections (grant_id, connection_id, connection_revision, \
                 environment) VALUES (?, ?, ?, ?)",
            )
            .bind(grant.id.to_string())
            .bind(connection.connection_id.to_string())
            .bind(connection.connection_revision)
            .bind(&connection.environment)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        }
        sqlx::query("UPDATE schedules SET enabled=1, suspended=NULL, updated_at=? WHERE id=?")
            .bind(format_time(now))
            .bind(grant.schedule_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)?;
        let saved = self
            .schedule(grant.schedule_id)
            .await?
            .ok_or_else(|| error("Could not read the schedule back."))?;
        Ok(Ok(saved))
    }

    /// Turns a schedule off and revokes it: its grant is deleted (Q-38).
    pub async fn disable_schedule(&self, id: ScheduleId, now: UtcMillis) -> DriverResult<()> {
        let mut tx = self.write().await?;
        sqlx::query("DELETE FROM grants WHERE schedule_id=?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        sqlx::query("UPDATE schedules SET enabled=0, suspended=NULL, updated_at=? WHERE id=?")
            .bind(format_time(now))
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)?;
        Ok(())
    }

    /// Deletes a schedule and its grant. Its runs keep their rows, with no
    /// schedule.
    pub async fn delete_schedule(&self, id: ScheduleId) -> DriverResult<()> {
        sqlx::query("DELETE FROM schedules WHERE id=?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(sql_error)?;
        Ok(())
    }

    /// Suspends an enabled schedule with a reason code, keeping its grant so
    /// **Confirm again** can say what changed. `false` when it was off or
    /// already suspended.
    pub async fn suspend_schedule(
        &self,
        id: ScheduleId,
        reason: &str,
        now: UtcMillis,
    ) -> DriverResult<bool> {
        let changed = sqlx::query(
            "UPDATE schedules SET suspended=?, last_outcome='suspended', last_reason=?, \
             updated_at=? WHERE id=? AND enabled=1 AND suspended IS NULL",
        )
        .bind(reason)
        .bind(reason)
        .bind(format_time(now))
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(sql_error)?
        .rows_affected();
        Ok(changed > 0)
    }

    /// The latest due time a run or chain run of `schedule` was admitted
    /// for. Its row is written before the run sends or starts anything, so
    /// the time counts as handled even when the schedule's own mark was
    /// never written (a failed write, or a crash just after admission).
    pub async fn latest_admitted_due(
        &self,
        schedule: ScheduleId,
    ) -> DriverResult<Option<UtcMillis>> {
        let latest: Option<String> = sqlx::query_scalar(
            "SELECT max(due_at) FROM (\
             SELECT max(due_at) AS due_at FROM runs WHERE schedule_id=?1 \
             UNION ALL SELECT max(due_at) FROM chain_runs WHERE schedule_id=?1)",
        )
        .bind(schedule.to_string())
        .fetch_one(&self.pool)
        .await
        .map_err(sql_error)?;
        latest
            .map(|text| parse_time(&text).ok_or_else(corrupt))
            .transpose()
    }

    /// Records what a due time came to. The last handled due time only moves
    /// forward, so a wall clock set back never makes a time new again. A run
    /// not yet admitted (its row is written by its owner) is linked later by
    /// [`AutomationRepository::link_schedule_run`]; the mark never waits for
    /// it.
    pub async fn mark_schedule(&self, id: ScheduleId, mark: &ScheduleMark) -> DriverResult<()> {
        let due = format_time(mark.due_at);
        // The last outcome, reason and run describe the latest due time
        // handled: a mark for an earlier one (a catch-up, or a refusal of
        // one, that ends after a later time ran) only adds to the counters.
        // SQLite reads every column below as it was before this update.
        sqlx::query(
            "UPDATE schedules SET \
             last_due_at=CASE WHEN last_due_at IS NULL OR last_due_at<?1 THEN ?1 ELSE last_due_at END, \
             last_outcome=CASE WHEN last_due_at IS NULL OR last_due_at<=?1 THEN ?2 ELSE last_outcome END, \
             last_reason=CASE WHEN last_due_at IS NULL OR last_due_at<=?1 THEN ?3 ELSE last_reason END, \
             last_run_id=CASE WHEN last_due_at IS NULL OR last_due_at<=?1 \
               THEN COALESCE((SELECT id FROM runs WHERE id=?4), last_run_id) ELSE last_run_id END, \
             last_chain_run_id=CASE WHEN last_due_at IS NULL OR last_due_at<=?1 \
               THEN COALESCE((SELECT id FROM chain_runs WHERE id=?5), last_chain_run_id) \
               ELSE last_chain_run_id END, \
             missed_count=missed_count+?6, \
             missed_last_at=COALESCE(?7, missed_last_at) WHERE id=?8",
        )
        .bind(&due)
        .bind(mark.outcome.as_str())
        .bind(mark.reason.as_deref())
        .bind(mark.run_id.map(|id| id.to_string()))
        .bind(mark.chain_run_id.map(|id| id.to_string()))
        .bind(mark.missed.max(0))
        .bind(mark.missed_last_at.map(format_time))
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(sql_error)?;
        Ok(())
    }

    /// Points the schedule at the run or chain run it started, once that
    /// run's row exists. `false` while it does not.
    pub async fn link_schedule_run(
        &self,
        id: ScheduleId,
        run: Option<RunId>,
        chain_run: Option<ChainRunId>,
    ) -> DriverResult<bool> {
        let changed = sqlx::query(
            "UPDATE schedules SET \
             last_run_id=COALESCE((SELECT id FROM runs WHERE id=?1), last_run_id), \
             last_chain_run_id=COALESCE((SELECT id FROM chain_runs WHERE id=?2), last_chain_run_id) \
             WHERE id=?3 AND (EXISTS(SELECT 1 FROM runs WHERE id=?1) \
               OR EXISTS(SELECT 1 FROM chain_runs WHERE id=?2))",
        )
        .bind(run.map(|id| id.to_string()))
        .bind(chain_run.map(|id| id.to_string()))
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(sql_error)?
        .rows_affected();
        Ok(changed > 0)
    }

    /// The schedule's grant, with the connections it binds.
    pub async fn grant(&self, schedule: ScheduleId) -> DriverResult<Option<Grant>> {
        let Some(row) = sqlx::query(
            "SELECT id, schedule_id, schedule_revision, target_fingerprint, job_revision, \
             production, granted_at FROM grants WHERE schedule_id=?",
        )
        .bind(schedule.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(sql_error)?
        else {
            return Ok(None);
        };
        let id: String = get(&row, "id")?;
        let connections = sqlx::query(
            "SELECT connection_id, connection_revision, environment FROM grant_connections \
             WHERE grant_id=? ORDER BY connection_id",
        )
        .bind(&id)
        .fetch_all(&self.pool)
        .await
        .map_err(sql_error)?
        .iter()
        .map(|row| {
            Ok(GrantConnection {
                connection_id: parsed(&get::<String>(row, "connection_id")?)?,
                connection_revision: get(row, "connection_revision")?,
                environment: get(row, "environment")?,
            })
        })
        .collect::<DriverResult<Vec<_>>>()?;
        Ok(Some(Grant {
            id: parsed(&id)?,
            schedule_id: parsed(&get::<String>(&row, "schedule_id")?)?,
            schedule_revision: get(&row, "schedule_revision")?,
            target_fingerprint: get(&row, "target_fingerprint")?,
            job_revision: get(&row, "job_revision")?,
            production: get::<i64>(&row, "production")? != 0,
            connections,
            granted_at: time_of(&row, "granted_at")?,
        }))
    }

    /// The schedules whose grants bind `connection`, through its index.
    pub async fn schedules_binding(
        &self,
        connection: ConnectionId,
    ) -> DriverResult<Vec<ScheduleId>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT g.schedule_id FROM grant_connections c JOIN grants g ON g.id=c.grant_id \
             WHERE c.connection_id=? ORDER BY g.schedule_id",
        )
        .bind(connection.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(sql_error)?;
        ids.iter().map(|id| parsed(id)).collect()
    }

    // ----- runs -----

    /// Writes the `running` row with its snapshot before any I/O. The
    /// fingerprint is the snapshot's.
    pub async fn admit_run(&self, run: &NewRun) -> DriverResult<()> {
        if run.kind == JobKind::Chain {
            return Err(DriverError::validation("A chain has no run of its own."));
        }
        check_json(&run.snapshot_json)?;
        sqlx::query(
            "INSERT INTO runs (id, kind, job_id, job_revision, job_version, chain_run_id, \
             step_index, origin, state, effect_unknown, reads_only, snapshot_json, fingerprint, \
             connection_id, connection_revision, program_fingerprint, schedule_id, due_at, \
             catch_up, started_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'running', 0, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run.id.to_string())
        .bind(run.kind.as_str())
        .bind(run.job_id.map(|id| id.to_string()))
        .bind(run.job_revision)
        .bind(run.job_version)
        .bind(run.chain_run_id.map(|id| id.to_string()))
        .bind(run.step_index)
        .bind(run.origin.as_str())
        .bind(flag(run.reads_only))
        .bind(&run.snapshot_json)
        .bind(fingerprint(run.snapshot_json.as_bytes()))
        .bind(run.connection_id.map(|id| id.to_string()))
        .bind(run.connection_revision)
        .bind(run.program_fingerprint.as_deref())
        .bind(
            run.scheduled
                .map(|scheduled| scheduled.schedule.to_string()),
        )
        .bind(run.scheduled.map(|scheduled| format_time(scheduled.due_at)))
        .bind(flag(
            run.scheduled.is_some_and(|scheduled| scheduled.catch_up),
        ))
        .bind(format_time(run.started_at))
        .execute(&self.pool)
        .await
        .map_err(sql_error)?;
        Ok(())
    }

    /// Records the program's process group and its start time while the run
    /// is `running`, for recovery.
    pub async fn set_process(
        &self,
        id: RunId,
        process_group: Option<i64>,
        process_started: Option<&str>,
    ) -> DriverResult<()> {
        let changed = sqlx::query(
            "UPDATE runs SET process_group=?, process_started=? \
             WHERE id=? AND state='running'",
        )
        .bind(process_group)
        .bind(process_started)
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(sql_error)?
        .rows_affected();
        if changed == 0 {
            return Err(DriverError::validation("The run is not running."));
        }
        Ok(())
    }

    /// Writes the terminal state once. Declared outputs, when the job opted
    /// in, are stored all or none within [`OUTPUT_QUOTA_BYTES`], in the same
    /// transaction.
    pub async fn finish_run(&self, id: RunId, finish: &RunFinish) -> DriverResult<()> {
        if !finish.state.is_terminal()
            || (finish.state == RunState::Failed) != finish.cause.is_some()
        {
            return Err(DriverError::validation("Invalid terminal state."));
        }
        let finished = format_time(finish.finished_at);
        let mut tx = self.write().await?;
        let state: Option<String> = sqlx::query_scalar("SELECT state FROM runs WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(sql_error)?;
        match state.as_deref() {
            None => {
                return Err(DriverError::new(ErrorCode::NotFound, "Run not found."));
            }
            Some("running") => {}
            Some(_) => return Err(DriverError::validation("The run already finished.")),
        }
        let stored = finish.outputs.as_ref().map(|outputs| {
            outputs
                .iter()
                .map(|output| output.value.as_ref().map_or(0, String::len))
                .sum::<usize>()
                <= OUTPUT_QUOTA_BYTES
        });
        sqlx::query(
            "UPDATE runs SET state=?, cause=?, effect_unknown=?, rows_delivered=?, \
             rows_discarded=?, rows_affected=?, truncated=?, server_code=?, exit_code=?, \
             exit_signal=?, failure_code=?, log_bytes_stdout=?, log_bytes_stderr=?, \
             log_bytes_jjp=?, outputs_stored=?, finished_at=? WHERE id=? AND state='running'",
        )
        .bind(finish.state.as_str())
        .bind(finish.cause.map(Cause::as_str))
        .bind(flag(finish.effect_unknown))
        .bind(finish.rows_delivered)
        .bind(finish.rows_discarded)
        .bind(finish.rows_affected)
        .bind(flag(finish.truncated))
        .bind(finish.server_code.as_deref())
        .bind(finish.exit_code)
        .bind(finish.exit_signal)
        .bind(finish.failure_code.as_deref())
        .bind(finish.log_bytes_stdout)
        .bind(finish.log_bytes_stderr)
        .bind(finish.log_bytes_jjp)
        .bind(stored.map(flag))
        .bind(&finished)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(sql_error)?;
        if let (Some(true), Some(outputs)) = (stored, finish.outputs.as_ref()) {
            let expires = format_time(finish.finished_at.plus(OUTPUT_VALUE_RETENTION));
            for (position, output) in outputs.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO run_outputs (run_id, position, name, kind, value, is_null, \
                     bytes, value_expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(id.to_string())
                .bind(i64::try_from(position).unwrap_or(i64::MAX))
                .bind(&output.name)
                .bind(parameter_kind_str(output.kind))
                .bind(output.value.as_deref())
                .bind(flag(output.value.is_none()))
                .bind(i64::try_from(output.value.as_ref().map_or(0, String::len)).unwrap_or(0))
                .bind(&expires)
                .execute(&mut *tx)
                .await
                .map_err(sql_error)?;
            }
        }
        tx.commit().await.map_err(sql_error)?;
        Ok(())
    }

    /// Writes a chain run's `running` row when its plan is confirmed.
    pub async fn admit_chain_run(&self, run: &NewChainRun) -> DriverResult<()> {
        check_json(&run.snapshot_json)?;
        sqlx::query(
            "INSERT INTO chain_runs (id, job_id, job_revision, origin, state, effect_unknown, \
             snapshot_json, fingerprint, schedule_id, due_at, catch_up, started_at) \
             VALUES (?, ?, ?, ?, 'running', 0, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run.id.to_string())
        .bind(run.job_id.map(|id| id.to_string()))
        .bind(run.job_revision)
        .bind(run.origin.as_str())
        .bind(&run.snapshot_json)
        .bind(fingerprint(run.snapshot_json.as_bytes()))
        .bind(
            run.scheduled
                .map(|scheduled| scheduled.schedule.to_string()),
        )
        .bind(run.scheduled.map(|scheduled| format_time(scheduled.due_at)))
        .bind(flag(
            run.scheduled.is_some_and(|scheduled| scheduled.catch_up),
        ))
        .bind(format_time(run.started_at))
        .execute(&self.pool)
        .await
        .map_err(sql_error)?;
        Ok(())
    }

    /// Writes a chain run's terminal state once.
    pub async fn finish_chain_run(&self, id: ChainRunId, finish: &ChainFinish) -> DriverResult<()> {
        if !finish.state.is_terminal()
            || (finish.state == RunState::Failed) != finish.cause.is_some()
        {
            return Err(DriverError::validation("Invalid terminal state."));
        }
        let changed = sqlx::query(
            "UPDATE chain_runs SET state=?, cause=?, failed_step=?, effect_unknown=?, \
             finished_at=? WHERE id=? AND state='running'",
        )
        .bind(finish.state.as_str())
        .bind(finish.cause.map(Cause::as_str))
        .bind(finish.failed_step.as_deref())
        .bind(flag(finish.effect_unknown))
        .bind(format_time(finish.finished_at))
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(sql_error)?
        .rows_affected();
        if changed == 0 {
            return Err(DriverError::validation("The chain run is not running."));
        }
        Ok(())
    }

    /// Marks what a closed application left running (ADR-0034 decision 5,
    /// ADR-0036 decision 5, ADR-0038 decision 4). Never replays anything and
    /// never creates a row.
    pub async fn recover(&self, now: UtcMillis) -> DriverResult<Recovered> {
        let stamp = format_time(now);
        let mut tx = self.write().await?;
        let run_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM runs WHERE state='running'")
            .fetch_all(&mut *tx)
            .await
            .map_err(sql_error)?;
        sqlx::query(
            "UPDATE runs SET state='interrupted', finished_at=?, \
             effect_unknown=CASE WHEN kind='process' OR reads_only=0 THEN 1 ELSE effect_unknown END \
             WHERE state='running'",
        )
        .bind(&stamp)
        .execute(&mut *tx)
        .await
        .map_err(sql_error)?;
        let chain_ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM chain_runs WHERE state='running'")
                .fetch_all(&mut *tx)
                .await
                .map_err(sql_error)?;
        sqlx::query(
            "UPDATE chain_runs SET state='interrupted', finished_at=?, effect_unknown=EXISTS( \
             SELECT 1 FROM runs WHERE runs.chain_run_id=chain_runs.id AND runs.effect_unknown=1) \
             WHERE state='running'",
        )
        .bind(&stamp)
        .execute(&mut *tx)
        .await
        .map_err(sql_error)?;
        let runs = runs_by_ids(&mut tx, &run_ids).await?;
        let chains = chains_by_ids(&mut tx, &chain_ids).await?;
        tx.commit().await.map_err(sql_error)?;
        let mut recovered = Recovered::default();
        for id in &run_ids {
            if let Some(run) = runs.get(id) {
                if let (JobKind::Process, Some(group)) = (run.kind, run.process_group) {
                    recovered.orphans.push(Orphan {
                        run_id: run.id,
                        job_id: run.job_id,
                        process_group: group,
                        process_started: run.process_started.clone(),
                    });
                }
                recovered.runs.push(run.clone());
            }
        }
        recovered.chain_runs = chain_ids
            .iter()
            .filter_map(|id| chains.get(id).cloned())
            .collect();
        Ok(recovered)
    }

    pub async fn run(&self, id: RunId) -> DriverResult<Option<RunRecord>> {
        sqlx::query(concat!("SELECT ", run_columns!(), " FROM runs WHERE id=?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(sql_error)?
            .as_ref()
            .map(run_record)
            .transpose()
    }

    pub async fn run_snapshot(&self, id: RunId) -> DriverResult<Option<String>> {
        sqlx::query_scalar("SELECT snapshot_json FROM runs WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(sql_error)
    }

    pub async fn chain_run(&self, id: ChainRunId) -> DriverResult<Option<ChainRunRecord>> {
        sqlx::query(concat!(
            "SELECT ",
            chain_columns!(),
            " FROM chain_runs WHERE id=?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(sql_error)?
        .as_ref()
        .map(chain_record)
        .transpose()
    }

    pub async fn chain_run_snapshot(&self, id: ChainRunId) -> DriverResult<Option<String>> {
        sqlx::query_scalar("SELECT snapshot_json FROM chain_runs WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(sql_error)
    }

    /// The runs of a chain run's steps, by step index. A step that never
    /// started has no row.
    pub async fn chain_steps(&self, id: ChainRunId) -> DriverResult<Vec<RunRecord>> {
        sqlx::query(concat!(
            "SELECT ",
            run_columns!(),
            " FROM runs WHERE chain_run_id=? ORDER BY step_index"
        ))
        .bind(id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(sql_error)?
        .iter()
        .map(run_record)
        .collect()
    }

    pub async fn outputs(&self, run: RunId) -> DriverResult<Vec<RunOutput>> {
        sqlx::query(
            "SELECT position, name, kind, value, is_null, bytes, value_expires_at, expired \
             FROM run_outputs WHERE run_id=? ORDER BY position",
        )
        .bind(run.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(sql_error)?
        .iter()
        .map(output_record)
        .collect()
    }

    /// The global and per-job history, newest first: runs outside chains and
    /// chain runs, merged by `started_at` then ID. Pass the last entry's
    /// `started_at` and [`HistoryEntry::id_text`] as `before` for the next
    /// page.
    pub async fn history(&self, filter: &HistoryFilter) -> DriverResult<Vec<HistoryEntry>> {
        let limit = i64::from(filter.limit.clamp(1, 1000));
        let rows = sqlx::query(
            "SELECT src, id FROM ( \
               SELECT 'run' AS src, id, started_at FROM runs \
                 WHERE chain_run_id IS NULL AND (?1 IS NULL OR job_id=?1) \
                   AND (?2 IS NULL OR state=?2) \
                   AND (?3 IS NULL OR (started_at, id) < (?3, ?4)) \
               UNION ALL \
               SELECT 'chain', id, started_at FROM chain_runs \
                 WHERE (?1 IS NULL OR job_id=?1) AND (?2 IS NULL OR state=?2) \
                   AND (?3 IS NULL OR (started_at, id) < (?3, ?4)) \
             ) ORDER BY started_at DESC, id DESC LIMIT ?5",
        )
        .bind(filter.job.map(|id| id.to_string()))
        .bind(filter.state.map(RunState::as_str))
        .bind(filter.before.as_ref().map(|(at, _)| format_time(*at)))
        .bind(filter.before.as_ref().map(|(_, id)| id.clone()))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(sql_error)?;
        let mut order = Vec::with_capacity(rows.len());
        let (mut run_ids, mut chain_ids) = (Vec::new(), Vec::new());
        for row in &rows {
            let src: String = get(row, "src")?;
            let id: String = get(row, "id")?;
            if src == "run" {
                run_ids.push(id.clone());
            } else {
                chain_ids.push(id.clone());
            }
            order.push((src, id));
        }
        let mut connection = self.pool.acquire().await.map_err(sql_error)?;
        let runs = runs_by_ids(&mut connection, &run_ids).await?;
        let chains = chains_by_ids(&mut connection, &chain_ids).await?;
        Ok(order
            .into_iter()
            .filter_map(|(src, id)| {
                if src == "run" {
                    runs.get(&id)
                        .cloned()
                        .map(|run| HistoryEntry::Run(Box::new(run)))
                } else {
                    chains.get(&id).cloned().map(HistoryEntry::Chain)
                }
            })
            .collect())
    }

    /// Whether a program run of `job` already happened at this `revision`
    /// (the job on its own, outside a chain) or this `version` (a chain
    /// step); given both, either counts. With neither, whether it ever ran.
    pub async fn program_has_run(
        &self,
        job: JobId,
        revision: Option<i64>,
        version: Option<i64>,
    ) -> DriverResult<bool> {
        let found: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE kind='process' AND job_id=?1 \
             AND ((?2 IS NULL AND ?3 IS NULL) \
               OR (?2 IS NOT NULL AND job_revision=?2 AND job_version IS NULL) \
               OR (?3 IS NOT NULL AND job_version=?3)))",
        )
        .bind(job.to_string())
        .bind(revision)
        .bind(version)
        .fetch_one(&self.pool)
        .await
        .map_err(sql_error)?;
        Ok(found)
    }

    /// Forgets a run's log sizes once its log folder is deleted.
    pub async fn forget_logs(&self, id: RunId) -> DriverResult<()> {
        sqlx::query(
            "UPDATE runs SET log_bytes_stdout=NULL, log_bytes_stderr=NULL, log_bytes_jjp=NULL \
             WHERE id=?",
        )
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(sql_error)?;
        Ok(())
    }

    /// Background pruning in short write transactions of at most 500 rows:
    /// output values past `value_expires_at` are emptied (name, kind and size
    /// stay); runs and chain runs finished 90 days before `now` are deleted
    /// with their steps and outputs.
    pub async fn prune(&self, now: UtcMillis) -> DriverResult<Pruned> {
        let mut pruned = Pruned::default();
        let batch = i64::try_from(PRUNE_BATCH).unwrap_or(500);
        let at = format_time(now);
        loop {
            let changed = sqlx::query(
                "UPDATE run_outputs SET value=NULL, expired=1 WHERE rowid IN ( \
                 SELECT rowid FROM run_outputs WHERE expired=0 AND value_expires_at<=? LIMIT ?)",
            )
            .bind(&at)
            .bind(batch)
            .execute(&self.pool)
            .await
            .map_err(sql_error)?
            .rows_affected();
            pruned.outputs_expired += changed;
            if changed < PRUNE_BATCH as u64 {
                break;
            }
        }
        let cutoff = format_time(now.minus(RUN_RETENTION));
        loop {
            let mut tx = self.write().await?;
            let ids: Vec<(String, String)> = sqlx::query_as(
                "SELECT id, kind FROM runs WHERE chain_run_id IS NULL AND state<>'running' \
                 AND finished_at<=? ORDER BY finished_at LIMIT ?",
            )
            .bind(&cutoff)
            .bind(batch)
            .fetch_all(&mut *tx)
            .await
            .map_err(sql_error)?;
            if ids.is_empty() {
                break;
            }
            let only: Vec<&String> = ids.iter().map(|(id, _)| id).collect();
            sqlx::query("DELETE FROM runs WHERE id IN (SELECT value FROM json_each(?))")
                .bind(ids_json(&only)?)
                .execute(&mut *tx)
                .await
                .map_err(sql_error)?;
            tx.commit().await.map_err(sql_error)?;
            pruned.runs_deleted += ids.len() as u64;
            pruned.program_runs.extend(
                ids.iter()
                    .filter(|(_, kind)| kind == "process")
                    .filter_map(|(id, _)| id.parse::<RunId>().ok()),
            );
            if ids.len() < PRUNE_BATCH {
                break;
            }
        }
        loop {
            let mut tx = self.write().await?;
            let ids: Vec<String> = sqlx::query_scalar(
                "SELECT id FROM chain_runs WHERE state<>'running' AND finished_at<=? \
                 ORDER BY finished_at LIMIT ?",
            )
            .bind(&cutoff)
            .bind(batch)
            .fetch_all(&mut *tx)
            .await
            .map_err(sql_error)?;
            if ids.is_empty() {
                break;
            }
            let json = ids_json(&ids)?;
            let steps: Vec<(String, String)> = sqlx::query_as(
                "SELECT id, kind FROM runs WHERE chain_run_id IN (SELECT value FROM json_each(?))",
            )
            .bind(&json)
            .fetch_all(&mut *tx)
            .await
            .map_err(sql_error)?;
            sqlx::query("DELETE FROM chain_runs WHERE id IN (SELECT value FROM json_each(?))")
                .bind(&json)
                .execute(&mut *tx)
                .await
                .map_err(sql_error)?;
            tx.commit().await.map_err(sql_error)?;
            pruned.chain_runs_deleted += ids.len() as u64;
            pruned.runs_deleted += steps.len() as u64;
            pruned.program_runs.extend(
                steps
                    .iter()
                    .filter(|(_, kind)| kind == "process")
                    .filter_map(|(id, _)| id.parse::<RunId>().ok()),
            );
            if ids.len() < PRUNE_BATCH {
                break;
            }
        }
        Ok(pruned)
    }
}

async fn chains_using_in(
    connection: &mut SqliteConnection,
    job: JobId,
    version: Option<i64>,
) -> DriverResult<Vec<ChainRef>> {
    sqlx::query(
        "SELECT DISTINCT j.id AS id, json_extract(j.definition_json, '$.name') AS name, \
         j.created_at AS created_at FROM jobs j, json_each(j.definition_json, '$.steps') s \
         WHERE j.kind='chain' AND json_extract(s.value, '$.job')=?1 \
         AND (?2 IS NULL OR json_extract(s.value, '$.version')=?2) \
         ORDER BY created_at, id",
    )
    .bind(job.to_string())
    .bind(version)
    .fetch_all(connection)
    .await
    .map_err(sql_error)?
    .iter()
    .map(|row| {
        Ok(ChainRef {
            id: parsed(&get::<String>(row, "id")?)?,
            name: get::<Option<String>>(row, "name")?.unwrap_or_default(),
        })
    })
    .collect()
}

async fn runs_by_ids(
    connection: &mut SqliteConnection,
    ids: &[String],
) -> DriverResult<HashMap<String, RunRecord>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    sqlx::query(concat!(
        "SELECT ",
        run_columns!(),
        " FROM runs WHERE id IN (SELECT value FROM json_each(?))"
    ))
    .bind(ids_json(ids)?)
    .fetch_all(connection)
    .await
    .map_err(sql_error)?
    .iter()
    .map(|row| run_record(row).map(|run| (run.id.to_string(), run)))
    .collect()
}

async fn chains_by_ids(
    connection: &mut SqliteConnection,
    ids: &[String],
) -> DriverResult<HashMap<String, ChainRunRecord>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    sqlx::query(concat!(
        "SELECT ",
        chain_columns!(),
        " FROM chain_runs WHERE id IN (SELECT value FROM json_each(?))"
    ))
    .bind(ids_json(ids)?)
    .fetch_all(connection)
    .await
    .map_err(sql_error)?
    .iter()
    .map(|row| chain_record(row).map(|chain| (chain.id.to_string(), chain)))
    .collect()
}

#[cfg(test)]
#[path = "automation_tests.rs"]
mod tests;
