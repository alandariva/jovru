# Persistence: metadata cache

Part of [Persistence](README.md).

## Schema: `metadata/<connection-id>.db`

| Table | Purpose |
| --- | --- |
| `metadata_info` | Single row: identity of the connection configuration the cache belongs to, and the cache version |
| `objects` | Published objects. `path_key` is the only copy of the structural path: each segment's JSON with `%` and `/` escaped, joined by `/`, so dots, slashes, wildcards and case cannot collide and a node's descendants are one key range (D-0019). `object_json` holds the object without its path, which reads rebuild from the key. `parent_key` plus the `objects_parent` index serve paged child reads |
| `refresh_runs` | Refresh attempts; a partial unique index allows only one `running` run |
| `staging_objects` | Objects of a run in progress |
| `protected_scopes` | Subtrees the run could not read (permission warnings) and must keep |
| `scope_status` | Per-scope state, last attempt, last success, version and message |

Keys: a node's descendants are exactly the keys from `key/` up to `key0`
(`0` follows `/`), so the prefix-range queries of `retain` and `activate` need
no escaping beyond the two characters. One function in `jovru-storage`
(`key`) builds keys and one (`path_of`) reads them back; no code reads a path
from `object_json`.

The cache records the identity of the configuration it was read from.
`MetadataRepository::prepare` reconciles it after the user saves a changed
profile; background readers use `open`, which never replaces another
configuration's identity.

Each connection's cache stays open while the application runs
(`backend` keeps one `MetadataRepository` per connection), so expanding,
searching and refreshing do not reopen the file. It is opened, and checked
with `quick_check`, at start, after the profile's identity changed and after
a failed read.

A cache is keyed by the driver's identity of the catalog
(`Driver::cache_identity`, D-0017),
not by the whole profile: changing compute, defaults or TLS keeps it. A
cache an earlier version keyed by the whole profile is adopted under the
narrower identity without being emptied (`MetadataRepository::adopt`).

The search compares `search_name` with the query in Unicode lower case,
filters by kind in SQL before the limit and reads the ancestors of the
matches in one query. A query of three characters or more matches through the
FTS5 trigram table `objects_fts`; a shorter one, which a trigram cannot
match, scans the covering index `objects_search` (`instr`). Search stays
inside its 50 ms budget at 500,000 objects
([Performance](../../standards/performance.md)).

Children are read a page at a time with a cursor (`PageCursor`), in a fixed
order of kinds (namespaces, tables, views, materialized views, columns,
foreign keys, partitions) and then by ordinal, name and key. Every query
walks the `objects_parent` index from the cursor, so a page costs the same
at any depth. `pages` reads several pages and the cache version from one
snapshot.

## Atomic metadata refresh

Two OS lock files guard each cache. The **run lock** (`<id>.run.lock`) is
held for a whole refresh, so owning it proves that no live refresh owns a
`running` run. The **writer lock** (`<id>.lock`) is held only while
publishing, retaining or resetting the cache, so a refresh that is still
reading the server does not block those; each waits up to five seconds for
the other holder.

1. `begin(scope)` takes the run lock (waiting up to five seconds for a
   refresh that is finishing) and inserts a `running` run.
2. The driver streams batches; `batch` writes them to `staging_objects`.
3. On success, `activate` takes the writer lock and replaces the objects of
   that scope with the staged ones **in one transaction**, keeps protected
   subtrees, bumps the cache version and records the scope status. A read
   that started before the configuration was reset, or before another
   publication, is refused as stale.
4. On failure or cancellation, `fail` records the error and discards the
   staged objects. The published objects are untouched.
5. When a cache is opened, `recover_interrupted` takes the run lock, marks
   runs left `running` by a crash as interrupted and removes them; the
   previous read stays.

The scope status is read as `jovru_storage::RefreshState`; the stored text
(`running`, `success`, `partial`, `warning`, `failed`, `cancelled`,
`interrupted`) is unchanged, and a value this version does not know reads as
`Unknown`.

A scope's ancestors (its database or catalog) are outside the scope, so the
driver does not emit them; `activate` adds the missing ones when the scope
published anything. Reading a choice publishes each last-level namespace on
its own. After a complete read, `retain(subtrees, nodes)` keeps only the
objects inside the chosen subtrees, the named namespaces and their ancestors,
removes everything else with its scope status in one transaction and bumps
the version. It waits up to five seconds for a publication that is finishing
to release the writer lock.

Readers that page through the cache compare the version before and after; if
a refresh published in between, they stop and ask the user to expand the node
again instead of mixing two reads.
