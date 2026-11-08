# ADR-0033: Automation in `automation.db`, run by an application-root service under a data-folder lock

- **Status:** Accepted (by the maintainer on 2026-11-08; not implemented)
- **Date:** 2026-11-08
- **Authors:** Alan de Oliveira (decisions Q-02, Q-04, Q-05, Q-06, Q-07), Claude (draft)

## Context

The [automation engine specification](../plans/automation-engine-specification.md)
needs durable job definitions and run history, runs that outlive the view
that started them, and protection against two instances recovering each
other's runs. On 2026-11-08 the maintainer answered the questions that block
it: Q-02 (approve `automation.db` and a minimal schema), Q-04 (pure DTOs in
`jykre-core`), Q-05 (an application-root service with a coordinated quit),
Q-06 (a second instance on the same data folder cannot run automation) and
Q-07 (2 active runs, 1 per connection, immediate refusal, 300 s timeout).
The maintainer asked that a lock can never be left behind in a folder for
good.

The [proof of concept](../plans/automation-sql-job-poc.md#8-findings)
confirmed the engine's shape without persistence: one owner per run,
admission limits, Stop and the deadline, and the completion rule.

Forces:

- `config.db` holds the only copy of the connection profiles and runs with
  `synchronous=FULL`; run history grows and is pruned.
- Job definitions are user data: losing them loses the user's work.
- Architecture says "dropping a view cancels its operations"; a job must not
  stop because its screen closed.
- Recovery must never replay a run, and two processes must never both
  recover the same runs.
- No new dependency is approved by this ADR except where it says so.

## Decision

### 1. One file, `automation.db`

- `automation.db` lives in the data folder (`JYKRE_CONFIG_DIR` respected),
  opened by `open_pool` with `synchronous=FULL`, since it holds job
  definitions. `jykre-storage` owns it through `AutomationRepository`; no
  other crate opens it.
- Its schema has numbered forward-only migrations tracked by
  `PRAGMA user_version`, as the metadata caches do
  ([ADR-0018](0018-metadata-cache-migrations.md)). A file from a newer
  version is refused, and automation stays off with a message; the rest of
  the application works.
- A file that fails `quick_check` is set aside as
  `automation.db.corrupt-<millis>` and a new one is created; job definitions
  are then lost, so the application says so. There is no backup rotation in
  this ADR.

### 2. Migration 1: jobs and runs

Definitions are canonical JSON (specification, section 6.2). Runs record
what ran, where, when and how it ended, never values, cells, scalars or
server messages (section 6.4); declared outputs are the subject of
[ADR-0034](0034-persisted-declared-outputs.md). Timestamps are UTC text,
`strftime('%Y-%m-%dT%H:%M:%fZ','now')`, as in `config.db`.

```sql
CREATE TABLE jobs (
  id              TEXT PRIMARY KEY,                -- UUID
  kind            TEXT NOT NULL CHECK (kind IN ('sql')),
  definition_json TEXT NOT NULL,                   -- canonical JSON
  revision        INTEGER NOT NULL DEFAULT 1,      -- +1 on every save
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);

CREATE TABLE runs (
  id                  TEXT PRIMARY KEY,            -- UUID
  job_id              TEXT REFERENCES jobs(id) ON DELETE SET NULL, -- NULL: draft run
  job_revision        INTEGER,                     -- the definition's revision at admission
  origin              TEXT NOT NULL CHECK (origin IN ('manual')),
  state               TEXT NOT NULL CHECK (state IN
                        ('running', 'succeeded', 'failed', 'cancelled', 'interrupted')),
  cause               TEXT,                        -- stable code; set when failed
  effect_unknown      INTEGER NOT NULL DEFAULT 0 CHECK (effect_unknown IN (0, 1)),
  reads_only          INTEGER NOT NULL CHECK (reads_only IN (0, 1)),
  snapshot_json       TEXT NOT NULL,               -- section 6.3, no values
  fingerprint         TEXT NOT NULL,               -- 'sha256:' + hex of snapshot_json
  connection_id       TEXT NOT NULL,
  connection_revision INTEGER NOT NULL,
  rows_delivered      INTEGER,
  rows_discarded      INTEGER,
  rows_affected       INTEGER,                     -- NULL for statements that return rows
  truncated           INTEGER NOT NULL DEFAULT 0 CHECK (truncated IN (0, 1)),
  server_code         TEXT,                        -- SQLSTATE or vendor code, never the message
  started_at          TEXT NOT NULL,
  finished_at         TEXT,
  CHECK ((state = 'running') = (finished_at IS NULL)),
  CHECK ((state = 'failed') = (cause IS NOT NULL))
);

CREATE INDEX runs_by_job   ON runs(job_id, started_at DESC);
CREATE INDEX runs_by_start ON runs(started_at);
CREATE INDEX runs_open     ON runs(state) WHERE state = 'running';
```

- `kind` widens with the migration of each later kind (`process`, `chain`);
  the tables of later phases (chain steps, outputs) arrive with their own
  migrations, each before the release that ships them.
- `reads_only` is what `jykre::sql::reads_only` said at admission, so
  recovery decides `effect_unknown` without parsing SQL again.
- The fingerprint uses SHA-256. `sha2` is already in the dependency tree;
  making it a direct dependency of `jykre-storage` is **part of this ADR's
  approval** ([Stack and dependencies](../standards/stack-and-dependencies.md)).
- A job refused before admission (more than one statement, a missing or
  malformed value) is not a run and leaves no row, as a tab that stops
  before sending leaves nothing. `failed` with cause `validation` is kept for
  what a driver refuses after admission.
- Run rows are pruned 90 days after `finished_at`, in the background, in
  batches that never hold a write transaction for long; pruning never blocks
  admission.

### 3. Durable admission and terminal writes

- Admission inserts the `running` row with its snapshot before any network
  I/O. If that insert fails, the run is refused and nothing is sent.
- The owner writes the terminal state once, after the completion rule
  confirms the statement or rules it unknowable, and only then publishes it.
  A failure to write the terminal state is reported and logged; the run is
  never sent again, and the row stays `running` until recovery marks it
  `interrupted`.

### 4. The data-folder lock

- The first instance to open Automation takes an **operating-system lock**
  on `automation.lock` in the data folder with `std::fs::File::try_lock`
  (an exclusive `flock` on Unix, `LockFileEx` on Windows), as the metadata
  caches' writer lock does, and keeps the file open for the life of the
  process. It takes it before migrating, recovering or admitting runs.
- **A lock cannot be left behind.** The operating system releases it when
  the process ends, however it ends: a normal exit, a crash, a kill or a
  power loss (the next boot holds no locks). What stays on disk is an empty
  file, and its existence means nothing: only a lock held by a live process
  counts. There is no PID file, no "lock by existence", no timestamp and no
  lease to expire, so there is nothing to clean up by hand.
- A second instance stays usable. Its Automation screen is read-only and
  says another Jykre Studio window is running automation on this data
  folder. Each time the screen opens, and when the user asks, it tries the
  lock again, so it takes over without a restart once the first instance
  closes.
- A process that is alive but hung keeps the lock; that is correct, since
  it may still be running statements. Network file systems may not honour
  advisory locks; the data folder is local by default, and that limitation
  is documented in [Persistence](../reference/persistence.md).

### 5. Recovery

When the lock holder starts automation it marks every `running` row as
`interrupted`, with `effect_unknown` unless `reads_only`, before admitting
anything. It never replays a run and uses no leases.

### 6. The service

- The engine is an **application-root service**: `AppView` creates it once,
  views subscribe to it, and closing a view never cancels a run. This is a
  documented exception to "dropping a view cancels its operations"
  ([Architecture](../reference/architecture.md#concurrency)).
- Its tasks run on the application's only Tokio runtime; `backend` exposes
  what the engine needs to spawn them there, and no other runtime is
  created.
- Quitting with runs active asks the user. On confirmation the engine stops
  every run, waits until each has closed its session and written its
  terminal state (at most about 11 s, the drivers' recovery window plus the
  close's deadline, for all runs at once), then the application exits.
  Declining keeps the application open.
- Admission allows at most **2 active runs** in the application and **1 per
  connection profile**, and refuses beyond them at once with the reason;
  the default timeout is **300 s** (Q-07).

### 7. DTOs in `jykre-core`

`crates/core/src/automation.rs` holds the pure types the storage and the
engine share: `JobId`, `RunId`, `RunState`, `Cause`, `RunOrigin` and the run
record. No I/O and no dependency beyond what core already has.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Tables in `config.db` | One file, one backup | History growth and pruning next to the only copy of the profiles; a bad migration risks the connections | The profiles' file stays small and stable |
| History only in memory | No schema, no lock | No history after restart, no recovery, no `interrupted` | Q-02 asked for durable history |
| Lock by file existence or PID file | Works without OS locks | Left behind by a crash; a reused PID can look alive; needs cleanup | The concern raised by the maintainer; OS locks have none of these |
| Lock with a lease renewed by time | Survives network file systems | A slow disk or a suspended laptop expires a live lock; two writers | Correctness depends on clocks |
| Single-instance application | No second-instance mode | Changes the whole application for one feature | Out of scope |
| Runs owned by their view | No exception to the architecture | A run dies when its screen closes | Q-05 |
| Quit cancels without asking | Simpler exit | Interrupts writes silently, leaving `effect_unknown` | Q-05 |

## Consequences

- Positive: history and definitions survive restarts; recovery is
  deterministic and never replays; a crashed instance never blocks the next
  one.
- Negative or costs: another file to migrate; a second instance cannot run
  jobs; quitting can wait up to about 11 s with runs active; one connection
  per run.
- What becomes required: every schema change of `automation.db` is a new
  migration; nothing writes input values, cells, scalars or server messages
  to it outside ADR-0034; the architecture documents the service exception.
- What becomes forbidden: replaying a run; a second automation file or data
  root; lock files whose existence carries meaning.

## How to validate

- A child process takes the lock and is killed (`SIGKILL`, `TerminateProcess`);
  the parent's `try_lock` then succeeds, and the file left behind does not
  stop it. Run on Windows and Linux separately.
- Two instances on one temporary data folder: the second is read-only and
  takes over after the first closes.
- Recovery marks `running` rows `interrupted`, with `effect_unknown` only for
  writes; no run is sent again.
- A forced exit between admission and the terminal write, and a disk
  failure before and after it.
- No input value, cell or server message is found anywhere in
  `automation.db` after a run with synthetic sentinel values (CA-13).
