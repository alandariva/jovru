# Stack and dependencies

## Approved stack

| Area | Choice | Notes |
| --- | --- | --- |
| Language | Rust stable, edition 2024, minimum 1.95 | The minimum is checked in CI (`msrv` job) |
| UI | GPUI through `gpui-kit` 0.9 (`gpui-pre-*` crates, `gpui-component`) | [ADR-0001](../adr/0001-native-ui-with-gpui.md). `gpui-kit` pins a matching set of GPUI crates; upgrade them together |
| Async runtime | Tokio 1, multi-thread, owned by `backend::run` | GPUI's own executors drive the UI; Tokio drives I/O |
| PostgreSQL, MySQL, SQLite (local storage and the SQLite driver) | SQLx 0.9 without default features, `runtime-tokio`, rustls with native roots; `sqlite` with its bundled SQLite | [ADR-0002](../adr/0002-sqlx-for-postgres-and-mysql.md), [ADR-0003](../adr/0003-sqlite-for-local-storage.md), [ADR-0027](../adr/0027-sqlite-driver.md). Another SQLite binding (`rusqlite`) cannot be linked beside it. Every SQLx crate is patched to a fork pinned to one revision (0.9.0 plus readers of the transaction state, and `libsqlite3-sys` up to 0.39 for SQLite 3.54), allowed in `deny.toml`, until a release has those readers ([ADR-0030](../adr/0030-transaction-state-through-a-pinned-sqlx-fork.md)) |
| Databricks | `reqwest` 0.12 with rustls, plus the Databricks CLI as an external program | [ADR-0005](../adr/0005-databricks-through-cli-and-rest.md) |
| Credentials | `keyring` 3 with the native backend of each platform; `secrecy` for in-memory secrets | [ADR-0004](../adr/0004-os-vault-for-credentials.md) |
| Errors | `thiserror` 2 | One classified `DriverError` across crates |
| Serialization | `serde`, `serde_json` | Configuration options and local JSON columns |
| Cancellation | `tokio-util` `CancellationToken` | Every long operation |
| Logs | `tracing`, `tracing-subscriber` (`env-filter`, `json`), `tracing-appender` | [ADR-0009](../adr/0009-structured-logging-and-timings.md). Used by the `jykre` crate; `tracing` also by `jykre-driver-databricks` for its HTTP and CLI spans and by `jykre-assistant` |
| Telemetry over OTLP | `opentelemetry` 0.33 and `opentelemetry_sdk` 0.33 (`trace`, `logs`), `opentelemetry-otlp` 0.33 (`http-proto` with `reqwest-client`, `grpc-tonic` with `tls-ring` and `tls-roots`), `tracing-opentelemetry` 0.34, `opentelemetry-appender-tracing` 0.33, all without default features (Apache-2.0; `tracing-opentelemetry` MIT) | Optional export of logs and traces to a collector ([ADR-0029](../adr/0029-otlp-export-of-logs-and-traces.md)). Approved 2026-11-06. Used only by `jykre::telemetry`. HTTP goes through the `reqwest` 0.13 already in the tree; gRPC brings `tonic` 0.14 and `prost` 0.14 with ring and the system's roots, so no `aws-lc-rs`. `opentelemetry-otlp`'s `http-proto` also compiles its metrics module, unused. 15 crates in all. The library still reads the standard `OTEL_EXPORTER_OTLP_*HEADERS` variables and merges them into the headers |
| Editor language features | `lsp-types` 0.97 with `proposed`, the version `gpui-kit` uses | The SQL editor's completion, hover and go to definition implement `gpui-component`'s providers, whose signatures take these types. Approved 2026-11-03; upgrade with the GPUI family |
| SQL formatting | `sqruff-lib` 0.44 (Apache-2.0) | **Format** in the editor, by the connection's dialect (`jykre_sql_language::format`, behind its default `sqruff` feature), with layout and keyword capitalisation rules only. Approved 2026-11-04 (N010). Declared only by `jykre-sql-language` ([ADR-0020](../adr/0020-editor-language-features-in-their-own-crates.md)). It builds all of sqruff's dialects: `sqruff-lib` asks for `sqruff-lib-dialects` with its default features, which cannot be turned off from here. It also brings `serde_yaml`/`unsafe-libyaml`, declared by `sqruff-lib-dialects` but not used by its code |
| Excel export | `rust_xlsxwriter` 0.102 (MIT OR Apache-2.0) without default features, with `constant_memory` | Writes XLSX for **Export** ([Results](../reference/results.md#export)), one row at a time to temporary files, so an export never holds the sheet. Approved 2026-11-04. It brings `zip` (with `zopfli` for deflate), `typed-path`, `tempfile` and, on Unix, `nix`. Used only by `jykre::export` |
| Reading CSV and XLSX for an import | `csv` 1.4 (`Unlicense/MIT`), `calamine` 0.38 (MIT) without default features, and `encoding_rs` 0.8 (`(Apache-2.0 OR MIT) AND BSD-3-Clause`) as a direct dependency, without `fast-legacy-encode` or `simd-accel` | Turn a CSV or one sheet of an `.xlsx` into rows for **Import from file…** ([ADR-0040](../adr/0040-importing-files-into-a-table.md), [proposal](../plans/csv-and-xlsx-reading-dependencies.md)). Approved 2026-11-08. Used only by `jykre::import`. `csv` reads one record at a time over our decoder, which uses `encoding_rs` without replacing malformed input; `calamine`'s cells reader streams the sheet (`worksheet_range` is never used), `dates`, `chrono` and `picture` off. They bring `csv-core`, `atoi_simd`, `debug_unsafe` (through `atoi_simd`, MIT OR Apache-2.0, not listed in the proposal; compiled with debug assertions in release by `[profile.release.package.debug_unsafe]`, approved 2026-11-08, so its indexing stays checked on untrusted files), `fast-float2`, `codepage` and `quick-xml` 0.42, a second `quick-xml` beside 0.41 on Linux. `zip`, already in the tree, is also a direct dependency of the `jykre` crate now (approved 2026-11-08), to check the size of a workbook's shared strings table before `calamine` reads it whole |
| Process-tree supervision | `process-wrap` 10.0.1 (`Apache-2.0 OR MIT`) without default features, with `tokio1`, `job-object`, `kill-on-drop`, `process-group` and `creation-flags` | Starts the automation engine's external programs in a process group (Unix) or a Job Object without a console window (Windows), so Stop, a deadline and a crashed Studio end the whole tree ([proposal](../plans/process-supervision-dependency.md), [ADR-0038](../adr/0038-external-program-runs.md)). Approved 2026-11-08. Used only by `jykre::automation::process`. One new crate: `nix`, `windows`, `futures` and `tokio` were already in the tree. Its `tracing` feature stays off: its spans would carry fields outside the telemetry allowlist. It holds the `unsafe` the workspace forbids in ours. Validated on Linux; Windows through the [checklist](../plans/releases/0.3.0-windows-checklist.md) |
| Fingerprints | `sha2` 0.10 (`MIT OR Apache-2.0`) | The SHA-256 of automation's published versions and of a program's entrypoint (`jykre-storage`'s `fingerprint`, [ADR-0034](../adr/0034-automation-database-service-and-lock.md)). Approved 2026-11-08 with ADR-0034 as a direct dependency of `jykre-storage` only; it was already in the tree through `self_update` |
| Time zones | `jiff` 0.2.39 (`Unlicense OR MIT`) with its default features | The time-zone arithmetic of automation's schedules: due times in an IANA zone, daylight-saving gaps and folds (`Disambiguation::Compatible`), the system zone and the offset shown with a due time ([proposal](../plans/schedule-time-zone-dependency.md), [ADR-0041](../adr/0041-schedules-and-grants.md)). Approved 2026-11-08 (Q-33). Used only by `jykre::automation::schedule` and the schedule views. No new crate: `jiff`, `jiff-static`, `jiff-tzdb` and `jiff-tzdb-platform` were already in the tree through `serde_with`. Linux and macOS read the system's zone database; Windows uses the one bundled with the build, so a zone whose rules change after a release needs an application update there. No cron or RRULE crate: a schedule's rule is our own form |
| Program logs in the console's code page (Windows only) | `oem_cp` 2.1 (MIT; its only dependency, `phf` 0.11, was already in the tree) and `windows-registry` 0.6 (`MIT OR Apache-2.0`, already in the tree), both safe APIs | Show a program's stdout or stderr that is not UTF-8 in the OEM code page Windows records under `OEMCP` (CP437, CP850…), as [ADR-0037](../adr/0037-program-logs-on-disk.md) was amended on 2026-11-09; the files on disk are never rewritten. Approved by the maintainer on 2026-11-09 over calling `GetOEMCP` and `MultiByteToWideChar` through `windows-sys`, which `unsafe_code = "forbid"` rules out |
| Platform | `dirs` (data folder, the export's default folder), `sys-locale` (UI language) | |
| Updates | `self_update` 1.3 (MIT) without default features, with `github`, `async`, `signatures`, `archive-tar`, `compression-tar-gz`, `archive-zip` and `compression-zip-deflate` | The release list, signature verification (zipsign, ed25519ph) and unpacking for the update check and self-install ([ADR-0033](../adr/0033-update-check-and-signed-self-install.md)). Approved 2026-11-08. Used only by `jykre::update`, with our own `reqwest` 0.13 client; its `Update`, `Download`, `MoveAll` and `restart` are not used. It brings 18 crates: `self-replace`, `tar`, `filetime`, `xattr` (Unix), `urlencoding`, `zipsign-api`, `ed25519-dalek`, `ed25519`, `curve25519-dalek` and its derive crate, `signature`, `pkcs8`, `der`, `spki`, `const-oid`, `base64ct` and `fiat-crypto`; `zip`, `flate2`, `sha2`, `semver`, `regex` and `tempfile` were already in the tree. Through feature unification `reqwest` 0.13 also compiles `blocking`, `json`, `http2` and `stream`. Its `rustls` feature stays off: it would compile `aws-lc-rs`, which `deny.toml` bans. The tests also use `zipsign-api` with `sign-tar` and `sign-zip`, `tar`, `flate2` and `zip` directly, to make signed archives |
| Windows executable resources | `winresource` 0.1 (MIT) without default features, as a build dependency | `build.rs` embeds `assets/icon/jykre.ico` as icon resource 1, the one GPUI loads for its windows, and the version details (Jykre Studio, the package version) in the `jykre` binary only. Approved 2026-11-07. It brings `version_check`; without `toml` it reads nothing from `Cargo.toml`. It runs the Windows SDK's `rc.exe` (`windres` on the GNU target) at build time and does nothing on other targets |
| AI providers | `genai` 0.7 (MIT OR Apache-2.0) without default features, with its own `reqwest` 0.13 (`rustls-no-provider`, declared as `reqwest-genai`) and `rustls` 0.23 with `ring` | The assistant's model calls ([ADR-0028](../adr/0028-agentic-ai-assistant-over-the-metadata-cache.md)). Approved 2026-11-06. Used only in `jykre-assistant`'s private transport. `ring` is installed as the process's crypto provider, so `aws-lc-rs` (and its C build) is not compiled; certificates come from the system's trust store. A second `reqwest` (0.13) and `hyper-rustls` sit beside the Databricks driver's 0.12: `cargo deny` warns about the duplicates. The system proxy feature is off |

Release tools: the `zipsign` CLI 0.2.2 (`cargo install --locked`), in the
release workflow's `sign` job only (approved 2026-11-08,
[ADR-0033](../adr/0033-update-check-and-signed-self-install.md)).

Development tools: `criterion` 0.8 without default features (benchmarks,
[ADR-0010](../adr/0010-benchmarks-with-criterion.md)), `cargo xtask` (gate and catalogs), `cargo-deny` (audit),
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
