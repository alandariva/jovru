# Technical debt

Known problems the maintainer accepted for now, with their severity and the
way out. A *Known deviations* table in another document says where the code
differs from a standard; this document tracks debts that need work of their
own, across areas.

## Severity

| Severity | Meaning |
| --- | --- |
| Critical | Can lose or misreport the user's data or change what a statement does. Next in line for work |
| High | Wrong or misleading behaviour in a common flow, with a workaround |
| Medium | Wrong behaviour in an uncommon flow, or a cost the user notices |
| Low | Polish, internal cleanliness, small costs |

## Summary

| ID | Severity | Area | Debt | Status |
| --- | --- | --- | --- | --- |
| TD-001 | Critical | Transactions, drivers | The transaction state is not read from the database protocol, and the probes that replace it can change what the session's statements see | Fix in place through a pinned SQLx fork (ADR-0030, proposed); open until upstream |
| TD-002 | Low | Reports, query tabs, PostgreSQL driver | Binding `:name` parameters runs a `SELECT set_config(...)` before the statement, which can take the snapshot of a transaction the report panel's or the query tab's SQL opened | Open |

## TD-001 — Transaction state is not read from the protocol

- **Severity:** Critical
- **Area:** Transactions; `jykre-core` (`Session`), `jykre-driver-postgres`,
  `jykre-driver-mysql`, `jykre-driver-sqlite`, `jykre::transaction`
- **Registered:** 2026-11-04
- **Status:** Fix in place through a pinned SQLx fork (see *Way out*,
  step 2, and [ADR-0030](../adr/0030-transaction-state-through-a-pinned-sqlx-fork.md),
  proposed); open until a SQLx release has the readers. The *Problem* below
  describes the state before that fix.

### Problem

The UI shows whether a tab's session has an open transaction, uncommitted
changes or a failed transaction, and asks before closing, moving or
reconfiguring a tab that would roll one back. Getting that state wrong can
lose work silently (a transaction rolled back without asking) or make the
user believe changes were committed when they were not.

Both servers report the exact state in every response: PostgreSQL in each
`ReadyForQuery` message (`I` idle, `T` in a transaction, `E` failed) and
MySQL in the status flags of each OK packet (`SERVER_STATUS_IN_TRANS`,
`SERVER_STATUS_AUTOCOMMIT`). SQLx 0.9 reads both but keeps them private
(`PgConnection::in_transaction` and `MySqlConnection::in_transaction` are
`pub(crate)`). Its public `Connection::is_in_transaction` counts only
transactions opened through SQLx's own API, so it is wrong for a `BEGIN` the
user typed. The drivers therefore cannot read the state that the protocol
already carries. SQLite keeps it in `sqlite3_get_autocommit`, which SQLx
reaches only through `unsafe` FFI ([ADR-0027](../adr/0027-sqlite-driver.md)), so
the SQLite driver reports nothing and only the inference applies.

What the application does instead, and why each falls short:

1. **Inference from the statements run** (`jykre::transaction`). It follows
   `BEGIN`, `COMMIT`, `ROLLBACK`, MySQL's `SET autocommit` and implicit
   commits, and PostgreSQL failures. It cannot see a transaction opened or
   ended inside a procedure, by a trigger or by a failed `COMMIT` it does not
   recognise.
2. **Probe queries after each run.** No query tells whether a transaction is
   open without side effects. In PostgreSQL any `SELECT` inside a
   transaction takes its snapshot. The session context probe that already
   runs after each run (`SELECT current_database(), current_schema()`) does
   this, and so would a transaction probe:
   - under `REPEATABLE READ` and `SERIALIZABLE`, the transaction's snapshot
     becomes the one at the end of the run that opened it, not the one at the
     user's first query;
   - `SET TRANSACTION ISOLATION LEVEL …` run after a `BEGIN` that ran on its
     own fails with "must be called before any query".

   In MySQL, the only queries that tell whether a `BEGIN` is open need the
   `PROCESS` privilege (`information_schema.innodb_trx`) or an enabled
   `performance_schema` consumer, which a normal user may not have.

### Way out

