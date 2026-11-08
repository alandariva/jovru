# Database drivers

The four adapters have `Ready` status and provide connection testing, SQL
sessions and metadata. PostgreSQL and MySQL use SQLx 0.9 with rustls TLS;
Databricks uses the authenticated CLI and HTTPS with redirects disabled; SQLite
opens a database file through SQLx's bundled SQLite
([ADR-0027](../adr/0027-sqlite-driver.md)). The UI
uses this backend and persists non-sensitive configuration and caches in
SQLite. Passwords stay in the session or, if the user chooses, in the system
vault.

## Layout

| Crate | Responsibility |
| --- | --- |
| `jykre-core` | Shared configuration, credentials, errors, async contracts, results, metadata and registry |
| `jykre-driver-postgres` | PostgreSQL configuration and adapter |
| `jykre-driver-mysql` | MySQL configuration and adapter |
| `jykre-driver-databricks` | CLI profile configuration and Databricks adapter |
| `jykre-driver-sqlite` | Database file configuration and SQLite adapter |
| `jykre-storage` | SQLite repositories for configuration, preferences and per-connection metadata |
| `jykre-secrets` | Native vault; no fallback to a password file |
| `jykre` | GPUI components and composition of the four drivers in `drivers::builtin_registry()` |

Each adapter depends on core; core depends neither on the adapters nor on
GPUI. The application knows the adapters only to build the registry. `backend`
runs operations on the Tokio runtime without blocking the UI. The simulated
demo stays separate and is enabled with `JYKRE_DEMO=1`.

`Driver::discover_namespaces` lists the namespaces directly below a parent
without reading relations or columns, so the user can choose what to
introspect ([ADR-0015](../adr/0015-introspect-only-what-the-user-chooses.md)).
PostgreSQL lists the configured database at the root and its schemas below
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

What each driver reads ([ADR-0016](../adr/0016-keys-defaults-and-relation-kinds.md)):

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
use jykre::drivers::{DriverKind, DriverStatus, builtin_registry};

let registry = builtin_registry()?;
let driver = registry.get(DriverKind::Postgres)?;
assert_eq!(driver.descriptor().status, DriverStatus::Ready);
```

A runnable demo lives in `examples/drivers.rs`:

```bash
cargo run --example drivers --locked
```

## Configuration and credentials

`ConnectionConfig` holds `driver` and `options`, a JSON value whose structure
belongs to the adapter. Each driver also exposes a typed Rust configuration in
its `config` module. `parse` deserializes and validates; `validate` checks
structures built directly. Unknown fields are rejected, including password and
token fields. Validate with the driver before storing or reusing the JSON.

PostgreSQL requires database and user; only the port has a default (`5432`):

```json
{
  "driver": "postgres",
  "options": {
    "host": "localhost",
    "database": "commerce",
    "user": "postgres",
    "sslMode": "require"
  }
}
```

MySQL allows omitting the database; the default port is `3306`:

```json
{
  "driver": "mysql",
  "options": {
    "host": "localhost",
    "user": "app",
    "sslMode": "required"
  }
}
```

Host accepts a hostname or an IP address, including IPv6, and rejects
connection URLs. Explicit ports must be between 1 and 65535. An optional
database must be omitted or `null` when not selected; an empty string is
invalid.

TLS is always explicit. PostgreSQL accepts `disable`, `prefer`, `require`,
`verify-ca` and `verify-full`. MySQL accepts `disabled`, `preferred`,
`required`, `verify-ca` and `verify-identity`. The modes are applied to the
connections. MySQL authentication methods that require RSA must use TLS.

`Credentials.password` uses `SecretString`, with a redacted `Debug`.
Credentials and `ResolvedConnection` do not implement serialization; only the
non-sensitive configuration can cross the persistence boundary. Passwords stay
out of `options`. Validation errors do not echo the received values.

### SQLite

A connection names an existing database file by its full path; `readOnly`
is optional and left out when off:

```json
{
  "driver": "sqlite",
  "options": {
    "path": "C:\\data\\shop.sqlite",
    "readOnly": true
  }
}
```

The path must be absolute and is never created: a missing file fails with
`NotFound` instead of opening an empty database. `:memory:`, `file:` URIs and
connection URLs are rejected. Every connection reads the schema table when it
opens, so a file that is not a database fails at the test. A read-only
connection refuses statements that write (`PermissionDenied`). SQLite needs
no password: the dialog shows none, the vault is never written and opening a
session never asks for one (`connection_store::signs_in_with_password`).
Connections keep SQLx's defaults, including `PRAGMA foreign_keys = ON`, and
wait up to 4 seconds for a lock another connection holds.

The dialog's **Browse…** opens the system's file dialog to fill the path.
The explorer shows the path beside the connection's name.

### Databricks

A metadata configuration needs only the authenticated profile name:

```json
{
  "driver": "databricks",
  "options": {
    "profile": "DEV"
  }
}
```

Optional fields: `cliPath`, `compute`, `warehouseId`, `clusterId`, `computeName`,
`defaultCatalog` and `defaultSchema`. Optional text fields cannot be empty.
`compute` defaults to `warehouse` for existing profiles. SQL requires the ID
matching the selected mode; IDs for both modes together are rejected.

```json
{"profile":"DEV","compute":"cluster","clusterId":"1234-example",
 "computeName":"Analytics","defaultCatalog":"main","defaultSchema":"sales"}
