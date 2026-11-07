# SQL dialect catalogs

This document describes the catalog integration implemented in Jykre Studio.
The [original proposal](../plans/sql-catalog-proposal.md) also describes a
future standalone repository and publishing pipeline; those sections are not
a claim that those services have been implemented. See
[ADR-0021](../adr/0021-sql-catalogs-read-at-run-time.md) for the recorded rationale
and its status.

## Data layout

`catalog/<dialect>/` contains `manifest.json`, `keywords.jsonl`,
`functions.jsonl` and `types.jsonl`, plus applicable license notices. The current
dialects are `postgresql`, `mysql` and `databricks`.

The manifest records the schema version, version scheme, tracked versions,
verification boundary and sources. Entries carry their validity ranges and
source references. The application currently resolves the latest tracked
version of each dialect. The format and version-resolution rules are described
in [the proposal](../plans/sql-catalog-proposal.md).

## Loading

At startup, `src/app/sql_editor/` requests catalog files through
`backend::read_text_files`, off the UI thread, with a 32 MB limit per file.
The first directory containing a catalog is used:

1. `JYKRE_SQL_CATALOG_DIR`;
2. `catalog` next to the executable;
3. the repository's `catalog` in development builds.

`crates/sql-language/src/catalog.rs` parses the data. Missing or invalid
catalogs retain the built-in fallback for the affected dialect and never
prevent startup. Settings reports which catalogs were loaded. Loading and
building the application do not download catalogs; release packages ship the
data directory beside the executable.

## Generation and validation

`xtask/src/catalogs.rs` implements `cargo xtask catalogs` generation and
validation. Generation is an explicit, separately reviewed operation using
upstream sources or the gate's database fixtures. The basic gate runs
`cargo xtask catalogs check` offline; it checks schema version, canonical JSON,
sorting, source references, version ordering and overlapping validity ranges.
See [Testing](../standards/testing.md#sql-catalogs) for regeneration commands,
source caching and test coverage.

## Scope boundaries

A standalone repository, its publication schedule, generated release snapshots
and optional SQLite distribution remain part of the proposal. This reference
does not approve them or change the catalog format, driver contracts,
dependencies or any ADR status.
