# Automation

How automation is built and stored: the service that runs jobs, the lock that
lets one instance run them, `automation.db`, published versions and chain
runs, declared outputs, external-program runs and their logs, and schedules
with their grants.

Automation runs SQL Jobs (one statement, through the existing drivers),
external programs (optionally speaking `jjp/1`), linear chains of both, and
schedules that start any of them while the application is open. This document
is the **short, binding reference** for names, limits and invariants. The
behaviour (definitions, confirmation, the completion rule, `jjp/1`, the
interface) is in the [Automation engine specification](../plans/automation-engine-specification.md);
where its *Implementation notes (0.3.0)* differ from its text, the code and
this document win. File formats and the SQLite configuration are in
[Persistence](persistence/automation-db.md#automation-automationdb), which this document
does not repeat; the interface is in [User experience](ux/README.md), and the flows
are in [Architecture](architecture.md#run-a-job).

## Where it lives

| Part | Location |
| --- | --- |
| IDs (`JobId`, `RunId`, …), `RunState`, `Cause`, `RunOrigin`, run and chain-run records, the schedule policies and outcomes: pure types, no I/O | `crates/core/src/automation.rs` (`jovru-core`) |
| `AutomationRepository` (the only code that opens `automation.db`), the migrations, pruning, `AutomationLock` | `crates/storage/src/automation.rs` (`jovru-storage`) |
| The engine: admission and queue, run owners, SQL and program executors, `jjp/1`, chains, program logs, the scheduler (`schedule/`, rules over `jiff`) | `src/automation/` (GPUI-free, testable without a window) |
| Views: render state and capture intent | `src/app/automation/` |

There is no automation crate and no executor trait: executors and events are
enums, and the engine uses bounded Tokio channels.

## Service and lock

- **An application-root service.** `AppView` creates the engine once; views
  subscribe to it, and closing a view never cancels a run. This is the one
  documented exception to "dropping a view cancels its operations"
  ([Architecture](architecture.md#concurrency)). Engine tasks run on the
  application's only Tokio runtime; `backend` exposes what the engine needs
  to spawn them there and no other runtime exists.
- **Quitting with runs active** asks the user. On confirmation the engine
  stops the scheduler first, stops every run, and waits until each has closed
  its session and written its terminal state, for all runs at once, at most
  about 11 s (`SETTLE`: the drivers' recovery window plus the close's
  deadline). Declining keeps the application open. A queued run is dropped
  before the active ones are stopped.
- **The data-folder lock.** The first instance to open Automation takes an
  exclusive operating-system lock on `automation.lock` (`AutomationLock`,
  `std::fs::File::try_lock`) **before migrating, recovering or admitting
  runs**. The lock lives as long as the engine; `Engine::shutdown` releases it
  once the runs have ended, so another instance can take over without waiting
  for the process to exit. The system releases it however the process ends,
  so a lock is never stranded: the file's existence means nothing, and there
  is no PID file, timestamp or lease. A hung process that is alive keeps the
  lock, which is correct. Network file systems may not honour it
  ([Persistence](persistence/automation-db.md#automation-automationdb) says what to do).
- **A second instance** stays usable: Automation is read-only (it opens the
  file with `open_read_only`, shows history and schedules, runs and saves
  nothing), says another Jovru window is running automation on this data
  folder, and tries the lock again each time the screen opens and on **Try
  again**. When it takes the lock over, its scheduler starts.
- **Admission limits.** At most **2 active runs** in the application and **1
  per connection profile** (program runs count only towards the first). A run
  that finds them full waits in a FIFO queue held in memory: at most **8**
  waiting in all, **4** per connection, for at most **60 s**. A run beyond a
  queue limit is refused at once with the reason. A run waiting past 60 s is
  refused, or fails its chain with `queue_timeout`; a chain step that finds
  the queue full fails the chain with `queue_full`. The limits count
  profiles, not physical resources: two SQLite profiles on one file are not
  serialized. They are revisited after measurement, not raised by default.
- **Timeouts.** A job's default timeout is **300 s**
  (`DEFAULT_TIMEOUT_MS`); a chain's is 3,600 s.

## Storage

`automation.db` lives in the data folder (`JOVRU_CONFIG_DIR` respected). It
opens through `open_pool` with `synchronous=FULL` (it holds job definitions)
and `secure_delete=ON`, only after `quick_check`. Timestamps are UTC text,
`strftime('%Y-%m-%dT%H:%M:%fZ','now')`, as in `config.db`. Definitions are
canonical JSON; the fingerprint is `'sha256:'` plus the hex SHA-256 of the
JSON it fingerprints (`sha2` is a direct dependency of `jovru-storage`).

### Migrations

Numbered, forward-only, tracked by `PRAGMA user_version`; `LATEST_VERSION` is
5. Each step runs in one transaction with foreign keys off and passes
`foreign_key_check` before it commits. A released step is never edited; every
schema change is a new step. A file from a newer version is refused untouched
and automation stays off; the rest of the application works. A table whose
`CHECK` must widen is rebuilt with SQLite's documented procedure (new table,
copy, drop, rename). There is no backup copy before a migration.

| Step | Adds | Decision |
| --- | --- | --- |
| 1 | `jobs` (kind `sql`), `runs`, their indexes | D-0034 |
| 2 | `run_outputs`; `runs.outputs_stored` | D-0035 |
| 3 | Kind `process`; the program's columns on `runs`; `connection_id` and `connection_revision` become nullable (`jobs` and `runs` rebuilt) | D-0038 |
| 4 | Kind `chain`; `job_versions`, `chain_runs`; `runs.chain_run_id`, `step_index`, `job_version` (`jobs` rebuilt) | D-0036 |
| 5 | `schedules`, `grants`, `grant_connections`; `origin` takes `schedule`; `schedule_id`, `due_at`, `catch_up` on `runs` and `chain_runs` (both rebuilt) | D-0041 |

[Persistence](persistence/automation-db.md#automation-automationdb) lists the tables and the
file's recovery from corruption.

### Runs

`runs` has one row per admitted run, SQL or program (`kind`), with `NULL` in
the columns that belong to the other kind. `chain_runs` has one row per chain
run. The constraints are part of the contract:

- `state` is `running`, `succeeded`, `failed`, `cancelled` or `interrupted`;
  `Queued` is memory only and `interrupted` is set only by recovery.
  `(state = 'running') = (finished_at IS NULL)` and `(state = 'failed') =
  (cause IS NOT NULL)`. `cause` is a stable code; `effect_unknown` is an
  independent 0/1 flag.
- A SQL run requires `connection_id` and `connection_revision`
  (`(kind = 'sql') = (connection_id IS NOT NULL)`, and the same for the
  revision); the exit code and signal, process group and program fingerprint
  exist only for `kind = 'process'`.
- `reads_only` is what `sql::reads_only` said at admission (0 for programs),
  so recovery decides `effect_unknown` without parsing SQL again.
- `snapshot_json` freezes what ran: exact SQL or the program's definition,
  parameter *specifications*, context, options and the connection's ID and
  profile revision. Never a value, a password or a message.
- `server_code` is the SQLSTATE or vendor code, never the message.
  `failure_code` is the `jjp/1` `fail` code, at most 64 characters.
  `log_bytes_*` are the sizes of the log files. `rows_affected` is `NULL` for
  statements that return rows.
- `job_id` is `ON DELETE SET NULL` (a draft run has none); history outlives
  its job.
- A job refused before admission (more than one statement, a missing or
  malformed value, a Stop while a program's file is being checked) is not a
  run and leaves no row. `failed` with cause `validation` is for what a driver refuses after
  admission.

**Durable admission.** The `running` row is inserted with its snapshot before
any network I/O or process start. If the insert fails, the run is refused and
nothing is sent. The run's owner (the engine task that executes it) alone
writes the terminal state, once, after the completion rule confirms the
statement or rules it unknowable. A failure to write it is reported and
logged; the run is never sent again, and the row stays `running` until
recovery marks it `interrupted`.

**Recovery** is in [Persistence](persistence/automation-db.md#automation-automationdb): the
lock holder, before admitting anything, marks every `running` run and chain
run `interrupted`, never replays one, uses no leases, and sets
`effect_unknown` on every interrupted program and every SQL run that was not
`reads_only`.

## Versions and chain runs

- **Publishing** a SQL Job or program job copies its definition into
  `job_versions` as version *n* (from 1) with its fingerprint. A version is
  never updated. The next number is past both the versions kept and any
  version a run of the job still names, so deleting the newest version never
  lets its number name another definition while its history exists.
- A chain step names `(job_id, version)`, never a draft. A version, and the
  job that owns it, cannot be deleted while any chain or schedule names it:
  the repository refuses and names the chains (or the schedule). The
  `job_versions` foreign key is `ON DELETE RESTRICT`; `delete_job` deletes
  the job's schedule, grant and unreferenced versions in one transaction,
  after the interface has asked. `job_versions` are never pruned.
- **A chain is a job** of kind `chain`, stored like any other and **not
  published**. Its run's snapshot freezes the chain definition together with
  every step's version definition, so the plan the user confirmed is what
  runs. A step whose program file or connection changed after the plan was
  confirmed fails the chain with `plan_changed`.
- **`chain_runs`** exists because a chain run has no connection, statement or
  counters. It records the origin, `failed_step` (the step's ID in the
  definition), `effect_unknown`, the snapshot and the plan's fingerprint.
  Each step's run is an ordinary row of `runs`, linked by `chain_run_id`
  (`ON DELETE CASCADE`), `step_index` (0-based) and `job_version`, and
  carries the chain's origin, schedule, due time and marker.
- **Two-step durable admission.** The chain run's row is written `running`
  when the plan is confirmed, before any step starts; each step's row is
  written when that step is admitted, before its own I/O. A step that never
  started has no row, and a chain holds no admission slot of its own: only
  its active step does.
- **Recovery** marks a `running` chain run `interrupted` after its steps were
  recovered, with `effect_unknown` when any step has it. It never starts a
  remaining step. There is no resume, retry or compensation: the user runs
  the chain again, as a new run.
- A chain run and its step runs are deleted together 90 days after
  `finished_at`.

## Declared outputs

Outputs let a chain pass a step's result to the next one and let a step run
again with the inputs it had.

- **Declared only.** A job declares its outputs by name and `ParameterKind`.
  A SQL Job maps each to a result column by position and states one row rule
  (`exactly_one`, `at_most_one`, `first`); a program supplies them in the
  `outputs` of its `jjp/1` `complete`. Nothing undeclared is ever kept: no
  cell, row or message. A job that declares outputs succeeds only when it
  produced them (cause `Output`).
- **In memory** a value takes at most 64 KiB and one step's outputs 256 KiB.
- **Opt-in persistence.** A definition persists outputs only with
  `persist_outputs: true`. Turning it on for a job whose connection is marked
  production asks for an extra confirmation, and every run of such a job says
  its outputs will be stored.
- **Quota.** One run stores at most **4 KiB** (4,096 UTF-8 bytes) of values,
  all or none. Past it nothing is stored, the run records that
  (`runs.outputs_stored = 0`) and does not fail, and the chain still uses the
  values in memory. `outputs_stored` is `NULL` when the job does not persist
  and `1` when stored.
- **Exact text.** A value is the database's or the program's text, SQL `NULL`
  apart from the text `NULL` (`is_null`), never through floating point.
- **`run_outputs`:** `(run_id, position)` is the key, `(run_id, name)` is
  unique, `run_id` cascades from `runs`. `value` is `NULL` when the output is
  `NULL` or expired; `bytes` is the UTF-8 length; `value_expires_at` is
  `finished_at` + 7 days; `expired` is 0/1. Constraints:
  `NOT (is_null = 1 AND value IS NOT NULL)` and
  `NOT (expired = 1 AND value IS NOT NULL)`.
- **Retention.** Values are emptied 7 days after the run finished, leaving
  name, kind and size (`secure_delete` overwrites the value); the metadata
  goes with the run row at 90 days.
- **Never elsewhere.** Values reach no log, span, telemetry, error report or
  snapshot. They are not secrets: the editor says outputs are not for tokens
  or passwords.

This is the one exception, with program logs below, to the rule that the
application never writes results to disk on its own
([Persistence](persistence/automation-db.md#automation-automationdb)).

## External programs and their logs

- A program run is a row of `runs` with `kind = 'process'`. It records the
  exit code or Unix signal; the process group ID and the process's start time
  as the system reports it (Linux only, from `/proc`; recorded as soon as the
  program starts), for recovery; the SHA-256 of the entrypoint file it ran
  (`program_fingerprint`); the `jjp/1` `fail` code; and the size of each log
  file. Never argument values beyond the definition, environment values, log
  content or the `fail` message.
- The version's file (path and SHA-256) lives in the version's definition
  JSON, not in a column: it is part of what was published. The file is
  checked before every run; a mismatch refuses the run and, on a schedule,
  suspends it (`program_changed`).
- **Recovery** offers to end a process group only when its leader still has
  the recorded start time; it never ends any other process, so a reused ID is
  never touched. Windows records no start time and lists no orphans: the Job
  Object ends the tree with Jovru.
- **Logs.** A program run writes up to three files in
  `automation/logs/<run-id>/` under the data folder: `stdout.log`,
  `stderr.log` and, under `jjp/1`, `jjp.log`. Nothing else is written there.
  - At most **1 MiB per file**, the final marker line included. Past it the
    stream is still read and discarded (so a full pipe never blocks the
    program) and the file ends with one marker line giving the bytes dropped.
    Bytes are written as received.
  - The viewer decodes UTF-8 with replacement and interprets nothing. A
    program's stdout or stderr that is not valid UTF-8 is shown in the
    console's code page (on Windows the OEM code page, from the registry's
    `OEMCP`, decoded by `oem_cp`); `jjp.log` stays UTF-8; a file is never
    rewritten.
  - A run's folder is deleted 7 days after the run finished, with the run's
    row if that is pruned first, or by **Delete logs**. A folder whose run no
    longer exists is removed at the next pruning, and a folder is never
    deleted while its run is active.
  - Log content never reaches `tracing`, the application's log files, OTLP,
    error reports or `automation.db`, which records only the sizes. The
    interface reads the files only when the user opens a run's log.
  - The program editor, the first-run warning and every log view say that
    logs are kept 7 days on this computer and that a program should print no
    passwords, tokens or personal data.

## Schedules and grants

A schedule runs one target by a rule, in a time zone, **only while the
application is open and only in the instance holding the lock**. Nothing runs
while it is closed. A scheduled run is an ordinary run through the same
admission, queue, limits, completion rule and history; only its origin
differs. The rules, DST handling, consent dialog and acceptance tests are in
the [specification](../plans/automation-engine-specification.md#951-schedules-while-the-application-is-open-approved-revision).

### `schedules`

- At most **one schedule per job or chain** (`job_id` is `UNIQUE`,
  `ON DELETE CASCADE`). It names a published version (`job_version`; `NULL`
  for a chain, whose revision is in the grant). The composite foreign key
  `(job_id, job_version)` to `job_versions` is `ON DELETE RESTRICT`, so
  deleting a named version is structurally refused. A draft never runs on a
  schedule.
- The **rule** is canonical JSON in `rule_json`: an *interval* (every 1
  minute to 24 hours from a UTC anchor, in absolute time) or a *calendar*
  (1 to 24 local `"HH:MM"` times on weekdays, month days or every day, in the
  schedule's IANA `time_zone`, resolved with `jiff` 0.2 and RFC 5545's
  `Compatible` disambiguation). Times in the file stay UTC text; local times
  are interpreted only in the schedule's zone.
- **Policies**, per schedule and under consent. `missed_policy` is `ignore`
  (the default), `run_once` or `queue`; `missed_limit` (N) is between 2 and 24
  and exists exactly when the policy is `queue`. `overlap_policy` is `skip`
  (the default), `queue_one` or `concurrent`. The editor proposes N = 3.
- `revision` grows when the rule, the zone or a policy (N included) changes;
  `notify` (an opt-in operating-system notification for a bad end) does not
  change what runs and raises nothing. `enabled` is 0/1; `suspended` is a
  reason code and is `NULL` unless `enabled`.
- **Bookkeeping**: `last_due_at` (the last due time handled, so a time is
  never handled twice, even if the wall clock moves back), `last_outcome`
  (`started`, `caught_up`, `waiting`, `skipped_running`, `missed`, `refused`,
  `suspended`), `last_reason` (a code such as `queue_full`, `queue_timeout`,
  `coalesced`, `input`; never a message), `last_run_id` and
  `last_chain_run_id` (`ON DELETE SET NULL`), `missed_count`,
  `missed_last_at`.
- The **next due time is not stored**; it is computed from the rule, the zone
  and the clock. There is no queue or occurrence table either: queued runs,
  catch-up times waiting their turn and the one due time *queue one* holds
  live in memory, and missed, skipped and refused times are only counters and
  the last outcome, so a schedule every minute cannot grow the file.
- **Suspension reasons** are stable codes written by the engine and not a
  `CHECK`: `connection_changed`, `environment_changed`, `connection_deleted`
  (also a profile the application cannot read now), `chain_changed`,
  `target_changed`, `program_changed`, `time_zone_unknown`. A code this
  version does not know reads as suspended for an unknown reason. Deleting a
  connection suspends the schedules whose grants name it, because grants do
  not reference `config.db`.

### `grants` and `grant_connections`

A grant is the consent of the specification's section 7.2: explicit,
revocable and bound to exactly what will run. A manual confirmation is never
promoted to it.

- `grants` has one row per enabled and confirmed schedule (`schedule_id`
  `UNIQUE`, `ON DELETE CASCADE`; `kind` is `schedule`, so later grants can
  join the table). It records `target_fingerprint` (the version's, or the
  chain plan's), `job_revision` (a chain's), `schedule_revision` (rule, zone
  and policies consented to), `production` (whether a production confirmation
  was part of it) and `granted_at`.
- `grant_connections` lists each connection the grant binds with its profile
  revision and environment label (`none`, `development`, `staging`,
  `production`), indexed by `connection_id` so the engine finds the grants a
  saved connection affects with one query.
- Any change to what a grant binds **suspends** the schedule until it is
  confirmed again; the grant is kept while suspended, so the dialog says what
  changed. The engine checks the grant when it learns new connection
  revisions and again before each admission. **Turning a schedule off or
  deleting it deletes its grant**; a revoked grant authorises nothing. No
  scheduler pass runs before the application root has handed over the
  connections once.
- **Inputs.** A scheduled run uses the version's defaults only; no input
  value is stored with a schedule, and a job with a required input without a
  default cannot be scheduled (a chain step may bind constants).
- **No dialogs.** The engine resolves the connection and its remembered
  password itself. A password not remembered ends the run `failed` with cause
  `Authentication`, its effect known, nothing sent.

### Runs started by a schedule

`runs` and `chain_runs` carry `origin` (`manual` or `schedule`), `schedule_id`
(`ON DELETE SET NULL`, so history outlives a deleted schedule), `due_at` and
`catch_up`, tied by constraints: `(origin = 'schedule') = (due_at IS NOT
NULL)`, `origin = 'schedule' OR schedule_id IS NULL`, `catch_up = 0 OR origin
= 'schedule'`. A refused scheduled run (queue full, queue timeout, defaults
that no longer pass) leaves no run row; the schedule records `refused` with
the reason.

### Time, missed times and overlap

- The scheduler sleeps until the earliest due time and **at most 30 s**,
  because a sleeping computer stops the monotonic clock; on waking it compares
  the wall clock (UTC) with each due time. A due time is **on time** while the
  wall clock is less than **60 s** past it; past that it is **missed**. The
  scheduler never considers a time before the consent or before
  `last_due_at`. Missed times of a calendar rule are counted over at most the
  last 3,660 days.
- **Missed times**: `ignore` runs nothing; `run_once` submits one catch-up for
  all of them, due at the latest missed time; `queue` submits one catch-up for
  each of the N most recent, oldest first, one after another, and counts the
  older ones only. The scheduler applies the policy wherever it finds missed
  times: at start, on waking, on taking the lock over, or when late.
- A **catch-up is a new run** with its own row, marked `catch_up`. It never
  re-executes, resumes or changes a run that started, and a run left
  `interrupted` by recovery stays so under every policy. Catch-ups not yet
  submitted live in memory and are dropped by quitting, losing the lock,
  turning the schedule off, editing it or a suspension; they stay counted as
  missed.
- **Overlap** applies to a due time that finds a run or chain run of the same
  schedule queued or active: `skip` starts nothing and records
  `skipped_running`; `queue_one` holds one due time in memory (a second is
  coalesced into it, recorded `skipped_running` with reason `coalesced`) and
  submits it when the schedule's runs have ended; `concurrent` submits at once
  under the normal queue and limits. Catch-ups go one at a time under every
  policy, and an on-time run is submitted before the catch-ups.

### Second instance

It reads `schedules` read-only, shows them, says they run in the other
instance, and runs none.

## Limits and retention

| What | Value | Where |
| --- | --- | --- |
| Active runs | 2 in the application, 1 per connection profile | `queue::Limits` |
| Queue | 8 in all, 4 per connection, 60 s, FIFO, memory only | `queue::Limits` |
| Job timeout; chain timeout | 300 s; 3,600 s | `definition.rs` |
| Chain steps; declared outputs | 2 to 16; at most 32 | `definition.rs` |
| Quit settle window | about 11 s | `engine::SETTLE` |
| Persisted outputs | 4 KiB per run, all or none; opt-in | `OUTPUT_QUOTA_BYTES` |
| Output values kept | 7 days after the run ended | `OUTPUT_VALUE_RETENTION` |
| Run and chain-run rows | 90 days after `finished_at` | `RUN_RETENTION` |
| Program log file; folder kept | 1 MiB each; 7 days after the run ended | `logs::LOG_LIMIT`, `engine::LOG_RETENTION` |
| Stop grace for a program | 5 s, then the tree is killed | `process::STOP_GRACE` |
| Pruning | in the background about hourly, in transactions of at most 500 rows, never blocking admission | `PRUNE_EVERY`, `PRUNE_BATCH` |
| Schedule interval | 1 minute to 24 hours; 1 to 24 calendar times a day | `schedule/rule.rs` |
| Missed-time N | 2 to 24 | `schedules.missed_limit` |
| On-time grace; longest scheduler sleep | 60 s; 30 s | `schedule::GRACE`, `MAX_SLEEP` |
| `job_versions`, schedules, grants | never pruned | |

## Measurements

Taken on 2026-11-08 with the headless SQL Job (the base of today's
executor), on SQLite and, under `cargo xtask check --full`, on PostgreSQL
14/18 and MySQL 8.4/9.7; Databricks was not run.

**The completion rule on a write.** `UPDATE … RETURNING` over 10,000 rows
with a 1,000-row limit (on MySQL a procedure that writes, then returns
rows): `succeeded` and `truncated`, 1,000 rows kept, 9,000 read and
discarded, every row updated once. The same write past a 300 ms deadline:
`failed` with `timeout` and `effect_unknown`, the rows changed together or not
at all, nothing sent again. A `SELECT` above the limit: truncated, nothing
drained, `effect_unknown` clear. No change to `DriverOutcome` was needed.

**Drain time** (`examples/automation_drain.rs`, release build; each row an
`INTEGER` and 200 bytes of text; 1,000 rows kept; databases in Docker on the
same 8-core Linux machine). Time from the first page to the confirmed end,
by rows returned:

| Database | 10,000 | 100,000 | 1,000,000 | Rows/s drained | Process peak |
| --- | --- | --- | --- | --- | --- |
| SQLite | 35 ms | 389 ms | 3,941 ms | 253,000–257,000 | 10.8–17.9 MB |
| PostgreSQL 14 | 10 ms | 107 ms | 1,033 ms | 882,000–967,000 | 7.4–7.6 MB |
| PostgreSQL 18 | 10 ms | 143 ms | 1,112 ms | 691,000–898,000 | 7.3–7.7 MB |
| MySQL 8.4 | 13 ms | 169 ms | 1,899 ms | 526,000–652,000 | 7.8–8.5 MB |
| MySQL 9.7 | 20 ms | 186 ms | 1,676 ms | 435,000–596,000 | 7.8–8.6 MB |

A result held at the row limit takes 211 KB whatever the size; the drain keeps
nothing. SQLite applies every change of an `UPDATE … RETURNING` in its first
step and buffers what it returns, so there the drain only reads. A million
returned rows were confirmed well inside the drivers' 5 s recovery window,
which is why the conservative rule stands.

**Beside a tab's transaction.** On PostgreSQL and MySQL a job's write commits
on its own while the tab's transaction keeps its row, and the tab's rollback
leaves the job's row. On SQLite the job's session does not see the tab's
uncommitted row, and a write waits for the 4 s busy timeout and fails if the
tab holds the write lock.

**Races.** Stop and the deadline fired together, 200 times, produced exactly
one outcome each, the same in the history, with every session closed. A third
run was refused at once, a second on a connection too (before the queue
existed). **Stop** ended a long SQLite statement within 0.7 s. Dropping every
subscriber leaves the run to finish.

**Results.** `ResultData` kept ordered columns with duplicate names, the
database's own text (`NUMERIC(38, 18)` exact) and NULL apart from the text
`NULL`, on all three databases, so jobs need no output type of their own.

## Rules

- Only `AutomationRepository` opens `automation.db`. There is no second
  automation file or data root.
- Every schema change is a new migration that follows
  [Persistence](persistence/files.md#migrations).
- Nothing writes input values, result cells, scalar results or server or
  program messages to `automation.db`, the application's log or telemetry
  (CA-13), apart from the two bounded exceptions: declared outputs and program
  logs.
- A run is never replayed: not by recovery, not by a catch-up, not by a chain.
- Lock files carry no meaning by existing, and a hung holder keeps the lock.
- A schedule never runs without a matching grant (policies included), and no
  scheduled run opens a dialog.

## Automation

The engine ([Architecture](architecture.md#run-a-job)) follows
these rules; the specification explains why
([sections 4.6, 5 and 9](../plans/automation-engine-specification.md)).

- **No replay, ever.** A run is not sent or started a second time after a
  failure, a timeout, a crash, a lost history write or a restart. There are no
  retries, no leases and no resume; the user starts a new run on purpose,
  knowing from the history what the earlier one did. A schedule's catch-up
  (D-0041) does not break this rule: it is a **new run** for a due time no
  run started for, marked `catch_up`, never a second sending of a run that
  started; an `interrupted` run is never caught up.
- **Completion rule (SQL).** A run succeeds only once the statement is
  confirmed complete. A statement that `reads_only` (by the dialect's tokens,
  a warning and not a boundary) keeps up to the job's row limit and the
  memory ceiling and stops reading there: the result is `truncated` and the
  run still succeeds. Any other statement keeps the same rows but is read **to
  its end** into a sink that counts and discards (`rows_discarded`) until the
  driver reports the end, and the engine never asks the driver to stop early,
  since stopping a write's result could cancel the statement (MySQL then
  treats the result as cut, drains for up to 5 s and may `KILL QUERY`;
  PostgreSQL cancels the backend at the next command). A page is not proof
  that a write finished. Draining a million returned rows took at most 3.9 s
  ([Measurements](#measurements)).
- **`effect_unknown`.** Set when something may have been done that the
  history cannot confirm: a SQL statement that is not `reads_only` and was
  sent but neither confirmed nor refused by the server (a deadline, a Stop,
  a lost connection, a protocol or internal failure); every program run that
  started and did not succeed, since Jovru cannot know what a program did;
  and every run recovery marks `interrupted` (SQL that only reads excepted). It
  is shown with the run and is never cleared by running again.
- **Terminal write before publish.** The owner writes the run's terminal
  state, and then publishes it. If the row cannot be written *before* the run
  starts, the run is refused and nothing is sent or started. If the terminal
  write fails *after* it ended, the user is told (`history_failed`), the run
  is not repeated, and its row stays `running` until recovery marks it
  `interrupted` at the next start, with `effect_unknown` as above. Chains
  follow the same order.
- **Arbitration.** The first event the owner observes decides the outcome and
  later ones are ignored. A SQL run decides between the confirmed completion,
  Stop and the deadline (a completion already confirmed wins over a Stop that
  became ready in the same instant, since it is what happened). A program
  decides between its exit, Stop, the deadline and a `jjp/1` protocol
  failure. A Stop is `cancelled`; a deadline is `failed` with `timeout`.
- **Stop and the deadline, SQL.** Every driver call gets the time left before
  the deadline as its timeout, and the owner also cancels at the deadline, so
  either side ends the run with `timeout`. The work's token is cancelled; the driver
  interrupts the server-side work and brings the session back to a known
  state, or closes it, within 5 s. The owner waits for that for at most 11 s
  and then drops the work, which drops the session. A write that was already
  sent is `effect_unknown`, even if the driver confirms completion while
  recovering. A job's session is its own, so a write job on a SQLite file that
  a tab holds a write transaction on waits the 4 s busy timeout and fails with
  `Database`, its effect known; it never commits or rolls back the tab's work.
- **Stop and the deadline, programs.** The program is asked to end and its
  whole tree is killed after 5 s ([Q-17](../plans/automation-engine-specification.md#12-decisions-required-from-the-maintainer)).
  Under `jjp/1` the request is a `cancel` message that never waits for the
  program to read it; otherwise it is `SIGTERM` to the process group on Unix.
  A protocol failure kills the tree at once, a program that exits on its own
  has whatever it left in its group or job killed, and a descendant that
  left the group (`setsid`) and still holds the pipes is abandoned after 2 s
  rather than keeping the run open. Dropping the run's future also kills the
  tree. On Windows the program cannot be asked to end (see Known deviations):
  it gets the 5 s and is then killed with its Job Object, which also ends the
  tree if Jovru itself dies.
- **Queue.** At most 2 runs are active and 1 per connection profile (programs
  count only towards the global limit); at most 8 wait, 4 per connection, in
  FIFO order, for at most 60 s. A run beyond the queue is refused at once
  with the limit it met; a run whose connection is busy does not block a
  later one on another connection. Waiting runs live in memory and are
  dropped when the application quits.
- **Chains.** A chain holds no slot of its own, so a global limit of 1 still
  progresses: each step releases its slot, session or program before the next
  asks for one. The first step that does not succeed stops the chain, and
  later steps never start. Stop cascades to the active or queued step; the
  chain's deadline cancels the step and fails the chain with `timeout`. A step
  whose connection profile changed after the plan was confirmed, or whose
  published program file changed, fails with `plan_changed` (**PlanChanged**);
  a step that finds the queue full fails with `queue_full`.
- **Quit.** `Engine::shutdown` refuses new runs, empties the queue, stops
  every run and chain, waits until each run has closed its session or its
  program and written its terminal state, and releases the lock so another
  instance can take over. With SQL runs that takes at most about 11 s, with
  programs the 5 s grace; the interface asks before quitting with runs
  active, and declining keeps the application open.
- **Schedules.** The scheduler runs only in the lock holder and stops first
  when quitting; catch-ups and a due time waiting under *queue one* live in
  memory and are dropped by quitting, losing the lock, turning the schedule
  off, editing it or a suspension (they stay counted as missed). Each
  schedule remembers its last handled due time and never considers an
  earlier one, so a wall clock set back repeats nothing. A time counts as
  handled once its mark is written, once the scheduler took it while the
  application runs, or once a run was admitted for it (the run's row holds
  the schedule and the due time), so a mark that could not be written, or a
  crash before it, never runs the same due time twice. Before each run the
  grant is checked against the connections' profile revisions and
  environment labels, the chain's revision, the target's fingerprint and the
  zone; any difference suspends the schedule and takes its queued run out of
  the queue, as **Stop** does (an active run is not stopped). A chain is
  queued until its first step starts: while that step is prepared or waits
  for a slot, turning the schedule off, deleting it or a suspension stops
  the chain, and no step runs; once a step started, the chain goes on. A scheduled run
  opens no dialog: without a remembered password it is admitted and fails
  with `authentication`, nothing sent; a chain fails before its first step.
  A scheduled run refused by a full queue or past its wait leaves no run row;
  the schedule records `refused` with the reason.
- **Recovery and retention** are in
  [Persistence](persistence/automation-db.md#automation-automationdb).

## Automation: external programs and `jjp/1`

An automation job can start an external program
(D-0038). **The program is trusted
code**: it runs with the user's privileges and is supervised, never
sandboxed. Jovru ends its process tree on Stop and on a deadline; it cannot
stop the program from reading the user's files or using the network. The
interface says so on the first run of each version
([specification](../plans/automation-engine-specification.md), Q-10), and
there is no import or export of jobs that could bring code in unseen.

**Starting a program** (`automation::process`):

- The program is started **without a shell**: `Command::new(program)` with
  the arguments as a vector, so no argument is parsed by `sh` or `cmd.exe`.
  A `.bat` or `.cmd` file, which Windows would run through `cmd.exe`, is
  refused before the run, whether it is named or found on `PATH`. The name
  is read as Windows reads it: trailing dots and spaces are dropped
  (`deploy.bat.` is `deploy.bat`), as is a `::$DATA` suffix.
- A program is an absolute path or a bare name looked up on `PATH` (and
  `PATHEXT` on Windows, so `tool` finds `tool.exe`). A relative path such as
  `bin/tool` is refused, as is a relative working directory: nothing depends
  on the directory Jovru was started in.
- **The environment is inherited** from Jovru, minus a denylist: `JOVRU_*`,
  `OTEL_*`, `NODE_OPTIONS`, `PYTHONPATH`, `PYTHONSTARTUP` and `PYTHONHOME`
  (matched without regard to case on Windows). Everything else, **secrets
  included**, reaches the program, and the interface lists what is removed. The job's
  own `unset` and `set` apply after the denylist.
- A published version of a program **pins its entrypoint file by SHA-256**.
  Nothing is copied: the program runs from its path, and a file that
  changed or disappeared blocks the run until the version is published again.
  The hash identifies what ran; it does not make the file safe.
- The program's stdin carries only the `jjp/1` token line (or end of file);
  its stdout and stderr are captured to log files and never inherit Jovru's
  console. On Windows it starts without a console window.
- No program path, argument, environment value, log content, token, input or
  output reaches `tracing`, an error message or a `Debug` output. The
  telemetry allowlist gains no attribute for it: a program run's span carries
  only the exit code and the error type.

**`jjp/1`** is how a program receives inputs and returns outputs
([specification](../plans/automation-engine-specification.md), section 9.2):

- A loopback listener (`127.0.0.1`, a port the system picks) exists **only
  while a `jjp/1` run is registered**: the first run opens it and the last to
  end closes it. Nothing listens at startup, and nothing listens for other
  hosts.
- Each run gets a **256-bit random token**. It reaches the program **through
  its stdin only**, never through the environment, an argument, a log or
  telemetry. The program presents it in `hello`, which **consumes** it at
  once: a wrong token, a reused one, a second connection and any message
  before `hello` are refused and close the connection. No `hello` within 30 s
  ends the run.
- Messages are one JSON-RPC object per line, at most 1 MiB, enforced before
  the line is buffered; at most 16 connections are served at once; a program
  that leaves more than 4 MiB of responses unread, or sends more than ten
  times the log quota, fails its run. Only `hello`, `progress`, `log`,
  `complete` and `fail` are accepted.
- Same-user debugging (ptrace, `/proc/<pid>/mem`) can read a process's
  memory, including its token; the token protects against other local users
  and other programs guessing the port, not against the same user.
- The values of a run never go to `tracing`, spans, history or errors
  (CA-13). `log` messages and the `fail` message go to the run's `jjp.log`
  only.

**Program logs** are the exception to "no cell or message is stored":
stdout, stderr and `jjp.log` are written as received under
`automation/logs/<run-id>/`, at most 1 MiB each and kept 7 days
([Persistence](persistence/automation-db.md#automation-automationdb),
D-0037). A program must not print
secrets, and the interface warns the user of it. They are read back only when
the user opens a run's log. Declared outputs the user chose to persist are
the other exception (D-0035).

## Automation: schedules and their consent

A schedule runs unattended while the application is open
(D-0041, specification section
9.5.6), so its consent is a durable **grant**, explicit and revocable:

- It is given in one consent dialog that states what will run (the version,
  or each step of the chain), the rule and zone with the next times, the
  missed-time and overlap policies, each connection with its environment
  label, the risks of each SQL step on a **Production** connection, each
  program's file and hash, and the defaults it will use. A manual run's
  confirmation, password or trusted-code warning is never promoted to a
  grant (P-15).
- The grant binds the target's fingerprint, the chain's revision, the
  schedule's revision (rule, zone and both policies) and each connection's
  profile revision and environment label. Any difference, a deleted
  connection, a changed program file or an unknown zone suspends the
  schedule until it is confirmed again. Turning it off deletes the grant.
- A scheduled run never asks for a password: it uses one only when the
  profile remembers it (the vault's), and otherwise fails with
  `authentication` before anything is sent. A password typed for this
  session is never handed to the scheduler.
- It uses the version's defaults only (Q-36); no input value is stored with
  a schedule. A grant stores IDs, revisions, labels and fingerprints, never
  a value, a password or a result.
- The opt-in notification of a bad end (Q-40) names the job, the outcome and
  the cause, never a value or a server message.

## Tests

- **Automation** ([specification](../plans/automation-engine-specification.md),
  section 11) is tested without a window, a server or the user's data. Every
  engine test opens a unique temporary data folder (never
  `connection_store::data_dir()`) and uses synthetic data:
  - `src/automation/tests.rs`: the engine through a scripted driver that
    answers as the test says (pages, errors, hangs, a lost connection) and
    synthetic SQLite files. They cover the completion rule, truncation, the
    deadline, Stop, arbitration, the queue and its limits, durable admission,
    a history that cannot be written (nothing sent; a terminal state never
    written means no replay), recovery, a second engine on the same folder,
    the quit, declared outputs, chains (a global limit of 1, a failed step,
    `plan_changed`, input mapping, published versions) and CA-13: sentinel
    values are searched for in every byte of `automation.db` and in the log.
  - `crates/storage/src/automation_tests.rs`: each migration keeps every row
    (seeded at each earlier version), a newer file is refused untouched, a
    corrupt one is set aside and recreated, the lock is exclusive and a
    process killed while holding it releases it (a child re-executes the test
    binary), recovery marks only `running` rows, pruning expires values and
    deletes old rows, outputs are stored all or none, history merges by
    keyset, deleted values are overwritten in the file (`secure_delete`), and
    no input value reaches the file.
  - `src/automation/process/tests.rs`: programs. The test binary **starts
    itself again as the child program**: `AUTOMATION_TEST_CHILD` (passed
    through the job's `environment.set`) selects a mode in
    `process/test_child.rs` (exit code, print, spam, `sleep`, a process
    tree, an orphan that holds the pipes, the `jjp/1` modes), so the same
    tests run on every platform without `sh`. They cover the environment
    denylist, no shell, `.bat`/`.cmd` refusal, the pinned SHA-256, Stop, the
    deadline and the 5 s grace with shortened timings, grandchildren killed,
    the bounded logs and a left-over group found by its start time.
  - `src/automation/jjp/tests.rs`: the protocol over a real loopback
    connection, no child needed: framing across chunks and UTF-8
    boundaries, the line cap, invalid and reused tokens, `hello` once,
    conflicting terminals, disconnects, `cancel` while the program reads
    nothing, the method allowlist, quotas, and tokens and values absent from
    logs and `Debug`.
  - The Node.js example (`examples/jjp/export.js`) has an end-to-end test,
    `automation::process::tests::the_node_example_exports_and_completes`.
    It is `#[ignore]`d with the reason "needs node on PATH" because CI
    runners are not required to have Node.js; run it with
    `cargo test --lib automation::process::tests::the_node_example -- --ignored`.
    Not running it is reported, not hidden.
  - Windows (Job Objects, no console window, `PATHEXT`, killing grandchildren,
    the lock after a kill) and the visual flows are checked by hand with the
    [manual checks](../standards/manual-checks.md); a Windows behaviour is not claimed before
    it passes there.
  - `examples/automation_drain.rs` measures the completion rule on a driver
    (SQLite by default, the local fixtures with `JOVRU_CONNECTION`); it is a
    measurement, not part of the gate. Its numbers are in
    [Measurements](#measurements).

## Decisions

The reasoning behind this area's structural choices is in [Decisions: Automation](../decisions/automation.md).

## Known deviations

| Standard | Current code |
| --- | --- |
| A program's environment holds no secret Jovru did not mean to give | Automation inherits Jovru's environment minus a short denylist ([Q-27](../plans/automation-engine-specification.md#12-decisions-required-from-the-maintainer)); variables that hold secrets reach the program. Accepted by the maintainer, shown in the interface |
| An external program is sandboxed | It is not: it runs with the user's privileges and may read files and reach the network (Q-10). Supervised, not contained |
| Stop asks a program to end before killing it | On Windows a program is not asked (`CTRL_BREAK_EVENT` needs `unsafe` Win32 calls, which the workspace forbids): one with no `jjp/1` waits the 5 s grace and is killed with its Job Object; under `jjp/1` it still receives `cancel` |
