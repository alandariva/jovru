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

`Driver::introspect_scoped` takes a structural `ObjectPath`. An empty scope
reads the whole connection; database/catalog, schema, table/view and column
read only the requested subset. PostgreSQL/MySQL parameterize the
`information_schema` filters; Databricks uses the individual Unity Catalog
endpoints when an object is selected. The application publishes the read after
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

Optional fields: `cliPath`, `warehouseId`, `defaultCatalog` and
`defaultSchema`. Optional text fields, when present, cannot be empty. SQL
requires `warehouseId`; connection testing and introspection do not.

The profile provides the host through the configuration file and the token
through the CLI; the token is kept only in memory. The driver does not use
`Credentials.password`. SQL execution uses Statement Execution on a SQL
Warehouse; metadata uses Unity Catalog REST, without opening a SQL session or
starting compute. Classic compute, Serverless via Python and OAuth implemented
inside the application are out of scope for now.

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
- `Session::close` consumes the session and can report close failures.
  Relational sessions keep physical state (`USE`, `SET`, temporary tables,
  transactions). Databricks keeps a logical catalog/schema context sent with
  each request, which follows successful `USE CATALOG`, `SET CATALOG` and
  `USE [SCHEMA | DATABASE]` statements, without promising transactions or
  other SQL state between statements.
- `Driver::introspect` takes an async `IntrospectionSink` and delivers batches.
  A sink failure stops the operation. Since batches may arrive before a
  failure, the consumer must publish a snapshot only after successful
  completion. The report allows warnings tied to paths.
- External operations take a `CancellationToken`. Drivers validate the
  configuration before starting I/O and honour prior cancellation, timeouts and
  limits.

`MetadataObject` represents a database, catalog, schema, table, view or column,
with a 1-based column position and optional type, nullability and comment. Its
`ObjectPath` holds kind/name segments; names with dots, slashes or Unicode
characters are neither split nor normalized. Table and view are alternatives in
the descriptor hierarchy, not consecutive levels. Turning paths into SQL needs
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
databases, excluding internal schemas. `information_schema` visibility is
respected.

Databricks reads `DATABRICKS_CONFIG_FILE` or `.databrickscfg` in the home
directory. Authenticate first with `databricks auth login --profile DEV`. The
connection test checks Unity Catalog access without starting compute. SQL uses
INLINE JSON_ARRAY and internal chunks from the same origin, read page by page.
The submit request waits up to 5 seconds for the statement, then status is
polled from 200 ms up to every 2 seconds. Inline results are capped at 25 MiB,
so a statement asks the server for at most five pages of rows; a longer result
reports that the rest was not read. The CLI token is reused for 5 minutes and
asked again when a request is rejected with 401. External
links are not followed. The initial submission waits for the response with the
statement ID, up to the CLI/HTTP limits, so it can be cancelled later, even if
cancelled during that interval. Cancellation or timeout after submission
requests remote cancellation. Metadata ignores `hive_metastore`.

Real tests with dedicated disposable fixtures:

```bash
cargo xtask check --full
```

The gate starts PostgreSQL 14/18 and MySQL 8.4/9.7 and removes the services at
the end. They use only localhost and database/user `jykre`, with the public
fixture password `jykre-test`. Ports and policy: [testing](testing.md). The
Databricks HTTP tests use a local mock server; the real CLI and workspace must
be validated in an authenticated environment.

References: [SQLx RawSql](https://docs.rs/sqlx/latest/sqlx/struct.RawSql.html) and
[Databricks Statement Execution](https://docs.databricks.com/api/statement-execution/v1/statement-execution).
