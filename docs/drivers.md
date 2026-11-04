# Database drivers

The three adapters have `Ready` status and provide connection testing, SQL
sessions and metadata. PostgreSQL and MySQL use SQLx 0.9 with rustls TLS;
Databricks uses the authenticated CLI and HTTPS with redirects disabled. The UI
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
| `jykre-storage` | SQLite repositories for configuration, preferences and per-connection metadata |
| `jykre-secrets` | Native vault; no fallback to a password file |
| `jykre` | GPUI components and composition of the three drivers in `drivers::builtin_registry()` |

Each adapter depends on core; core depends neither on the adapters nor on
GPUI. The application knows the adapters only to build the registry. `backend`
runs operations on the Tokio runtime without blocking the UI. The simulated
demo stays separate and is enabled with `JYKRE_DEMO=1`.

`Driver::discover_namespaces` lists the namespaces directly below a parent
without reading relations or columns, so the user can choose what to
introspect ([ADR-0015](adr/0015-introspect-only-what-the-user-chooses.md)).
PostgreSQL lists the configured database at the root and its schemas below
it (without `pg_catalog`, `information_schema`, TOAST and temporary schemas);
MySQL lists the visible databases, without the system ones; Databricks lists
Unity Catalog catalogs (without `hive_metastore`) and the schemas of a
catalog, calling only the listing endpoints. Below the last namespace level
the list is empty. `DriverCapabilities::namespace_levels` names those levels
from the hierarchy; the application derives everything from it.

`Driver::introspect_scoped` takes a structural `ObjectPath`. An empty scope
reads the whole connection; database/catalog, schema, table/view and column
read only the requested subset. PostgreSQL reads `pg_catalog` and MySQL
`information_schema`, with parameterized filters; Databricks uses the
individual Unity Catalog endpoints when an object is selected.

What each driver reads ([ADR-0016](adr/0016-keys-defaults-and-relation-kinds.md)):

