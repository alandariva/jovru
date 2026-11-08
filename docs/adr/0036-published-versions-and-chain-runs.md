# ADR-0036: Published job versions and chain runs in `automation.db`

- **Status:** Accepted (by the maintainer on 2026-11-08; implemented in 0.3.0)
- **Date:** 2026-11-08
- **Authors:** Claude (draft), for the maintainer's review

## Context

Version 0.3.0 ships SQL → script as a linear chain
([specification](../plans/automation-engine-specification.md), section 9.3,
revision of 2026-11-08). A chain must run exactly what was confirmed, so it
names **published versions** of its jobs, not their editable definitions; and
its history must link each step's run to the chain run that started it.
[ADR-0034](0034-automation-database-service-and-lock.md) created `jobs` and
`runs` for SQL Jobs and said later kinds widen them with migrations of their
own; [ADR-0035](0035-persisted-declared-outputs.md) added `run_outputs`.
A persistence schema change needs an ADR
([Workflow](../standards/workflow.md#architecture-decisions)).

Forces:

- Editing a job must never change a chain that was already confirmed, nor
  a run already in the history.
- A chain run has no connection of its own; its steps do.
- Recovery must stay deterministic and never replay (ADR-0034, decision 5).
- Nothing about values changes: inputs are never stored, outputs only under
  ADR-0035.

## Decision

1. **Versions.** Publishing a job copies its definition into `job_versions`
   as version *n*, which is never updated. A chain step names
   `(job_id, version)`. A version referenced by any chain cannot be deleted
   (the repository refuses it and names the chains), and neither can its job.
2. **Chains are jobs** of kind `chain`, with their canonical JSON in `jobs`
   like any other. Chains are not published: a chain run's snapshot freezes
   the chain definition together with every step's version definition.
3. **Chain runs have their own table**, `chain_runs`, since they have no
   connection, no statement and no counters. Each step's run stays an
   ordinary row of `runs`, linked by `chain_run_id` and `step_index`, with the
   version it ran.
4. **Admission is durable in two steps.** The chain run's row is written,
   `running`, when the plan is confirmed and before any step starts; each
   step's row is written when that step is admitted, before its own I/O, as
   ADR-0034 decision 3 says. A step that never started has no row.
5. **Recovery** marks a `running` chain run `interrupted` after its steps were
   recovered, with `effect_unknown` when any of its step runs has it. It
   never starts a remaining step.
6. **No queue table.** `Queued` lives in memory only (specification,
   section 9.3.6): a queued run has done no I/O and leaves no row.

Migration, after phase 3's migration for external programs, inside one
transaction. `jobs` is rebuilt with SQLite's documented procedure for
changing a `CHECK` (new table, copy, drop, rename, `foreign_key_check`),
since its `kind` constraint widens:

```sql
-- jobs: kind gains 'chain' (phase 3 added 'process').
CREATE TABLE jobs_new (
  id              TEXT PRIMARY KEY,
  kind            TEXT NOT NULL CHECK (kind IN ('sql', 'process', 'chain')),
  definition_json TEXT NOT NULL,
  revision        INTEGER NOT NULL DEFAULT 1,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);
-- (copy, drop jobs, rename jobs_new to jobs)

CREATE TABLE job_versions (
  job_id          TEXT NOT NULL REFERENCES jobs(id) ON DELETE RESTRICT,
  version         INTEGER NOT NULL CHECK (version >= 1),
  definition_json TEXT NOT NULL,                 -- frozen; never updated
  fingerprint     TEXT NOT NULL,                 -- 'sha256:' + hex
  published_at    TEXT NOT NULL,
  PRIMARY KEY (job_id, version)
);

CREATE TABLE chain_runs (
  id             TEXT PRIMARY KEY,               -- UUID
  job_id         TEXT REFERENCES jobs(id) ON DELETE SET NULL,
  job_revision   INTEGER,
  origin         TEXT NOT NULL CHECK (origin IN ('manual')),
  state          TEXT NOT NULL CHECK (state IN
                   ('running', 'succeeded', 'failed', 'cancelled', 'interrupted')),
  cause          TEXT,
  failed_step    TEXT,                           -- the step's ID in the definition
  effect_unknown INTEGER NOT NULL DEFAULT 0 CHECK (effect_unknown IN (0, 1)),
  snapshot_json  TEXT NOT NULL,                  -- the confirmed plan, no values
  fingerprint    TEXT NOT NULL,
  started_at     TEXT NOT NULL,
  finished_at    TEXT,
  CHECK ((state = 'running') = (finished_at IS NULL)),
  CHECK ((state = 'failed') = (cause IS NOT NULL))
);

ALTER TABLE runs ADD COLUMN chain_run_id TEXT REFERENCES chain_runs(id) ON DELETE CASCADE;
ALTER TABLE runs ADD COLUMN step_index   INTEGER;  -- 0-based; NULL outside a chain
ALTER TABLE runs ADD COLUMN job_version  INTEGER;  -- NULL for a job run on its own

CREATE INDEX runs_by_chain       ON runs(chain_run_id, step_index) WHERE chain_run_id IS NOT NULL;
CREATE INDEX chain_runs_by_job   ON chain_runs(job_id, started_at DESC);
CREATE INDEX chain_runs_by_start ON chain_runs(started_at);
CREATE INDEX chain_runs_open     ON chain_runs(state) WHERE state = 'running';
```

- `chain_runs` are pruned with their steps 90 days after `finished_at`, as
  runs are (ADR-0034); deleting a chain run deletes its step runs and, through
  them, their outputs.
- `job_versions` are never pruned: a chain may name a version for as long as
  it exists. Deleting a job deletes its versions only when no chain names
  any of them.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Chains name jobs, not versions | No versions table | Editing a job silently changes every chain using it | The plan the user confirmed must be what runs (P-21) |
| Snapshot only, no published versions | Each run is still exact | Nothing stops a chain from picking up an edit between two runs; no stable thing to name | Versions give chains a fixed target and runs keep their snapshot too |
| Chain runs as rows of `runs` | One table | `runs` has statement, connection and counter columns a chain does not have; more `NULL`s and weaker `CHECK`s | A table per kind of run keeps every constraint meaningful |
| Persist `queued` | A crash shows what was waiting | A row for a run that did nothing; recovery for a state that has no effect | Nothing to recover: queued runs did no I/O |
| Resume an interrupted chain | Less manual work | Needs effects known up to the step; risks replay | Excluded (section 9.6); the user runs it again |

## Consequences

- Positive: a chain runs exactly what was published and confirmed; history
  shows a chain and each step; recovery stays one rule per table.
- Negative or costs: a table rebuild of `jobs` in the migration; versions
  accumulate for as long as chains name them; publishing is a step the user
  must take before a job can be used in a chain.
- What becomes required: publishing in the interface (phase 2); the
  repository refuses deleting a version or job a chain names.
- What becomes forbidden: updating a row of `job_versions`; a chain step
  that names a draft definition; starting a remaining step at recovery.

## How to validate

- Publishing twice gives versions 1 and 2 with their own definitions;
  editing the job changes neither.
- Deleting a version or a job a chain names is refused, naming the chain.
- A forced exit in the middle of step 2 of 3: the chain run and step 2 become
  `interrupted` (with `effect_unknown` as step 2's rule says), step 3 has no
  row, and nothing runs at the next start.
- The migration on a database with jobs and runs from migrations 1–3 keeps
  every row and passes `foreign_key_check`.
