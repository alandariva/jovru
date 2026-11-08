# Reliability, errors and logs

## Failure isolation

| Failure | Allowed effect | Forbidden effect |
| --- | --- | --- |
| SQL error in a tab | That tab shows the error in its results area and footer; its session stays open with its state | Affecting another tab, another connection or the application; closing the session |
| Connection lost during a run | The run fails with a connection error; the session is discarded and the tab says session settings start over | Freezing the UI; reusing the broken session |
| Kept session no longer answers (idle disconnect) | The next run opens a new session and says so | Retrying a statement on the new session |
| Timeout | The run is cancelled on the server when possible; the tab says it timed out | The query keeping running on the server without the user knowing |
| Failed or cancelled metadata refresh | The scope shows the failure; the last good read stays in use | An empty tree or a half-published read |
| One chosen namespace fails, or reading the choice is cancelled | The namespaces already read stay published; the others keep their last good read; the status names what failed. Nothing is removed from the cache after a cancellation | Losing the whole read for one namespace |
| Expired Databricks session | A clear authentication error telling the user to run `databricks auth login` | Retry loops; asking for a password |
| Panic inside a backend task | The operation fails with `Internal` | Closing the application |
| Corrupted metadata cache | The cache is set aside and recreated; the user is told to refresh | The application failing to start |
| OS vault unavailable | The password is used for this session only; the UI says why | Falling back to a file |
| A saved connection, the font size or the last session's tabs cannot be read at start | That item is left out or takes its default, and the explorer says which | Hiding the other connections or the restored tabs |
| An export fails or is cancelled (disk full, no permission, the server, the user) | The export bar says why; the temporary file is removed and the chosen file keeps what it had; the session comes back to the tab unless it was lost | A half-written file, a replaced file, or a session left out |
| An update check fails (no network, GitHub limiting requests, an unreadable answer) | Nothing is shown outside **Preferences › About**, which says why; the next automatic check waits at least an hour, or what GitHub asks, at most a day | A dialog or a notice for a background check; checking again at once |
| An update fails or is cancelled (download, signature, unpacking, the swap) | About says why and offers the release page; the folder keeps the running version, the downloaded files are removed, and a failed swap is undone step by step | A half-replaced folder; installing an archive that did not verify |

## Mechanisms

