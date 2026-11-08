# ADR-0003: SQLite for local storage, with one metadata file per connection

- **Status:** Accepted (recorded 2026-11-02 for a decision already in effect)
- **Date:** 2026-11-02

## Context

The application stores connection profiles and preferences, and caches
metadata that can reach hundreds of thousands of objects per connection. The
explorer and search must work offline and quickly, and a failed refresh must
never lose the previous read.

## Decision

- `config.db` holds profiles and preferences; each connection's metadata
  lives in its own `metadata/<connection-id>.db`.
- Access goes through repositories in `jykre-storage`, on SQLx's SQLite
  driver, with WAL, foreign keys, a busy timeout and a `quick_check` on open.
- Metadata refreshes stage objects and publish a scope in one transaction.
- Details: [Persistence](../reference/persistence.md).

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| JSON files | Trivial | No transactions, no paged reads, all in memory | Earlier builds used `connections.json`; it was migrated away |
| One database for everything | One file | A corrupted or huge cache would endanger profiles | Separate files isolate rebuildable data |
| An embedded key-value store | Fast | No SQL for paging and search | SQLite is enough and already in the stack |

## Consequences

- Positive: transactions, paged and indexed reads, offline browsing, a
  corrupted cache can be set aside without touching profiles.
- Negative: schema changes need migrations ([Persistence](../reference/persistence.md#migrations)).
- Required: no secrets and no query results in these files.
