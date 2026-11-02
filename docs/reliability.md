# Reliability, errors and logs

## Failure isolation

| Failure | Allowed effect | Forbidden effect |
| --- | --- | --- |
| SQL error in a tab | That tab shows the error in its results area and footer | Affecting another tab, another connection or the application |
| Connection lost during a run | The run fails with a connection error; the session is discarded | Freezing the UI; reusing the broken session |
| Timeout | The run is cancelled on the server when possible; the tab says it timed out | The query keeping running on the server without the user knowing |
| Failed or cancelled metadata refresh | The scope shows the failure; the last good read stays in use | An empty tree or a half-published read |
| Expired Databricks session | A clear authentication error telling the user to run `databricks auth login` | Retry loops; asking for a password |
| Panic inside a backend task | The operation fails with `Internal` | Closing the application |
| Corrupted metadata cache | The cache is set aside and recreated; the user is told to refresh | The application failing to start |
| OS vault unavailable | The password is used for this session only; the UI says why | Falling back to a file |

## Mechanisms

- **A session per run.** Each run opens its own session and closes it at the
  end, so no idle session can go stale between runs. Cancellation, timeouts,
  errors and interruption by a limit discard the session instead of reusing a
  protocol in an unknown state.
- **Panics are contained.** `backend::run` spawns each operation on Tokio; a
  panic surfaces as a `JoinError` and becomes `Internal` ("Database task
  failed"), not a crash.
- **Timeouts on everything that touches the network** (below).
- **Cancellation everywhere.** Every external operation takes a
  `CancellationToken` and honours it before and during I/O. Closing a tab
  cancels its run.
- **Retries** only for idempotent reads (introspection, Databricks polling,
  token resolution), with exponential backoff and jitter, at most three
  attempts. A user's statement is **never** retried automatically: it may be
  an `UPDATE`.
- **Clean shutdown.** On exit, running operations are cancelled and sessions
  are closed within a short deadline.

## Timeouts

| Operation | Timeout |
| --- | --- |
| PostgreSQL/MySQL: connect, test, open a session | 15 s per step |
| Statement execution (all drivers) | `ExecutionOptions.timeout`, default 300 s; `None` disables it |
| PostgreSQL/MySQL introspection | 300 s |
| Server-side cancel attempt after cancellation or timeout | 5 s |
| Databricks: test or open | 90 s |
| Databricks introspection | 300 s |
| Databricks HTTP request | 30 s |
| Databricks CLI (token resolution) | 60 s |

A new network operation gets an explicit timeout and a row in this table.

## Error classification

Every error that reaches the UI carries an `ErrorCode`. Drivers map their
database's errors; the UI chooses titles and suggested actions by code, never
by message text.

| `ErrorCode` | Examples | Retriable by the user |
| --- | --- | --- |
| `Validation` | Invalid configuration or input, empty SQL, zero row limit | No, fix the input |
| `Authentication` | Wrong password, expired CLI session, vault unavailable | No |
| `PermissionDenied` | Missing privilege on an object or catalog | No |
| `Connection` | Refused connection, DNS failure, TLS failure, reset connection | Yes |
| `Timeout` | Application or server timeout | Yes |
| `Cancelled` | The user cancelled | — |
| `Database` | Syntax error, missing object, constraint violation | No |
| `NotFound` | Unknown connection, object or warehouse | No |
| `Protocol` | Unexpected server response | Maybe; report it |
| `NotImplemented`, `DriverNotRegistered`, `DuplicateDriver` | Programming or composition errors | No |
| `Internal` | Bugs, local storage failures, a panicked task | No; report it |

A test of the error mapping is required in each driver ([Testing](testing.md)).

## Logs

There is no logging framework yet. [ADR-0009](adr/0009-structured-logging-and-timings.md)
(proposed) defines the target:

- `tracing` with `tracing-subscriber`, initialised by the application.
- JSON lines in `<data folder>/logs/jykre.log`, rotated daily, keeping 7 files;
  readable console output in debug builds.
- Default level `info` for `jykre*` crates and `warn` for dependencies,
  configurable with `JYKRE_LOG` (`EnvFilter` syntax).
- A panic hook records panics with a backtrace, without sensitive data.

### Levels

| Level | Use |
| --- | --- |
| `error` | A failure the user will notice that is not routine: a bug, corruption, a failed migration |
| `warn` | A recovered abnormal situation: a retry, an unavailable vault, a slow operation (over 100 ms on the UI thread or over the expected time), a refresh with permission warnings |
| `info` | Lifecycle: start and exit, a run finished (duration, rows, no SQL), a refresh finished (counts) |
| `debug` | Diagnostic detail: timings per operation, introspection steps |
| `trace` | SQL text and payloads; **off by default** |

### Structured fields

Use them whenever they exist in context: `connection_id`, `driver`,
`execution_id`, `operation`, `elapsed_ms`, `rows`, `error_code`. Spans
(`#[instrument(skip_all, fields(...))]`) propagate them; `skip_all` prevents
logging arguments by accident.

### Never logged

Passwords, tokens, authorization headers, connection URLs, cell values, query
parameters, and SQL above `trace`. See [Security](security.md#logs-and-diagnostics).

## Diagnostics for users

- Error messages in the UI offer **Copy details**: code, technical message,
  time, application version and run identifier, so a report needs no
  screenshot.
- Once logs exist, a menu entry opens the log folder.

## Known deviations

| Standard | Current code |
| --- | --- |
| Structured logs and a panic hook | None ([ADR-0009](adr/0009-structured-logging-and-timings.md), proposed) |
| Retries with backoff for idempotent reads | No automatic retries |
| Clean shutdown with a deadline | Dropping a workspace cancels its run; there is no explicit shutdown sequence |
| Copy details on errors | Errors show their message only |
