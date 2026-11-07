# Stack and dependencies

## Approved stack

| Area | Choice | Notes |
| --- | --- | --- |
| Language | Rust stable, edition 2024, minimum 1.95 | The minimum is checked in CI (`msrv` job) |
| UI | GPUI through `gpui-kit` 0.9 (`gpui-pre-*` crates, `gpui-component`) | [ADR-0001](adr/0001-native-ui-with-gpui.md). `gpui-kit` pins a matching set of GPUI crates; upgrade them together |
| Async runtime | Tokio 1, multi-thread, owned by `backend::run` | GPUI's own executors drive the UI; Tokio drives I/O |
| PostgreSQL, MySQL, SQLite (local storage and the SQLite driver) | SQLx 0.9 without default features, `runtime-tokio`, rustls with native roots; `sqlite` with its bundled SQLite | [ADR-0002](adr/0002-sqlx-for-postgres-and-mysql.md), [ADR-0003](adr/0003-sqlite-for-local-storage.md), [ADR-0027](adr/0027-sqlite-driver.md). Another SQLite binding (`rusqlite`) cannot be linked beside it |
| Databricks | `reqwest` 0.12 with rustls, plus the Databricks CLI as an external program | [ADR-0005](adr/0005-databricks-through-cli-and-rest.md) |
| Credentials | `keyring` 3 with the native backend of each platform; `secrecy` for in-memory secrets | [ADR-0004](adr/0004-os-vault-for-credentials.md) |
| Errors | `thiserror` 2 | One classified `DriverError` across crates |
| Serialization | `serde`, `serde_json` | Configuration options and local JSON columns |
| Cancellation | `tokio-util` `CancellationToken` | Every long operation |
| Logs | `tracing`, `tracing-subscriber` (`env-filter`, `json`), `tracing-appender` | [ADR-0009](adr/0009-structured-logging-and-timings.md). Used by the `jykre` crate; `tracing` also by `jykre-driver-databricks` for its HTTP and CLI spans and by `jykre-assistant` |
| Telemetry over OTLP | `opentelemetry` 0.33 and `opentelemetry_sdk` 0.33 (`trace`, `logs`), `opentelemetry-otlp` 0.33 (`http-proto` with `reqwest-client`, `grpc-tonic` with `tls-ring` and `tls-roots`), `tracing-opentelemetry` 0.34, `opentelemetry-appender-tracing` 0.33, all without default features (Apache-2.0; `tracing-opentelemetry` MIT) | Optional export of logs and traces to a collector ([ADR-0029](adr/0029-otlp-export-of-logs-and-traces.md)). Approved 2026-11-06. Used only by `jykre::telemetry`. HTTP goes through the `reqwest` 0.13 already in the tree; gRPC brings `tonic` 0.14 and `prost` 0.14 with ring and the system's roots, so no `aws-lc-rs`. `opentelemetry-otlp`'s `http-proto` also compiles its metrics module, unused. 15 crates in all. The library still reads the standard `OTEL_EXPORTER_OTLP_*HEADERS` variables and merges them into the headers |
| Editor language features | `lsp-types` 0.97 with `proposed`, the version `gpui-kit` uses | The SQL editor's completion, hover and go to definition implement `gpui-component`'s providers, whose signatures take these types. Approved 2026-11-03; upgrade with the GPUI family |
| SQL formatting | `sqruff-lib` 0.44 (Apache-2.0) | **Format** in the editor, by the connection's dialect (`jykre_sql_language::format`, behind its default `sqruff` feature), with layout and keyword capitalisation rules only. Approved 2026-11-04 (N010). Declared only by `jykre-sql-language` ([ADR-0020](adr/0020-editor-language-features-in-their-own-crates.md)). It builds all of sqruff's dialects: `sqruff-lib` asks for `sqruff-lib-dialects` with its default features, which cannot be turned off from here. It also brings `serde_yaml`/`unsafe-libyaml`, declared by `sqruff-lib-dialects` but not used by its code |
| Excel export | `rust_xlsxwriter` 0.102 (MIT OR Apache-2.0) without default features, with `constant_memory` | Writes XLSX for **Export** ([Results](results.md#export)), one row at a time to temporary files, so an export never holds the sheet. Approved 2026-11-04. It brings `zip` (with `zopfli` for deflate), `typed-path`, `tempfile` and, on Unix, `nix`. Used only by `jykre::export` |
| Platform | `dirs` (data folder, the export's default folder), `sys-locale` (UI language) | |
| Windows executable resources | `winresource` 0.1 (MIT) without default features, as a build dependency | `build.rs` embeds `assets/icon/jykre.ico` as icon resource 1, the one GPUI loads for its windows, and the version details (Jykre Studio, the package version) in the `jykre` binary only. Approved 2026-11-07. It brings `version_check`; without `toml` it reads nothing from `Cargo.toml`. It runs the Windows SDK's `rc.exe` (`windres` on the GNU target) at build time and does nothing on other targets |
| AI providers | `genai` 0.7 (MIT OR Apache-2.0) without default features, with its own `reqwest` 0.13 (`rustls-no-provider`, declared as `reqwest-genai`) and `rustls` 0.23 with `ring` | The assistant's model calls ([ADR-0028](adr/0028-agentic-ai-assistant-over-the-metadata-cache.md)). Approved 2026-11-06. Used only in `jykre-assistant`'s private transport. `ring` is installed as the process's crypto provider, so `aws-lc-rs` (and its C build) is not compiled; certificates come from the system's trust store. A second `reqwest` (0.13) and `hyper-rustls` sit beside the Databricks driver's 0.12: `cargo deny` warns about the duplicates. The system proxy feature is off |

Development tools: `criterion` 0.8 without default features (benchmarks,
[ADR-0010](adr/0010-benchmarks-with-criterion.md)), `cargo xtask` (gate and catalogs), `cargo-deny` (audit),
Docker with Compose v2 (gate fixtures and local databases), and optionally the
Databricks CLI.

## Database compatibility

| Database | Validated versions | Where |
| --- | --- | --- |
| PostgreSQL | 14 and 18 | `cargo xtask check --full` |
| MySQL | 8.4 and 9.7 | `cargo xtask check --full` |
| Databricks | SQL Warehouses or running classic all-purpose clusters, with Unity Catalog | Local HTTP tests plus manual check with an authenticated CLI ([Testing](testing.md)) |
| SQLite | The version SQLx bundles (`libsqlite3-sys` in `Cargo.lock`) | `cargo xtask check`, against synthetic files |

## Policy for new dependencies

Every new dependency, Rust or tool, needs the **maintainer's prior approval**.
AI agents never add or upgrade dependencies on their own.

Approval is a gate, not a preference against dependencies. A mature crate is
often cheaper than our own code once its maintenance, edge cases and tests are
counted. When a dependency would clearly serve better than what we would
write, **propose it** instead of working around it: do not narrow a feature,
ship a weaker behaviour or write a large hand-rolled replacement just to avoid
asking. If the task cannot wait for the decision, deliver the version without
the dependency, record the trade-off in the task report and in the pending
item, and leave the proposal for the maintainer.

A proposal states:

1. **Why**: what it gives over what already exists and over our own code,
   counting the cost of writing, testing and maintaining that code. "About a
   hundred lines" is a reference for what counts as a trivial helper, not a
   threshold.
2. **License**, which must be in the `deny.toml` allow-list. GPL, AGPL and SSPL
   are not accepted.
3. **Health**: active maintenance and no open advisories. `cargo deny --locked
   check` must pass; an accepted advisory needs a reason and a review date in
   `deny.toml`.
4. **Weight**: what it adds to the build, including anything it duplicates
   (a second async runtime, TLS stack or date library), weighed against what
   it gives.
5. **Minimal features**: turn off `default-features` when they bring unneeded
   code.

Mechanics:

- Shared versions are declared once in `[workspace.dependencies]` of the root
  `Cargo.toml`; crates use `workspace = true`. A crate may declare a dependency
  locally only when it needs per-target feature sets (as `jykre-secrets` does
  for `keyring`).
- A change to `Cargo.toml` or `Cargo.lock` requires `cargo deny --locked check`
  (included in `cargo xtask check --full`).
- Upgrades of the GPUI family (`gpui-kit` and the crates it pins) are their own
  task, followed by a manual check of the application.

## Known deviations

| Standard | Current code |
| --- | --- |
| Shared versions only in `[workspace.dependencies]` | The root package declares `gpui-kit`, `lsp-types`, `sys-locale` and `dirs` with local versions |