```

The dialog uses an explicit **Load compute** action and a searchable select for
warehouses or classic all-purpose clusters (name, stable ID and server state).
It preserves the saved selection even offline or when the resource is missing
from the refreshed list. Changing profile/CLI identity or compute type clears
that selection. Editing preserves CLI path, catalog and schema. The explorer
shows the selected resource name/ID alongside the profile.

The profile field is a searchable select populated by **Load CLI profiles**,
which runs `databricks auth profiles --skip-validate --output json` with the
configured CLI executable and `DATABRICKS_CONFIG_FILE`. Listing is cancellable,
limited to 60 seconds/1 MiB and exposes only workspace profile names and safe
HTTPS hosts. Account-console hosts are excluded; workspace profiles carrying
`account_id` and `workspace_id` are kept. The `valid` field from a skip-validate
listing is not used to filter profiles. The saved name remains selected
when unavailable/offline; choosing another profile clears the compute. Profile
listing skips authentication; tests and SQL resolve the selected profile token.
New forms initially show a selection prompt. After loading, they prefer the
CLI's `default: true`, then a literal `DEFAULT` profile or the only available
profile. With multiple profiles and no default, the user chooses explicitly.
Refreshing an edited/saved connection never replaces its profile automatically.

Connection setup obtains the selected profile's host from the same CLI metadata
used by discovery, against the same canonical configuration file. Jykre does
not parse INI syntax itself, so BOMs and section comments supported by the CLI
work consistently. A missing selected workspace profile reports `NotFound`; a
profile without a safe HTTPS workspace URL reports `Validation`. Another
profile is never substituted. Host lookup skips authentication and has a
60-second timeout and 1 MiB output bound.
The token comes through the CLI and is kept only in memory. The driver does not use
`Credentials.password`. SQL execution uses Statement Execution on a SQL
Warehouse, or Command Execution API 1.2 on a running classic all-purpose
cluster. A classic session creates and reuses a SQL context per tab, confirms
readiness and destroys it on close. Context defaults are applied using quoted
`USE` commands; user statements are sent unchanged, except that the
`:name` markers of a statement with parameters become typed literals, since
the command API cannot bind values: text as `decode(X'<hex>', 'UTF-8')`,
numbers as `CAST('…' AS DECIMAL(p,s))` (or `DOUBLE` with an exponent),
`TRUE`/`FALSE`, `DATE'…'`, `TIMESTAMP'…'` and a typed `CAST(NULL AS …)`
([ADR-0026](../adr/0026-named-parameters-in-sql-tabs.md)). Warehouses send the
statement unchanged with the Statement API's typed `parameters`. Metadata uses Unity
Catalog REST without opening a SQL session or starting compute. Jobs clusters,
non-warehouse serverless compute and OAuth implemented inside the app remain
out of scope. A stopped classic cluster reports an actionable error.

A request the REST API rejects (any endpoint: statements, metadata,
compute, contexts) keeps Databricks' whole `message`, verbatim, as the
error's message, and `HTTP <status> · <error_code>` as the server's code,
with the SQLSTATE when the message carries one. The code still follows the
status: 401 `Authentication`, 403 `PermissionDenied`, 404 `NotFound`, 429
`Connection`, anything else `Database`. Only the first 64 KiB of the body is
read; a body that is not a Databricks error (a proxy's page, nothing at all)
is never shown, and the error says only that the request was rejected, with
its status. A rejected request is not repeated, except a 401 once with a
fresh token.

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
- `Driver::session_connection` ([ADR-0025](../adr/0025-report-panels.md))
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
  from what the session knows instead of querying, so it takes no snapshot.
- `Session::transaction` reports the transaction state as the server sees
  it (`TransactionStatus`: idle, open with or without changes, failed, or
  MySQL's manual commit), or `None` when the driver cannot tell; the
  application then infers it. It is asked when the context is. Asking must
  not change what the session's statements see. The PostgreSQL, MySQL and
  SQLite drivers read the state the database keeps, through public readers
  that a pinned SQLx fork adds
  ([ADR-0030](../adr/0030-transaction-state-through-a-pinned-sqlx-fork.md)):
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

  Whether a MySQL or SQLite transaction wrote is still inferred from the
  statements; see
  [technical debt TD-001](../plans/technical-debt.md#td-001--transaction-state-is-not-read-from-the-protocol).
- `Session::encryption` reports whether the session's traffic is encrypted
  (`Encryption`: TLS with its version, plain text, local, or unknown), as
  the connection negotiated it, not as the profile asked
  ([ADR-0032](../adr/0032-session-encryption-reported-by-drivers.md)). The
  application asks once, right after a run opened the session and before
  its first statement. PostgreSQL reads `pg_stat_ssl` for its backend (and
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
- [ADR-0017](../adr/0017-driver-metadata-contract-batch.md) adds, all with
  defaults that keep the previous behaviour:
  - `cache_identity`: the options that decide which catalog a cache
    describes. PostgreSQL: host, port, database and user; MySQL: host,
    port and user; Databricks: the CLI profile; SQLite: the file's path.
  - `introspect_level` with `IntrospectionLevel::Names` (namespaces and
    relations only) or `Full`. A namespace read for the first time is
    published at `Names` and then `Full`.
  - `inspect`: what a relation (indexes, constraints, triggers; for
    Databricks keys and column defaults) or a last-level namespace
    (functions, procedures with their parameters, sequences, custom types
    and enums) has beyond the refreshed tree, read only when the user asks,
    within 60 seconds, never cached.
  - `definition`: a view's source, or a table's DDL (`SHOW CREATE` in MySQL,
    built from the catalog in PostgreSQL). Databricks gives table DDL only
    through SQL compute, which metadata never starts.
  - `left_out_of_everything`: Databricks leaves the `system` catalog out of
    **Everything** until it is chosen by name.
- A chosen namespace the user may not read is reported as an
  `IntrospectionWarning` on its path instead of being omitted: PostgreSQL
  checks `USAGE` on the schema; MySQL tries `USE` (for a login without
  global privileges MySQL answers "access denied" for a missing database
  too, so both read as no access); Databricks reports permission errors
  per catalog or schema. The cache keeps that subtree's last good read.
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
table. Turning paths into SQL needs
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

1. Create a crate in `crates/driver-<database>` that depends on `jykre-core`.
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
   `jykre::drivers::builtin_registry()`. Extend the composition tests.

## Validation

The backend tests do not compile GPUI:

```bash
cargo test -p jykre-core -p jykre-driver-postgres -p jykre-driver-mysql -p jykre-driver-databricks -p jykre-driver-sqlite --locked
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

