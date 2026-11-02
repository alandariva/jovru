# ADR-0009: Structured logging and timings with `tracing`

- **Status:** Accepted (implemented on 2026-11-02)
- **Date:** 2026-11-02

## Context

The application has no logs. Diagnosing a user's failure, a slow operation or
a crash depends on reproducing it. Performance budgets have no record of
production timings.

## Decision

- Add `tracing`, `tracing-subscriber` (with `EnvFilter` and JSON output) and
  `tracing-appender`.
- Logs go to `<data folder>/logs/jykre.log` as JSON lines, rotated daily,
  keeping 7 files; debug builds also print readable lines to the console.
- Default level `info` for `jykre*` crates, `warn` for dependencies;
  `JYKRE_LOG` overrides it.
- A panic hook records panics with a backtrace.
- Operations record `elapsed_ms` at `debug`; UI-thread work over 100 ms and
  operations slower than expected are `warn`.
- SQLx statement logging is turned off; `reqwest` header logging is never
  enabled. What may not be logged is listed in
  [Reliability](../reliability.md#never-logged).

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| `log` + `env_logger` | Small | No spans or structured fields | Fields like `connection_id` and `elapsed_ms` matter |
| No logs | Nothing to leak | No diagnostics | Support and performance need them |

## Consequences

- Positive: failures and slow operations can be diagnosed from a file the
  user can send.
- Negative: three dependencies; every log call must respect the privacy
  rules.
- Required after acceptance: no `println!`/`eprintln!` in production code;
  new operations log their outcome and timing.

## How to validate

A test captures log output during a run with a password and SQL text and
asserts that neither appears above `trace`.

## Implementation notes

Implemented on 2026-11-02 (`jykre::logging`).

- `tracing-appender` names daily files `jykre.<date>.log` (for example
  `jykre.2026-11-02.log`) instead of a single `jykre.log` renamed on rotation;
  the last 7 are kept.
- Slow UI-thread work is measured with `logging::UiTimer`; cache reads warn
  above their 50 ms budget.