- **A session per tab** ([ADR-0014](../adr/0014-a-session-per-tab.md)). A tab
  opens a session on its first run and keeps it until the tab closes, its
  connection changes or the session is lost, so state such as the current
  schema and temporary tables carries over between runs (only autocommit is
  supported; see [Architecture](../reference/architecture.md#known-deviations)). While a
  result has more pages, the session keeps it open until the tab runs
  anything else.
  A failed statement keeps the session. After a cancellation, a timeout or an
  interruption by a limit the driver cancels the statement on the server and
  brings the protocol back to a known state, within 5 seconds, or closes the
  session; a session in an unknown state is never reused. A kept session is
  pinged before each run and replaced, with a notice, when it does not answer.
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
  an `UPDATE`. A round of the AI assistant is not retried either: it
  costs tokens, and the user retries it with **Try again**.
- **Clean shutdown.** On exit, running operations are cancelled and sessions
  are closed within a short deadline.

## Timeouts

| Operation | Timeout |
| --- | --- |
| PostgreSQL/MySQL: connect, test, open a session | 15 s per step |
| Statement execution (all drivers), and each further page | `ExecutionOptions.timeout`, chosen per tab: default 300 s; `None` disables it |
| PostgreSQL/MySQL introspection | 300 s |
| SQLite: open the file and read its header, test, open a session | 15 s per step |
| SQLite: wait for a lock another connection holds (busy timeout) | 4 s, below the 5 s recovery, since the busy wait does not see an interrupt |
| SQLite introspection | 300 s; namespace discovery 60 s |
| Server-side cancel attempt after cancellation or timeout | 5 s |
| Bringing a session back to a known state after a failure, or pinging a kept session | 5 s |
| Databricks: test, warehouse open or initial classic authentication/compute check | 90 s |
| Databricks introspection | 300 s |
| Inspection or definition of an object (all drivers) | 60 s |
| Reading, writing or removing a password in the OS vault | 10 s; past it the vault counts as unavailable, so the password is used for the session only and the connection dialog never stays stuck saving |
| Databricks HTTP request | 30 s |
| Databricks: first wait for a statement, inside its submit request | 5 s, then status polls |
| Databricks: status polls of a warehouse statement or classic command | Every 2 s for the first 10 s since submission (a classic command's first poll comes 2 s after it), then a quarter of the time elapsed, at most 10 s |
| Databricks compute discovery | 90 s, cancellable without starting compute |
| Databricks CLI profile discovery | 60 s, cancellable; skips per-profile validation, output bounded to 1 MiB |
| Databricks classic context creation/readiness | 90 s for readiness/setup, after the bounded submission obtains its ID; destroyed on failure/cancellation |
| Databricks classic context destroy | 5 s, including recovery after an unconfirmed cancel |
| Databricks CLI token reuse | At most 5 min or 30 s before reported expiry; shared by auth identity, one coordinated forced refresh on 401 |
| Databricks CLI (token resolution) | 60 s |
| Export: each part read from the session | `ExecutionOptions.timeout` of the tab, as any page |
| Export: creating, writing one part (about 16 MB) or finishing the file | 60 s (`backend::FILE_TIMEOUT`), so a network share that stops answering fails the export |
| Clipboard: estimating or encoding a grid selection/query page | 60 s per phase, checked between rows/cells on the blocking pool; cancellable, with no partial clipboard update |
| AI assistant: connecting to the provider | 15 s |
| AI assistant: first byte of an answer | 120 s |
| AI assistant: silence between pieces of a streamed answer | 60 s |
| AI assistant: a whole round | 300 s; cancellable at any time (**Stop**), which closes the connection |
| AI assistant: **Test** in Preferences | 30 s, cancellable |
| AI assistant: one tool reading the metadata cache | 10 s; past it the model is told so and the round goes on |
| AI assistant: a query the user approved (`run_query`) | The tab's statement time limit; **Stop** cancels it and ends the turn. It runs in the assistant's own session, kept between queries and closed when the tab's connection changes or the tab closes; it reads at most 4 MB |
| AI assistant: reading, saving or removing the API key in the OS vault | 10 s, as passwords |
| Telemetry: one export of a batch of spans or log records to the collector | 10 s, then the batch is retried at most three times with backoff, as OpenTelemetry recommends; past the queue (2,048), new items are dropped |
| Telemetry: what is queued when the application exits | 3 s |
| Final writes of the open tabs and the reports when the application exits | 200 ms, GPUI's shutdown limit (`SHUTDOWN_TIMEOUT`); past it the write is abandoned and the last debounced save remains |
| Telemetry: **Test** in Preferences | 10 s, cancellable |
| Telemetry: reading, saving or removing the collector's headers in the OS vault | 10 s, as passwords |
| Update check: connecting to GitHub, then the whole release list | 15 s each; cancellable |
| Update download | 15 s to connect, 30 s of silence while the body arrives, 10 min in all; cancellable |
| Update: verifying the signature and unpacking the archive | 120 s; cancellable. The swap that follows is renames only and is neither cancelled nor timed: a rename refused on Windows (an antivirus holding the file) is tried 5 times, 200 ms apart, then undone |

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

Logs follow [ADR-0009](../adr/0009-structured-logging-and-timings.md) and are set
up by `jykre::logging::init` when the application starts:

- `tracing` with `tracing-subscriber`; `tracing-appender` writes in the
  background.
- JSON lines in `<data folder>/logs/jykre.<date>.log`, one file per day,
  keeping the last 7; readable lines on the console in debug builds.
- Default level `info` for `jykre*` crates and `warn` for dependencies,
  configurable with `JYKRE_LOG` (`EnvFilter` syntax, for example
  `JYKRE_LOG=jykre=debug`).
- A panic hook records panics with their location and a backtrace.
- Runs (`backend::query`) log their outcome at `info` (duration, statements,
  rows, error code); each statement logs its time, size and limit at `debug`;
  SQL text only at `trace`. Metadata refreshes log their outcome and duration.
- `logging::UiTimer` measures UI-thread work (publishing a result, rebuilding
  the grid view, copying) and warns above 100 ms; `logging::timing` warns when
  a cache read exceeds its 50 ms budget.
- SQLx statement logging is off in the drivers and in local storage, and
  `reqwest` never logs headers. A unit test runs a batch with a password and
  SQL text and checks neither appears in the logs above `trace`.
- When the user turns it on, the same events (except `trace`, the frame
  probe, UI-thread timings below a warning and events with a free-text
  field) and the spans of external calls are also sent to an OpenTelemetry
  collector ([ADR-0029](../adr/0029-otlp-export-of-logs-and-traces.md),
  [Security](security.md#telemetry)). UI-thread timings use the target
  `jykre::ui_thread`.

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

A span around an external call (a database, a server, a program, the vault,
a file) is a `client` span, `jykre::client_span!` in the library, with
OpenTelemetry semantic-convention names for what telemetry may export
(`db.system.name`, `db.namespace`, `db.operation.name`,
`db.response.returned_rows`, `error.type`, `http.request.method`,
`url.template`, `process.exit.code`, `gen_ai.*`) and `jykre.*` for the rest.
It records how it ended with `telemetry::attributes::record_outcome`.
`backend::run` carries the caller's span into the runtime.

### Never logged

Passwords, tokens, authorization headers, connection URLs, cell values, query
parameters, and SQL above `trace`. See [Security](security.md#logs-and-diagnostics).

## Diagnostics for users

- Error messages in the UI offer **Copy details**: code, technical message,
  time, application version and run identifier, so a report needs no
  screenshot.
- **Open log folder**, under Diagnostics in Preferences, opens the log
  folder in the system's file manager.

## Known deviations

| Standard | Current code |
| --- | --- |
| Retries with backoff for idempotent reads | No automatic retries |
| Clean shutdown with a deadline | Dropping a workspace cancels its run; there is no explicit shutdown sequence |