## Real use and limits

`examples/connect.rs` takes `JYKRE_CONNECTION` (non-sensitive JSON) and
`JYKRE_PASSWORD` separately. `JYKRE_SQL` runs one statement;
`JYKRE_METADATA=1` reads metadata. Run it with
`cargo run --example connect --locked`. The backend needs a Tokio runtime;
network calls must not block GPUI.

Relational connections have a 15-second timeout; introspection, 300 seconds.
Each run accepts one SQL statement. Columns are delivered even for a SELECT
without rows. Text protocol values are `Cell::Text`, keeping decimals without
rounding; NULL is `Cell::Null` and MySQL binaries are `Cell::Binary`.

MySQL takes a statement's columns from preparing it, and sends the
statement itself through the text protocol. When preparing describes no
columns (a `CALL`, whose procedure's results are unknown until it runs, or
a statement MySQL cannot prepare, error 1295), the columns come with the
first row instead, so a procedure's rows arrive with their names and the
same exact values and NULL as the `SELECT` run directly. A procedure can
return several result sets, but a run delivers one result: the first result
set that has rows is delivered and paged like any other, and the result sets
after it are read to the end of the procedure and dropped, without being
reported. A result set without rows shows no columns, since SQLx gives a
result set's columns only with its rows, so a procedure whose only result is
empty looks like a statement without rows. When rows came back,
`rows_affected` is `None`, as for a `SELECT`; a procedure that returns no
rows reports what its last statement changed.

A database error keeps the relational session and its state, including an
open transaction (which PostgreSQL then requires to be rolled back). After a
cancellation, a timeout, an interruption by a limit or the sink, or a value
that cannot be decoded, the driver makes one attempt to cancel the statement
on the server and then reads up to the end of the response, each limited to 5
seconds; if that fails the session is closed, so an incomplete protocol is
never reused. MySQL first reads and drops the rest of a result interrupted by
a limit, and kills the statement only when that takes too long, because the
statement may already be done. MySQL never abandons a read on a session's
connection halfway: SQLx reads a packet's header and its payload in separate
steps, and a read dropped between them would put the protocol out of step.
When a cancellation, a timeout or the drain's limit arrives during a read,
the statement is killed first and that read is then finished, within the same
5 seconds as the rest of the response. A lost connection closes the session.
PostgreSQL inspects the configured database; MySQL inspects the visible
databases, excluding internal schemas. Both read only the namespaces the user
chose, and respect what the user is allowed to see.

