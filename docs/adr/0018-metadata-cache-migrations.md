# ADR-0018: Numbered migrations for the metadata cache, starting with search names and publication times

- **Status:** Proposed
- **Date:** 2026-11-03
- **Authors:** Maintainer, with an AI agent

## Context

[Persistence](../persistence.md#migrations) sets the standard for schema
changes: numbered, forward-only steps applied in one transaction that also
sets `PRAGMA user_version`. Its *Known deviations* record that only version 1
exists and that the first schema change must introduce the mechanism.

Two items of the metadata backlog need the cache schema to change:

- **MET-014.** The search reads `instr(lower(name), lower(?))`. SQLite's
  `lower` folds ASCII only, so `ação` does not find `AÇÃO`. It also decodes
  every object's JSON to filter the palette's tables after the limit.
- **MET-017.** A partial refresh (some parts without permission) publishes,
  but only a complete one sets `last_success_at`. The explorer then says
  "Last successful refresh: none" about data that was just read.

MET-022 (the path stored three times) may need a later step. This ADR fixes
the mechanism for it as well.

## Decision

1. The metadata file gets a list of numbered steps in `jykre-storage`.
   Opening a file runs the steps between its `user_version` and the
   application's, in order, in one transaction that ends by setting
   `user_version`. A file newer than the application is refused, as today. A
   failed step leaves the file at its previous version. The cache is not
   backed up: it can be rebuilt. Every step has a test that migrates a file
   of the previous version.
2. **Step 2** adds `search_name TEXT` to `objects` and to `staging_objects`.
   - The value is the object's name lower-cased by Rust's Unicode case
     mapping, written by the application whenever an object is staged, and
     filled for existing rows by the step itself.
   - It also adds `last_published_at TEXT` to `scope_status`, set by every
     publication, partial or complete.
   - `last_success_at` keeps meaning a complete read.
3. The search compares `search_name` with the query lower-cased the same
   way, filters by kind in SQL, and reads the ancestors of the matches in one
   query. The benchmark measured a scan at 235 ms (and 112 ms over a covering
   index `objects_search`) at 500,000 objects, above the 50 ms budget, so step
   2 also creates an FTS5 table with the trigram tokenizer (`objects_fts`,
   external content, kept in step by triggers): a query of three characters
   or more matches through it in about 3.5 ms. A shorter one, which a trigram
   cannot match, scans `objects_search` (about 114 ms at that size).
4. The configuration file `config.db` keeps its own version and is not
   touched by this ADR.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Recreate the cache on a schema change | No migration code | Every upgrade rereads every chosen namespace from the server | The standard asks for migrations, and rereading large catalogs is slow |
| A SQLite function registered for Unicode folding | No stored column | SQLx exposes no portable way to register functions; the index cannot use it | A stored column is simple and indexable |
| FTS5 trigram index now | Substring search without a scan | Larger file, more writes per refresh; not needed if the scan meets the budget | Measured first, adopted if needed |

## Consequences

- Positive: Unicode search; the palette finds tables past columns of the same
  name; a partial refresh shows when it published.
- Negative or costs: the first open after the upgrade fills `search_name` for
  every cached object, once.
- What becomes required or forbidden: a released step is never edited; a
  change to the metadata schema is a new step with its test.

## How to validate

- A test migrates a version-1 file with objects and statuses and checks the
  new columns.
- The search test finds `AÇÃO` with `ação` and finds a table behind 200
  columns of the same name.
- The metadata benchmark reports the search time at 500,000 objects.
