# ADR-0035: Declared scalar outputs persisted for chains, small, opt-in and short-lived

- **Status:** Accepted (by the maintainer on 2026-11-08; not implemented)
- **Date:** 2026-11-08
- **Authors:** Alan de Oliveira (decision Q-03), Claude (draft)

## Context

Version 0.3.0 delivers SQL → script: a SQL Job's output feeds an external
program in a linear chain
([specification](../plans/automation-engine-specification.md), sections 4.7
and 9.3). In memory, outputs live only while the application runs. The
specification forbids persisting any value without an ADR (section 6.4,
CA-13), and [Persistence](../reference/persistence.md) says the application
never writes query results to disk on its own. On 2026-11-08 the maintainer
answered Q-03 by approving the review's proposed defaults (D29): at most
4 KiB per run, values kept 7 days, metadata 90 days, opt-in per job and an
extra confirmation for production.

Forces:

- A chain step may need an earlier step's output after a restart, or a step
  run again on its own may need the outputs it was fed.
- Outputs are results: a `COUNT(*)` is data, and a value may be personal or
  confidential.
- Secrets and tokens must never become ordinary outputs (section 9.5).

## Decision

1. **Declared outputs only.** A job declares its outputs by name and kind
   (`ParameterKind`), and how each is produced: for a SQL Job, an explicit
   mapping from a result column by position, with the rule for zero, one and
   several rows stated in the definition; for an external program, the
   outputs of its `complete` message (`jjp/1`). Nothing undeclared is ever
   kept: no cell, row or message.
2. **Opt-in per job.** A definition persists its outputs only with
   `persist_outputs: true`. Without it, outputs are in memory only, as
   before. Turning it on for a job whose connection is marked production
   asks for an extra confirmation, and every run of such a job says in its
   confirmation that its outputs will be stored.
3. **Small.** The values of one run take at most **4 KiB** of text in total.
   A run whose outputs exceed it stores none of them, records that they were
   not stored, and the chain still uses them in memory; the run itself does
   not fail for it.
4. **Short-lived.** Values are deleted **7 days** after the run finished,
   leaving their name, kind and size; that metadata goes with the run row
   after **90 days** ([ADR-0034](0034-automation-database-service-and-lock.md)).
   Pruning runs in the background and never blocks admission.
5. **Never elsewhere.** Values never reach logs, spans, telemetry, error
   reports or the snapshot. They are not secrets: the editor says outputs
   are not for tokens or passwords, and a later secret broker (phase 7) is a
   separate store.
6. **Exact text.** A value is stored as the database's or the program's
   text, NULL apart from the text `NULL`, never through floating point
   (golden rule 9).

Migration 2 of `automation.db`:

```sql
CREATE TABLE run_outputs (
  run_id           TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  position         INTEGER NOT NULL,          -- declaration order
  name             TEXT NOT NULL,
  kind             TEXT NOT NULL,             -- a ParameterKind
  value            TEXT,                      -- NULL when the output is NULL or expired
  is_null          INTEGER NOT NULL CHECK (is_null IN (0, 1)),
  bytes            INTEGER NOT NULL,          -- UTF-8 length of the value
  value_expires_at TEXT NOT NULL,             -- finished_at + 7 days, UTC
  expired          INTEGER NOT NULL DEFAULT 0 CHECK (expired IN (0, 1)),
  PRIMARY KEY (run_id, position),
  UNIQUE (run_id, name),
  CHECK (NOT (is_null = 1 AND value IS NOT NULL)),
  CHECK (NOT (expired = 1 AND value IS NOT NULL))
);

CREATE INDEX run_outputs_expiring ON run_outputs(value_expires_at) WHERE expired = 0;
```

And on `runs`, `outputs_stored INTEGER` (`NULL` when the job does not
persist outputs, `0` when they exceeded the quota, `1` when stored).

This ADR amends the specification's section 6.4 and CA-13, and the
Persistence rule that results are never written on the application's own:
declared, opt-in outputs within these limits are the one exception.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Outputs in memory only | Nothing to protect on disk | No chain across restarts; a step run again alone has no inputs | Q-03 chose persistence |
| Never persist, forbid for good | Simplest privacy story | Same as above, permanently | Q-03 |
| Larger quota or longer retention | Fewer "not stored" runs | More data at rest, longer exposure | The defaults were approved as they are |
| Store outputs inside `runs` as JSON | One table | Harder to expire values while keeping metadata; quota checks in JSON | A table keeps each rule a column |

## Consequences

- Positive: chains survive restarts; a step can be run again with the inputs
  it had; the history shows which outputs a run produced and their size.
- Negative or costs: values at rest for up to 7 days; another migration;
  the production confirmation adds a step.
- What becomes required: tests that a job without opt-in stores nothing,
  that the quota stores all or none, that values expire on time and that no
  value reaches a log or span.
- What becomes forbidden: persisting anything not declared, or any output
  of a job that did not opt in.

## How to validate

- A job with opt-in and sentinel values: the values are in `run_outputs`,
  and nowhere in logs, spans or the snapshot.
- 4,096 bytes are stored; 4,097 store nothing and set `outputs_stored = 0`.
- With the clock moved past 7 days, pruning empties `value` and sets
  `expired`; past 90 days the run and its outputs are gone.
- Turning opt-in on for a production job asks for confirmation.
