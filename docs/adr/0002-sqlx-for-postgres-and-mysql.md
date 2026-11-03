# ADR-0002: SQLx for PostgreSQL and MySQL, with rustls and the text protocol

- **Status:** Accepted (recorded 2026-11-02 for a decision already in effect);
  the session lifetime is superseded by [ADR-0014](0014-a-session-per-tab.md)
- **Date:** 2026-11-02

## Context

The relational drivers must run arbitrary user SQL, stream rows, keep exact
values (decimals, large integers, temporal text), support cancellation and
timeouts, and use TLS identically on every platform.

## Decision

- PostgreSQL and MySQL use SQLx 0.9 without default features, with
  `runtime-tokio` and `tls-rustls-ring-native-roots`.
- User SQL runs through the text protocol (`RawSql`), unchanged; values arrive
  as text and keep their exact representation.
- MySQL authentication methods that need RSA require TLS; the RSA feature is
  not enabled.
- Each run uses a dedicated session; cancellation, timeouts and interrupted
  reads discard it.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| `tokio-postgres` + `mysql_async` | Mature protocol crates | Two APIs, two TLS integrations | SQLx covers both, plus SQLite for local storage |
| Binary protocol with typed decoding | Typed values | Type mapping per database, risk of lossy conversions | Text keeps exact values without per-type code |

## Consequences

- Positive: one library for both databases and local storage; rustls
  everywhere.
- Negative: text protocol values need presentation rules (`cell_format`);
  no prepared-statement reuse for user SQL.
- Required: drivers never convert exact numbers through floats.
