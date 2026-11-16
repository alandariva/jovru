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
| An automation run fails, times out or is stopped | The run's history row says its state, its cause and whether its effect is unknown; its session is closed or its program tree ended; other runs, tabs and sessions are untouched | Sending or starting it again; leaving a session or a descendant process running |
| The history of a run cannot be written | Before it starts: the run is refused and nothing is sent. After it ended: the user is told, the run is not repeated, and recovery marks the row `interrupted` at the next start | Sending without a record; repeating a run to get its record |
| Another instance holds `automation.lock`, or `automation.db` is from a newer version or corrupt | Automation is read-only or off, saying why; a corrupt file is set aside and recreated, and the user is told its jobs are lost. The rest of the application works | Two instances recovering each other's runs; the application failing to start |
| An update fails or is cancelled (download, signature, unpacking, the swap) | About says why and offers the release page; the folder keeps the running version, the downloaded files are removed, and a failed swap is undone step by step | A half-replaced folder; installing an archive that did not verify |

## Mechanisms

- **A session per tab** (D-0014). A tab
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
| Export: creating, writing one part (about 16 MB) or finishing the file | 60 s (`backend::FILE_TIMEOUT`), so a network share that stops answering fails the export; finishing is sealing the file (flush and sync), then the rename, each within its own 60 s |
| Clipboard: estimating or encoding a grid selection/query page | 60 s per phase, checked between rows/cells on the blocking pool; cancellable, with no partial clipboard update |
| AI assistant: connecting to the provider | 15 s |
| AI assistant: first byte of an answer | 120 s |
| AI assistant: silence between pieces of a streamed answer | 60 s |
| AI assistant: a whole round | 900 s, room for an 8,192-token answer from a slow local model; cancellable at any time (**Stop**), which closes the connection |
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
| Automation: a SQL Job or a program run | The job's own timeout, default 300 s; reaching it fails the run with `timeout` |
| Automation: a run waiting for a slot | 60 s in the queue (FIFO, at most 8 waiting, 4 per connection), then it leaves with `queue_timeout` |
| Automation: a stopped SQL run bringing its session back | 11 s in all (the drivers' 5 s recovery plus the close's 5 s and a margin); past it the work is dropped, which drops the session |
| Automation: a stopped program, from the request to end to the kill of its tree | 5 s; the pipes and the `jjp/1` connection then get 2 s to end and a killed leader 5 s to be reaped |
| Automation: a `jjp/1` program saying `hello` | 30 s from its start; past it the run fails with `protocol` and the tree is killed |
| Automation: the scheduler's sleep | At most 30 s between two readings of the wall clock, so a computer that slept is noticed on waking |
| Automation: a scheduled due time | 60 s of grace: a due time the scheduler reaches later is missed and follows the schedule's missed-time policy |
| MCP: one cache tool (`search_objects`, `describe_relation`) | 10 s (`TOOL_TIMEOUT`); past it the existing error text is the result |
| MCP: waiting for the user's decision | 120 s per decision; the card closes and the client gets an error result, nothing sent |
| MCP: an approved statement | 300 s, `ExecutionOptions`' default; **Stop** cancels; an error preview is sent only if the user sends it |
| MCP: reading the token from the vault | 10 s, as passwords; past it the request is HTTP 503 |
| MCP: writing or removing the token in the vault | 10 s; creation is refused and removal is retried |
| MCP: reading a request's headers and body | 10 s; the connection is closed |
| MCP: an idle HTTP connection | 60 s; the connection is closed |
| MCP: shutdown | Within the application's shutdown deadline; the listener closes, pending cards are cancelled and sessions are closed |

A new network operation gets an explicit timeout and a row in this table.

## Automation

The automation engine's reliability rules (no replay, the completion rule,
`effect_unknown`, arbitration, Stop and the deadline, the queue, chains, quit
and schedules) are in
[Automation](../reference/automation.md#automation-1).


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

Logs follow D-0009 and are set
up by `jovru::logging::init` when the application starts:

- `tracing` with `tracing-subscriber`; `tracing-appender` writes in the
  background.
- JSON lines in `<data folder>/logs/jovru.<date>.log`, one file per day,
  keeping the last 7; readable lines on the console in debug builds.
- Default level `info` for `jovru*` crates and `warn` for dependencies,
  configurable with `JOVRU_LOG` (`EnvFilter` syntax, for example
  `JOVRU_LOG=jovru=debug`).
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
  collector (D-0029,
  [Security](security.md#telemetry)). UI-thread timings use the target
  `jovru::ui_thread`.
  - The bridges are added to the subscriber when logging starts, behind
    per-layer filters. Until the preferences are read, what they capture is
    held in memory (at most 1,024 spans and 1,024 log records) and then sent
    or dropped, so the start of the application can be exported. A signal
    turned off stops being captured at once.
  - The SDK's batch processors run on threads of their own and each export
    is polled inside the application's only Tokio runtime; no second
    runtime is created. Timeouts are in [Timeouts](#timeouts).
  - Log records are copied before export without `log.file`,
    `log.module_path`, `code.*` (from the `log` crate bridge: a path on the
    computer that built a dependency) and the event name, which the bridge
    takes from the source file and line.
  - Counts and sizes on spans are recorded as `i64`
    (`telemetry::attributes::count`), because the span bridge records
    unsigned integers as text. `list_profiles` and `list_compute` of the
    Databricks connection dialog have the driver's HTTP and CLI spans only.

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
a file) is a `client` span, `jovru::client_span!` in the library, with
OpenTelemetry semantic-convention names for what telemetry may export
(`db.system.name`, `db.namespace`, `db.operation.name`,
`db.response.returned_rows`, `error.type`, `http.request.method`,
`url.template`, `process.exit.code`, `gen_ai.*`) and `jovru.*` for the rest.
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

## Decisions

The reasoning behind this area's structural choices is in [Decisions: Reliability, errors and logs](../decisions/reliability.md).

## Known deviations

| Standard | Current code |
| --- | --- |
| Retries with backoff for idempotent reads | No automatic retries |
| Clean shutdown with a deadline | Dropping a workspace cancels its run and closes its sessions in the background, without waiting; only the final writes of tabs and reports and the automation engine (`Engine::shutdown`) are awaited |

The deviation of Stop on Windows, where a program is not asked to end first, is in [Automation](../reference/automation.md#known-deviations).
