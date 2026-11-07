# ADR-0005: Databricks through CLI profiles and REST APIs

- **Status:** Accepted (recorded 2026-11-02 for a decision already in effect)
- **Date:** 2026-11-02
- **Amended:** 2026-11-03, with maintainer approval to extend execution to
  classic all-purpose clusters and update this ADR in place.

## Context

Databricks authentication (OAuth, tokens, profiles) is already handled by the
Databricks CLI on the user's machine. Implementing OAuth inside the
application would add a flow to maintain and tokens to store.

## Decision

- A connection names an authenticated CLI profile. The CLI resolves the token
  when needed; the token stays in memory and is never stored by Jykre.
  Authentication and HTTP pools are shared by profile, resolved CLI config
  file, CLI executable and workspace origin. Refreshes are serialized; reuse
  lasts at most five minutes or until 30 seconds before the CLI token expires.
  A 401 forces one CLI refresh and one retry, coordinated by token generation.
- SQL runs through the Statement Execution API on a SQL Warehouse (INLINE,
  JSON_ARRAY, polling), or Command Execution API 1.2 on a running classic
  all-purpose cluster, according to the connection's explicit compute type.
  Classic sessions keep one SQL execution context per tab, wait until it is
  ready and destroy it on close. Cancellation is confirmed remotely; a context
  that cannot be recovered is discarded. User SQL is never rewritten/replayed.
- The connection dialog discovers resources through warehouse/cluster REST
  listing APIs on an explicit **Load compute** action. A searchable, virtualized
  select saves the stable ID and display name; reopening shows the saved
  selection immediately without network I/O. Missing resources stay visible as
  unavailable until the user changes the selection.
- The CLI profile is also a searchable select, populated on **Load CLI profiles**
  using `databricks auth profiles --skip-validate --output json`. Account-console
  hosts are excluded; workspace profiles may carry account and workspace IDs.
  New forms prefer the CLI's declared default; a saved/explicit selection is
  never replaced by that preference. Profile discovery does not acquire tokens or authenticate every
  profile; test/query authenticates the selected one. Reopening keeps the saved
  profile even when it cannot be listed. Profile changes clear compute selection.
- Metadata uses Unity Catalog REST without starting compute. Connection testing
  checks configured compute access/state without submitting SQL. Metadata-only
  connections still need no compute ID. Jobs clusters and serverless non-warehouse
  compute are excluded; classic clusters are never started automatically.
- Classic table results preserve numeric wire text, NULL and structured cells.
  Only the bounded rows returned by the API are locally paged, without rerunning
  SQL. Server truncation is reported; no remote pagination is invented.
- HTTPS only, redirects disabled, external result links not followed.
- The CLI runs without a shell, with a timeout and `kill_on_drop`.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| OAuth inside the application | No CLI needed | A flow to maintain, tokens to store | The CLI already does it |
| JDBC/ODBC drivers | Full SQL feature set | A JVM or native drivers to ship | Too heavy |
| Classic all-purpose compute via Command Execution | Reuses CLI/REST stack and SQL contexts | API result caps, distinct lifecycle | Included in the 2026-11-03 amendment |
| Serverless via Python, jobs clusters | Wider coverage | Additional runtime and lifecycle | Out of scope |

## Consequences

- Positive: no persisted Databricks secret in the application; tabs reuse tokens
  and HTTP pools while keeping independent execution contexts.
- Negative: the CLI must be installed and logged in; SQL needs a Warehouse ID or
  a running classic all-purpose Cluster ID. The classic API can truncate results.
- Required: real changes are validated with an authenticated CLI and a test
  workspace ([Testing](../standards/testing.md)).
