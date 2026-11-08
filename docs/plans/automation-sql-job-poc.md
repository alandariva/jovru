# Automation engine — SQL Job proof of concept

**Date:** November 7, 2026
**Approval:** Proposed
**Implementation:** Not started
**Specification:** [Automation engine specification](automation-engine-specification.md), version 1.1

## 1. Purpose

Reduce the risk of phase 1 of the specification before the maintainer answers
the questions that block it (Q-01…Q-08). The proof of concept tests only what
is **new** in a SQL Job: reports already prove parameterized SQL with a form,
so the questions left are how a run ends, how it is owned and cancelled, and
whether it can share a connection with a tab safely.

It needs **no approval** because it stays inside these limits:

- no new dependency, no SQLite schema or migration, no ADR;
- no change to `jykre-core`, the driver contracts or the drivers;
- no GPUI, no view, nothing wired into the application;
- run history kept in memory only.

The code lives in `src/automation/` in the `jykre` library, GPUI-free like
`src/reports`, on a branch that does not merge into 0.2.0
([specification, section 1.4](automation-engine-specification.md#14-release-placement)).
Whether it becomes the base of phase 1 or is discarded is decided with its
findings.

## 2. Hypotheses

| ID | Hypothesis | Open risk it addresses |
| --- | --- | --- |
| H1 | A SQL Job runs through existing contracts only: `ParameterSpec`, `parameters::bind`, the dialect splitter and a new session opened as `backend::query_in_context` opens one, in a pinned context. | P-04, P-20; no new trait or crate (D04) |
| H2 | The completion rule (spec section 4.6) works without changing `DriverOutcome`: a `Session::fetch` loop into a discarding, counting sink confirms the end of a write's result on every driver, within the deadline. | P-05; answers Q-08 |
| H3 | A run never commits, rolls back or changes the context of a transaction open in another session of the same connection. | CA-02 |
| H4 | A GPUI-free engine with one owner per run, admission limits (2 global, 1 per connection, refusal), immediate cancellation and a deadline distinct from Stop keeps a single terminal state under races, and keeps running when its subscriber is dropped. | P-08, P-09, P-27; Q-05, Q-07 |
| H5 | The in-memory output keeps ordered columns, rows as arrays, duplicate column names, exact decimals and NULL distinct from the text `NULL`. | P-06, golden rule 9 |

## 3. Scope

### 3.1 In

- `SqlJob`: connection, pinned `SessionContext`, SQL, `Vec<ParameterSpec>`,
  timeout (default 300 s) and row limit (default 1,000), built in code; no
  storage.
- `Executor::run_sql`: validate one statement, bind, open a new session, run,
  apply the completion rule, close the session, return a `RunOutcome` with
  state, cause, `effect_unknown`, `truncated`, delivered rows,
  `rows_affected` and the in-memory result.
- `Engine`: admission, one task per run as its owner, `CancellationToken`,
  deadline, bounded event channel, in-memory run list, and a subscriber handle
  whose drop does not cancel.
- Tests in section 4.

### 3.2 Out

`automation.db`, the data-directory lock and recovery, any UI, external
processes, `jjp`, chains, queues, cache, secrets, artifacts, Databricks live
runs (no profile in the gate; covered by reasoning in the spec's driver table
and recorded as not run).

## 4. Experiments and tests

All fixtures are synthetic. SQLite runs in the basic gate; PostgreSQL and
MySQL run as `live` tests in `tests/live.rs` under `cargo xtask check --full`.

| ID | Hypothesis | Experiment | Pass condition |
| --- | --- | --- | --- |
| E1 | H1 | Bind `:name` next to `::text`, inside comments, strings and dollar quotes; two statements | Values bound as parameters; markers inside literals untouched; two statements refused before a session opens |
| E2 | H2 | `UPDATE … RETURNING` over 10,000 rows with a 1,000-row limit, on PostgreSQL and SQLite; on MySQL, which has no `RETURNING`, a procedure call that writes and then returns rows | Every row updated; run `Succeeded` with `truncated` and `rows_affected` correct; drain time recorded |
| E3 | H2 | Same statement with a deadline shorter than the drain | Run `Failed` with cause `Timeout` and `effect_unknown`; no second execution observed |
| E4 | H2 | A `SELECT` above the limit | Truncated without draining; `effect_unknown` not set |
| E5 | H3 | Open a transaction with an uncommitted row in one session; run a job on the same connection that reads and writes another table | The open transaction still holds its row and context; the job's write is committed on its own |
| E6 | H4 | Three runs at once on two connections | Third refused immediately with the limit as reason; per-connection second run refused |
| E7 | H4 | Stop and deadline fired at the same instant, many iterations | Exactly one terminal state per run; cause matches the event the owner observed first |
| E8 | H4 | Drop the subscriber handle mid-run | Run reaches its terminal state; session closed |
| E9 | H5 | `SELECT a.id, b.id`, a `NUMERIC(38, 18)` value, `NULL` and `'NULL'` | Two columns in order; decimal text identical to the source; NULL and the text distinct |

## 5. Measurements

For each driver in E2: rows drained per second, time from the first page to
confirmed end, and peak memory of the run. These numbers decide Q-08: if the
drain confirms completion within the 5 s recovery window for realistic
results, phase 1 keeps the conservative rule; if not, the PoC report proposes
the typed completion field in `DriverOutcome` as a contract change for the
maintainer.

## 6. Deliverables

1. The code and tests above, passing `cargo xtask check` and
   `cargo xtask check --full`, with any test not run (Databricks) reported as
   such.
2. A *Findings* section appended to this document: results per hypothesis,
   the measurements of section 5, what phase 1 inherits, what it must change,
   and a recommendation for Q-07 and Q-08.

## 7. Exit

The proof of concept ends when every hypothesis is confirmed or refuted with
evidence. A refuted hypothesis updates the specification before phase 0
closes; it is not worked around in phase 1.