SQLite sends user SQL as written, through `RawSql`. Columns come from
preparing the statement, so a query without rows still has them; their type
is the declared type's affinity (`TEXT`, `INTEGER`, `REAL`, `BLOB`,
`NUMERIC`), or none for an expression. Cells follow each value's storage
class: `INTEGER` is `Cell::Integer`, `REAL` `Cell::Float` (SQLite stores a
double, so nothing is rounded on the way), `TEXT` `Cell::Text`, or
`Cell::Binary` when its bytes are not UTF-8, `BLOB` `Cell::Binary`, `NULL`
`Cell::Null`. Rows affected are counted only for a statement that changed
rows (`total_changes()` before and after), since SQLite's `changes()` keeps
the last write's count. `:name` parameters become `?N`, bound in order;
numbers are bound as their text and read as `CAST(?N AS NUMERIC)`, booleans
as `1`/`0`, dates and timestamps as text. A cancellation or a timeout
interrupts the statement through SQLite's progress handler and waits up to 5
seconds for the connection to be idle; a result cut short is first left to
end on its own, since interrupting a write inside a transaction rolls the
transaction back. If the connection is not idle in time, the session is
closed. A page kept open holds its statement, and a read lock on the file,
until the result ends or another statement runs. Errors keep SQLite's message
and its extended result code as the vendor code; `SQLITE_READONLY`,
`SQLITE_PERM` and `SQLITE_AUTH` are `PermissionDenied`, `SQLITE_CANTOPEN`
`NotFound`. **Explain** runs `EXPLAIN QUERY PLAN`.

Databricks reads `DATABRICKS_CONFIG_FILE` or `.databrickscfg` in the home
directory. Authenticate first with `databricks auth login --profile DEV`. The
connection test checks Unity Catalog and configured compute access (classic
clusters must be running), without starting compute or executing SQL. Warehouse SQL uses
INLINE JSON_ARRAY and internal chunks from the same origin, read page by page.
The submit request waits up to 5 seconds for the statement, then status is
polled every 2 seconds until 10 seconds have passed since submission, and
from then on after a quarter of the time elapsed, at most 10 seconds apart.
Inline results are capped at 25 MiB, so a statement asks the server for at
most five pages of rows; a longer result reports that the rest was not read.
The CLI token is reused for 5 minutes and shared between sessions, discovery,
testing and metadata, and refreshed before its reported expiration (30-second margin). Concurrent refreshes coalesce; a
401 forces one refresh and retry. External
links are not followed. The initial submission waits for the response with the
statement ID, up to the CLI/HTTP limits, so it can be cancelled later, even if
cancelled during that interval. Cancellation or timeout after submission
requests remote cancellation. Metadata ignores `hive_metastore`.

Classic execution submits a SQL command and polls its status on the same
schedule, the first poll 2 seconds after submission (the API has no
server-side wait). Cancellation or timeout cancels it and confirms a terminal
status within five seconds, polling from 200 ms and doubling; failed
recovery destroys/discards the context. Classic results have the API's own
server cap: returned rows are paged locally and `truncated` is propagated.
Numbers and structured cells are preserved from raw JSON before decoding,
avoiding decimal rounding. HTTP response bodies are bounded to 26 MiB,
including the JSON envelope, for both modes.

Real tests with dedicated disposable fixtures:

```bash
cargo xtask check --full
```

The SQLite tests need no fixture service: they create synthetic files in
the system's temporary folder and run in the basic gate.

The gate starts PostgreSQL 14/18 and MySQL 8.4/9.7 and removes the services at
the end. They use only localhost and database/user `jykre`, with the public
fixture password `jykre-test`. Ports and policy: [testing](../standards/testing.md). The
Databricks HTTP tests use a local mock server; the real CLI and workspace must
be validated in an authenticated environment.

The separate native-CLI profile regression uses synthetic configurations only,
without token resolution or network queries:

```bash
cargo test -p jykre-driver-databricks native_cli_profile_syntax -- --ignored
```

It checks connection setup for UTF-8 BOMs and section comments. It is ignored
in the portable gate because the native Databricks CLI is an external prerequisite.

References: [SQLx RawSql](https://docs.rs/sqlx/latest/sqlx/struct.RawSql.html) and
[Databricks Statement Execution](https://docs.databricks.com/api/statement-execution/v1/statement-execution).
Classic execution: [Command Execution](https://docs.databricks.com/api/command-execution/v2/create).
Profile discovery: [Databricks CLI auth profiles implementation](https://github.com/databricks/cli/blob/main/cmd/auth/profiles.go).
