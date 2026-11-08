# ADR-0029: Optional export of logs and traces over OTLP, with metadata only

- **Status:** Accepted (implemented on 2026-11-06)
- **Date:** 2026-11-06
- **Authors:** Alan de Oliveira (with an AI agent)

## Context

The local log ([ADR-0009](0009-structured-logging-and-timings.md)) only
helps once a user sends the file. A team running Jykre Studio on many
machines wants failures, slow operations and external calls in its own
observability stack, as they happen. External calls are where time and
failures concentrate: database sessions and statements, Databricks REST
and its CLI, the AI provider, the operating system vault and the files an
export writes.

What leaves the computer must be stricter than the local log. SQL, values,
credentials, connection settings, hosts and paths must never reach a
collector, even if a field is added by mistake. High-frequency UI events
(mouse moves, cell selection, frames) must not be sent: they cost
performance and bandwidth and say little.

## Decision

- Export **logs and traces** (no metrics) over **OTLP**, to a collector the
  user names in **Preferences › Diagnostics › Telemetry**, over
  **HTTP/protobuf** or **gRPC**. Logs and traces are turned on separately;
  both are **off by default**. Only Preferences configure it; the
  application reads no `OTEL_*` variable of its own.
- Dependencies (approved 2026-11-06), used only by `jykre::telemetry`:
  `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp` (HTTP over the
  `reqwest` 0.13 already in the tree, gRPC over `tonic` with ring and the
  system's roots), `tracing-opentelemetry` and
  `opentelemetry-appender-tracing`. `tracing` is added to
  `jykre-driver-databricks` for its HTTP and CLI spans.
- The bridges are added to the subscriber when logging starts, behind
  per-layer filters. Until the preferences are read, what they capture is
  held in memory (at most 1,024 spans and 1,024 log records) and then sent
  or dropped, so the start of the application can be exported. A signal
  turned off stops being captured at once.
- **Logs** are the local log's events, filtered by `JYKRE_LOG`, except
  `trace` (SQL and payloads), the frame probe, UI-thread timings below a
  warning, the exporter's own stack, and events with a field that may hold
  free text (`sql`, `file`, `path`, `url`, `error`, `account`), which stay
  local.
- **Traces** are Jykre's spans around every external call, `client` kind,
  using OpenTelemetry semantic-convention names. Before export every
  attribute outside an allowlist (`jykre::telemetry::attributes::ALLOWED`)
  is removed, with span events, links and status descriptions. Exported:
  the database system and namespace, the operation (the statement's first
  keyword from a fixed list, otherwise `OTHER`), row counts, sizes, SQLSTATE,
  error codes, the HTTP method, status and URL template (identifiers
  replaced by `{id}`), the CLI subcommand and exit code, the AI provider,
  model and token counts, the connection's identifier. Never: SQL text,
  values, bound parameters, credentials, hosts, ports, users, paths, request
  or response bodies, server error messages.
- The trace context is not sent to databases, Databricks or AI providers.
- The resource names the service, its version, an identifier of the run,
  the operating system and the architecture; never the computer's or the
  user's name.
- The collector's address is HTTPS, or plain HTTP only on this computer.
  Headers for its authentication are kept in the system vault
  (`telemetry:otlp:headers`), as a secret.
- The SDK's batch processors run on threads of their own; each export is
  polled inside the application's only Tokio runtime, so no second runtime
  is created. Exports time out after 10 s; the exit waits at most 3 s for
  what is queued.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Ship the local log file to a collector (an agent tailing it) | No new dependency | No traces or spans; every machine needs an agent; the file holds what the local policy allows, not what may leave | External calls need traces; the export policy must be stricter |
| Our own OTLP encoder | Fewer crates | Protobuf, gRPC, retries and batching to write and maintain | The OpenTelemetry crates are the reference implementation |
| `reqwest-blocking-client` for HTTP | The SDK's default | A hidden second Tokio runtime per client | One runtime only ([Architecture](../reference/architecture.md#concurrency)) |
| The SDK's async-runtime batch processors | No adapter | Behind experimental features | The thread-based processors are the stable API |
| Metrics now | Histograms of database and HTTP times | More code and data | Not asked for; derivable from spans in a collector |
| Configuration through `OTEL_*` variables | Standard | Hidden from users; a second way to turn it on | Preferences are where users choose what leaves |

## Consequences

- Positive: a team sees failures and slow external calls in its own
  stack, with database names, operations, times and error codes, without
  SQL or values.
- Negative or costs: 15 more crates (`tonic`, `prost`, the OpenTelemetry
  family); background network traffic when the user turns it on (an
  exception to "nothing remote on the interactive path" in
  [Performance](../standards/performance.md)); every span field must be in the
  allowlist to be exported.
- Required: a new span around an external call uses `jykre::client_span!`
  (or the same fields in a backend crate) and only allowlisted names for
  what may leave; a new allowlisted name is reviewed like a log field.
  Free text never goes into a span field.

## How to validate

- Tests capture what an in-memory exporter receives during a run with a
  synthetic password, SQL text, cell values and a host, and assert none of
  them is exported while the expected attributes are
  (`backend::tests::telemetry_of_a_run_carries_metadata_only`,
  `telemetry::tests`).
- A fake collector on `127.0.0.1:0` receives protobuf with the configured
  headers on `/v1/traces`; a 401 is an authentication failure.
- A manual check against an OpenTelemetry Collector with the `debug`
  exporter, over HTTP and gRPC.

## Implementation notes

Implemented on 2026-11-06 (`jykre::telemetry`).

- Log records are copied before export without `log.file`, `log.module_path`
  and `code.*` (from the `log` crate bridge, a path on the computer that
  built a dependency) and without the event name, which the bridge takes
  from the source file and line. A real collector showed GPUI errors
  carrying such paths before this.
- The span bridge records unsigned integers as text, so counts and sizes
  are recorded as `i64` (`telemetry::attributes::count`).
- `list_profiles` and `list_compute` of the Databricks connection dialog
  have the driver's HTTP and CLI spans only, without a span of their own.
- The OTLP library still merges `OTEL_EXPORTER_OTLP_*HEADERS` into the
  headers when those variables are set.
