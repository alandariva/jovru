# ADR-0032: Session encryption reported by the drivers

- **Status:** Accepted (by the maintainer on 2026-11-07; implemented)
- **Date:** 2026-11-07
- **Authors:** AI agent, at the maintainer's request (milestone M3 of
  [Version 0.2.0](../plans/releases/0.2.0.md); the approach was chosen by the
  maintainer on 2026-11-07, before this record was written)

## Context

M3 asks that the active connection show whether its session is encrypted
and warn when it uses plain text
([Security](../standards/security.md#known-deviations)). The profile only
says what TLS mode was asked for: a PostgreSQL `prefer` or a MySQL
`preferred` profile falls back to plain text when the server offers no TLS,
and nothing tells the user. Only the open connection knows what was
negotiated.

The `Session` contract ([Drivers](../reference/drivers.md)) has no way to
say it. Changing a core contract needs the maintainer's approval and an ADR
([Workflow](../standards/workflow.md#architecture-decisions)).

## Decision

- `jykre-core` gains `Encryption`: `Unknown` (the default), `Tls { version }`
  (the protocol version when the server names it), `Plaintext` and `Local`
  (no network: a file on this computer).
- `Session` gains `async fn encryption(&mut self, cancel) ->
  DriverResult<Encryption>`, whose default answers `Unknown`, so a driver
  that cannot tell needs no change.
- The application asks once per session, in `backend::run_statements`,
  right after a run opened it (the tab had none, or the kept one was lost)
  and before its first statement: no result is open for paging and no
  transaction has a snapshot that asking could take. A failure is logged at
  `debug` and reads as `Unknown`; it never stops the run. The answer travels
  in `SessionRun::encryption` (`None` when the run kept the tab's session).
- Drivers:
  - PostgreSQL reads `ssl` and `version` from `pg_stat_ssl` for
    `pg_backend_pid()`; inside a transaction without a snapshot it answers
    `Unknown` without querying.
  - MySQL reads `SHOW SESSION STATUS LIKE 'Ssl_version'` over the text
    protocol; an empty value is plain text.
  - Databricks answers TLS: CLI profiles are accepted only with an `https`
    workspace URL.
  - SQLite answers `Local`.
- The query tab shows a lock beside **Connected** for TLS (its tooltip names
  the version) and an amber **Not encrypted** warning, with what it exposes
  and how to require TLS, for plain text. `Local` and `Unknown` show nothing.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Infer from the configured mode | No contract change, no query | `prefer`/`preferred` cannot tell; a fallback to plain text stays silent | Does not meet M3 |
| Read the negotiated state from the pinned SQLx fork | No extra query | Widens the fork ([ADR-0030](0030-transaction-state-through-a-pinned-sqlx-fork.md)) to keep until upstream; a dependency change | The maintainer chose the contract with server queries |
| Ask on every run | Always current | A query per run; TLS cannot change within a session | Once per session is enough |

## Consequences

- Positive: the tab tells the truth about its own session, including a
  silent fallback to plain text.
- Negative or costs: one short query when a PostgreSQL or MySQL tab session
  opens (report, export and metadata sessions do not ask).
- What becomes required or forbidden: a driver that can tell must answer
  without changing what the session's statements see; asking happens only
  before the first statement.

## How to validate

- `backend` unit test: a new session is asked once, before its statements;
  a kept one is not; a reopened one is.
- Live tests (`cargo xtask check --full`): the PostgreSQL fixture, whose
  profile disables TLS, answers `Plaintext`; the MySQL fixture, which
  requires TLS, answers `Tls` with a `TLS…` version. SQLite answers `Local`.
- In the application: the badge beside **Connected** for each case.
