# ADR-0021: SQL dialect catalogs read at run time from a data directory

- **Status:** Proposed
- **Date:** 2026-11-04
- **Authors:** AI agent, at the maintainer's request (plan approved 2026-11-04)

## Context

The editor's highlighting, completion and quoting need what each dialect has
built in: keywords and whether they are reserved, built-in functions with
their signatures, and data types. Until now the tokenizer knew 33 keywords
and 12 functions, the same for every dialect.

The data changes with database releases, not with the application.
[The catalog specification](../plans/sql-catalog-proposal.md) defines it as
text files, one JSON line per entry, with the range of versions each entry is
valid for, so that a new release only adds or closes the lines that changed.
The data is meant to live, later, in a repository of its own that other tools
can use. For now it lives in this repository and must behave as if it came
from outside.

## Decision

1. **The catalogs live in `catalog/`**, laid out as the specification says:
   `catalog/<dialect>/manifest.json` plus `keywords.jsonl`,
   `functions.jsonl` and `types.jsonl`, for `postgresql`, `mysql` and
   `databricks`.
2. **The application reads them at run time, not at build time.** It reads
   from the first directory that holds a catalog among:
   - `JYKRE_SQL_CATALOG_DIR`;
   - `catalog` next to the executable;
   - in a development build, the repository's `catalog`.

   The read happens once at start, on the runtime (`backend::read_text_files`,
   limited to 32 MB per file); `jykre_sql_language::catalog` parses the
   files. A dialect whose files are missing or invalid, or every dialect when
   no directory is found, keeps the built-in words the editor knew before. A
   catalog never stops the application.
3. **The release packages ship `catalog/` next to the executable**
   (`.github/workflows/release.yml`).
4. **`cargo xtask catalogs` generates and checks the data.**
   - The generator reads only sources whose terms allow it:
     - PostgreSQL: `kwlist.h`, `pg_proc.dat` and `pg_type.dat` at each
       release tag; descriptions under the PostgreSQL License, kept in
       `LICENSE-postgresql`.
     - MySQL: `INFORMATION_SCHEMA.KEYWORDS` and the names (never the text) of
       the help topics of the gate's Docker servers.
     - Databricks: Apache Spark's keyword table and function schema, plus
       optional exports from a workspace.
     - Types the sources do not give are curated.
   - `cargo xtask catalogs check` validates the format (canonical JSON,
     order, sources, versions, no overlapping validity) and runs in the basic
     gate.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Embed the files with `include_str!` | No I/O at start, nothing to package | Changing the data needs a rebuild; does not behave like external data | The maintainer chose reading at run time |
| Query each server for its keywords and functions | Exact for the server's version | Needs a connection; MySQL has no catalog of built-in signatures; network on the interactive path | Catalogs must work offline and before connecting |
| Copy lists from formatters (sql-formatter, sqlfluff) | Ready | Hand-made, behind current releases, no versions | Used only to compare, not as a source |

## Consequences

- **Positive:**
  - Keywords, functions and types follow each dialect and version.
  - The data can move to its own repository without changing the
    application; pointing `JYKRE_SQL_CATALOG_DIR` elsewhere already
    simulates that.
- **Negative or costs:**
  - The package carries a directory next to the executable.
  - The catalogs are read once at start (off the UI thread).
  - Generating needs the network (PostgreSQL, Spark) or Docker (MySQL).
  - The Databricks keywords that only a workspace knows (such as `QUALIFY`)
    need an export from one.
- **What becomes required or forbidden:**
  - Catalog text only from sources whose license allows it
    (`redistributable`).
  - The catalogs are regenerated when a validated database version changes.

## How to validate

- `cargo xtask catalogs check` passes, and its tests refuse what the format
  forbids.
- The `the_repository_catalog_parses` test reads every dialect.
- With `JYKRE_SQL_CATALOG_DIR` pointing to a missing directory the
  application starts with the built-in words, and Settings says the catalogs
  were not found.
