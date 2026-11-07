# ADR-0027: SQLite database files through SQLx's bundled SQLite

- **Status:** Proposed
- **Date:** 2026-11-06
- **Authors:** Alan de Oliveira (decisions), drafted with an AI agent

## Context

Users want to open SQLite database files next to their PostgreSQL, MySQL and
Databricks connections. SQLite is a library, not a server: there is no host,
user or password, no `USE`, and a value's type belongs to the value, not to
its column. The application already links SQLite through SQLx 0.9
(`jykre-storage`, [ADR-0003](0003-sqlite-for-local-storage.md)), and a second
SQLite binding (`rusqlite`) would link a second `libsqlite3-sys`, which Cargo
refuses for a `links` crate.

Decisions made by the maintainer on 2026-11-06:

1. The driver uses SQLx's `sqlite` feature, with no new crate in
   `Cargo.lock`.
2. A connection opens an existing file read-write, with a read-only option,
   and never creates a missing file.
3. Schemas are `main` and the attached databases (`temp` left out); `main`
   is the default choice to introspect ([ADR-0015](0015-introspect-only-what-the-user-chooses.md)).
4. The editor's SQL catalog for SQLite (`catalog/sqlite`) is a later task.

## Decision

- A new crate, `jykre-driver-sqlite`, depending only on `jykre-core`, SQLx
  (`sqlite`), `futures-util`, `serde` and `tokio`, registered in
  `drivers::builtin_registry()` as `DriverKind::Sqlite` with
  `SqlDialect::Sqlite`.
- Configuration: `{"path": "<absolute path>", "readOnly": true}` (`readOnly`
  left out when off). Relative paths, `:memory:` and `file:` URIs are
  rejected: an in-memory database differs per connection, so metadata would
  describe another database than the tab's.
- Every connection opens with `create_if_missing(false)`, a 4 s busy timeout,
  SQLx's defaults otherwise (`foreign_keys = ON`), and reads the schema table
  at once, so a file that is not a database fails at the test.
- A session keeps one connection in a task of its own, as the MySQL and
  PostgreSQL drivers do, so a result stays open between pages. Cancellation
  and timeouts interrupt the running statement through SQLite's progress
  handler (`LockedSqliteHandle::set_progress_handler`, a safe SQLx API), and
  the session waits until the connection's thread is idle before the next
  statement. No `unsafe` code is added.
- User SQL is sent as written (`RawSql`). `:name` parameters become SQLite's
  numbered `?N`, bound in order; numbers are bound as text and read with
  `CAST(?N AS NUMERIC)` ([ADR-0023](0023-named-parameters-bound-by-drivers.md),
  [ADR-0026](0026-named-parameters-in-sql-tabs.md)).
- Cells follow each value's storage class: `INTEGER` → `Cell::Integer`,
  `REAL` → `Cell::Float`, `TEXT` → `Cell::Text` (bytes that are not UTF-8 →
  `Cell::Binary`), `BLOB` → `Cell::Binary`, `NULL` → `Cell::Null`.
- Metadata comes from the pragma table functions (`pragma_database_list`,
  `pragma_table_list`, `pragma_table_xinfo`, `pragma_foreign_key_list`,
  `pragma_index_list`) and the schema table; nothing needs a session.
- The tokenizer gets SQLite's rules: `"x"`, `` `x` `` and `[x]` names, no
  backslash escapes, comments that do not nest, `?`, `?NNN`, `:name`,
  `@name` and `$name` parameters, trigger bodies kept whole. **Explain** asks
  for `EXPLAIN QUERY PLAN`.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| `rusqlite` | Direct C API: `sqlite3_interrupt`, autocommit state, error offsets | A second `libsqlite3-sys` cannot be linked beside SQLx's; a new dependency | Conflicts with `jykre-storage` |
| Raw FFI to SQLite through SQLx's handle | The transaction state and the error position | `unsafe` code, forbidden by the workspace lints; `libsqlite3-sys` is semver-exempt in SQLx | Not without a lint exception |
| Create the file when missing | One step less for a new database | A mistyped path silently opens an empty database | Rejected by the maintainer |

## Consequences

- Positive: SQLite with no new external crate and no `unsafe`; its tests run
  against real files in the basic gate, without Docker.
- Negative or costs:
  - `Session::transaction` reports nothing (SQLx keeps
    `sqlite3_get_autocommit` private); the application infers the state from
    the statements, as for MySQL's `BEGIN` ([TD-001](../plans/technical-debt.md#td-001--transaction-state-is-not-read-from-the-protocol)).
    *Amended by [ADR-0030](0030-transaction-state-through-a-pinned-sqlx-fork.md)
    on 2026-11-07:* the driver now reads `sqlite3_get_autocommit` through
    `LockedSqliteHandle::is_autocommit()`, a safe reader the pinned SQLx fork
    adds, so a transaction SQLite ends on its own shows as ended; only
    whether it wrote is inferred.
  - Errors carry no position (SQLx does not expose `sqlite3_error_offset`).
  - Interrupting a write inside an explicit transaction makes SQLite roll
    the whole transaction back.
  - A page kept open holds the statement, and with it a read lock on the
    file, until the result ends or the next statement runs.
  - Foreign keys have no names in SQLite's pragmas, so the driver names them
    by what they link (`(customer_id) → customers`).
  - Attached databases are per connection, so a metadata connection only
    sees `main`: an `ATTACH` run in a tab is not in the explorer.
- What becomes required or forbidden: SQLite is reached only through this
  driver and SQLx; another SQLite binding stays forbidden while SQLx links
  `libsqlite3-sys`.

## How to validate

`cargo test -p jykre-driver-sqlite` runs the driver against synthetic files:
opening only existing files, storage classes, rows affected, paging,
cancellation and timeouts that keep the session, read-only refusal, bound
parameters, metadata, inspection and definitions.