| | PostgreSQL | MySQL | Databricks |
| --- | --- | --- | --- |
| Relation kinds | Tables (ordinary, partitioned, foreign), views, materialized views | Tables, views | Tables, views, materialized views (`table_type`) |
| Column type | `format_type` (`character varying(80)`, `integer[]`, enum names) | `column_type` (`varchar(80)`, `enum('a','b')`) | `type_text` |
| Default | `pg_get_expr` (the expression) | `column_default` (the value as MySQL reports it) | Not read |
| Comments | Relations and columns | Relations and columns (empty means none; a view's `VIEW` is not a comment) | Relations and columns |
| Primary key | Position of each column | Position of each column | `table_constraints` [NV with a real workspace] |
| Foreign keys | A node per key, with its columns and the referenced table | Same | `table_constraints` [NV with a real workspace] |
| Partitions | Listed under their root table (same schema), without columns; a partition in another schema is a table | Not listed | Not applicable |

PostgreSQL shows a relation under the same rule as `information_schema`: the
user owns it or has a privilege on it. Keys and partitions are only listed
under a relation the user can see.

`Driver::default_namespaces` suggests the first choice of what to
introspect: `public` of the configured PostgreSQL database, the configured
MySQL database, or the Databricks profile's default catalog and schema when
both are set. The dialog checks them, marked **Default**, when the list
shows them and nothing was chosen yet; nothing is read until the user saves. The application publishes the read after
success, replacing only that scope and keeping subtrees that have permission
warnings.

`DriverRegistry::register` takes an `Arc<dyn Driver>` and rejects duplicates
without replacing the original driver. `get` returns a shared reference or
`DriverNotRegistered`. `descriptors` lists PostgreSQL, MySQL and Databricks in
that order, regardless of registration order. Consumers must check
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
`USE` commands; user statements are sent unchanged. Metadata uses Unity
Catalog REST without opening a SQL session or starting compute. Jobs clusters,
non-warehouse serverless compute and OAuth implemented inside the app remain
out of scope. A stopped classic cluster reports an actionable error.

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
  statement (warehouses) or switches the cluster's SQL context.
- `Session::context` reports where unqualified names resolve now:
  `current_database()` and `current_schema()` in PostgreSQL, `DATABASE()` in
  MySQL (as the schema), the followed catalog and schema in Databricks.
  Asking discards a result still open for paging, so the application asks
  after a run whose last result is complete, or after its last page. The
  default implementation reports an empty context. In PostgreSQL, right
  after `BEGIN` or `START TRANSACTION` (and until a statement other than
  `SET`, `SHOW`, `SAVEPOINT` or `RELEASE` runs) it answers from what the
  session knows instead of querying, so it takes no snapshot.
- `Session::transaction` reports the transaction state as the server sees
  it (`TransactionStatus`: idle, open with or without changes, failed, or
  MySQL's manual commit), or `None` when the driver cannot tell; the
  application then infers it. It is asked when the context is. Asking must
  not change what the session's statements see:
  - PostgreSQL probes with the simple query protocol
    (`transaction_timestamp() <> statement_timestamp()`,
    `pg_current_xact_id_if_assigned()`); a failed transaction answers with
    SQLSTATE 25P02. Right after `BEGIN` it answers "open, nothing written"
    without querying, under the same rule as the context, so `SET
    TRANSACTION` and the isolation level's snapshot keep working.
  - MySQL reports only `autocommit = 0` (`@@autocommit`): whether a `BEGIN`
    is open needs privileges a user may not have.
  - Databricks reports nothing.

  The protocol carries the exact state, but SQLx keeps it private; see
  [technical debt TD-001](technical-debt.md#td-001--transaction-state-is-not-read-from-the-protocol).
- `Session::close` consumes the session and can report close failures.
  Relational sessions keep physical state (`USE`, `SET`, temporary tables,
  transactions). Databricks warehouses keep a logical catalog/schema context sent with
  each request, which follows successful `USE CATALOG`, `SET CATALOG` and
  `USE [SCHEMA | DATABASE]` statements, without promising transactions or
  other SQL state between statements. Classic clusters keep their actual SQL
  execution context, including successful context changes, between runs.
- [ADR-0017](adr/0017-driver-metadata-contract-batch.md) adds, all with
  defaults that keep the previous behaviour:
  - `cache_identity`: the options that decide which catalog a cache
    describes. PostgreSQL: host, port, database and user; MySQL: host,
    port and user; Databricks: the CLI profile.
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
  through a connection of its own; sessions stay in `database`). A partition
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

SQL generated by the application (previews, session commands) asks the driver
how to write names:

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
cargo test -p jykre-core -p jykre-driver-postgres -p jykre-driver-mysql -p jykre-driver-databricks --locked
```

They cover registration and duplicates, configuration, errors before I/O
starts, prior cancellation, Databricks metadata without a warehouse,
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

A database error keeps the relational session and its state, including an
open transaction (which PostgreSQL then requires to be rolled back). After a
cancellation, a timeout, an interruption by a limit or the sink, or a value
that cannot be decoded, the driver makes one attempt to cancel the statement
on the server and then reads up to the end of the response, each limited to 5
seconds; if that fails the session is closed, so an incomplete protocol is
never reused. MySQL first reads and drops the rest of a result interrupted by
a limit, and kills the statement only when that takes too long, because the
statement may already be done. A lost connection closes the session.
PostgreSQL inspects the configured database; MySQL inspects the visible
databases, excluding internal schemas. Both read only the namespaces the user
chose, and respect what the user is allowed to see.

Databricks reads `DATABRICKS_CONFIG_FILE` or `.databrickscfg` in the home
directory. Authenticate first with `databricks auth login --profile DEV`. The
connection test checks Unity Catalog and configured compute access (classic
clusters must be running), without starting compute or executing SQL. Warehouse SQL uses
INLINE JSON_ARRAY and internal chunks from the same origin, read page by page.
The submit request waits up to 5 seconds for the statement, then status is
polled from 200 ms up to every 2 seconds. Inline results are capped at 25 MiB,
so a statement asks the server for at most five pages of rows; a longer result
reports that the rest was not read. The CLI token is reused for 5 minutes and
shared between sessions, discovery, testing and metadata, and refreshed before
its reported expiration (30-second margin). Concurrent refreshes coalesce; a
401 forces one refresh and retry. External
links are not followed. The initial submission waits for the response with the
statement ID, up to the CLI/HTTP limits, so it can be cancelled later, even if
cancelled during that interval. Cancellation or timeout after submission
requests remote cancellation. Metadata ignores `hive_metastore`.

Classic execution submits a SQL command and polls its status. Cancellation or
timeout cancels it and confirms a terminal status within five seconds; failed
recovery destroys/discards the context. Classic results have the API's own
server cap: returned rows are paged locally and `truncated` is propagated.
Numbers and structured cells are preserved from raw JSON before decoding,
avoiding decimal rounding. HTTP response bodies are bounded to 26 MiB,
including the JSON envelope, for both modes.

Real tests with dedicated disposable fixtures:

```bash
cargo xtask check --full
```

The gate starts PostgreSQL 14/18 and MySQL 8.4/9.7 and removes the services at
the end. They use only localhost and database/user `jykre`, with the public
fixture password `jykre-test`. Ports and policy: [testing](testing.md). The
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
See the [2026-11-03 validation report](databricks-validation-2026-11-03.md) for
completed checks, measurements and remaining visual/authenticated checks.
