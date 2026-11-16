# Technical debt

Debts the maintainer accepted for now that need work of their own, each with
its severity and the way out. A *Known deviations* row in a standard or
reference document says only that the code does not yet follow that rule; when
the gap is one of these debts, the row points here and the story lives here.
Resolved debts are deleted; git keeps the history.

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
| TD-001 | Critical | Transactions, drivers | The transaction state is not read from the database protocol everywhere, and the probes that cover the rest can change what the session's statements see | Fix in place through a pinned SQLx fork (D-0030); open until upstream |
| TD-002 | Low | Reports, query tabs, PostgreSQL driver | Binding `:name` parameters runs a `SELECT set_config(...)` before the statement, which can take the snapshot of a transaction the report panel's or the query tab's SQL opened | Open |
| TD-003 | Medium | SQL editor, query tabs | Statements are split by the application with dialect rules and keyword-recognised blocks, not as the database splits them | Open |

## TD-001 — Transaction state is not read from the protocol

- **Severity:** Critical
- **Area:** Transactions; `jovru-core` (`Session`), `jovru-driver-postgres`,
  `jovru-driver-mysql`, `jovru-driver-sqlite`, `jovru::transaction`
- **Registered:** 2026-11-04
- **Status:** Fix in place through a pinned SQLx fork (see *Way out*,
  [D-0030](../decisions/drivers.md#d-0030)); open until a SQLx release has the
  readers ([transact-rs/sqlx#4512](https://github.com/transact-rs/sqlx/pull/4512),
  opened 2026-11-07)

### Problem

The UI shows whether a tab's session has an open transaction, uncommitted
changes or a failed transaction, and asks before closing, moving or
reconfiguring a tab that would roll one back. Getting that state wrong can
lose work silently or make the user believe changes were committed when they
were not.

Both servers report the exact state in every response (PostgreSQL in
`ReadyForQuery`, MySQL in the OK packet's status flags) and SQLite keeps it in
`sqlite3_get_autocommit`, but SQLx 0.9 keeps all three private. The
application used to infer the state from the statements run
(`jovru::transaction`), which cannot see a transaction opened or ended inside
a procedure, by a trigger or by a failed `COMMIT`, and to probe the database,
which in PostgreSQL takes the transaction's snapshot (so `SET TRANSACTION
ISOLATION LEVEL` after a lone `BEGIN` failed and `REPEATABLE READ` read from
the wrong moment) and in MySQL needs privileges a normal user may not have.

What remains after the fix:

- PostgreSQL asks `pg_current_xact_id_if_assigned()` whether an open
  transaction with a snapshot wrote: one round trip per run inside a
  transaction, and the snapshot rule reads the statement's first words.
- Whether a MySQL or SQLite transaction wrote is inferred from the
  statements, and so is everything under Databricks.
- The fork goes away only once a SQLx release has the readers.

### Way out

1. **Mitigation (done).** `Session::transaction` reports what the driver can
   get without side effects; inference applies only when the driver cannot
   tell. PostgreSQL answers "open, nothing written" without querying right
   after `BEGIN`, `START TRANSACTION` or `SET TRANSACTION`, so no snapshot is
   taken before the user's first query.
2. **Fix (in place, 2026-11-07).** Every SQLx crate points to a fork pinned to
   one revision ([D-0030](../decisions/drivers.md#d-0030)), approved by the
   maintainer. PostgreSQL reads `ReadyForQuery`, MySQL the status flags and
   SQLite `sqlite3_get_autocommit`. Live tests cover a transaction begun in a
   MySQL procedure, a deadlock's rollback, `COMMIT AND CHAIN`, an interrupted
   write and `OR ROLLBACK` in SQLite, and `BEGIN` followed by `SET
   TRANSACTION ISOLATION LEVEL` in another run.
3. **Upstream.** Move to a SQLx release that exposes the readers and drop the
   `[patch]`.

### Done when

- A SQLx release exposes the state and the workspace no longer patches SQLx.
- The drivers report the state from the protocol with no query for every
  statement a session runs, and no query the application adds takes a
  snapshot or changes what the user's next statement sees.
- Inference remains only for drivers that cannot report the state.

## TD-002 — Binding named parameters in PostgreSQL runs a query first

- **Severity:** Low
- **Area:** Reports, query tabs; `jovru-driver-postgres`
- **Registered:** 2026-11-04
- **Status:** Open

### Problem

A report or query tab statement with parameters is preceded, in the same
session, by
`SELECT set_config(name, value, false) FROM unnest($1, $2)`, which binds the
values ([D-0023](../decisions/drivers.md#d-0023),
[D-0026](../decisions/drivers.md#d-0026)). In a
transaction that the tab's SQL opened (`BEGIN` before the statement) and
that has no snapshot yet, that query takes the snapshot just before the
statement would. It runs only for statements that carry parameters, which
are queries that take a snapshot anyway, so what they read does not change;
but a parameterized statement that must come first in a transaction cannot
have parameters. The values also stay in the report's sessions (one per
database its panels use, [D-0025](../decisions/results.md#d-0025)) until their
next run, and in a query tab's session until its next run. Query tabs make
this more likely than reports, since users open transactions there.

### Way out

Bind the values with the extended protocol of the statement itself once
results can be decoded in binary, or with a protocol-level setting that
takes no snapshot.

### Done when

- Binding a report's or a query tab's parameters adds no query that takes a
  snapshot.

## TD-003 — Statements are split in the application

- **Severity:** Medium
- **Area:** SQL editor, query tabs, reports; `jovru-sql` (`statements`)
- **Registered:** 2026-11-11
- **Status:** Open

### Problem

`jovru_sql::statements` splits a script with each dialect's lexical rules, not
with a splitter the driver provides. It recognises blocks by their keywords
(`CREATE PROCEDURE/FUNCTION/TRIGGER/EVENT … BEGIN … END`, `BEGIN ATOMIC`, a
Databricks `BEGIN … END`) and splits any other construct that holds semicolons
on them, so such a construct can run as several statements.

### Way out

Let the driver or the dialect own the splitting, as the database does it, and
keep the keyword rules only where the database offers no splitter.

### Done when

- A script is split as its database splits it, with a test for each construct
  that holds semicolons the driver supports.
