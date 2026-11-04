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
| TD-001 | Critical | Transactions, drivers | The transaction state is not read from the database protocol, and the probes that replace it can change what the session's statements see | Mitigated; fix open |

## TD-001 — Transaction state is not read from the protocol

- **Severity:** Critical
- **Area:** Transactions; `jykre-core` (`Session`), `jykre-driver-postgres`,
  `jykre-driver-mysql`, `jykre::transaction`
- **Registered:** 2026-11-04
- **Status:** Mitigated (see *Way out*, step 1); the fix is open

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
already carries.

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
2. **Fix.** Read the protocol state: get SQLx to expose it (an upstream pull
   request, or a `[patch]` until a release has it). Then the drivers return
   it from `Session::transaction` with no query at all, the probes go away
   and the contract does not change. Adding or patching a dependency needs
   the maintainer's approval ([Stack and dependencies](stack-and-dependencies.md)).

### Done when

- The PostgreSQL and MySQL drivers report the state from the protocol, with
  no query, for every statement a session runs.
- No query the application adds to a session takes a snapshot or changes
  what the user's next statement sees; a live test covers `BEGIN` followed
  by `SET TRANSACTION ISOLATION LEVEL` in another run.
- Inference remains only for drivers that cannot report the state.
