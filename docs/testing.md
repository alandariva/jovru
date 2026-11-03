# Local gate and testing policy

A task that changes code must pass the basic gate before it is reported as
done. Changes to drivers or execution contracts, and application releases,
require the full gate. Changes to `Cargo.toml`/`Cargo.lock` also require the
dependency audit. GitHub Actions runs the same commands (see
`.github/workflows/`).

## Commands

```bash
cargo xtask check
cargo xtask check --full
# Standalone audit after changing dependencies:
cargo deny --locked check
```

The basic gate checks the direct dependencies between crates (including dev
and build), formatting, Clippy with warnings treated as errors and
`cargo test --workspace --locked`. It includes binary, library, contract,
local Databricks HTTP and gate tests, plus the `compile_fail` credential
doctests. Clippy also checks the examples. `live` tests are compiled but
ignored in basic mode; Cargo prints their names and counts. No database,
Docker or Databricks profile is needed in this mode.

With dependencies already in the Cargo cache, the basic gate can run offline:

```bash
CARGO_NET_OFFLINE=true cargo xtask check
```

```powershell
$env:CARGO_NET_OFFLINE = 'true'
cargo xtask check
Remove-Item Env:CARGO_NET_OFFLINE
```

The gate stops at the first error, exits with a non-zero code and reports that
later steps were not run. An unavailable command is also a failure; there is
no fallback that skips checks.

## Architecture

- Core does not depend on the application, the drivers, GPUI/Tauri or
  SQLx/reqwest.
- Drivers depend on core and external libraries; they do not depend on the
  application, other drivers or the UI.
- The application composes the drivers; `xtask` is a separate tool.
- The check covers direct Cargo edges, including dev and build dependencies.
  Where logic lives and transitive dependencies still need review.

## Full mode

Requires `cargo-deny` (`cargo install cargo-deny --locked`), Docker with
Compose v2 and access to the advisory database and the images (or suitable
caches).

After the basic gate and the real integration tests, it runs the benchmarks
in `benches/` in quick mode (`cargo bench --bench hot_paths -- --quick`), only
to prove they still work, and then the vulnerability, license and source
audit configured in `deny.toml`. An alert
that blocks the gate must be fixed or receive a documented decision with a
reason and a review date in `deny.toml`, never a silent suppression.

It starts dedicated fixtures, waits for the health checks and runs
`--test live -- --ignored` for each version:

| Database | Image | Local port |
| --- | --- | --- |
| PostgreSQL | postgres:14 | 55424 |
| PostgreSQL | postgres:18 | 55428 |
| MySQL | mysql:8.4 | 55394 |
| MySQL | mysql:9.7 | 55399 |

Services publish only on 127.0.0.1. Database/user: `jykre`; the public,
fixture-only password is `jykre-test`. Images follow these version lines and
are not pinned by digest. Seeds in `tests/fixtures/` create tables with keys,
defaults and comments, a view, a materialized view and a partitioned table
(PostgreSQL), a foreign key (MySQL) and synthetic data. Never use production credentials or data.

Each run creates a Compose project `jykre-gate-<pid>` and tries to remove it,
with its volumes, at the end, even if startup or a test fails. An abrupt
interruption can leave resources behind: run
`docker compose -p <project> -f docker-compose.test.yml down --volumes` only
for that run's project. Ports are fixed, so concurrent full runs can conflict.

The tests cover connection, invalid authentication, exact values and NULL,
empty results, metadata, timeout, cancellation, row limits and stops requested
by the receiver. A session must keep its state after a database error, a
timeout, a cancellation and an interruption by a limit or the receiver; a new
session must run normally. TLS with certificates and effective server-side interruption need
additional scenarios before the guarantees are extended.

## Local databases for manual testing

`docker-compose.dev.yml` starts persistent databases with synthetic data for
opening the app and exploring by hand. They are separate from the gate
fixtures above: other ports, a fixed Compose project (`jykre-dev`) and named
volumes that survive restarts, so both can run at the same time.

```bash
docker compose -f docker-compose.dev.yml up -d --wait
# Stop, keeping the data:
docker compose -f docker-compose.dev.yml down
# Reset to the seed:
docker compose -f docker-compose.dev.yml down --volumes
```

| Database | Image | Host:port | Databases |
| --- | --- | --- | --- |
| PostgreSQL | postgres:18 | 127.0.0.1:55432 | `jykre` (schemas `public`, `catalog`, `sales`) |
| MySQL | mysql:8.4 | 127.0.0.1:55306 | `jykre`, `catalog`, `sales` |

User `jykre`, fixture-only password `jykre-test`, as in the gate. The seeds in
`dev/seed/` run only when a volume is created, so edit them and reset to apply
changes. They create:

- `catalog.product`, `sales.customer`, `sales.orders` and `sales.order_item`
  with foreign keys, an enum, a view (`sales.order_summary`) and a function
  (`sales.customer_total`) for the metadata tree.
- `sales.event_log` with 100,000 rows for row limits, scrolling and
  cancellation; `pg_sleep(n)` or `SLEEP(n)` gives a slow query for timeouts.
- `type_showcase` (in `public`/`jykre`) with one column per common type and
  rows with typical, NULL, extreme, Unicode and long values.

Point the app at an isolated folder so these connections stay apart from your
own, for example `JYKRE_CONFIG_DIR=target/dev-config cargo run`.

## Rules for changes and review

- Test public behaviour and error cases; a bug fix gets a regression test when
  it can be automated. A contract change tests consumers and drivers.
- Do not remove, ignore or weaken tests to pass the gate. Fixing an incorrect
  test requires explaining why in the report.
- Tests use synthetic data and isolated resources, do not depend on order and
  must have a bounded duration. Flakiness requires a record, an owner and a fix;
  retrying until it passes does not replace investigation.
- Ignored tests need a reason and a way to run them. Relational `live` tests
  always run in full mode; authenticated Databricks tests are separate.
- The native vault test runs manually:
  `cargo test -p jykre-secrets --test native -- --ignored`.
- A Databricks change requires, besides the local HTTP tests, a check with an
  authenticated CLI and test workspace. Record commands and results, without
  passwords or tokens.
  Cover warehouse and running classic cluster selection, saved selection on
  reopen, explicit discovery/cancellation, SQL, context reuse and cancellation.
  Local HTTP fixtures cover these protocols, expiration/concurrent token refresh,
  exact classic JSON cells and failed recovery without real credentials.
- UI changes require opening the app and checking the affected flow, tabs,
  content preservation, shortcuts, focus and dividers where relevant. Record
  that check separately. Use `JYKRE_CONFIG_DIR` with an isolated folder and
  only the local fixtures. `JYKRE_DEMO=1` opens the simulated demo.
- Report basic/full/audit as passed, failed or not run (with the reason),
  ignored tests, visual checks, deviations and pending items. Reviewing the
  diff is still required. Do not present a partial run as a passed gate.

Numeric coverage and benchmarks that block on their numbers come after
measuring representative scenarios; for now, prioritize critical contracts, SQL, credentials and the
tab model. Nextest can be evaluated when run time justifies it.

Performance has budgets in [performance](performance.md). Algorithms whose
cost grows with the data get a unit test with a generous time budget (ten
times the measured time or more), so complexity regressions fail the gate; a
change on a measured path reports before and after numbers.

## Lightweight hook

The `.githooks/pre-commit` hook only checks formatting. Enable it per checkout:

```bash
git config core.hooksPath .githooks
```

Passing the hook does not replace the gate or the review. When validating
another operating system, record the platform and the result; a Windows run
does not prove that a Linux graphical session works.
