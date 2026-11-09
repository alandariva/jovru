# ADR-0041: Schedules and their grants in `automation.db`, with runs that record the schedule that started them

- **Status:** Accepted on 2026-11-08, with a change: missed times (Q-34)
  and overlap (Q-35) are a policy chosen per schedule rather than fixed
  rules, so `schedules` holds both policies and N, and runs and chain runs
  carry a catch-up marker (decisions 1 and 3, migration 5 below)
- **Date:** 2026-11-08
- **Authors:** Claude (draft), for the maintainer's review (milestone A13 of
  [Version 0.3.0](../plans/releases/0.3.0.md))

## Context

Milestone A13 adds schedules while the application is open. The
[specification's revision](../plans/automation-engine-specification.md#951-schedules-while-the-application-is-open-approved-revision)
(sections 9.5.1–9.5.10) defines their behaviour:

- a rule (an interval, or a calendar rule) in an IANA time zone;
- runs only while the application is open, in the instance holding the
  lock;
- times missed and overlap follow a policy chosen per schedule (section
  9.5.5): missed times are ignored (the default), run once, or queued up to
  N; overlap is skipped (the default), queued once, or allowed;
- consent bound to what runs, suspended by any change;
- the same queue, limits and history as manual runs, with the origin
  recorded.

A13 asks for an ADR for the `Schedule` and `Grant` tables and their
migration, accepted before implementation. A persistence schema change
needs one ([Workflow](../standards/workflow.md#architecture-decisions)).

The schema today (migration 4, `crates/storage/src/automation.rs`):

- `runs.origin` and `chain_runs.origin` are `CHECK (origin IN ('manual'))`;
- `RunOrigin` in `jykre-core` has `Manual` only;
- SQLite cannot widen a `CHECK` in place.

[ADR-0036](0036-published-versions-and-chain-runs.md) and
[ADR-0038](0038-external-program-runs.md) already rebuilt tables with
SQLite's documented procedure: new table, copy, drop, rename,
`foreign_key_check`.

Forces:

- No input value, cell or server message may be stored (specification
  6.4, CA-13). A schedule therefore holds a rule, a zone and references, and
  uses the version's defaults (Q-36).
- A grant must say exactly what was consented to, so that any difference
  is detectable without reading values.
- A refused scheduled run leaves no run row (section 4.3), yet the user
  must see it happened.
- Recovery stays as it is: nothing is replayed (ADR-0034, decision 5).

## Decision

1. **`schedules`:** one row per schedule, at most one per job or chain
   (Q-39).
   - It names its target: `job_id`, plus `job_version` for a SQL or program
     job. A chain is named by its job alone, and its revision is in the
     grant.
   - It holds the rule as canonical JSON, the zone as an IANA name, the
     missed-time policy with its N, the overlap policy, whether it is
     enabled, why it is suspended, whether a bad end shows an operating
     system notification (Q-40), and the bookkeeping of section 9.5.5: the
     last handled due time, the last outcome, the missed count.
   - The rule, the zone and both policies (N included) are what the consent
     covers: changing any of them raises `revision`. The notification
     choice does not change what runs, so it raises nothing.
   - The next due time is **not stored**. It is computed from the rule, the
     zone and the clock. Catch-up times not yet submitted are not stored
     either (decision 5).
2. **`grants`:** the consent of section 7.2, one row per enabled and
   confirmed schedule. `kind` is `'schedule'`, so later grants (`jjp/1`
   capabilities) can join the table with a migration of their own.
   - A grant records the target's fingerprint, the chain's revision for a
     chain, the schedule's revision (rule and zone), whether a production
     confirmation was part of it, and when it was given.
   - `grant_connections` lists each connection it binds, with its profile
     revision and environment label. The engine finds the grants a saved
     connection affects with one indexed query.
   - Turning a schedule off, or deleting it, deletes its grant. Revoking is
     deleting, since a revoked grant authorises nothing. The history keeps
     the runs.
3. **Runs record the schedule.** `runs` and `chain_runs` are rebuilt so that
   `origin` takes `'manual'` or `'schedule'`. Both gain `schedule_id`
   (`ON DELETE SET NULL`, so history outlives a deleted schedule), `due_at`,
   which is set exactly when the origin is `'schedule'`, and `catch_up`,
   set only on a scheduled run that stands for missed times (Q-34). A
   catch-up run is a new run with its own row; it never re-executes or
   changes a run that started. A scheduled chain's step runs carry the
   chain's origin, schedule, due time and marker. `RunOrigin` gains
   `Schedule => "schedule"`.
4. **Deleting.**
   - A version a schedule names cannot be deleted. The repository refuses
     it and names the schedule, as it does for chains. The composite
     foreign key below makes the refusal structural.
   - Deleting a job deletes its schedule and the schedule's grant in the
     same transaction, after the interface has asked.
   - Deleting a connection suspends the schedules whose grants name it (the
     engine sets `suspended`). Grants do not reference `config.db`, which is
     another file.
5. **No queue or occurrence table.** Queued runs stay in memory (ADR-0036,
   decision 6), and so do the catch-up times waiting their turn and the
   one due time an overlap policy of *queue one* holds. Missed, skipped
   and refused times live only as the schedule's counters and last
   outcome, never as rows per time, so a schedule every minute cannot grow
   the file.
6. **Dependency.** The time-zone arithmetic uses `jiff` 0.2, as the
   [proposal](../plans/schedule-time-zone-dependency.md) asks (Q-33,
   approved 2026-11-08). Times stay UTC text in the file, as everywhere in
   `automation.db`. The rule's local times are text (`"09:00"`),
   interpreted only in the schedule's zone.

Migration 5, after ADR-0036's migration 4, inside one transaction with
foreign keys off, checked with `foreign_key_check` before the commit:

```sql
CREATE TABLE schedules (
  id                TEXT PRIMARY KEY,                    -- UUID
  job_id            TEXT NOT NULL UNIQUE REFERENCES jobs(id) ON DELETE CASCADE,
  job_version       INTEGER,                             -- NULL for a chain
  rule_json         TEXT NOT NULL,                       -- canonical JSON, section 9.5.2
  time_zone         TEXT NOT NULL,                       -- IANA name
  missed_policy     TEXT NOT NULL DEFAULT 'ignore' CHECK (missed_policy IN
                      ('ignore', 'run_once', 'queue')),  -- Q-34
  missed_limit      INTEGER CHECK (missed_limit BETWEEN 2 AND 24), -- N of 'queue'
  overlap_policy    TEXT NOT NULL DEFAULT 'skip' CHECK (overlap_policy IN
                      ('skip', 'queue_one', 'concurrent')), -- Q-35
  notify            INTEGER NOT NULL DEFAULT 0 CHECK (notify IN (0, 1)), -- Q-40 (c)
  revision          INTEGER NOT NULL DEFAULT 1,          -- grows when the rule, zone or a policy changes
  enabled           INTEGER NOT NULL CHECK (enabled IN (0, 1)),
  suspended         TEXT,                                -- reason code; NULL when not suspended
  last_due_at       TEXT,                                -- the last due time handled, UTC
  last_outcome      TEXT CHECK (last_outcome IN
                      ('started', 'caught_up', 'waiting', 'skipped_running',
                       'missed', 'refused', 'suspended')),
  last_reason       TEXT,                                -- e.g. 'queue_full', 'coalesced'; never a message
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
  id                 TEXT PRIMARY KEY,                   -- UUID
  kind               TEXT NOT NULL CHECK (kind IN ('schedule')),
  schedule_id        TEXT NOT NULL UNIQUE REFERENCES schedules(id) ON DELETE CASCADE,
  schedule_revision  INTEGER NOT NULL,                   -- the rule and zone consented to
  target_fingerprint TEXT NOT NULL,                      -- 'sha256:' + hex: the version's, or the chain plan's
  job_revision       INTEGER,                            -- a chain's revision; NULL for a version
  production         INTEGER NOT NULL CHECK (production IN (0, 1)),
  granted_at         TEXT NOT NULL
);

CREATE TABLE grant_connections (
  grant_id            TEXT NOT NULL REFERENCES grants(id) ON DELETE CASCADE,
  connection_id       TEXT NOT NULL,                     -- config.db's connection, not a foreign key
  connection_revision INTEGER NOT NULL,
  environment         TEXT NOT NULL CHECK (environment IN
                        ('none', 'development', 'staging', 'production')),
  PRIMARY KEY (grant_id, connection_id)
);

CREATE INDEX grant_connections_by_connection ON grant_connections(connection_id);

-- runs: origin gains 'schedule'; schedule_id and due_at arrive.
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
-- INSERT INTO runs_new (every column of migration 4) SELECT … FROM runs;
-- (origin keeps 'manual'; schedule_id and due_at are NULL, catch_up 0)

-- chain_runs: the same widening.
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
-- INSERT INTO chain_runs_new (…) SELECT … FROM chain_runs;

-- Drop and rename in this order: chain_runs, then runs (runs reference
-- chain_runs, run_outputs reference runs). With foreign keys off and
-- SQLite's renaming of references, every foreign key keeps naming
-- `runs` and `chain_runs`.
DROP TABLE chain_runs;
ALTER TABLE chain_runs_new RENAME TO chain_runs;
DROP TABLE runs;
ALTER TABLE runs_new RENAME TO runs;

CREATE INDEX runs_by_job         ON runs(job_id, started_at DESC);
CREATE INDEX runs_by_start       ON runs(started_at);
CREATE INDEX runs_open           ON runs(state) WHERE state = 'running';
CREATE INDEX runs_by_chain       ON runs(chain_run_id, step_index) WHERE chain_run_id IS NOT NULL;
CREATE INDEX runs_by_schedule    ON runs(schedule_id, started_at DESC) WHERE schedule_id IS NOT NULL;
CREATE INDEX chain_runs_by_job   ON chain_runs(job_id, started_at DESC);
CREATE INDEX chain_runs_by_start ON chain_runs(started_at);
CREATE INDEX chain_runs_open     ON chain_runs(state) WHERE state = 'running';
CREATE INDEX chain_runs_by_schedule ON chain_runs(schedule_id, started_at DESC) WHERE schedule_id IS NOT NULL;
```

- `schedules` is created **before** the rebuilt `runs` and `chain_runs`, which
  reference it. `schedules.last_run_id` references `runs`, whose final name
  exists once the renames are done. The whole step commits only after
  `foreign_key_check` passes, as steps 3 and 4 do.
- `user_version` becomes 5, and `LATEST_VERSION` becomes 5. A version-4
  build refuses the file untouched ("created by a newer version"), as
  [Persistence](../reference/persistence.md#automation-automationdb) says
  for any newer file.
- **Retention:** schedules and grants are never pruned. Runs and chain runs
  keep their 90 days. A pruned run sets `last_run_id` to NULL.
- **Recovery** is unchanged: a `running` scheduled run becomes `interrupted`
  like any other, and nothing is replayed. A catch-up (Q-34) is never
  submitted for an interrupted run: it stands for a due time no run
  started for, and is itself a new row.
- **Suspension reasons** are stable codes written by the engine:
  `connection_changed`, `environment_changed`, `connection_deleted`,
  `chain_changed`, `target_changed`, `program_changed`,
  `time_zone_unknown`. They are not a `CHECK`, so a later reason needs no
  migration; a code this version does not know reads as suspended for an
  unknown reason.
- **The second instance** reads `schedules` read-only with
  `open_read_only`, shows them, and runs none.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Keep `origin = 'manual'` and add only `schedule_id` | No rebuild | The origin would lie; the `CHECK` could not tie `due_at` to the origin | The history must say who started a run |
| Grant columns in `schedules` | One table fewer | Grants for `jjp/1` capabilities (section 7.2) would need their own later; revocation and consent would mix with bookkeeping | A13 names a `Grant` table; it keeps consent separate and replaceable |
| Bindings as JSON in the grant | No child table | Finding the grants a saved connection affects means scanning and parsing every grant | One indexed table |
| A row per occurrence (missed, skipped, refused) | A full audit of every time | A schedule every minute missed through a weekend writes thousands of rows that carry nothing a counter does not | Counters and the last outcome on the schedule |
| Store the next due time | Faster list | Stale after a zone database update, a clock change or a lock takeover | Computed from the rule and the zone |
| Store input values with the schedule | Schedules for jobs with required inputs | Breaks section 6.4 and CA-13 | Q-36: defaults only; chains bind constants |
| Fixed rules for missed times and overlap (the proposal: never run, skip) | No policy columns; one behaviour to explain | Some jobs must run after a closed night (a daily load) or must not lose a time while a long run ends | The maintainer chose per-schedule policies (Q-34, Q-35), with *ignore* and *skip* as defaults |
| Store the catch-up times waiting their turn | Catch-ups survive a quit | A run would start at an unexpected moment after a restart, and a table per time returns | They live in memory; dropped ones stay counted as missed |

## Consequences

- Positive:
  - A schedule runs exactly what was consented to. Any change suspends it,
    and the reason is visible.
  - The history tells scheduled runs apart and links them to their
    schedule.
  - Missed, skipped and refused times are visible without growing the
    file.
  - The user chooses, per schedule and under consent, whether missed times
    are caught up and whether a due time waits for or runs beside an
    active run; catch-up runs are marked in the history.
- Negative or costs:
  - A rebuild of `runs` and `chain_runs`, which are the largest tables, in
    migration 5. It takes about as long as migration 3 took for `runs`, on
    files whose history is at most 90 days old.
  - A composite foreign key on `schedules`.
  - Two tables for consent.
- What becomes required:
  - The repository refuses deleting a version a schedule names.
  - The engine checks the grant before each admission and when connection
    revisions change.
  - Deleting a job asks about its schedule.
  - Persistence documents migration 5.
- What becomes forbidden:
  - Storing input values or results with a schedule.
  - Running a schedule without a grant, or with a grant that does not
    match, the policies included.
  - Catching up missed times under the *ignore* policy, more than one run
    under *run once*, or more than N under *queue up to N*.
  - Re-executing a run that started: a catch-up is always a new run.
  - Two runs of one schedule at once, except under *allow concurrent runs*.

## How to validate

- Migration 5 on an empty file, and on a version-4 fixture with SQL,
  program and chain runs, outputs and versions: every row kept, the origin
  `'manual'`, `foreign_key_check` empty, the indexes present. A file at
  version 6 is refused untouched.
- The `CHECK`s: a scheduled run without `due_at`, a manual run with a
  `schedule_id` and a manual run marked catch-up are refused. A disabled
  schedule cannot be suspended. An unknown policy, a *queue* policy
  without N, an N outside 2–24, and an N with another policy are refused.
- Deleting a version a schedule names is refused, naming it. Deleting the
  job deletes the schedule and its grant. Deleting a schedule keeps its
  runs, with `schedule_id` set to NULL.
- `grant_connections_by_connection` finds the grants a connection's new
  revision suspends.
- The engine tests of the specification's
  [section 9.5.10](../plans/automation-engine-specification.md#9510-tests-and-acceptance).
