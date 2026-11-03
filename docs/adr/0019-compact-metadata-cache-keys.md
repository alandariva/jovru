# ADR-0019: Compact keys and a single copy of each path in the metadata cache

- **Status:** Proposed
- **Date:** 2026-11-03
- **Authors:** Maintainer, with an AI agent

## Context

MET-022 asked to measure the metadata cache before changing how it stores
paths. A synthetic cache of 500,000 tables, one database and 100 schemas,
built with the application's encoding, measures 775 MB. Most of it is the
same path in five places:

| Column or index | Bytes |
| --- | ---: |
| `path_key` (hex-encoded JSON segments) | 111 MB |
| `parent_key` (the same, for the parent) | 73.5 MB |
| `path_json` | 57 MB |
| The path inside `object_json` | about 57 MB of its 111 MB |
| The primary key index and `objects_parent` repeat `path_key` and `parent_key` | about 300 MB |

The step 2 indexes (`objects_search`, `objects_fts`) add to this. Hex
doubles every byte of the key, so that prefix ranges stay unambiguous.

## Decision (proposed)

A later migration step would:

1. Encode keys with an escaping that keeps byte order and prefix ranges (for
   example `\x00` as the separator, with an escape for the separator and the
   escape byte inside names) instead of hex, about halving every key.
2. Drop `path_json`, since `object_json` already holds the path.
3. Write `object_json` without the path and rebuild the path from the key
   when reading.

The estimated gain is 40 to 50 percent of the file. The change touches
every prefix-range query (`retain`, `activate`, `ensure_nodes`), so it needs
its own migration step that rewrites every key, with tests that compare
reads before and after the step.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Keep the layout | No migration | Caches several times larger than their data | The measurement shows the waste is most of the file |
| Integer ids for paths, with a parent id column | Smallest keys | Every read joins to rebuild paths; prefix ranges need a recursive query | Larger change than escaping |
| Compress `object_json` | Smaller rows | CPU on every read; keys and indexes unchanged | Keys and indexes are the larger part |

## Consequences

- Positive: smaller files, faster opening (`quick_check` reads every page)
  and a smaller WAL during refreshes.
- Negative or costs: a migration that rewrites every row of every cache,
  once.
- What becomes required or forbidden: keys are built by one function in
  `jykre-storage`, with tests of their order and prefix ranges.

## How to validate

The size measurement above, repeated after the step, with the paging,
search and refresh tests unchanged.
