# ADR-0037: External program runs in `automation.db`

- **Status:** Accepted (by the maintainer on 2026-11-08; not implemented)
- **Date:** 2026-11-08
- **Authors:** Alan de Oliveira (decisions Q-15…Q-19, Q-25…Q-29), Claude (draft)

## Context

Phase 3 of the [automation specification](../plans/automation-engine-specification.md)
(section 9.1, revision of 2026-11-08) runs external programs. The schema of
[ADR-0033](0033-automation-database-service-and-lock.md) was written for SQL
Jobs: every run has a connection, a statement and row counters, and `jobs`
accepts only the kind `sql`. A program run has no connection; it has an exit
code, a process group to recover, a file whose hash the version pins, and
logs on disk ([ADR-0036](0036-program-logs-on-disk.md)).
[ADR-0035](0035-published-versions-and-chain-runs.md) assumes this migration
comes before its own. A persistence schema change needs an ADR.

## Decision

1. **One `runs` table for both kinds**, with a `kind` column, so history,
   pruning, recovery and the chain links of ADR-0035 stay one code path.
   Columns that belong to one kind are `NULL` for the other, and `CHECK`s
   keep each kind's required columns.
2. **What a program run records:** the exit code or the Unix signal; the
   process group ID and the process's start time as the system reports it,
   for recovery (section 9.1.6); the SHA-256 of the entrypoint file it ran;
   the `fail` code under `jjp/1`; and the size of each log file. Never
   arguments' values beyond the definition, environment values, log content
   or the `fail` message.
3. **The version's file** (path and SHA-256) lives in the version's
   definition JSON (ADR-0035's `job_versions`), not in a column: it is part of
   what was published.
4. **Recovery** marks `running` program runs `interrupted` with
   `effect_unknown`, and offers to end a group whose leader still has the
   recorded start time; it never ends any other process (Q-29).

Migration 3, inside one transaction. `jobs` and `runs` are rebuilt with
SQLite's documented procedure (new table, copy, drop, rename,
`foreign_key_check`), since a `CHECK` widens and `connection_id` becomes
nullable:

```sql
CREATE TABLE jobs_new (
  id              TEXT PRIMARY KEY,
  kind            TEXT NOT NULL CHECK (kind IN ('sql', 'process')),
  definition_json TEXT NOT NULL,
  revision        INTEGER NOT NULL DEFAULT 1,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);
-- (copy, drop jobs, rename jobs_new to jobs)

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
  reads_only          INTEGER NOT NULL CHECK (reads_only IN (0, 1)),  -- 0 for programs
  snapshot_json       TEXT NOT NULL,
  fingerprint         TEXT NOT NULL,
  -- SQL runs
  connection_id       TEXT,
  connection_revision INTEGER,
  rows_delivered      INTEGER,
  rows_discarded      INTEGER,
  rows_affected       INTEGER,
  truncated           INTEGER NOT NULL DEFAULT 0 CHECK (truncated IN (0, 1)),
  server_code         TEXT,
  -- program runs
  exit_code           INTEGER,
  exit_signal         INTEGER,                     -- Unix only
  process_group       INTEGER,                     -- Unix only, for recovery
  process_started     TEXT,                        -- the system's start time of the leader
  program_fingerprint TEXT,                        -- 'sha256:' + hex of the entrypoint run
  failure_code        TEXT,                        -- jjp/1 fail code, at most 64 characters
  log_bytes_stdout    INTEGER,
  log_bytes_stderr    INTEGER,
  log_bytes_jjp       INTEGER,
  outputs_stored      INTEGER,                     -- ADR-0034
  started_at          TEXT NOT NULL,
  finished_at         TEXT,
  CHECK ((state = 'running') = (finished_at IS NULL)),
  CHECK ((state = 'failed') = (cause IS NOT NULL)),
  CHECK ((kind = 'sql') = (connection_id IS NOT NULL)),
  CHECK ((kind = 'sql') = (connection_revision IS NOT NULL)),
  CHECK (kind = 'process' OR (exit_code IS NULL AND exit_signal IS NULL
         AND process_group IS NULL AND program_fingerprint IS NULL))
);
-- (copy every run as kind 'sql', drop runs, rename runs_new to runs)

CREATE INDEX runs_by_job   ON runs(job_id, started_at DESC);
CREATE INDEX runs_by_start ON runs(started_at);
CREATE INDEX runs_open     ON runs(state) WHERE state = 'running';
```

`outputs_stored`, added to `runs` by ADR-0034's migration 2, is carried into
the rebuilt table.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| A table per kind (`sql_runs`, `process_runs`) | Strict columns per kind | History, pruning, recovery and chain links split in two; a chain step points to one of two tables | One table with `CHECK`s keeps one path |
| Program details as JSON in a column | No rebuild | Recovery and pruning query inside JSON; no constraints | Columns are cheap and checkable |
| Record the PID instead of the group and start time | Simpler | A reused PID could be killed | Q-29 requires the start time |
| Copy the program file into the database | Reproducible runs | Q-16 chose no copy | — |

## Consequences

- Positive: one history for SQL and program runs; recovery can tell a live
  orphan from a reused ID; a version's file is checked before every run.
- Negative or costs: a rebuild of `jobs` and `runs` in one migration; more
  nullable columns in `runs`.
- What becomes required: migration tests from migrations 1–2 with rows of
  every state, then `foreign_key_check`; recovery tests with a live group, a
  dead one and a reused ID (Linux).
- What becomes forbidden: storing environment values, log content or `fail`
  messages in `automation.db`; ending a process whose start time does not
  match.

## How to validate

- A database with SQL runs from migrations 1–2 migrates with every row kept
  and `kind = 'sql'`.
- A program run that exits 3 records `exit_code = 3`, cause `Exit`; one
  killed by a signal records `exit_signal`.
- After a forced exit of Studio with a program running (Linux), the next
  start offers to end it; with the leader's ID reused by another process, it
  offers nothing.
