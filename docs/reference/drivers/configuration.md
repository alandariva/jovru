# Drivers: configuration and runtime

Part of [Database drivers](README.md).

## SQLx and the pinned fork

PostgreSQL and MySQL use SQLx 0.9 without default features, with
`runtime-tokio` and `tls-rustls-ring-native-roots`, so TLS is rustls on every
platform (D-0002). SQLx's RSA feature is not enabled, which is why MySQL
authentication methods that need RSA require TLS. User SQL runs through the
text protocol (`RawSql`), unchanged: values arrive as text and keep their
exact representation, so drivers never convert exact numbers through floats,
and the presentation rules live in `cell_format`. The cost is that user SQL
gets no prepared-statement reuse.

SQLite is reached only through SQLx's `sqlite` feature. Another SQLite binding
(`rusqlite`) stays forbidden while SQLx links `libsqlite3-sys`, since Cargo
refuses two crates that link the same native library (D-0027).

Every SQLx crate (`sqlx`, `sqlx-core`, `sqlx-macros`, `sqlx-macros-core`,
`sqlx-mysql`, `sqlx-postgres`, `sqlx-sqlite`) is replaced through
`[patch.crates-io]` in the root `Cargo.toml` by the `alandariva/sqlx` fork, so
the tree keeps a single `sqlx-core` (D-0030). The fork is SQLx's `v0.9.0` plus
read-only readers of the transaction state, and a `libsqlite3-sys` bump to
0.39 (bundled SQLite 3.54.1, for RUSTSEC-2026-0459):
`PgConnection::transaction_status()` (with `PgTransactionStatus` made public),
`MySqlConnection::server_in_transaction()` and `server_autocommit()` (`ping()`
refreshes them and a new connection takes them from the handshake) and
`LockedSqliteHandle::is_autocommit()`. Rules:

- All SQLx crates point to the same revision. A change to the fork is a new
  revision, never a moving branch.
- The fork carries only this API; anything else goes upstream or gets its own
  decision.
- `deny.toml` allows the repository in `[sources] allow-git`, with a review
  date. Upgrading SQLx means applying the commit to the new tag, publishing a
  new revision and running the full gate.
