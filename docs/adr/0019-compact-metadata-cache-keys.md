# ADR-0019: Compact keys and a single copy of each path in the metadata cache

- **Status:** Accepted
- **Date:** 2026-11-03
- **Authors:** Maintainer, with an AI agent

## Context

MET-022 asked to measure the metadata cache before changing how it stores
paths. A first synthetic cache of 500,000 tables, one database and 100
schemas, built with the application's encoding before step 2, measured
775 MB. Most of it was the same path in five places:

| Column or index | Bytes |
| --- | ---: |
| `path_key` (hex-encoded JSON segments) | 111 MB |
| `parent_key` (the same, for the parent) | 73.5 MB |
| `path_json` | 57 MB |
| The path inside `object_json` | about 57 MB of its 111 MB |
| The primary key index and `objects_parent` repeat `path_key` and `parent_key` | about 300 MB |

The step 2 indexes (`objects_search`, `objects_fts`) repeat `path_key` once
more: the same cache at version 2 takes 892 MB after a `VACUUM`, and 1.5 GB as
a full refresh leaves it (the staging rows it copies from leave their pages
free). Hex doubles every byte of the key, so that prefix ranges stay
unambiguous.

## Decision

Step 3 of the metadata migrations
([ADR-0018](0018-metadata-cache-migrations.md)):

1. Keys are each segment's JSON with `%` written `%25` and `/` written `%2F`,
   joined by `/`, instead of hex: about half the size. No segment holds the
   separator, so a node's descendants are still exactly the keys from
   `key/` up to `key0` ('0' follows '/'), and the prefix-range queries
   (`retain`, `activate`) are unchanged. One function in `jykre-storage`
   builds keys and one reads them back, with tests of their round trip,
   order and ranges.
2. `path_json` is dropped from `objects` and `staging_objects`.
3. `object_json` is written without the path, and reads rebuild the path
   from the key.

The step rewrites every key of `objects` and `scope_status` from the stored
JSON paths, drops the refreshes a closed application left running (marking
their scopes interrupted, as startup recovery does) and builds the secondary
indexes again after the rewrite. Migration steps run on a connection of their
own with foreign keys off, as SQLite advises for schema changes, and fail
when `PRAGMA foreign_key_check` finds a broken reference, leaving the file at
its previous version. After the step the file is vacuumed once, to give the
freed half back.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Keep the layout | No migration | Caches several times larger than their data | The measurement shows the waste is most of the file |
| `\x00` as the separator, with escapes | Keys slightly shorter | Every prefix-range query changes; NUL in TEXT is fragile across tools | Escaping two characters keeps the separator and the queries |
| Integer ids for paths, with a parent id column | Smallest keys | Every read joins to rebuild paths; prefix ranges need a recursive query | Larger change than escaping |
| Compress `object_json` | Smaller rows | CPU on every read; keys and indexes unchanged | Keys and indexes are the larger part |
| Rewrite rows with foreign keys on | No pragma change | 89 s for 500,000 objects: every key update checks the self-reference | 18–20 s with foreign keys off and a check before committing |

## Consequences

- Positive: the 500,000-object cache goes from 892 MB to 461 MB vacuumed
  (−48%), and from 1.5 GB to 753 MB as a refresh leaves it (−50%). Two-character
  search drops from 114 ms to 93 ms over the smaller index.
- Negative or costs:
  - The first open after the upgrade rewrites and vacuums every cache once,
    off the UI thread: about 20 s at 500,000 objects, well under a second for
    a few thousand.
  - Reads decode paths from keys: a first page of 200 children went from
    about 0.44 ms to 0.65 ms, and a search of three characters or more from
    3.5 ms to 7.4 ms, both far inside their 50 ms budgets.
- What becomes required or forbidden: keys are built only by `key` and read
  only by `path_of`; no code reads a path from `object_json`.

## How to validate

- `keys_round_trip_and_a_node_s_range_holds_its_descendants_only`: names
  with `/`, `%`, `%2F`, `0` and accents round-trip, and a node's range holds
  its descendants only.
- `a_version_2_cache_compacts_its_keys_and_reads_back_the_same`: a version-2
  file reads back the same objects, pages, search, relations and statuses,
  without `path_json`, with its indexes, vacuumed, and later refreshes and
  `retain` find their ranges.
- `a_step_that_breaks_a_reference_leaves_the_file_as_it_was`.
- The metadata benchmark and the size of a 500,000-object cache, as above
  (release build, 2026-11-03).
