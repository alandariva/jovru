# Architecture decision records

An ADR records a structural decision: its context, the decision, the
alternatives and the consequences. It exists so nobody has to guess **why**
something is the way it is, and so a decision is not undone by accident.

## Rules

- A new ADR is a new file `NNNN-short-title.md` based on the
  [template](0000-template.md), written on its own or with the code that motivates
  it.
- Statuses: **Proposed** → **Accepted** (approved by the maintainer and
  committed) → **Superseded by ADR-NNNN** or **Deprecated**.
- Agents may write ADRs as *Proposed*; only the maintainer changes the status
  to *Accepted*.
- An accepted ADR is not rewritten. To change a decision, write a new ADR and
  mark the old one as superseded.
- ADRs 0001 to 0006 were recorded on 2026-11-02 for decisions already in
  effect in the code. Their *Alternatives considered* were reconstructed when
  recording them; the maintainer may correct them.

## Index

| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-native-ui-with-gpui.md) | Native UI with GPUI through `gpui-kit` | Accepted |
| [0002](0002-sqlx-for-postgres-and-mysql.md) | SQLx for PostgreSQL and MySQL, with rustls and the text protocol | Accepted |
| [0003](0003-sqlite-for-local-storage.md) | SQLite for local storage, with one metadata file per connection | Accepted |
| [0004](0004-os-vault-for-credentials.md) | Operating system vault for credentials, with no file fallback | Accepted |
| [0005](0005-databricks-through-cli-and-rest.md) | Databricks through CLI profiles and REST APIs | Accepted |
| [0006](0006-layered-crates-independent-of-the-ui.md) | Layered crates independent of the UI, with a static driver registry | Accepted |
| [0007](0007-application-logic-outside-views.md) | Application logic outside views | Accepted (implemented) |
| [0008](0008-compact-result-storage.md) | Compact result storage with a configurable row limit and a memory ceiling | Accepted (implemented) |
| [0009](0009-structured-logging-and-timings.md) | Structured logging and timings with `tracing` | Accepted (implemented) |
| [0010](0010-benchmarks-with-criterion.md) | Benchmarks with `criterion` for hot paths | Accepted (implemented) |
| [0011](0011-workspace-lints.md) | Workspace-wide lints for panics, output and async locks | Accepted (implemented) |
| [0012](0012-result-grid-as-a-cached-entity.md) | Result grid as its own cached entity | Accepted (implemented) |
| [0013](0013-thin-ancestors-of-frequently-notified-views.md) | Thin ancestors for frequently notified views | Accepted (not implemented) |
| [0014](0014-a-session-per-tab.md) | A session per tab, kept between runs and after errors | Accepted (implemented; amended for report tabs by 0025) |
| [0015](0015-introspect-only-what-the-user-chooses.md) | Introspect only what the user chooses | Accepted (implemented) |
| [0016](0016-keys-defaults-and-relation-kinds.md) | Keys, defaults and relation kinds in the metadata model | Accepted (implemented) |
| [0017](0017-driver-metadata-contract-batch.md) | Cache identity, reading levels and on-demand inspection in the driver contract | Accepted (implemented) |
| [0018](0018-metadata-cache-migrations.md) | Numbered migrations for the metadata cache, starting with search names and publication times | Accepted (implemented) |
| [0019](0019-compact-metadata-cache-keys.md) | Compact keys and a single copy of each path in the metadata cache | Accepted |
| [0020](0020-editor-language-features-in-their-own-crates.md) | Editor language features in their own crates, apart from query execution | Accepted (implemented) |
| [0021](0021-sql-catalogs-read-at-run-time.md) | SQL dialect catalogs read at run time from a data directory | Accepted (implemented) |
| [0022](0022-result-pages-shared-across-threads.md) | Result pages shared across threads with `Arc` (supersedes in part 0012) | Accepted (implemented) |
| [0023](0023-named-parameters-bound-by-drivers.md) | Named parameters bound by the drivers (amends 0002 for report SQL) | Accepted (implemented) |
| [0024](0024-reports.md) | Reports, with stored definitions, tabs of their own and a print HTML the user saves | Accepted (implemented; amended by 0025) |
| [0025](0025-report-panels.md) | Report panels, each with its connection, database, SQL and chart, run on a session per connection and database (amends 0024 and, for reports, 0014) | Accepted (implemented) |
| [0026](0026-named-parameters-in-sql-tabs.md) | Named parameters in SQL tabs, and typed literals on classic Databricks clusters (amends 0023 and 0005) | Accepted (implemented) |
| [0027](0027-sqlite-driver.md) | SQLite database files through SQLx's bundled SQLite | Accepted (implemented; amended by 0030) |
| [0028](0028-agentic-ai-assistant-over-the-metadata-cache.md) | An agentic AI assistant over the metadata cache, with every request reviewable | Accepted (implemented) |
| [0029](0029-otlp-export-of-logs-and-traces.md) | Optional export of logs and traces over OTLP, with metadata only (amends 0009) | Accepted (implemented) |
| [0030](0030-transaction-state-through-a-pinned-sqlx-fork.md) | Transaction state read from the protocol through a pinned SQLx fork (amends 0002 and 0027) | Accepted (implemented) |
| [0031](0031-saved-queries-as-sql-files.md) | Saved queries as plain `.sql` files named by their title, with tags and associations kept locally | Accepted (implemented) |
| [0032](0032-session-encryption-reported-by-drivers.md) | Session encryption reported by the drivers (amends the `Session` contract) | Accepted (implemented) |
| [0033](0033-automation-database-service-and-lock.md) | Automation in `automation.db`, run by an application-root service under a data-folder lock | Accepted (not implemented) |
| [0034](0034-persisted-declared-outputs.md) | Declared scalar outputs persisted for chains, small, opt-in and short-lived | Accepted (not implemented) |