1. **Mitigation (done).** `Session::transaction` reports the state
   the driver can get without side effects, and the application uses
   inference only when the driver says it cannot tell:
   - PostgreSQL probes with the simple query protocol
     (`transaction_timestamp() <> statement_timestamp()` and
     `pg_current_xact_id_if_assigned()`), but not right after `BEGIN`,
     `START TRANSACTION` or `SET TRANSACTION`: there it answers "open,
     nothing written" without querying, and the context probe answers from
     what the session already knows, so no snapshot is taken before the
     user's first query.
   - MySQL reports `autocommit = 0` from `@@autocommit` and leaves a `BEGIN`
     to inference.

   A live test (`live_the_transaction_state_is_reported_without_taking_a_snapshot`)
   covers `BEGIN` followed by `SET TRANSACTION ISOLATION LEVEL` in another
   run. What remains: one extra round trip per run in PostgreSQL, the
   snapshot rule depends on reading the statement's first word, and MySQL
   still infers a `BEGIN`.
2. **Fix (in place, 2026-11-07).** Read the protocol state: get SQLx to
   expose it (an upstream pull request, or a `[patch]` until a release has
   it). Then the drivers return it from `Session::transaction`, the probes
   go away and the contract does not change. Adding or patching a
   dependency needs the maintainer's approval
   ([Stack and dependencies](../standards/stack-and-dependencies.md)).

   The maintainer chose the patch: every SQLx crate points to a fork pinned
   to one revision ([ADR-0030](../adr/0030-transaction-state-through-a-pinned-sqlx-fork.md)).
   PostgreSQL reads `ReadyForQuery`, MySQL the status flags and SQLite
   `sqlite3_get_autocommit`; the timestamp probe and the `@@autocommit`
   query are gone. The snapshot rule also learned `… AND CHAIN`, which made
   both PostgreSQL probes take the chained transaction's snapshot. Live
   tests cover a transaction begun in a MySQL procedure, a deadlock's
   rollback, `COMMIT AND CHAIN` and, in SQLite, an interrupted write and
   `OR ROLLBACK`; the `BEGIN` and `SET TRANSACTION` test still passes
   unchanged. What remains:
   - PostgreSQL still asks `pg_current_xact_id_if_assigned()` whether an
     open transaction with a snapshot wrote, one round trip per run inside
     a transaction, and the snapshot rule still reads the statement's first
     words.
   - Whether a MySQL or SQLite transaction wrote is inferred from the
     statements.
   - The fork goes away only once a SQLx release has the readers; the
     upstream pull request is not open yet.

### Done when

- The PostgreSQL and MySQL drivers report the state from the protocol, with
  no query, for every statement a session runs.
- No query the application adds to a session takes a snapshot or changes
  what the user's next statement sees; a live test covers `BEGIN` followed
  by `SET TRANSACTION ISOLATION LEVEL` in another run.
- Inference remains only for drivers that cannot report the state.

## TD-002 — Binding named parameters in PostgreSQL runs a query first

- **Severity:** Low
- **Area:** Reports, query tabs; `jykre-driver-postgres`
- **Registered:** 2026-11-04
- **Status:** Open

### Problem

A report or query tab statement with parameters is preceded, in the same
session, by
`SELECT set_config(name, value, false) FROM unnest($1, $2)`, which binds the
values ([ADR-0023](../adr/0023-named-parameters-bound-by-drivers.md),
[ADR-0026](../adr/0026-named-parameters-in-sql-tabs.md)). In a
transaction that the tab's SQL opened (`BEGIN` before the statement) and
that has no snapshot yet, that query takes the snapshot just before the
statement would. It runs only for statements that carry parameters, which
are queries that take a snapshot anyway, so what they read does not change;
but a parameterized statement that must come first in a transaction cannot
have parameters. The values also stay in the report's sessions (one per
database its panels use, [ADR-0025](../adr/0025-report-panels.md)) until their
next run, and in a query tab's session until its next run. Query tabs make
this more likely than reports, since users open transactions there.

### Way out

Bind the values with the extended protocol of the statement itself once
results can be decoded in binary, or with a protocol-level setting that
takes no snapshot.

### Done when

- Binding a report's or a query tab's parameters adds no query that takes a
  snapshot.

