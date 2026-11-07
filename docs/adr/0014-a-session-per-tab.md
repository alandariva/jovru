# ADR-0014: A session per tab, kept between runs and after errors

- **Status:** Proposed (amended for report tabs by [ADR-0025](0025-report-panels.md): a session per connection and database a report's panels use)
- **Date:** 2026-11-03

## Context

`Ctrl Enter` runs one statement at a time: the one at the cursor or the
selected text. With a session per run
([ADR-0002](0002-sqlx-for-postgres-and-mysql.md), [Reliability](../standards/reliability.md)),
`USE sales` in one run did not apply to the `SELECT` run next, temporary tables
vanished and a transaction could not span runs. A failed statement, a
cancellation or a result cut by the row limit also discarded the session, so
the context was lost at the moments the user was most likely to continue
working.

A session kept between runs can go stale while idle, and a session whose
statement was interrupted may be in an unknown protocol state.

## Decision

- Each tab keeps one session between runs. It opens on the tab's first run and
  closes when the tab closes, its connection changes or the session is lost.
  The workspace hands the session to `backend::query` and takes it back with
  the result; a session returned by a superseded run, or after the connection
  changed, is closed.
- A failed statement does not end the session. After a database error the
  session is kept as it is. After a cancellation, a timeout, an interruption by
  a limit or the sink, or a value that cannot be decoded, the driver cancels
  the statement on the server and reads up to the end of the response, each
  within 5 seconds, and closes the session if that fails.
- `Session` gains `is_open` (the session can be reused) and `ping` (a kept
  session still answers, checked before each run within 5 seconds). A session
  that does not answer is replaced and the tab says that session settings
  started over. A statement is never retried.
- A result longer than one page stays open in the session; the next page is
  read from it (`Session::fetch`) until the tab runs anything else. PostgreSQL
  and MySQL keep the connection in a task of its own that holds the row
  stream between pages.
- Only autocommit is supported for now. An explicit transaction stays open in
  the session but is not shown nor confirmed on close (technical debt, see
  [Architecture](../reference/architecture.md#known-deviations)).
- Databricks Statement Execution has no server session: the driver keeps the
  catalog and schema and follows successful `USE CATALOG`, `SET CATALOG` and
  `USE [SCHEMA | DATABASE]` statements.
- Classic Databricks clusters use a real SQL Command Execution context per
  tab, with readiness checks, confirmed cancellation and explicit destruction
  ([ADR-0005 amendment](0005-databricks-through-cli-and-rest.md)).

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Keep a session per run | No idle session can go stale; no recovery code | Context is lost between `Ctrl Enter` runs | Running one statement at a time is the main way to work in the editor |
| Keep the session but discard it after any failure | Simpler drivers | A typo or the row limit loses the context | Errors are routine while writing SQL |
| Reconnect and replay `USE`/`SET` statements | Survives lost connections | Needs parsing of every dialect's session commands; temporary tables and transactions cannot be replayed | Saying that the session started over is honest and simpler |
| Limit MySQL rows on the server with `sql_select_limit` | No rows to drain after the limit | Changes a session variable the user may set themselves | Draining with a deadline keeps the user's session untouched |

## Consequences

- Positive: `USE`, `SET` and temporary tables apply to later runs in the same
  tab; errors keep them, and long results are read page by page without
  running the statement again.
- Negative or costs: one ping per run; a result interrupted by the limit is
  drained (MySQL) or cancelled and drained (PostgreSQL) before the session is
  reused; an explicit transaction stays open, unseen, until the user ends it
  or the tab closes.
- What becomes required: drivers report whether a session can be reused and
  recover or close it after every failure; tests check that state survives
  errors, timeouts, cancellations and interrupted reads.

## How to validate

- Backend tests with a stateful test driver: state survives runs and errors, a
  lost session is replaced and reported, a dead session is replaced without
  retrying the statement.
- Live tests (`cargo xtask check --full`) on PostgreSQL and MySQL: after a
  database error, a timeout, a cancellation, the row limit and a stop by the
  receiver, the session runs the next statement and keeps a session setting.
