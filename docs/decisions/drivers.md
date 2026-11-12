# Decisions: Database drivers

Why this area is built as it is. Each entry records what was decided, why, and which alternatives lost. The rules themselves are in [Database drivers](../reference/drivers/README.md). Entries are numbered `D-NNNN`, and the numbers are stable.

<a id="d-0002"></a>
## D-0002: SQLx for PostgreSQL and MySQL, with rustls and the text protocol

*Accepted 2026-11-02 (recorded for a decision already in effect) · implemented* · Session lifetime superseded by D-0014

**Decision.** PostgreSQL and MySQL use SQLx 0.9 without default features, with `runtime-tokio` and `tls-rustls-ring-native-roots`. User SQL runs through the text protocol (`RawSql`), unchanged. The rules and consequences are in [SQLx and the pinned fork](../reference/drivers/configuration.md#sqlx-and-the-pinned-fork). The original rule that each run uses a dedicated session, discarded after cancellation, timeouts and interrupted reads, was replaced by a session per tab (D-0014).
**Why.** The relational drivers must run arbitrary user SQL, stream rows, keep exact values (decimals, large integers, temporal text), cancel, time out and use TLS identically on every platform. Text keeps exact values without per-type decoding code; SQLx also serves local storage (SQLite).
**Rejected.** `tokio-postgres` plus `mysql_async`: mature, but two APIs and two TLS integrations, where SQLx covers both and SQLite. The binary protocol with typed decoding: typed values, but a type mapping per database and a risk of lossy conversions.

<a id="d-0005"></a>
## D-0005: Databricks through CLI profiles and REST APIs

*Accepted 2026-11-02 (recorded for a decision already in effect); amended 2026-11-03 to cover classic all-purpose clusters · implemented* · Amended by D-0026

**Decision.** A connection names an authenticated Databricks CLI profile; the CLI resolves the token, which stays in memory and is never stored. SQL runs through the Statement Execution API on a SQL warehouse or Command Execution API 1.2 on a running classic all-purpose cluster, by the connection's explicit compute type; metadata uses Unity Catalog REST without starting compute. Details: [Databricks](../reference/drivers/configuration.md#databricks) and [Real use and limits](../reference/drivers/configuration.md#real-use-and-limits). User SQL is never replayed, and rewritten only as D-0026 says.
**Why.** The CLI already handles OAuth, tokens and profiles on the user's machine; reimplementing that would add a flow to maintain and tokens to store. The cost is that the CLI must be installed and logged in, SQL needs a warehouse ID or a running cluster ID, and the classic API can truncate results (rows returned are paged locally, never re-run; no remote pagination is invented). Real changes are validated with an authenticated CLI and a test workspace ([testing](../standards/testing.md)).
**Rejected.** OAuth inside the application: no CLI needed, but a flow to maintain and tokens to store. JDBC/ODBC: full SQL feature set, but a JVM or native drivers to ship. Classic all-purpose compute through Command Execution: first out, included by the 2026-11-03 amendment (reuses the CLI/REST stack and SQL contexts; API result caps and a distinct lifecycle). Serverless through Python, and jobs clusters: wider coverage, but an extra runtime and lifecycle, so out of scope.

<a id="d-0015"></a>
## D-0015: Introspect only what the user chooses

*Accepted 2026-11-07 · implemented*

**Decision.** A connection introspects nothing until the user chooses, from namespaces listed on demand one level at a time; the choice is stored as rules and each last-level namespace is read and published on its own. See the paragraphs on `discover_namespaces` and the choice in [Layout](../reference/drivers/contract.md#layout).
**Why.** Refreshing read everything a connection could see (every schema, database or catalog with all columns) as one atomic read. A server with hundreds of schemas, of which the user needs two, paid the full cost on every refresh; a large Databricks workspace could exceed the 300-second limit and publish nothing; saving a connection started the full read unasked. Cost now follows what the user works with and large workspaces publish progressively with a limit per request. The total time of *everything* is not shorter, since namespaces are read in sequence. It costs one core trait method (with a default) and one repository operation (`retain`).
**Rejected.** Keeping the full read and filtering only the explorer: a filter hides work, it does not save it. Preselecting a default (current schema, default catalog) and reading it at once: reads before the user decides, and a wrong default still costs a read (the default is only checked in the dialog, D-0016). One atomic publication for the whole choice: one failure loses everything. Weighting the time estimate by table counts: extra queries, costly in Unity Catalog.

<a id="d-0016"></a>
## D-0016: Keys, defaults and relation kinds in the metadata model

*Accepted 2026-11-07 · implemented*

**Decision.** `ObjectKind` gains `MaterializedView`, `ForeignKey` and `Partition`; `MetadataObject` gains optional `default_value`, `primary_key` and `references` (`ForeignKeyTarget`), left out when empty so older caches still read and no SQLite migration is needed. PostgreSQL reads `pg_catalog`; `Driver::default_namespaces` suggests the first choice. What each driver reads is in the table under [Layout](../reference/drivers/contract.md#layout) and in the `MetadataObject` paragraph of [Contract flow](../reference/drivers/contract.md#contract-flow).
**Why.** PostgreSQL's `information_schema` reports `ARRAY` or `USER-DEFINED` instead of the real type, omits materialized views and lists every partition as a table; no driver read keys or defaults, which the explorer, autocomplete and inspection need. The price is three more catalog joins per schema in PostgreSQL and one more query in MySQL. A real Databricks workspace still has to confirm that listings include `table_constraints`.
**Rejected.** Keeping `information_schema` in PostgreSQL and adding key views: the gaps are what the change is for. New SQLite tables for keys and partitions: a first schema migration for data that fits the object JSON. Reading indexes, checks, routines and sequences now: more queries per refresh for data the explorer does not use; left to on-demand inspection (D-0017).

<a id="d-0017"></a>
## D-0017: Cache identity, reading levels and on-demand inspection in the driver contract

*Accepted 2026-11-07 · implemented*

**Decision.** The `Driver` trait gains `cache_identity`, `introspect_level` (`Names` or `Full`), `inspect`, `definition` and `left_out_of_everything`, each with a default that keeps the previous behaviour. Warnings for unreadable namespaces use the existing `IntrospectionReport::warnings`. Detail is in [Contract flow](../reference/drivers/contract.md#contract-flow).
**Why.** The cache belongs to a catalog, but its identity was the whole configuration, so changing compute, default catalog or TLS mode emptied it. A schema with thousands of tables showed nothing until fully read. Indexes, constraints, routines, sequences, triggers and types were not read, and reading them every refresh would slow a refresh for what few people look at. Golden rule 3 keeps database knowledge in the driver, and these needed the drivers to say it, so the changes are batched into one contract change. Costs: one more read per namespace the first time (names, then full) and five more methods to keep consistent. Contract tests (`crates/core/tests/contracts.rs`) cover every default.
**Rejected.** Computing the identity in the application from known option names: option names are each driver's own (golden rule 3). Names only, with columns read when a relation is expanded: completion and hover find no columns until a server read. New `ObjectKind`s for routines, sequences, triggers and types: every refresh would read them, the tree would grow and four drivers would have to emit them consistently. Typed structs per inspected kind: each database exposes different attributes and the UI only lists them.

<a id="d-0023"></a>
## D-0023: Named parameters bound by the drivers

*Accepted 2026-11-07 · implemented* · Amends D-0002 (report SQL only: markers are replaced by fixed references before it is sent) · Amended by D-0026

**Decision.** Requests carry typed `QueryParameter`s with the byte ranges of their `:name` markers; each driver binds the values apart from the SQL text and the only change to the SQL is replacing markers with fixed references through `QueryRequest::with_references`. Contract, marker rules and per-driver binding are in [Named parameters](../reference/drivers/parameters.md#named-parameters).
**Why.** Reports run saved SQL with values typed per run. Two written rules apply: values are always bound, never interpolated, and the SQL the user wrote is sent as written. The text protocol (D-0002) means PostgreSQL and MySQL decode rows with `as_str()`; binding the statement itself would move it to SQLx's prepared path with binary results and a decoder per type, and give reports and SQL tabs two decoding paths that could show a value differently. Binding in a separate statement that returns no rows the user sees keeps one decoding path. `:name` is the marker users know and Databricks reads natively, but a colon already means something in each dialect (`x::int`, `@a := 1`, `raw:owner`, `arr[lo:hi]`), so `named_markers` reads tokens, not text. Costs: one extra round trip on PostgreSQL and MySQL, SQL that is not byte-for-byte what was written, and values left in the tab's own session until its next run.
**Rejected.** Interpolating typed literals: one round trip and works everywhere, but the value is in the text and safety depends on dialect and server settings (`standard_conforming_strings`, `NO_BACKSLASH_ESCAPES`); a quoting bug is an injection (D-0026 accepts it only for classic clusters, hex-encoded). The extended protocol with markers turned into `$1` or `?`: reverses D-0002 for one feature, and MySQL cannot prepare every statement. Plain text replacement: injection by construction. Native `:name` only: only Databricks warehouses support it.

<a id="d-0026"></a>
## D-0026: Named parameters in SQL tabs, and typed literals on classic Databricks clusters

*Accepted 2026-11-07 · implemented* · Amends D-0023 (parameters were limited to reports and classic clusters refused them) and D-0005 ("user SQL is never rewritten" gains the same marker exception)

**Decision.** Query tabs bind `:name` parameters through the same request, validation and drivers as reports, with a per-field type that is **Automatic** by default, and classic Databricks clusters bind them as typed literals instead of refusing them. The tab and cluster rules are in [Named parameters](../reference/drivers/parameters.md#named-parameters).
**Why.** Running a `SELECT` again with another id meant editing a literal or writing the value into the text, which the bound-values rule exists to avoid; DataGrip and DBeaver bind variables in consoles and Beekeeper Studio has query parameters. A tab has no definition saying a type, and on PostgreSQL `id = :id` fails when bound as text, hence inference with an override. The command API (`/api/1.2/commands/execute`, body `{clusterId, contextId, language, command}`) has no field for values. Values are kept with the tab like its SQL and context, in `config.db`.
**Rejected.** Keeping tabs without parameters: the problem this solves. A type chosen for every field, text by default: `id = :id` fails until Number is picked. Values only in memory: lost on restart unlike the SQL. A Python context and `spark.sql(sql, args=…)` on classic clusters: a second context per tab, another result shape, values encoded in Python source, Python is out of scope in D-0005, and not verifiable without a workspace. Databricks session variables (`DECLARE VARIABLE`, `SET VAR`): the value is still in the text of the `SET VAR`. Classic clusters keep refusing: reports and tabs with parameters could not run on classic compute.

<a id="d-0027"></a>
## D-0027: SQLite database files through SQLx's bundled SQLite

*Accepted 2026-11-07 · implemented*

**Decision.** A `jovru-driver-sqlite` crate opens existing SQLite files through SQLx's `sqlite` feature, as `DriverKind::Sqlite`, with no new crate in `Cargo.lock` and no `unsafe`. Configuration, behaviour, metadata and limits are in [SQLite](../reference/drivers/configuration.md#sqlite) and [Real use and limits](../reference/drivers/configuration.md#real-use-and-limits). The maintainer's 2026-11-06 choices: SQLx's feature; open an existing file read-write with a read-only option and never create a missing one; schemas are `main` and the attached databases, with `main` the default choice (D-0015); the editor's SQL catalog comes later.
**Why.** SQLite is a library, not a server: no host, user or password, no `USE`, and a value's type belongs to the value. The application already links SQLite through SQLx for local storage, and a second binding would link a second `libsqlite3-sys`, which Cargo refuses. Costs: errors carry no position (SQLx does not expose `sqlite3_error_offset`); a page kept open holds a read lock; foreign keys have no names in SQLite's pragmas, so they are named by what they link. D-0030 later gave the driver the transaction state through the fork.
**Rejected.** `rusqlite`: direct C API (`sqlite3_interrupt`, autocommit state, error offsets) but conflicts with `jovru-storage`'s `libsqlite3-sys`. Raw FFI through SQLx's handle: needs `unsafe`, forbidden by the workspace lints, and `libsqlite3-sys` is semver-exempt in SQLx. Creating a missing file: a mistyped path would silently open an empty database.

<a id="d-0030"></a>
## D-0030: Transaction state read from the protocol through a pinned SQLx fork

*Accepted and implemented 2026-11-07; pinned revision updated 2026-11-09 with the maintainer's approval (`8ddadbe`, for RUSTSEC-2026-0459)* · Amends D-0002 and D-0027

**Decision.** Every SQLx crate is patched to the `alandariva/sqlx` fork (`v0.9.0` plus read-only readers of the transaction state), and the drivers answer `Session::transaction` from it with the contract unchanged. The rules, API and exit are in [SQLx and the pinned fork](../reference/drivers/configuration.md#sqlx-and-the-pinned-fork); the per-driver reading is in [Contract flow](../reference/drivers/contract.md#contract-flow).
**Why.** Version 0.2.0 must show an open, pending or failed transaction wherever the database lets a client read it (TD-001). The databases already say it (`ReadyForQuery` status, MySQL status flags, `sqlite3_get_autocommit`), but SQLx keeps them private and its `is_in_transaction` counts only transactions begun through its API. The previous probes had gaps: PostgreSQL's rule read the first word, so `COMMIT AND CHAIN` made the probe take the new transaction's snapshot and a later `SET TRANSACTION ISOLATION LEVEL` failed (reproduced on PostgreSQL 18); MySQL missed transactions begun in procedures or rolled back by a deadlock; SQLite showed an interrupted or `OR ROLLBACK` transaction as open. Costs: a fork in a personal account to maintain and a git source in the tree.
**Rejected.** Probe queries (`SAVEPOINT` in MySQL and PostgreSQL): no dependency change and no snapshot, but they leave traces on the user's servers (binlog, logged errors, rollback counters). Keeping the probes and inference: leaves the gaps. SQLite `BEGIN` probe: two statements per run and depends on the error text. SQLite FFI through `as_raw_handle`: needs `unsafe` (D-0027). Waiting for upstream: no pull request existed and M1 could not wait. Our own wire protocol: out of proportion.

Regression tests: the fork's `tests/postgres`, `tests/mysql` and `tests/sqlite`, and in this repository the live tests `live_the_transaction_state_is_reported_without_taking_a_snapshot`, `live_commit_and_chain_opens_a_transaction_without_taking_its_snapshot` (PostgreSQL), `live_the_transaction_state_is_read_from_the_status_flags`, `live_a_deadlock_victim_reports_its_transaction_rolled_back` (MySQL) and the SQLite file test `the_transaction_state_is_read_from_the_connection`, all run by `cargo xtask check --full`.

<a id="d-0032"></a>
## D-0032: Session encryption reported by the drivers

*Accepted 2026-11-07 · implemented*

**Decision.** `Session::encryption` (default: unknown) returns `Encryption` (`Tls { version }`, `Plaintext`, `Local` or `Unknown`), asked once per session before its first statement. Behaviour and per-driver reading are in [Contract flow](../reference/drivers/contract.md#contract-flow).
**Why.** Milestone M3 asks that the active connection show whether its session is encrypted and warn on plain text. The profile only says what mode was asked for, and `prefer`/`preferred` fall back silently; only the open connection knows. Costs one short query when a PostgreSQL or MySQL tab session opens.
**Rejected.** Inferring from the configured mode: cannot tell a fallback. Reading the negotiated state from the pinned fork: no extra query, but it widens the fork (D-0030) and is a dependency change; the maintainer chose the contract with server queries. Asking on every run: a query per run, though TLS cannot change within a session.

<a id="d-0040"></a>
## D-0040: Importing CSV and XLSX files into a table, through the driver's write path

*Accepted 2026-11-08 · implemented* · Amends the `Session` contract and the stack

**Decision.** Rows from a CSV or an `.xlsx` sheet are inserted through the editing write path (D-0039) in a session of its own, all or nothing in one transaction, with bound values and bounded memory. The driver contract (`table_target`, `begin_writes`, `end_writes`, `WriteScope`, `WriteValue::Boolean`) is in [Importing rows](../reference/drivers/editing-and-import.md#importing-rows); the files, mapping, progress and outcomes are in [results](../reference/results/transfer.md#import); the encoding and streaming rules are in [Import encodings](../reference/drivers/editing-and-import.md#import-encodings) and [Import memory and streaming](../reference/drivers/editing-and-import.md#import-memory-and-streaming); the dependencies (`csv`, `calamine`, `encoding_rs`) are in [D-0043](stack-and-dependencies.md#d-0043).
**Why.** Milestone A11 requires values bound by the driver and converted by the database, CSV text reaching the table as written, bounded memory off the UI thread, a timeout, and a failure or cancellation that says what it left. One write path shares binding and exactness rules with edits, and a failure leaves the table untouched. Reading runs on the blocking pool and writing on the runtime, joined by a bounded channel of 2 batches. Maintainer decisions: I-0 approve the three crates; I-1 all or nothing; I-2 the import never creates the table ("New table from this file…" drafts a `CREATE TABLE` in a new query tab); I-3 Databricks excluded for 0.3.0; I-4 an empty CSV field is NULL or empty text per import, NULL by default; I-5 `.xlsx` only. Forbidden: replacing undecodable bytes, writing file values to logs, importing through a tab's session, creating tables inside the import. Costs: three dependencies, three more `Session` methods, a transaction as long as the file. Live tests per driver are required for any driver that implements `row_writes`. `calamine` was kept with debug assertions on for `debug_unsafe` in release builds, by the maintainer's decision.
**Rejected.** PostgreSQL `COPY FROM STDIN` and MySQL `LOAD DATA LOCAL INFILE`: much faster, but per-database formats and escaping, `LOCAL INFILE` must be enabled on client and server and lets a server read client files, and values are not bound by our driver (`COPY` may come later as a PostgreSQL optimisation behind the same contract). Importing in the tab's session: a long import blocks the tab and a mid-way failure interacts with the user's transaction. Generating `INSERT` statements into the editor: values as literals and millions of statements in an editor. Reading the whole file first: memory grows with it. Committing every batch: half-imported tables after a failure.
