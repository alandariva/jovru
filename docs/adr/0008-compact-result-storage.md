# ADR-0008: Compact result storage with a configurable row limit and a memory ceiling

- **Status:** Accepted (implemented on 2026-11-02)
- **Date:** 2026-11-02

## Context

Results are kept as `Vec<Vec<String>>` plus a `HashSet<(usize, usize)>` of
NULL positions. Each cell is a separate allocation, and each NULL lookup
hashes. With the fixed 1,000-row limit this is cheap, but users need larger
results, and the number of rows says little about memory: one wide text
column can outweigh many narrow ones.

## Decision

- A result is stored compactly: one UTF-8 buffer for all cell text, a vector
  of cell offsets and a NULL bitmap. Column metadata is stored once.
- The row limit is configurable per connection, up to 100,000 rows.
- A memory ceiling per result (default 256 MB) stops reading even below the
  row limit. The footer says which limit applied.
- Building the compact result and dropping driver cells happen on the Tokio
  runtime; dropping a replaced result happens off the UI thread.
- A new run in a tab releases the tab's previous result; closing the tab
  releases everything.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Keep `Vec<Vec<String>>` | Simple | One allocation per cell, no exact memory accounting | Does not scale to 100,000 wide rows |
| Apache Arrow | Columnar, fast | A large dependency for text cells | Too heavy for what is needed |
| Spill to disk | Unlimited size | Disk I/O, results on disk | Results are never written to disk |

## Consequences

- Positive: fewer allocations, an exact memory figure for the ceiling, cheap
  NULL checks.
- Negative: cells are read through an accessor instead of `&rows[r][c]`; copy
  and filter code changes.
- Required: the grid and copy code use the accessor; exact values are never
  altered by the representation.

## How to validate

A unit test checks values, NULLs, empty text and Unicode round-trip through
the representation. Measure memory and frame time at 100,000 × 30 columns in
a release build; frame budgets in [Performance](../standards/performance.md#budgets)
must hold.

## Implementation notes

Implemented on 2026-11-02 (`jykre::result`).

- The per-connection row limit is stored in `config.db` as the setting
  `connection.<connection-id>.row_limit`, in the existing `settings` table, so
  no schema migration was needed. A missing or out-of-range value reads as the
  default (1,000).
- To tell a result that has exactly the row limit from one that has more, the
  backend asks the driver for one row beyond the limit; that extra row is not
  kept, and only its arrival marks the limit as reached.
- A new run releases the tab's previous result when it starts, so a cancelled
  run leaves the grid empty.
- Measured: a 100,000 × 30 result takes about 38 MB (the previous
  representation was estimated at about 102 MB).
