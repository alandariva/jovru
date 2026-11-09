# ADR-0030: Transaction state read from the protocol through a pinned SQLx fork (amends 0002 and 0027)

- **Status:** Accepted (implemented on 2026-11-07; accepted by the
  maintainer on 2026-11-07)
- **Amended:** 2026-11-09, with the maintainer's approval: the pinned
  revision is now `8ddadbe8083766320e8ca1417c62ce3c8a689658`, one more
  commit on the same branch that lets `sqlx-sqlite` take `libsqlite3-sys`
  0.39 (bundled SQLite 3.54.1), for RUSTSEC-2026-0459. Nothing else in the
  fork changed
- **Date:** 2026-11-07
- **Authors:** Alan de Oliveira (with an AI agent)

## Context

Version 0.2.0 must show whether a tab's session has an open transaction,
pending changes or a failed one, matching the real session state wherever the
database lets a client read it (M1 in
[Version 0.2.0](../plans/releases/0.2.0.md),
[TD-001](../plans/technical-debt.md#td-001--transaction-state-is-not-read-from-the-protocol)).

Each database already tells the client:

- PostgreSQL ends every response with `ReadyForQuery`, whose status is `I`
  (idle), `T` (in a transaction) or `E` (failed transaction).
- MySQL puts `SERVER_STATUS_IN_TRANS` and `SERVER_STATUS_AUTOCOMMIT` in the
  status flags of every OK and EOF packet. ERR packets carry no flags.
- SQLite keeps it in the connection (`sqlite3_get_autocommit`).

SQLx 0.9.0 ([ADR-0002](0002-sqlx-for-postgres-and-mysql.md),
[ADR-0027](0027-sqlite-driver.md)) reads all three but keeps them private.
Its public `Connection::is_in_transaction` only counts the transactions begun
through SQLx's own API, so it misses a `BEGIN` the user typed. The workspace
forbids `unsafe`, so the SQLite handle cannot be read through FFI either.

Until now the drivers made up for it, with gaps:

- PostgreSQL ran a probe query after each run, skipped right after `BEGIN`
  so it would not take the transaction's snapshot. The rule read the
  statement's first word, so `COMMIT AND CHAIN` made the probe (and the
  context query) take the new transaction's snapshot; a later
  `SET TRANSACTION ISOLATION LEVEL` then failed. This was reproduced on
  PostgreSQL 18.
- MySQL reported only `autocommit = 0`. A transaction begun inside a
  procedure, or rolled back by the server after a deadlock, was invisible.
- SQLite reported nothing. A transaction rolled back by SQLite itself (an
  interrupted write, `INSERT OR ROLLBACK`) still showed as open.

## Decision

- Every SQLx crate (`sqlx`, `sqlx-core`, `sqlx-macros`, `sqlx-macros-core`,
  `sqlx-mysql`, `sqlx-postgres`, `sqlx-sqlite`) is replaced through
  `[patch.crates-io]` in the root `Cargo.toml` by
  <https://github.com/alandariva/sqlx> at revision
  `e538af09c6df345aedd232d132b99f5b6491d486`: tag `v0.9.0` plus one commit
  on branch `jykre/transaction-status`. Patching all of them keeps a single
  `sqlx-core` in the tree. `deny.toml` allows that repository in
  `[sources] allow-git`, with a review date.
- That commit adds read-only API and changes nothing else:
  - `PgConnection::transaction_status()` returns the status of the last
    `ReadyForQuery`, after reading responses still pending; the status type
    becomes public as `PgTransactionStatus`.
  - `MySqlConnection::server_in_transaction()` and `server_autocommit()`
    read the last status flags. `ping()` now refreshes them, and a new
    connection takes them from the handshake.
  - `LockedSqliteHandle::is_autocommit()` wraps `sqlite3_get_autocommit`.
- The drivers answer `Session::transaction` from that state. The contract
  does not change.
  - PostgreSQL sends no query to tell idle, open or failed. Whether an open
    transaction wrote is read with `pg_current_xact_id_if_assigned()` only
    once the transaction has a snapshot; before that it wrote nothing. The
    snapshot rule also treats `… AND CHAIN` as opening a transaction and
    `ROLLBACK TO` as taking no snapshot.
  - MySQL sends nothing. Autocommit off is reported as `Manual`, an open
    transaction as open with unknown changes. Every failed or cut statement
    already ends with a ping (`resynchronize`), so the flags are current
    after an error.
  - SQLite reads the handle; changes are unknown.
  - What a driver leaves unknown (whether a MySQL or SQLite transaction
    wrote) and drivers that report nothing (Databricks) keep the inference
    of `jykre::transaction`, which also learns `AND CHAIN`.
- The patch is temporary. The same API is proposed upstream, rebased on
  its `main`, in
  [transact-rs/sqlx#4512](https://github.com/transact-rs/sqlx/pull/4512)
  (opened 2026-11-07). When a SQLx release has these
  readers, under these names or others, the drivers move to it and the
  patch and the `allow-git` entry are removed.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Query probes: MySQL `SAVEPOINT`/`RELEASE SAVEPOINT`, PostgreSQL `SAVEPOINT` | No dependency change. Tested on the dev databases: no snapshot or read view, no privilege needed | MySQL writes `SAVEPOINT` to the binlog of a transaction that wrote and counts error 1305 on every idle run. PostgreSQL logs an `ERROR` with the statement and counts a rollback on every idle run | They leave traces on the user's servers. The maintainer chose the patch on 2026-11-07 |
| Keep the current probes and inference | Nothing to maintain | The gaps in *Context* remain; PostgreSQL keeps a round trip per run | Does not meet M1 |
| SQLite: probe with `BEGIN` (it fails inside a transaction) | No dependency change. Validated: no snapshot, works read-only | Two statements per run; depends on the error's text | The patch covers SQLite as well |
| SQLite: FFI through `LockedSqliteHandle::as_raw_handle` | Already public in SQLx | Needs `unsafe`, forbidden by the workspace lints; `libsqlite3-sys` is semver-exempt in SQLx | [ADR-0027](0027-sqlite-driver.md) already rejected it |
| Wait for an upstream release | No fork | No upstream pull request existed, and the timing is unknown | M1 cannot wait for it |
| Our own wire protocol | Full control | A database client to write and maintain | Out of proportion |

## Consequences

- Positive:
  - The state shown matches the session's in PostgreSQL, MySQL and SQLite,
    including transactions opened or ended by procedures, by the server
    (a deadlock) or by SQLite itself.
  - MySQL and SQLite answer without running anything; PostgreSQL runs a
    query only to tell whether an open transaction with a snapshot wrote.
- Negative or costs:
  - A fork to maintain, in a personal GitHub account. Upgrading SQLx means
    applying the commit to the new tag, publishing a new revision and
    running the full gate.
  - A git source in the tree: builds fetch it from GitHub, and `cargo deny`
    needs the `allow-git` entry.
  - Whether a MySQL or SQLite transaction wrote is still inferred from the
    statements.
- What becomes required or forbidden:
  - All SQLx crates point to the same revision. A change to the fork is a
    new revision, never a moving branch.
  - The fork carries only this API. Anything else goes upstream or gets its
    own decision.
  - The patch and the `allow-git` entry go away once a SQLx release has the
    readers.

## How to validate

- In the fork, `tests/postgres`, `tests/mysql` and `tests/sqlite` cover
  `BEGIN` sent as a query, a partly read result, a failed statement,
  `COMMIT AND CHAIN`, a transaction begun in a procedure, `autocommit`, the
  status before the first query, a deadlock and `INSERT OR ROLLBACK`. Each
  test was checked to fail without its fix (PostgreSQL 18, MySQL 8.4).
- In Jykre, the live tests
  `live_the_transaction_state_is_reported_without_taking_a_snapshot` (left
  as it was) and `live_commit_and_chain_opens_a_transaction_without_taking_its_snapshot`
  (PostgreSQL), `live_the_transaction_state_is_read_from_the_status_flags`
  and `live_a_deadlock_victim_reports_its_transaction_rolled_back` (MySQL),
  and the SQLite file test `the_transaction_state_is_read_from_the_connection`
  (an interrupted write and `OR ROLLBACK`), all run by
  `cargo xtask check --full`.
