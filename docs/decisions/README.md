# Decisions

The structural choices behind Jovru: what was decided, why, and which alternatives
lost. There is one file per area, named after the standard or reference document
whose rules it explains. Decisions are numbered (`D-0033`) and the numbers are
stable; code comments, plans and research cite them, and every entry has an anchor
named after its number (`#d-0033`).

A decision that is proposed but not yet accepted is an ADR in `../adr/` until it is.
Once accepted it is folded into the file of its area and the proposal is deleted.
This table finds a decision by its number.

| Decision | Title | File |
| --- | --- | --- |
| [D-0001](architecture.md#d-0001) | Native UI with GPUI through `gpui-kit` | `architecture.md` |
| [D-0002](drivers.md#d-0002) | SQLx for PostgreSQL and MySQL, with rustls and the text protocol | `drivers.md` |
| [D-0003](persistence.md#d-0003) | SQLite for local storage, one metadata file per connection | `persistence.md` |
| [D-0004](security.md#d-0004) | Operating system vault for credentials, with no file fallback | `security.md` |
| [D-0005](drivers.md#d-0005) | Databricks through CLI profiles and REST APIs | `drivers.md` |
| [D-0006](architecture.md#d-0006) | Layered crates independent of the UI, with a static driver registry | `architecture.md` |
| [D-0007](ui-guidelines.md#d-0007) | Application logic outside views | `ui-guidelines.md` |
| [D-0008](results.md#d-0008) | Compact result storage with a configurable row limit and a memory ceiling | `results.md` |
| [D-0009](reliability.md#d-0009) | Structured logging and timings with `tracing` | `reliability.md` |
| [D-0010](performance.md#d-0010) | Benchmarks with `criterion` for hot paths | `performance.md` |
| [D-0011](rust-guidelines.md#d-0011) | Workspace-wide lints for panics, output and async locks | `rust-guidelines.md` |
| [D-0012](results.md#d-0012) | Result grid as its own cached entity | `results.md` |
| [D-0013](ui-guidelines.md#d-0013) | Thin ancestors of frequently notified views | `ui-guidelines.md` |
| [D-0014](architecture.md#d-0014) | A session per tab, kept between runs and after errors | `architecture.md` |
| [D-0015](drivers.md#d-0015) | Introspect only what the user chooses | `drivers.md` |
| [D-0016](drivers.md#d-0016) | Keys, defaults and relation kinds in the metadata model | `drivers.md` |
| [D-0017](drivers.md#d-0017) | Cache identity, reading levels and on-demand inspection in the driver contract | `drivers.md` |
| [D-0018](persistence.md#d-0018) | Numbered migrations for the metadata cache | `persistence.md` |
| [D-0019](persistence.md#d-0019) | Compact keys and one copy of each path in the metadata cache | `persistence.md` |
| [D-0020](architecture.md#d-0020) | Editor language features in their own crates | `architecture.md` |
| [D-0021](sql-catalogs.md#d-0021) | SQL catalogs read at run time from a data directory | `sql-catalogs.md` |
| [D-0022](results.md#d-0022) | Result pages shared across threads with `Arc` | `results.md` |
| [D-0023](drivers.md#d-0023) | Named parameters bound by the drivers | `drivers.md` |
| [D-0024](results.md#d-0024) | Reports, with stored definitions, tabs of their own and a print HTML the user saves | `results.md` |
| [D-0025](results.md#d-0025) | Report panels, each with its connection, database, SQL and chart, run on a session per connection and context | `results.md` |
| [D-0026](drivers.md#d-0026) | Named parameters in SQL tabs, and typed literals on classic Databricks clusters | `drivers.md` |
| [D-0027](drivers.md#d-0027) | SQLite database files through SQLx's bundled SQLite | `drivers.md` |
| [D-0028](assistant.md#d-0028) | An agentic AI assistant over the metadata cache, with every request reviewable | `assistant.md` |
| [D-0029](reliability.md#d-0029) | Optional export of logs and traces over OTLP, with metadata only | `reliability.md` |
| [D-0030](drivers.md#d-0030) | Transaction state read from the protocol through a pinned SQLx fork | `drivers.md` |
| [D-0031](persistence.md#d-0031) | Saved queries as plain `.sql` files, with tags and associations kept locally | `persistence.md` |
| [D-0032](drivers.md#d-0032) | Session encryption reported by the drivers | `drivers.md` |
| [D-0033](security.md#d-0033) | Daily update check and signed self-install from GitHub releases | `security.md` |
| [D-0034](automation.md#d-0034) | Automation in `automation.db`, run by an application-root service under a data-folder lock | `automation.md` |
| [D-0035](automation.md#d-0035) | Declared outputs persisted for chains, small, opt-in and short-lived | `automation.md` |
| [D-0036](automation.md#d-0036) | Published job versions and chain runs | `automation.md` |
| [D-0037](automation.md#d-0037) | External programs' logs kept on disk for 7 days | `automation.md` |
| [D-0038](automation.md#d-0038) | External program runs in `automation.db` | `automation.md` |
| [D-0039](results.md#d-0039) | Editing result data through the driver, which reports each column's source and the row key | `results.md` |
| [D-0040](drivers.md#d-0040) | Importing CSV and XLSX files into a table, through the driver's write path | `drivers.md` |
| [D-0041](automation.md#d-0041) | Schedules and their grants in `automation.db`, with runs that record the schedule | `automation.md` |
| [D-0042](assistant.md#d-0042) | An MCP server for the assistant's tools | `assistant.md` |
| [D-0043](stack-and-dependencies.md#d-0043) | Reading CSV and XLSX files with `csv`, `calamine` and `encoding_rs` | `stack-and-dependencies.md` |
| [D-0044](stack-and-dependencies.md#d-0044) | Process-tree supervision with `process-wrap` | `stack-and-dependencies.md` |
| [D-0045](stack-and-dependencies.md#d-0045) | Time zones of schedules with `jiff`, without a cron grammar | `stack-and-dependencies.md` |
