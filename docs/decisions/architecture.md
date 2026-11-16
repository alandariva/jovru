# Decisions: Architecture

Why this area is built as it is. Each entry records what was decided, why, and which alternatives lost. The rules themselves are in [Architecture](../reference/architecture.md). Entries are numbered `D-NNNN`, and the numbers are stable.

<a id="d-0001"></a>
## D-0001: Native UI with GPUI through `gpui-kit`

*Accepted 2026-11-02 (recorded for a decision already in effect) · implemented*

**Decision.** The interface is drawn natively with GPUI, consumed through `gpui-kit`; there is no WebView, no IPC layer and no frontend language besides Rust (see [Overview](../reference/architecture.md#overview)). Views follow the [UI guidelines](../standards/ui-guidelines.md); in particular long collections are virtualized, because GPUI rebuilds a notified view on every frame.
**Why.** The product promise is a database workspace that starts quickly, stays light and answers immediately, in Rust end to end. A WebView shell adds a second runtime, an IPC boundary and a JavaScript heap that multiplies the memory of large results. One process gives direct access to data without serialization and GPU rendering. The cost: GPUI is young and its API moves, so upgrades need a manual check of the whole application.
**Rejected.** Tauri with a web frontend (large ecosystem, familiar tooling, but WebView memory, IPC serialization and two languages conflict with the lightness goal); egui / iced (pure Rust, but fewer ready components for an IDE-like layout with editor, docks and virtual lists, where GPUI fits better).

<a id="d-0006"></a>
## D-0006: Layered crates independent of the UI, with a static driver registry

*Accepted 2026-11-02 (recorded for a decision already in effect) · implemented*

**Decision.** `jovru-core` holds contracts and types without I/O dependencies; each driver, `jovru-storage` and `jovru-secrets` is its own crate that depends only on `jovru-core`; none depends on GPUI. Only the `jovru` crate composes the drivers, statically, in `drivers::builtin_registry()`. `cargo xtask check` enforces the edges ([Dependency rules](../reference/architecture.md#dependency-rules)).
**Why.** Drivers, storage and credentials must be testable without a window, reusable by other front ends (examples, a future CLI) and isolated from each other, so that adding a database does not touch the others: it is one crate and one registration line. The cost is more crates and `Cargo.toml` files to keep in sync.
**Rejected.** One crate (simple builds, but everything compiles GPUI and boundaries erode; tests and reuse need the split); dynamic driver plugins (third-party drivers, but ABI instability and a security surface, with no need yet).

<a id="d-0014"></a>
## D-0014: A session per tab, kept between runs and after errors

*Accepted 2026-11-07 · implemented · Amended by D-0025 for report tabs (a session per connection and database that a report's panels use)*

**Decision.** Each tab keeps one session between runs, so `USE`, `SET`, temporary tables and an open transaction apply to later runs, and errors keep it. `Session` gains `is_open` and `ping`; a result longer than a page stays open in the session and the next page is read from it with `Session::fetch`. The flow is in [Run a query](../reference/architecture.md#run-a-query); the recovery after each kind of failure is in [Drivers](../reference/drivers/README.md) and [Reliability](../standards/reliability.md).
**Why.** `Ctrl Enter` runs one statement at a time. With a session per run, `USE sales` did not apply to the next `SELECT`, temporary tables vanished and a transaction could not span runs; a failure, a cancellation or the row limit also discarded the session, exactly when the user was most likely to continue. The costs: one ping per run, and a result interrupted by the limit is drained (MySQL) or cancelled and drained (PostgreSQL) before the session is reused. Backend tests with a stateful test driver and live tests on PostgreSQL and MySQL check that state survives errors, timeouts, cancellations, the row limit and a stop by the receiver.
**Rejected.** A session per run (no idle session to go stale and no recovery code, but the context is lost between runs, the main way of working); keeping the session but discarding it after any failure (simpler drivers, but a typo or the row limit loses the context); reconnecting and replaying `USE`/`SET` (survives lost connections, but needs every dialect's session commands parsed and cannot replay temporary tables or transactions; saying that the session started over is honest and simpler); limiting MySQL rows with `sql_select_limit` (no rows to drain, but it changes a session variable the user may set; draining with a deadline leaves the session untouched).

<a id="d-0020"></a>
## D-0020: Editor language features in their own crates

*Accepted 2026-11-07 · implemented*

**Decision.** What query execution and the editor share lives in `jovru-sql` (tokenizer by dialect, statement splitting, `statement_at`, `statement_lines`, normalisation, `explain`, `map_offset`, qualified names); the editor's language features live in the pure `jovru-sql-language` (`completion`, with the queries and relations of each statement in `completion::scopes`, and `format`), with engines as optional features that have a fallback (`sqruff`, on by default; without it Format uses a built-in layout). `src/app/sql_editor/` is the only bridge to the editor, and the library never uses `jovru-sql-language`: the backend only reads the cache (`backend::cached_relations`) and the adapter builds the name index. The enforced edges are [Dependency rules](../reference/architecture.md#dependency-rules) 5 and 6; the layers are in the table above.
**Why.** Highlighting, completion, hover, go to definition and Format had grown inside code that execution also depends on (`sql` held Format and the demo's preview check, `completion` borrowed `ddl` helpers, the `Workspace` held the completion caches and the Format flow). The editor research plans more features (dialect catalogs, a statement model for completion and semantic highlighting); several may be replaced or dropped (tree-sitter 0.26 with `tree-sitter-sequel` 0.3 was tried in a proof of concept on 2026-11-04 and not adopted: with the dialect lexer masking strings, comments and parameters it found the right tables at the cursor in 104 of 120 cases (87%) where the token scope model found 120 (100%), and it took p95 4.2–6.0 ms on a 19.6 KB statement against 0.8–1.1 ms). Tangled, removing one would touch what runs queries and every engine would compile into the library. Now a feature is removed by not installing it in `SqlEditor::new` and deleting its module; dropping them all means dropping `jovru-sql-language` and `src/app/sql_editor/`, and execution builds without sqruff. The cost: two more crates and a few calls the workspace makes to keep `SqlEditor` in step (dialect, connection, context, names).
**Rejected.** Modules inside the `jovru` crate with Cargo features (no new crates, but nothing stops the library from using them and engines compile into it; the boundary would rest on review alone); one `jovru-sql` crate with everything (one crate fewer, but execution would depend on sqruff and future engines); keeping the layout (no work, but removing or replacing a feature touches execution and the workspace).
