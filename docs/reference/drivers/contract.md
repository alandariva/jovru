# Drivers: layout and contract

Part of [Database drivers](README.md).

## Layout

| Crate | Responsibility |
| --- | --- |
| `jovru-core` | Shared configuration, credentials, errors, async contracts, results, metadata and registry |
| `jovru-driver-postgres` | PostgreSQL configuration and adapter |
| `jovru-driver-mysql` | MySQL configuration and adapter |
| `jovru-driver-databricks` | CLI profile configuration and Databricks adapter |
| `jovru-driver-sqlite` | Database file configuration and SQLite adapter |
| `jovru-storage` | SQLite repositories for configuration, preferences and per-connection metadata |
| `jovru-secrets` | Native vault; no fallback to a password file |
| `jovru` | GPUI components and composition of the four drivers in `drivers::builtin_registry()` |

Each adapter depends on core; core depends neither on the adapters nor on
GPUI. The application knows the adapters only to build the registry. `backend`
runs operations on the Tokio runtime without blocking the UI. The simulated
demo stays separate and is enabled with `JOVRU_DEMO=1`.

`Driver::discover_namespaces` lists the namespaces directly below a parent
without reading relations or columns, so the user can choose what to
introspect (D-0015). PostgreSQL lists the configured database at the root and its schemas below
it (without `pg_catalog`, `information_schema`, TOAST and temporary schemas);
MySQL lists the visible databases, without the system ones; Databricks lists
Unity Catalog catalogs (without `hive_metastore`) and the schemas of a
catalog, calling only the listing endpoints; SQLite lists the schemas of
`pragma_database_list` without `temp` (`main`, and attached databases, which
a fresh metadata connection does not have). Below the last namespace level
the list is empty. `DriverCapabilities::namespace_levels` names those levels
from the hierarchy; the application derives everything from it.

`Driver::introspect_scoped` takes a structural `ObjectPath`. An empty scope
reads the whole connection; database/catalog, schema, table/view and column
read only the requested subset. PostgreSQL reads `pg_catalog` and MySQL
`information_schema`, with parameterized filters; Databricks uses the
individual Unity Catalog endpoints when an object is selected. SQLite reads
the pragma table functions with bound arguments.

A connection introspects nothing until the user chooses, in the connection
dialog, from the namespaces `discover_namespaces` lists one level at a time;
it can be saved without a choice (offline, say) and then stays pending, with
the explorer offering to choose. The choice is a set of rules stored as the
setting `connection.<id>.metadata_scope` ([persistence](../persistence/README.md)):
*everything*, a namespace with all that is created inside it later (a
database with its schemas, a catalog), or a namespace of the last level. A
refresh lists the broader rules, then reads and publishes **each last-level
namespace on its own**, in order; a failure or a cancellation keeps what was
already published and the previous cache of the rest. The UI shows how many
namespaces were read and an estimate of the time left from a moving average of
the time per namespace (not weighted by table counts, which would cost extra
queries in Unity Catalog). After a complete refresh,
`MetadataRepository::retain` removes what the choice no longer covers and the
namespaces a broad rule no longer finds on the server; changing the choice
reads only what it added. A refresh of the whole connection means a refresh of
the choice, and every driver implements `discover_namespaces`.

What each driver reads (D-0016):

