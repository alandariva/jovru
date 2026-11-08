# Automation engine — SQL Job proof of concept

**Date:** November 7, 2026
**Approval:** Proposed
**Implementation:** Partial — code and basic-gate experiments complete; PostgreSQL and MySQL live experiments written, not yet run ([Findings](#8-findings))
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

## 8. Findings

**Date:** November 8, 2026. **Branch:** `claude/automation-sql-job`, cut from
`release/0.2.0` and not merged into it.

### 8.1 What was built

- `src/automation/` in the `jykre` library: `SqlJob`, `prepare`, the
  executor and `Engine`, with no new dependency, schema, ADR, change to
  `jykre-core` or the drivers, and nothing wired into the application.
- 17 unit tests in `src/automation/tests.rs` (a scripted driver for
  admission and races, synthetic SQLite files for statements), run by the
  basic gate.
- Four live tests in `tests/live.rs` (`automation::*`, E2, E3, E5, E9) for
  PostgreSQL and MySQL, run by `cargo xtask check --full`.
- `examples/automation_drain.rs`, the measurement of section 5, on a new
  SQLite file or on a fixture named by `JYKRE_CONNECTION`.

### 8.2 Checks

| Check | Result |
| --- | --- |
| `cargo xtask check` | Passed (Linux) |
| `cargo xtask check --full` | **Not run**: no Docker on the machine used, and `cargo-deny` is not installed. The four live tests are compiled and ignored |
| Databricks | Not run, as planned (no profile in the gate) |
| E7 repeated | The 200-iteration race passed in three further runs |

### 8.3 Results by hypothesis

| Hypothesis | Result | Evidence |
| --- | --- | --- |
| H1 | **Confirmed** (SQLite; PostgreSQL dialect checked offline) | `reports::params::requests` binds `ParameterSpec` values; markers in strings, comments, quoted names and dollar quotes and the `::` cast are left alone, and the SQL is sent as written. Two statements, or none, are refused before a session opens (the scripted driver opened none). A hostile value reached SQLite as a value |
| H2 | **Confirmed on SQLite**; PostgreSQL and MySQL pending `--full` | `UPDATE … RETURNING` over 10,000 rows with a 1,000-row limit: `Succeeded`, `truncated`, 1,000 rows kept, 9,000 read and discarded, every row updated once. The same write past a 300 ms deadline: `Failed`/`Timeout`, `effect_unknown`, rows changed together or not at all. A `SELECT` above the limit: truncated, nothing drained, `effect_unknown` clear. No `DriverOutcome` change was needed |
| H3 | **Confirmed on SQLite**, with a limitation; PostgreSQL and MySQL pending `--full` | The job's session does not see the tab's uncommitted row, and the tab's transaction is still open with its row afterwards. SQLite has one writer, so a job that writes while a tab holds a write transaction waits for the lock (4 s) and fails with `Database` and its effect known; it never commits or rolls back the tab's work |
| H4 | **Confirmed** | Third run refused at once with the global limit, second run on a connection refused with the connection; Stop ends a run within a second and closes its session; 200 runs with Stop and the deadline fired together each produced exactly one outcome, the same in the history, with every session closed; dropping every handle leaves the run to finish; `shutdown` stops every run, waits, and refuses new ones |
| H5 | **Confirmed** on SQLite and through a scripted exact decimal; PostgreSQL and MySQL `DECIMAL(38, 18)` pending `--full` | `ResultData` already keeps ordered columns with duplicate names, rows by position, the database's text and NULL apart from the text `NULL`. Phase 1 needs no new output type |

### 8.4 Measurements (section 5)

SQLite only, release build, `examples/automation_drain.rs`, each row an
`INTEGER` and 200 bytes of text:

| Rows returned | First page to confirmed end | Rows/s drained | Result held | Process peak |
| --- | --- | --- | --- | --- |
| 10,000 | 35 ms | 257,000 | 211 KB | 10.8 MB |
| 100,000 | 389 ms | 254,000 | 211 KB | 13.7 MB |
| 1,000,000 | 3,941 ms | 253,000 | 211 KB | 17.9 MB |

The result held stays at the row limit whatever the size: the drain keeps
nothing. SQLite applies every change of an `UPDATE … RETURNING` in its first
step and buffers what it returns, so on SQLite the drain only reads. The
PostgreSQL and MySQL numbers come from running the same example against the
gate's fixtures, which is still to do.

### 8.5 What phase 1 inherits

- `SqlJob`, `prepare` and the completion rule as written: one statement by
  the dialect's splitter, `reports::params::requests` for binding,
  `sql::reads_only` to choose the rule.
- A write's sink never asks the driver to stop. Returning `Stop` would drop
  the result early: MySQL then treats it as cut, drains for up to 5 s and may
  `KILL QUERY`, and PostgreSQL cancels the backend at the next command. Rows
  past the limits are counted and dropped instead.
- Every driver call gets the time left before the deadline as its timeout,
  and the owner also cancels at the deadline, so either side ends the run
  with `Timeout` and the driver's own recovery runs.
- One owner task per run; the outcome is recorded before it is published, and
  a confirmed completion ready in the same instant as Stop or the deadline
  wins. Progress and the outcome travel through one `watch` channel per run:
  bounded, the latest value replaces earlier progress, and the outcome, as
  the last value, is never lost (section 5.6).
- `ResultData` as the in-memory output.

### 8.6 What phase 1 must change or decide

- **A backend function of its own.** `backend::query_in_context` cannot run a
  job: it settles the session after the run, which discards a write's
  result still open for paging. The executor calls `session_connection`,
  `open_session`, `execute`, `fetch` and `backend::close_session` directly;
  phase 1 moves that into `backend` (spec section 5.1 already allows a new
  function) with its telemetry spans.
- **The runtime handle.** `Engine` needs a handle to the application's only
  runtime, which `backend` keeps private; phase 1 exposes it or builds the
  engine inside `backend`.
- **`rows_affected` is `None` for a statement that returns rows**, as the
  drivers report it. The history's count for `UPDATE … RETURNING` is the rows
  read (kept and discarded); the persisted fields of section 6.4 should say
  so.
- **Refusals are not runs.** A job that fails `prepare` is refused before
  admission and leaves no history, like a tab that stops before sending.
  `Failed`/`Validation` remains for what a driver refuses after admission.
  Section 5.4 should state this.
- **Effect after Stop.** Once Stop or the deadline is observed, a sent write
  is marked `effect_unknown` even if the driver then confirms completion
  while recovering. This is conservative and follows the arbitration rule;
  phase 1 may report the late confirmation without changing the state.
- **SQLite writers.** Section 4.4's "new session per run" means a write job
  on a SQLite file that a tab is writing waits up to 4 s and fails. Document
  it beside the per-connection limit (section 5.3).
- `Interrupted` is not implemented: it needs durable history and recovery.

### 8.7 Recommendations

- **Q-07:** confirm 2 global runs, 1 per connection, immediate refusal and a
  300 s timeout. The limits behaved as specified; a run costs one connection,
  opened and closed with it.
- **Q-08:** keep the conservative rule in phase 1, with no change to
  `DriverOutcome`. On SQLite the drain confirmed a million returned rows in
  under 4 s. Revisit only if the PostgreSQL or MySQL measurements show a
  realistic result that cannot be confirmed within the 5 s recovery window.

### 8.8 Left to finish the proof of concept

1. Run `cargo xtask check --full` on a machine with Docker and `cargo-deny`,
   and record the four live tests' results.
2. Run `examples/automation_drain.rs` against PostgreSQL 14/18 and MySQL
   8.4/9.7 and add the numbers to 8.4. On MySQL, E2 and E3 also show whether
   a `CALL` that writes and then returns rows reaches the sink as expected.
