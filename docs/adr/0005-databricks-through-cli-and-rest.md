# ADR-0005: Databricks through CLI profiles and REST APIs

- **Status:** Accepted (recorded 2026-11-02 for a decision already in effect)
- **Date:** 2026-11-02

## Context

Databricks authentication (OAuth, tokens, profiles) is already handled by the
Databricks CLI on the user's machine. Implementing OAuth inside the
application would add a flow to maintain and tokens to store.

## Decision

- A connection names an authenticated CLI profile. The CLI resolves the token
  when needed; the token stays in memory and is never stored.
- SQL runs through the Statement Execution API on a SQL Warehouse (INLINE,
  JSON_ARRAY, polling); metadata uses the Unity Catalog REST API without
  starting compute.
- HTTPS only, redirects disabled, external result links not followed.
- The CLI runs without a shell, with a timeout and `kill_on_drop`.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| OAuth inside the application | No CLI needed | A flow to maintain, tokens to store | The CLI already does it |
| JDBC/ODBC drivers | Full SQL feature set | A JVM or native drivers to ship | Too heavy |
| Classic compute and Serverless via Python | Wider coverage | Different APIs and lifecycle | Out of scope for now |

## Consequences

- Positive: no Databricks secret in the application.
- Negative: the CLI must be installed and logged in; SQL needs a Warehouse ID.
- Required: real changes are validated with an authenticated CLI and a test
  workspace ([Testing](../testing.md)).
