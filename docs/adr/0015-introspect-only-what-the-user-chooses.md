# ADR-0015: Introspect only what the user chooses

- **Status:** Accepted (by the maintainer on 2026-11-07; implemented)
- **Date:** 2026-11-03
- **Authors:** Alan de Oliveira (maintainer), with an AI agent

## Context

Refreshing a connection read everything it could see: every schema of a
PostgreSQL database, every MySQL database and every Unity Catalog catalog,
with all columns, as one atomic read. A server with hundreds of schemas, of
which the user needs two, paid the full cost on every refresh, and a large
Databricks workspace could exceed the 300-second limit and publish nothing.
Saving a new connection started that full read without asking.

## Decision

- A connection introspects **nothing until the user chooses**. The choice is
  made in the connection dialog, from a list of namespaces (databases,
  catalogs, schemas) read on demand, one level at a time, and can be changed
  later. A connection can be saved without a choice (for example offline);
  it then stays pending and the explorer offers to choose.
- The choice is a set of rules stored as the setting
  `connection.<id>.metadata_scope`: *everything*, a namespace with all that is
  created inside it later (a database with schemas, a catalog), or a
  namespace of the last level.
- `Driver::discover_namespaces` lists the namespaces below a parent without
  reading relations or columns. `DriverCapabilities::namespace_levels` derives
  the namespace levels from the descriptor's hierarchy, so the application
  never decides by `DriverKind`.
- A refresh of the choice lists the broader rules, then reads and publishes
  **each last-level namespace on its own**, in order. A failure or a
  cancellation keeps the namespaces already published and the previous cache
  of the others. The UI shows how many were read and an estimate of the time
  left, from a moving average of the time per namespace.
- After a complete refresh, `MetadataRepository::retain` removes what the
  choice no longer covers, and the namespaces a broad rule no longer finds on
  the server. Changing the choice reads only what it added.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Keep the full read and filter only the explorer | No contract change | Cost and failures stay; a filter hides, it does not save work | The problem is the remote work, not the view |
| Pre-select a default (current schema, default catalog) and read it right away | One step less | Reads before the user decides; a wrong default still costs a read | The maintainer chose an explicit choice for every driver |
| One atomic publication for the whole choice | Simplest progress model | One failure loses everything; no partial progress | Large workspaces would keep failing as before |
| Weight the estimate by table counts | More accurate with uneven schemas | Extra queries; costly in Unity Catalog | The maintainer chose the estimate by namespace |

## Consequences

- Positive: refresh cost follows what the user works with; large Databricks
  workspaces publish progressively and each request has its own limit;
  progress and time left are visible.
- Negative or costs: one more core trait method (with a default) and a
  repository operation; namespaces are read in sequence, so the total time
  of *everything* is not shorter than before.
- What becomes required: drivers implement `discover_namespaces`; consumers
  derive levels from the descriptor; a refresh of the whole connection means a
  refresh of the choice.

## How to validate

- `backend` tests read only the chosen namespaces, report progress for each,
  keep a failed namespace's cache, remove nothing after a cancellation and
  drop what the choice no longer covers, against a two-level fake driver.
- The Databricks HTTP test lists catalogs and schemas without calling
  `/tables`; the live tests list the PostgreSQL database and schemas and the
  MySQL databases.
- Manually: a PostgreSQL connection with several schemas, choosing two, reads
  only those two and shows the bar and the time left.