- The patch is temporary: the same API is proposed upstream
  (transact-rs/sqlx#4512). When a SQLx release has the readers, the drivers
  move to it and the patch and the `allow-git` entry are removed.

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
connections. MySQL authentication methods that require RSA must use TLS: without it, `caching_sha2_password` (MySQL 8's default) signs in only while the server holds the account's hash from an earlier TLS sign-in, and otherwise fails as an authentication error that says TLS is required, never as a protocol error.

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
wait up to 4 seconds for a lock another connection holds. `:memory:` is
rejected because an in-memory database differs per connection, so the
metadata would describe another database than the tab's.

The `jovru-driver-sqlite` crate depends only on `jovru-core`, SQLx (`sqlite`),
`futures-util`, `serde` and `tokio`, adds no `unsafe` code and registers as
`DriverKind::Sqlite` with `SqlDialect::Sqlite`. A session keeps one
connection in a task of its own, as the other relational drivers do, so a
result stays open between pages. Metadata comes from the pragma table
functions (`pragma_database_list`, `pragma_table_list`,
`pragma_table_xinfo`, `pragma_foreign_key_list`, `pragma_index_list`) and the
schema table, and needs no session. The tokenizer follows SQLite's rules:
`"x"`, `` `x` `` and `[x]` names, no backslash escapes, comments that do not
nest, `?`, `?NNN`, `:name`, `@name` and `$name` parameters, and trigger
bodies kept whole. Interrupting a write inside an explicit transaction makes
SQLite roll the whole transaction back; attached databases are per
connection, so the metadata connection sees only `main` and an `ATTACH` run
in a tab is not in the explorer. The editor's SQL catalog for SQLite
(`catalog/sqlite`) does not exist yet.

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
used by discovery, against the same canonical configuration file. Jovru does
not parse INI syntax itself, so BOMs and section comments supported by the CLI
work consistently. A missing selected workspace profile reports `NotFound`; a
profile without a safe HTTPS workspace URL reports `Validation`. Another
profile is never substituted. Host lookup skips authentication and has a
60-second timeout and 1 MiB output bound.
The token comes through the CLI and is kept only in memory. The driver does not use
`Credentials.password`. The CLI runs without a shell, with a timeout and
`kill_on_drop`. Authentication and HTTP pools are shared by profile, resolved
CLI config file, CLI executable and workspace origin (D-0005). Only HTTPS is
used. SQL execution uses Statement Execution on a SQL
Warehouse, or Command Execution API 1.2 on a running classic all-purpose
cluster. A classic session creates and reuses a SQL context per tab, confirms
readiness and destroys it on close. Context defaults are applied using quoted
`USE` commands; user statements are sent unchanged, except that the
`:name` markers of a statement with parameters become typed literals, since
the command API cannot bind values: text as `decode(X'<hex>', 'UTF-8')`,
numbers as `CAST('…' AS DECIMAL(p,s))` (or `DOUBLE` with an exponent),
`TRUE`/`FALSE`, `DATE'…'`, `TIMESTAMP'…'` and a typed `CAST(NULL AS …)`
([Named parameters](parameters.md#named-parameters), D-0026). Warehouses send the
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

### External processes (Databricks CLI)

The authentication cache is scoped by CLI profile, canonical configuration
file, executable and HTTPS workspace origin. It never mixes workspace tokens.
Idle entries are pruned on cache access; token refreshes are serialized and
expired/rejected tokens are discarded. The CLI receives the same explicit
configuration file used to resolve the workspace. HTTP pools are shared within
that identity. Compute names/IDs are non-sensitive profile options; tokens
never reach connection options or the select's state.

- The CLI runs with **arguments as a vector** (`Command::new(path).args(...)`),
  never through a shell, so a profile name cannot inject a command.
- The executable comes from the configuration (`cliPath`) or the `PATH`.
- Its output contains the token: never log stdout. On failure, report the exit
  status and a filtered message, not the raw output.
- The process has a timeout (60 s) and `kill_on_drop(true)`.

## Real use and limits

`examples/connect.rs` takes `JOVRU_CONNECTION` (non-sensitive JSON) and
`JOVRU_PASSWORD` separately. `JOVRU_SQL` runs one statement;
`JOVRU_METADATA=1` reads metadata. Run it with
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
never reused. Both drivers check the cancellation and the time limit before
reading each row, the time limit against the clock, so a result that arrives
faster than it is read still stops at its next row: SQLx's reads do not wait
while data is ready, and Tokio's timer may not fire meanwhile. Both drivers
first read and drop the rest of a result the reader left (interrupted by a
limit or the sink, or left open for paging when the tab runs anything else),
and cancel or kill the statement only when that takes longer than 5 seconds,
because the statement may already be done: cancelling it would roll back its
implicit transaction, and with it a write whose returned rows were shown (in
PostgreSQL, a `SELECT` that writes as it returns rows; an `UPDATE …
RETURNING` finishes before it sends its first row). MySQL never abandons a read on a session's
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
closed. The cancellation and the time limit are checked before each row, the
time limit against the clock, since SQLx's row channel does not make the
reading wait while the connection's thread is ahead. A page kept open holds
its statement, and a read lock on the file,
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
the end. They use only localhost and database/user `jovru`, with the public
fixture password `jovru-test`. Ports and policy: [testing](../../standards/testing.md). The
Databricks HTTP tests use a local mock server; the real CLI and workspace must
be validated in an authenticated environment.

The separate native-CLI profile regression uses synthetic configurations only,
without token resolution or network queries:

```bash
cargo test -p jovru-driver-databricks native_cli_profile_syntax -- --ignored
```

It checks connection setup for UTF-8 BOMs and section comments. It is ignored
in the portable gate because the native Databricks CLI is an external prerequisite.

References: [SQLx RawSql](https://docs.rs/sqlx/latest/sqlx/struct.RawSql.html) and
[Databricks Statement Execution](https://docs.databricks.com/api/statement-execution/v1/statement-execution).
Classic execution: [Command Execution](https://docs.databricks.com/api/command-execution/v2/create).
Profile discovery: [Databricks CLI auth profiles implementation](https://github.com/databricks/cli/blob/main/cmd/auth/profiles.go).
