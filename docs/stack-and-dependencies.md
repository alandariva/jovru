# Stack and dependencies

## Approved stack

| Area | Choice | Notes |
| --- | --- | --- |
| Language | Rust stable, edition 2024, minimum 1.95 | The minimum is checked in CI (`msrv` job) |
| UI | GPUI through `gpui-kit` 0.9 (`gpui-pre-*` crates, `gpui-component`) | [ADR-0001](adr/0001-native-ui-with-gpui.md). `gpui-kit` pins a matching set of GPUI crates; upgrade them together |
| Async runtime | Tokio 1, multi-thread, owned by `backend::run` | GPUI's own executors drive the UI; Tokio drives I/O |
| PostgreSQL, MySQL, local SQLite | SQLx 0.9 without default features, `runtime-tokio`, rustls with native roots | [ADR-0002](adr/0002-sqlx-for-postgres-and-mysql.md), [ADR-0003](adr/0003-sqlite-for-local-storage.md) |
| Databricks | `reqwest` 0.12 with rustls, plus the Databricks CLI as an external program | [ADR-0005](adr/0005-databricks-through-cli-and-rest.md) |
| Credentials | `keyring` 3 with the native backend of each platform; `secrecy` for in-memory secrets | [ADR-0004](adr/0004-os-vault-for-credentials.md) |
| Errors | `thiserror` 2 | One classified `DriverError` across crates |
| Serialization | `serde`, `serde_json` | Configuration options and local JSON columns |
| Cancellation | `tokio-util` `CancellationToken` | Every long operation |
| Logs | `tracing`, `tracing-subscriber` (`env-filter`, `json`), `tracing-appender` | [ADR-0009](adr/0009-structured-logging-and-timings.md). Used by the `jykre` crate |
| Platform | `dirs` (data folder), `sys-locale` (UI language) | |

Development tools: `cargo xtask` (gate and catalogs), `cargo-deny` (audit),
Docker with Compose v2 (gate fixtures and local databases), and optionally the
Databricks CLI.

## Database compatibility

| Database | Validated versions | Where |
| --- | --- | --- |
| PostgreSQL | 14 and 18 | `cargo xtask check --full` |
| MySQL | 8.4 and 9.7 | `cargo xtask check --full` |
| Databricks | SQL Warehouses with Unity Catalog | Manual check with an authenticated CLI ([Testing](testing.md)) |

## Policy for new dependencies

Every new dependency, Rust or tool, needs the **maintainer's prior approval**.
AI agents never add or upgrade dependencies on their own. The task report
states:

1. **Why** the need cannot be met with what already exists or with about a
   hundred lines of our own code.
2. **License**, which must be in the `deny.toml` allow-list. GPL, AGPL and SSPL
   are not accepted.
3. **Health**: active maintenance and no open advisories. `cargo deny --locked
   check` must pass; an accepted advisory needs a reason and a review date in
   `deny.toml`.
4. **Weight**: what it adds to the build. Avoid crates that bring a second
   async runtime, a second TLS stack or a second date library.
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

### Not accepted

- `openssl` or `native-tls`: rustls is used everywhere, so builds behave the
  same on every platform.
- A second async runtime (`async-std`, `smol`). GPUI's executors and the single
  Tokio runtime are the only ones.
- An ORM (`diesel`, `sea-orm`): local storage is small and uses SQLx directly.
- A WebView or a second UI toolkit, which would undo [ADR-0001](adr/0001-native-ui-with-gpui.md).
- A dependency that only covers a trivial helper.

## Known deviations

| Standard | Current code |
| --- | --- |
| Shared versions only in `[workspace.dependencies]` | The root package declares `gpui-kit`, `sys-locale` and `dirs` with local versions |
