# SQL dialect catalogs

This document describes the catalog integration implemented in Jovru.
The [original proposal](../plans/sql-catalog-proposal.md) also describes a
future standalone repository and publishing pipeline; those sections are not
a claim that those services have been implemented. See
D-0021 for the recorded rationale and its status.

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

1. `JOVRU_SQL_CATALOG_DIR`;
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
upstream sources or the gate's database fixtures, and only sources whose
terms allow redistribution (`redistributable` in the manifest):

- PostgreSQL: `kwlist.h`, `pg_proc.dat` and `pg_type.dat` at each release
  tag; descriptions under the PostgreSQL License, kept in
  `LICENSE-postgresql`.
- MySQL: `INFORMATION_SCHEMA.KEYWORDS` and the names (never the text) of the
  help topics of the gate's Docker servers.
- Databricks: Apache Spark's keyword table and function schema, plus optional
  exports from a workspace (some keywords, such as `QUALIFY`, only a
  workspace knows).
- Types the sources do not give are curated. Lists copied from formatters
  (sql-formatter, sqlfluff) are used only to compare, never as a source.

The catalogs are regenerated when a validated database version changes.
Generation needs the network (PostgreSQL, Spark) or Docker (MySQL). The
basic gate runs
`cargo xtask catalogs check` offline; it checks schema version, canonical JSON,
sorting, source references, version ordering and overlapping validity ranges.
See [SQL catalogs](#sql-catalogs) for regeneration commands,
source caching and test coverage. Its tests refuse what the format forbids,
and `the_repository_catalog_parses` reads every dialect. With
`JOVRU_SQL_CATALOG_DIR` pointing to a missing directory the application starts
with the built-in words, and Settings says the catalogs were not found.

### SQL catalogs

`catalog/` holds the SQL dialect catalogs the editor reads at run time
([D-0021](../decisions/sql-catalogs.md#d-0021)). The basic gate only
checks them, offline. Regenerating them is a task of its own, reviewed in
the diff:

```bash
# PostgreSQL: kwlist.h, pg_proc.dat and pg_type.dat of each release tag (network)
cargo xtask catalogs postgresql REL_14_0,REL_15_0,REL_16_0,REL_17_0,REL_18_0 --verified-through 18.6
# MySQL: the gate's MySQL services in Docker (8.4 and 9.7)
cargo xtask catalogs mysql
# Databricks: Apache Spark's keyword table and function schema (network),
# optionally with exports from a workspace
cargo xtask catalogs databricks v4.0.1 [--keywords sql_keywords.csv] [--functions functions.csv]
```

Downloads are kept in `target/catalog-sources/`, so a second run works
offline. Each generation checks the dialect it wrote.

## Scope boundaries

A standalone repository, its publication schedule, generated release snapshots
and optional SQLite distribution remain part of the proposal. This reference
does not approve them or change the catalog format, driver contracts,
dependencies or the status of any decision.

## Decisions

The reasoning behind this area's structural choices is in [Decisions: SQL dialect catalogs](../decisions/sql-catalogs.md).
