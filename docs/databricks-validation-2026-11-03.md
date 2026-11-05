# Databricks connection changes: validation (2026-11-03)

## Scope

Warehouse Statement Execution and classic all-purpose Command Execution use the
existing core contracts. CLI authentication and HTTP pools are shared by auth
identity. Searchable selects discover compute via REST and profiles via
`databricks auth profiles --skip-validate --output json`. Saved selections are
restored without network access. ADR-0005 was amended with maintainer approval.
No dependencies, SQLite schemas or core trait signatures changed.

## Automated and CLI checks

- `cargo xtask check`: **passed** again after the final explorer presentation
  adjustment, covering the final source snapshot.
- `cargo xtask check --full`: **passed**, including architecture, formatting,
  Clippy with warnings denied, workspace tests/doctests, all eight live tests
  across PostgreSQL 14/18 and MySQL 8.4/9.7, quick benchmarks and dependency audit.
- `cargo deny --locked check`: **passed** within the full gate; existing
  duplicate-version warnings remain. Cargo manifests/lockfile were unchanged.
- Databricks tests: **29 passed** (25 unit/HTTP, four configuration/contract).
  Coverage includes profile parsing, cache identity/expiry/concurrent refresh,
  resource discovery, both execution paths, exact classic JSON cells,
  context reuse, cancellation during creation/execution and failed recovery.
- Native CLI **1.23.0**: **passed** profile discovery with an isolated synthetic
  configuration containing only `fixture` / `https://fixture.invalid`.
  No login or token resolution against a real workspace was performed.
- Diff review: completed; no tests removed/ignored/weakened or new lint bypasses.
- The pre-existing native vault test remains ignored by the gate; it requires
  the separate manual unlocked-vault check. Relational live tests skipped in
  the basic phase were explicitly executed in full mode.

Earlier gate attempts hit sandbox output-file permissions and a Windows lock
on the running test executable. Both were resolved for the successful full run.

## Visual check and performance

The release app was opened with `JYKRE_CONFIG_DIR` pointing to an isolated
synthetic-data folder and `JYKRE_PROBE=1`. Observed: the saved classic resource
name/ID, catalog/schema and CLI path were restored; the compute select opened,
showed focus and accepted Enter; a failed discovery kept the selection.

During this check, interval p95 frame times were **2.81–7.90 ms**, excluding
startup. This is an initial sample of the compute-selector iteration, with two
synthetic connections, not a load test or a measurement of the final profile
selector. There was no previous baseline for this connection-form path. Full
gate quick benchmarks completed (grid construction: 0.666 ms unfiltered /
12.111 ms filtered; result conversion: 50.982 ms; SQL splitting: 0.915 ms;
metadata pages: 0.215 / 0.531 ms). Those measured algorithms were not changed.

Computer Use was stopped by the user's Escape key. No further UI input was
issued. **Visual validation is partial:** the final CLI-profile select, final
explorer detail line, save/reopen after changes and final keyboard/focus flow
still need inspection. The later profile-select change passed compilation,
Clippy and automated tests, and its discovery command was checked with the
real CLI using only synthetic configuration.

## Remaining external validation

Authenticated SQL/metadata checks against a dedicated Databricks test workspace
were **not run**: no such fixture or credentials were provided. Local HTTP
fixtures cover both protocols, but do not replace this required external check.

## Follow-up: workspace host resolution

The application previously listed profiles using the CLI but read the host with
its own INI parser. Connection setup now uses the same bounded CLI metadata
loader as discovery, with the selected name and the canonical configuration
file. It does not read raw INI contents, substitute another profile or acquire
a token during host lookup. Unknown profiles and missing/unsafe workspace URLs
produce distinct classified errors with actionable messages.

- Portable Databricks suite: **31 passed** (27 unit/HTTP, four contract tests).
- Follow-up `cargo xtask check --full`: **passed**, including the basic gate,
  all relational live fixtures, benchmarks and audit; existing duplicate-version
  audit warnings and the separate native-vault check remain unchanged.
- Native CLI regression: **passed separately**, checking full client creation
  against synthetic UTF-8 BOM and commented-section configurations. This new
  test is ignored by the portable gate because it requires the external CLI;
  it was explicitly executed with `native_cli_profile_syntax -- --ignored`.
- CLI normalization of a synthetic bare hostname was also verified: the CLI
  returned its HTTPS origin. No real credentials, user configuration changes
  or requests to a real Databricks workspace were used for these checks.
- No visual layout was changed for this follow-up. Authenticated workspace
  validation remains not run as described above.

## Follow-up: workspace IDs and initial profile selection

Workspace profiles carrying both `account_id` and `workspace_id` are retained.
Account-console hosts are classified by destination instead of the presence of
an account ID. Skip-validate's `valid: false` does not filter profiles. New
forms start without a fabricated `DEFAULT`, then prefer the CLI's declared
default; saved/explicit profiles stay selected on refresh.

- Portable driver tests: **34 passed** (30 unit/HTTP and four contract tests).
- The separately run native CLI regression **passed**, including a synthetic
  profile with account/workspace IDs and a CLI default preference.
- DataVetra's profile, authentication, HTTP and classic-discovery implementation
  was inspected read-only as requested. Profile metadata IDs are not a reason
  to reject a workspace there either; cluster discovery uses the same REST path.
- The user took over visual testing and subsequently confirmed profile/compute
  discovery worked. No further computer-control actions were taken. Agent-run
  authenticated SQL/metadata execution and final keyboard/focus checks remain
  not run. The native-vault test remains separately manual.
- The requested pull advanced `release/0.1.0` by five commits. Its single
  translation conflict was resolved by merging catalogs and regenerating them,
  retaining both upstream and Databricks messages with no untranslated entries.
- Final post-pull `cargo xtask check --full`: **passed**, including Clippy,
  workspace tests/doctests, relational live fixtures, quick benchmarks and
  dependency audit. The external/manual checks listed above remain not run.
