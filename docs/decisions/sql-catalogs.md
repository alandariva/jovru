# Decisions: SQL dialect catalogs

Why this area is built as it is. Each entry records what was decided, why, and which alternatives lost. The rules themselves are in [SQL dialect catalogs](../reference/sql-catalogs.md). Entries are numbered `D-NNNN`, and the numbers are stable.

<a id="d-0021"></a>
## D-0021: SQL catalogs read at run time from a data directory

*Accepted 2026-11-07 (plan approved 2026-11-04) · implemented*

**Decision.** The dialect catalogs live in `catalog/` and the application reads them at run time, not at build time, from the first directory that holds one ([Loading](../reference/sql-catalogs.md#loading)). Release packages ship `catalog/` next to the executable, and `cargo xtask catalogs` generates and checks the data ([Generation and validation](../reference/sql-catalogs.md#generation-and-validation)).
**Why.** The editor's highlighting, completion and quoting need what each dialect has built in (keywords and whether they are reserved, functions with their signatures, types), where the tokenizer used to know 33 keywords and 12 functions, the same for every dialect. That data changes with database releases, not with the application, and is meant to live later in a repository of its own that other tools can use; for now it is in this repository and behaves as if it came from outside (pointing `JOVRU_SQL_CATALOG_DIR` elsewhere already simulates that). Costs: a directory next to the executable, a read at start (off the UI thread), and generation that needs the network (PostgreSQL, Spark) or Docker (MySQL). Databricks keywords that only a workspace knows (such as `QUALIFY`) need an export from one.
**Rejected.** Embedding the files with `include_str!`: no I/O at start and nothing to package, but changing the data needs a rebuild and it does not behave like external data; the maintainer chose reading at run time. Querying each server for keywords and functions: exact for the server's version, but it needs a connection, MySQL has no catalog of built-in signatures, and it puts the network on the interactive path; catalogs must work offline and before connecting. Copying lists from formatters (sql-formatter, sqlfluff): hand-made, behind current releases and unversioned, so used only to compare.