| | PostgreSQL | MySQL | Databricks | SQLite |
| --- | --- | --- | --- | --- |
| Relation kinds | Tables (ordinary, partitioned, foreign), views, materialized views | Tables, views | Tables, views, materialized views (`table_type`) | Tables (virtual ones too), views; SQLite's own `sqlite_` tables are left out |
| Column type | `format_type` (`character varying(80)`, `integer[]`, enum names) | `column_type` (`varchar(80)`, `enum('a','b')`) | `type_text` | The declared type, as written (`VARCHAR(80)`); none for a column declared without one |
| Default | `pg_get_expr` (the expression) | `column_default` (the value as MySQL reports it) | Not read | The expression as written (`'anonymous'`) |
| Comments | Relations and columns | Relations and columns (empty means none; a view's `VIEW` is not a comment) | Relations and columns | SQLite has none |
| Primary key | Position of each column | Position of each column | `table_constraints` [NV with a real workspace] | Position of each column |
| Foreign keys | A node per key, with its columns and the referenced table | Same | `table_constraints` [NV with a real workspace] | Same; SQLite keeps no key names, so a key is named `(columns) → table`, and a key that names no columns references the parent's primary key |
| Partitions | Listed under their root table (same schema), without columns; a partition in another schema is a table | Not listed | Not applicable | Not applicable |

PostgreSQL shows a relation under the same rule as `information_schema`: the
user owns it or has a privilege on it. Keys and partitions are only listed
under a relation the user can see.

`Driver::default_namespaces` suggests the first choice of what to
introspect: `public` of the configured PostgreSQL database, the configured
MySQL database, the Databricks profile's default catalog and schema when
both are set, or SQLite's `main`. The dialog checks them, marked **Default**, when the list
shows them and nothing was chosen yet; nothing is read until the user saves. The application publishes the read after
success, replacing only that scope and keeping subtrees that have permission
warnings.

`DriverRegistry::register` takes an `Arc<dyn Driver>` and rejects duplicates
without replacing the original driver. `get` returns a shared reference or
`DriverNotRegistered`. `descriptors` lists PostgreSQL, MySQL, Databricks and
SQLite in that order, regardless of registration order. Consumers must check
`DriverDescriptor.status`: a stub's capabilities describe planned support.

```rust
use jovru::drivers::{DriverKind, DriverStatus, builtin_registry};

let registry = builtin_registry()?;
let driver = registry.get(DriverKind::Postgres)?;
assert_eq!(driver.descriptor().status, DriverStatus::Ready);
```

A runnable demo lives in `examples/drivers.rs`:

```bash
cargo run --example drivers --locked
```

## Contract flow

- `Driver::test` validates and tests a connection; `open_session` creates a
  `Box<dyn Session>`. A configuration with another driver's identity yields
  `Validation`.
- `Session::execute` takes a `QueryRequest`: SQL without rewriting,
  catalog/schema context, row limit and timeout. Defaults are 1,000 rows and
  300 seconds. `timeout: None` disables the timeout; a zero limit, a zero
  timeout and empty SQL are invalid. The driver applies these options.
- `RowSink` receives columns before rows. On `SinkControl::Stop`, the driver
  must stop delivering data. The sink is synchronous and must not do blocking
  I/O. `Cell` distinguishes NULL from text and keeps exact decimals as strings;
  temporal and binary types also have their own representations.
- A result is read page by page. `Session::execute` delivers up to
  `max_rows` rows; when more exist, `DriverOutcome::has_more` is set and the
  result stays open: `Session::fetch` reads the next page, and running,
  pinging or closing anything else discards it. `DriverOutcome::truncated`
  says the driver stopped before the end and the rest cannot be read.
  PostgreSQL and MySQL keep the connection in a task of its own that holds
  the row stream between pages.
- A database error keeps the server's own message and its codes in
  `ServerDiagnostic`, with the error's `position` (one-based, in characters,
  in the statement as sent) when the driver knows it. PostgreSQL sends it.
  MySQL and Databricks write it only into the message, and their drivers
  read it from there (`position.rs` in each crate, unit-tested on the
  servers' message shapes): MySQL from a syntax error's `near '<the rest of
  the statement>' at line <n>`, found in the statement on that line;
  Databricks from `(line <n>, pos <c>)`, `; line <n> pos <c>` (a zero-based
  character) or a query context `== SQL(line <n>, position <c>) ==`
  (one-based), but not from a view's context. Neither sets it when it wrote
  `:name` parameters into the statement's text, since the position would
  then be in a text the user did not write. SQLite gives none.
- A session lives as long as its tab and runs many statements. A failed
  statement does not end it: the driver restores the protocol (cancelling the
  statement on the server when it may still run) or closes the session.
  `Session::is_open` tells whether it can be reused; `Session::ping` checks a
  kept session before a run, within 5 seconds.
- The context in `ExecutionOptions::context` (the tab's catalog and schema)
  is applied only when it differs from the last context the session applied
  or reported, so a `USE` or `SET search_path` the user ran is not undone by
  the next statement. PostgreSQL puts the chosen schema first in
  `search_path` (`"schema", "$user", public`) and refuses another database;
  MySQL runs `USE`; Databricks sends the catalog and schema with each
  statement (warehouses) or switches the cluster's SQL context. SQLite has
  no statement that changes where unqualified names resolve (`temp`, `main`,
  then the attached databases): it accepts `main` and refuses another schema
  with `Validation`, asking to qualify the names instead.
- `Driver::session_connection` (D-0025)
  gives the connection a session opens with so that names resolve in a
  context from its first statement; report panels open their sessions with
  it, one per context. The default returns the connection unchanged, since
  MySQL and Databricks switch within a session. PostgreSQL, whose sessions
  are bound to one database, returns the profile's connection with the
  context's database when the profile offers the server's databases
  (`allDatabases`), with the same user and credentials, and a validation
  error otherwise.
- `Session::context` reports where unqualified names resolve now:
  `current_database()` and `current_schema()` in PostgreSQL, `DATABASE()` in
  MySQL (as the schema), the followed catalog and schema in Databricks,
  always `main` in SQLite.
  Asking discards a result still open for paging, so the application asks
  after a run whose last result is complete, or after its last page. The
  default implementation reports an empty context. In PostgreSQL, right
  after a transaction opens without a snapshot (`BEGIN`, `START
  TRANSACTION`, or ending one `AND CHAIN`), and until a statement other than
  `SET`, `SHOW`, `SAVEPOINT`, `RELEASE` or `ROLLBACK TO` runs, it answers
  from what the session knows instead of querying, so it takes no snapshot
  (`… AND CHAIN` counts as opening a transaction, `ROLLBACK TO` as taking no
  snapshot).
- `Session::transaction` reports the transaction state as the server sees
  it (`TransactionStatus`: idle, open with or without changes, failed, or
  MySQL's manual commit), or `None` when the driver cannot tell; the
  application then infers it. It is asked when the context is. Asking must
  not change what the session's statements see. The PostgreSQL, MySQL and
  SQLite drivers read the state the database keeps, through public readers
  that a pinned SQLx fork adds (D-0030, [SQLx and the pinned
  fork](configuration.md#sqlx-and-the-pinned-fork)):
  - PostgreSQL reads the status of the last `ReadyForQuery` (idle, open,
    failed) with no query. Whether an open transaction wrote is read with
    `pg_current_xact_id_if_assigned()` over the simple query protocol, only
    once the transaction has a snapshot; before that, under the same rule as
    the context, it answers "nothing written", so `SET TRANSACTION` and the
    isolation level's snapshot keep working.
  - MySQL reads the status flags of the last OK or EOF packet with no query:
    autocommit off is `Manual`, otherwise an open transaction (including one
    begun in a procedure) is open with unknown changes. An error packet
    carries no flags, but every failed or cut statement ends with a ping,
    which reads them again, so a deadlock's rollback shows.
  - SQLite reads `sqlite3_get_autocommit` from the connection, so a
    transaction SQLite ended on its own (an interrupted write,
    `OR ROLLBACK`) shows as idle; changes are unknown.
  - Databricks reports nothing.

  What a driver leaves unknown, and Databricks, keep the application's
  inference (`jovru::transaction`, which also learns `AND CHAIN`). Whether a
  MySQL or SQLite transaction wrote is still inferred from the
  statements; see
  [technical debt TD-001](../../plans/technical-debt.md#td-001--transaction-state-is-not-read-from-the-protocol).
- `Session::encryption` reports whether the session's traffic is encrypted
  (`Encryption`: TLS with its version, plain text, local, or unknown), as
  the connection negotiated it, not as the profile asked
  (D-0032): a `prefer` or `preferred` profile falls back to plain text
  silently when the server offers no TLS, and only the open connection knows.
  The application asks once per session, in `backend::run_statements`, right
  after a run opened it (the tab had none, or the kept one was lost) and
  before its first statement, so no result is open for paging and no
  transaction has a snapshot that asking could take. The answer travels in
  `SessionRun::encryption` (`None` when the run kept the tab's session); a
  failure is logged at `debug`, reads as unknown and never stops the run.
  Report, export and metadata sessions do not ask. The query tab shows a lock
  beside **Connected** for TLS (the tooltip names the version) and an amber
  **Not encrypted** warning for plain text; local and unknown show nothing. A
  driver that can tell must answer without changing what the session's
  statements see. PostgreSQL reads `pg_stat_ssl` for its backend (and
  answers unknown, without querying, inside a transaction that has no
  snapshot yet); MySQL reads the `Ssl_version` session status, empty for
  plain text; Databricks answers TLS, since profiles need an `https` URL;
  SQLite answers local. The default answers unknown.
- `Session::close` consumes the session and can report close failures.
  Relational sessions keep physical state (`USE`, `SET`, temporary tables,
  transactions). Databricks warehouses keep a logical catalog/schema context sent with
  each request, which follows successful `USE CATALOG`, `SET CATALOG` and
  `USE [SCHEMA | DATABASE]` statements, without promising transactions or
  other SQL state between statements. Classic clusters keep their actual SQL
  execution context, including successful context changes, between runs.
- D-0017 adds, all with defaults that keep the previous behaviour (a driver
  that overrides none behaves as before):
  - `cache_identity`: the options that decide which catalog a cache
    describes, so changing compute, the default catalog or schema, or the TLS
    mode keeps the cache. The default is the whole options object.
    PostgreSQL: host, port, database and user; MySQL: host,
    port and user (the default database does not change what the server
    lists); Databricks: the CLI profile; SQLite: the file's path. The
    application opens and prepares caches with this identity, and adopts,
    without emptying it, a cache whose stored identity is the full
    configuration of a previous version.
  - `introspect_level` with `IntrospectionLevel::Names` (namespaces and
    relations only: no columns, keys or partitions) or `Full`
    (`introspect_scoped`). The default reads `Full` for both. A namespace the
    cache does not hold yet is published at `Names` and then `Full`, so names
    show before columns; a namespace already cached is read at `Full` only,
    so its columns never vanish in between.
  - `inspect`: what a relation (indexes, constraints, triggers; for
    Databricks keys and column defaults) or a last-level namespace
    (functions, procedures with their parameters, sequences, custom types
    and enums) has beyond the refreshed tree, read only when the user asks
    (**Inspect** in the explorer), within 60 seconds, never cached and never
    part of a refresh. The result is a list of `InspectionItem { kind, name,
    detail }`; `InspectedKind` is `Index`, `PrimaryKey`, `Unique`, `Check`,
    `ForeignKey`, `Exclusion`, `Trigger`, `Function`, `Procedure`,
    `Sequence`, `Type` or `Default` (a column's default the refresh does not
    read), and `detail` holds the columns, the signature with its
    parameters, the enum labels or the definition, as the database writes
    them. The default reports `NotImplemented`. Routines, sequences, triggers
    and types have no `ObjectKind`: they stay out of the explorer tree.
  - `definition`: the DDL or source of a relation or routine, only by
    explicit action: a view's source, or a table's DDL (`SHOW CREATE` in
    MySQL, built from the catalog in PostgreSQL). Databricks gives table DDL
    only through SQL compute, which metadata never starts. The default
    reports `NotImplemented`.
  - `left_out_of_everything`: Databricks leaves the `system` catalog out of
    **Everything** until it is chosen by name. The default leaves out none.
- A chosen namespace the user may not read is reported as an
  `IntrospectionWarning` on its path instead of being omitted: PostgreSQL
  checks `USAGE` on the schema; MySQL tries `USE` (for a login without
  global privileges MySQL answers "access denied" for a missing database
  too, so both read as no access); Databricks reports permission errors
  per catalog or schema. The cache keeps that subtree's last good read, and
  the explorer shows the node as **No access** with the date of that read.
- PostgreSQL options: `searchPath` (the `search_path` sessions start with;
  its first schema is the suggested choice) and `allDatabases` (the root of
  discovery lists every database that accepts connections, each read
  through a connection of its own; query tabs' sessions stay in `database`,
  and a report panel's session opens in its context's database). A partition
  kept in another schema than its root table is listed under the root, with
  `stored_at` naming where it is read.
- Databricks reads one schema with its table listing alone (no GET of its
  catalog and schema first), asks for `omit_columns` at `Names`, and reads
  column defaults from `type_json` (`CURRENT_DEFAULT`). Whether
  `include_browse` should list objects the user can only browse was not
  verified without a real workspace.
- `Driver::discover_namespaces` validates the configuration and the parent,
  honours cancellation and has a 60-second limit. It needs no SQL session and
  never starts compute. The default implementation reports `NotImplemented`.
- `Driver::introspect` takes an async `IntrospectionSink` and delivers batches.
  A sink failure stops the operation. Since batches may arrive before a
  failure, the consumer must publish a snapshot only after successful
  completion. The report allows warnings tied to paths.
- External operations take a `CancellationToken`. Drivers validate the
  configuration before starting I/O and honour prior cancellation, timeouts and
  limits.

`MetadataObject` represents a database, catalog, schema, table, view,
materialized view, column, foreign key or partition, with a 1-based column
position and optional type, nullability, comment, default, position in the
primary key and, for a foreign key, what it references (`ForeignKeyTarget`).
The fields added after the first cache version are optional and left out
when empty, so older caches still read. `ObjectPath::relation_path` gives the
relation a node is read from (a partition is read as a table). Its
`ObjectPath` holds kind/name segments; names with dots, slashes or Unicode
characters are neither split nor normalized. Table, view and materialized view are
alternatives in the descriptor hierarchy, not consecutive levels; keys and
partitions sit beside columns, outside the hierarchy, and refresh with their
table. `ObjectKind` is `#[non_exhaustive]`, so a consumer matching it
handles kinds added later; drivers fill the key, default and reference fields
whenever their database exposes them, and consumers treat keys and
partitions as leaves. Turning paths into SQL needs
the driver's quoting; a path is not a SQL statement.

SQL generated by the application (previews, table Data views, session
commands) asks the driver how to write names:

- `Driver::quote_identifier` quotes one name with the descriptor's
  `identifier_quote`, doubling that character inside the name.
- `Driver::relation_name` joins the quoted structural segments of a path,
  leaving columns out. PostgreSQL also leaves the database out, because a
  session is bound to one database. A driver overrides these defaults when its
  rules differ; callers never decide by `DriverKind`.

## Adding a driver

1. Create a crate in `crates/driver-<database>` that depends on `jovru-core`.
   The `crates/*` pattern adds it to the workspace automatically.
2. Add its identity to `DriverKind` and, if needed, its dialect to
   `SqlDialect`. Drivers are registered statically; there are no dynamic
   plugins.
3. Define a non-sensitive configuration with local validation and implement
   `Driver` with `#[async_trait]`, re-exported by core. Keep the `Stub` status
   while external operations are unavailable.
4. Implement the operations and `Session`, honouring classified errors,
   cancellation, limits, timeouts, sink interruption and closing. Add protocol
   dependencies only to the adapter crate.
5. Add the dependency to the application and register the adapter in
   `jovru::drivers::builtin_registry()`. Extend the composition tests.

## Validation

The backend tests do not compile GPUI:

```bash
cargo test -p jovru-core -p jovru-driver-postgres -p jovru-driver-mysql -p jovru-driver-databricks -p jovru-driver-sqlite --locked
```

They cover registration and duplicates, configuration, errors before I/O
starts, prior cancellation, Databricks metadata without a warehouse, the
SQLite driver against real files it creates in a temporary folder,
credential protection, metadata paths and contract consumption by test
implementations. The negative compilation doctests check that credentials and
resolved connections cannot be serialized. The test implementations only
demonstrate the contract; they are not drivers available in the application.

Full validation, including the other components:

```bash
cargo fmt --all --check
cargo check --workspace --all-targets --locked
cargo test --workspace --locked
```
