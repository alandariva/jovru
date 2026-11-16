# Decisions: Reliability, errors and logs

Why this area is built as it is. Each entry records what was decided, why, and which alternatives lost. The rules themselves are in [Reliability, errors and logs](../standards/reliability.md). Entries are numbered `D-NNNN`, and the numbers are stable.

<a id="d-0009"></a>
## D-0009: Structured logging and timings with `tracing`

*Accepted 2026-11-02 · implemented 2026-11-02*

**Decision.** The application logs through `tracing` (`tracing-subscriber` with `EnvFilter` and JSON output, `tracing-appender`), set up by `jovru::logging::init`, with the file, level, panic hook and timing rules in [Logs](../standards/reliability.md#logs). Production code never prints; new operations log their outcome and timing, and what may not be logged is in [Never logged](../standards/reliability.md#never-logged).
**Why.** The application had no logs, so diagnosing a failure, a slow operation or a crash depended on reproducing it, and the performance budgets had no record of production timings. A file the user can send fixes both. The costs are three dependencies and a privacy rule on every log call. `tracing-appender` names daily files `jovru.<date>.log` rather than renaming a single file on rotation. Validation is a test that captures log output during a run with a password and SQL text and asserts neither appears above `trace`.
**Rejected.** `log` with `env_logger`: small, but no spans or structured fields, and fields like `connection_id` and `elapsed_ms` matter. No logs: nothing to leak, but support and performance work need diagnostics.

<a id="d-0029"></a>
## D-0029: Optional export of logs and traces over OTLP, with metadata only

*Accepted 2026-11-06 · implemented 2026-11-06*

**Decision.** Logs and traces (no metrics) can be exported over OTLP, HTTP/protobuf or gRPC, to a collector the user names in **Preferences › Diagnostics › Telemetry**; the two signals are turned on separately and both are off by default. What may leave, the allowlist and the collector's address rules are in [Security](../standards/security.md#telemetry); the span rules are in [Structured fields](../standards/reliability.md#structured-fields). New dependencies were approved on 2026-11-06 and are used only by `jovru::telemetry` (see [Stack and dependencies](../standards/stack-and-dependencies.md)).
**Why.** The local log helps only once a user sends the file; a team running Jovru on many machines wants failures, slow operations and external calls (sessions and statements, Databricks REST and CLI, the AI provider, the vault, export files) in its own observability stack as they happen. What leaves the computer must be stricter than the local log, even if a field is added by mistake, and high-frequency UI events cost performance and bandwidth for little information. The costs are 15 more crates (`tonic`, `prost`, the OpenTelemetry family) and background traffic when the user turns it on, an exception to "nothing remote on the interactive path" in [Performance](../standards/performance.md). The tests are named in [Security](../standards/security.md#telemetry); a manual check against an OpenTelemetry Collector with the `debug` exporter covers HTTP and gRPC. The collector library still merges `OTEL_EXPORTER_OTLP_*HEADERS` into the headers when those variables are set.
**Rejected.** Shipping the local log file through an agent tailing it: no new dependency, but no traces, an agent on every machine, and the file holds what the local policy allows, not what may leave. Our own OTLP encoder: fewer crates, but protobuf, gRPC, retries and batching to maintain. `reqwest-blocking-client` for HTTP: the SDK default, but a hidden second Tokio runtime per client. The SDK's async-runtime batch processors: behind experimental features, so the thread-based ones are used. Metrics now: not asked for and derivable from spans in a collector. Configuration through `OTEL_*` variables: standard, but hidden from users and a second way to turn it on.
