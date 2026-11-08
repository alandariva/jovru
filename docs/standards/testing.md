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
and build), the SQL catalogs in `catalog/` (`cargo xtask catalogs check`),
formatting, Clippy with warnings treated as errors and
`cargo test --workspace --locked`, then Clippy and the tests of
`jykre-sql-language` without its default features (the built-in formatter
instead of sqruff). It includes binary, library, contract,
local Databricks HTTP, SQLite file (synthetic databases created in the system's
temporary folder, `crates/driver-sqlite/tests/file.rs`) and gate tests, plus
the `compile_fail` credential
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
- `jykre-sql` depends only on core and on no editor engine;
  `jykre-sql-language` only on core and `jykre-sql`; neither on GPUI or
  `lsp-types`. No library file outside `src/app/` and `src/main.rs` uses
  `jykre_sql_language` (a source check).
- `jykre-assistant` depends on core and external libraries only; core never
  depends on `genai`. The assistant's code names no result data, bound
  parameters, sessions, credentials or server reads (`assistant_privacy`,
  a source check; `tests.rs` files may build fixtures with them, and
  `src/assistant/released.rs` alone may name a query's result).
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
`--test live -- --ignored` for each version, first in the driver's crate and
then in `jykre` (`tests/live.rs`, with `JYKRE_TEST_DRIVER` set to `postgres`
or `mysql`), which exports 250,000 generated rows through the application's
backend: the rest of an open result to CSV, a statement run again to XLSX,
and a cancellation that must leave the chosen file as it was. The same file
(`mod automation`) runs the automation engine's SQL Jobs, the experiments of
the [proof of concept](../plans/automation-sql-job-poc.md), through
`automation::Engine` on tables it creates and drops: a write returning more
rows than the limit, the same write past its deadline and never run again, a
job beside a tab's open transaction, and exact values in the job's result:

| Database | Image | Compose service |
| --- | --- | --- |
| PostgreSQL | postgres:14 | `postgres-min` |
| PostgreSQL | postgres:18 | `postgres-max` |
| MySQL | mysql:8.4 | `mysql-lts` |
| MySQL | mysql:9.7 | `mysql-current` |

Services publish only on 127.0.0.1, on a free port Docker picks for each run;
the gate reads it with `docker compose port` and hands it to the tests in
`JYKRE_TEST_PORT`. Database/user: `jykre`; the public,
fixture-only password is `jykre-test`. Images follow these version lines and
are not pinned by digest. Seeds in `tests/fixtures/` create tables with keys,
defaults and comments, a view, a materialized view and a partitioned table
(PostgreSQL, with a partition kept in another schema), a foreign key (MySQL),
an index, a check constraint, a trigger and a routine for inspection, an enum
and a sequence (PostgreSQL), and a login `gate_reader` (same fixture-only
password) that may not use one schema or database, for permission warnings,
with synthetic data. Never use production credentials or data.

Each run creates a Compose project `jykre-gate-<pid>-<suffix>` and tries to
remove it, with its volumes, at the end, even if startup or a test fails. An
abrupt interruption can leave resources behind: run
`docker compose -p <project> -f docker-compose.test.yml down --volumes` only
for that run's project.

To run one live suite by hand, start the fixtures under a project of your own
and pass the published port:

```bash
docker compose -p jykre-manual -f docker-compose.test.yml up -d --wait
docker compose -p jykre-manual -f docker-compose.test.yml port postgres-min 5432
JYKRE_TEST_PORT=<port> cargo test -p jykre-driver-postgres --locked --test live -- --ignored
JYKRE_TEST_PORT=<port> JYKRE_TEST_DRIVER=postgres cargo test -p jykre --locked --test live -- --ignored
docker compose -p jykre-manual -f docker-compose.test.yml down --volumes
```

The tests cover connection, invalid authentication, exact values and NULL,
empty results, metadata, timeout, cancellation, row limits and stops requested
by the receiver. A session must keep its state after a database error, a
timeout, a cancellation and an interruption by a limit or the receiver, also
when the interruption lands halfway through reading a row (MySQL, with rows of
several megabytes handed on slowly, see [Flaky tests](#flaky-tests)); a new
session must run normally. TLS with certificates and effective server-side interruption need
additional scenarios before the guarantees are extended.

## SQL catalogs

`catalog/` holds the SQL dialect catalogs the editor reads at run time
([ADR-0021](../adr/0021-sql-catalogs-read-at-run-time.md)). The basic gate only
checks them, offline. Regenerating them is a task of its own, reviewed in
the diff:

```bash
# PostgreSQL: kwlist.h, pg_proc.dat and pg_type.dat of each release tag (network)
cargo xtask catalogs postgresql REL_14_0,REL_15_0,REL_16_0,REL_17_0,REL_18_0 --verified-through 18.6
# MySQL: the gate's MySQL services in Docker (8.4 and 9.7)
cargo xtask catalogs mysql
# Databricks: Apache Spark's keyword table and function schema (network),
# optionally with exports from a workspace
cargo xtask catalogs databricks v4.0.1 [--keywords sql_keywords.csv] [--functions functions.csv]
```

Downloads are kept in `target/catalog-sources/`, so a second run works
offline. Each generation checks the dialect it wrote.

## Completion scopes

`crates/sql-language/tests/fixtures/scopes.txt` holds 122 statements in the
three dialects, each with the cursor marked, the relations whose columns
completion offers there and the columns CTEs and derived tables make;
`tests/scopes.rs` checks them through `completion::request`, with an index
that holds every relation the cases read, so one read in the wrong query
fails. New behaviour adds cases; a
change that alters a case's answer updates it and says why in the report.

## Concurrent runs

Gates (basic or full) can run at the same time from separate worktrees. Each
run has its own Compose project, its own published ports and its own
application data folder: the gate sets `JYKRE_CONFIG_DIR` to an empty
temporary folder for the tests and the benchmarks, fails if anything is
written there, and removes it at the end. The user's real `config.db`,
metadata caches and logs are never used.

Two runs in the same checkout share `target/`: Cargo makes the second wait
for the first, and the benchmark output in `target/criterion` is shared.
Concurrent runs also slow each other down, so benchmark numbers taken then are
not comparable.

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
  must have a bounded duration. They never call
  `connection_store::data_dir()` or touch the application data folder: open
  repositories in a temporary folder with a unique name, and bind servers to
  port 0. Flakiness requires a record (in [Flaky tests](#flaky-tests)), an
  owner and a fix; retrying until it passes does not replace investigation.
- Updates are tested without reaching GitHub (`jykre::update::tests`): a
  server on `127.0.0.1` serves the release list, the redirect and the
  archive, and archives are signed in the test with a key made from a fixed
  seed. The committed release key never signs anything in a test. The whole
  flow against real signed pre-releases is a manual check on Windows and
  Linux, recorded with the release
  ([ADR-0033](../adr/0033-update-check-and-signed-self-install.md)).
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
- The AI assistant is tested against a local fake of an OpenAI-compatible
  server (port 0, synthetic text, a synthetic key): streaming, tool calls,
  token counts, statuses classified without their bodies, cancellation
  closing the connection and the silence and round limits
  (`crates/assistant`). `src/assistant/tests.rs` runs **Fix**, **Write** and
  **Ask** through the agentic loop against the fake and a temporary
  metadata cache, and proves that a result value and a bound `:name` value
  never reach a request, that every reviewed text is in what the provider
  received, and that rounds are never logged with their text or the key.
  The same file runs `run_query` on a synthetic SQLite file of 60 rows,
  answered as the user would (send with a column hidden, withhold,
  decline): a hidden column's value, a withheld result and a declined
  query never reach a request, a declined query never runs, at most 50
  rows are sent, and a call with two statements is refused before the user
  is asked. The loop's tests (`src/assistant/run.rs`) prove that nothing is
  left out of a request on its own, that each request knows what the last
  sent one carried, that pruning keeps the calls and stops a sent value
  from being sent again, and that a summary replaces the conversation only
  when the model answered with one.
  Real providers are checked by hand: record the provider, model and
  outcome, never the key.
- Telemetry ([ADR-0029](../adr/0029-otlp-export-of-logs-and-traces.md)) is
  tested with the SDK's in-memory exporters behind the same bridges and
  filters as the application (`jykre::telemetry::testing::capture`): a run
  with a synthetic password, SQL text, values and host exports none of them,
  only allowlisted attributes, and its logs carry the run's trace. The OTLP
  transport is tested against a fake collector on port 0 (protobuf, the
  configured headers, a 401 as an authentication failure) and a closed gRPC
  port. A real collector is checked by hand, with an isolated
  `JYKRE_CONFIG_DIR`.
- **Automation** ([specification](../plans/automation-engine-specification.md),
  section 11) is tested without a window, a server or the user's data. Every
  engine test opens a unique temporary data folder (never
  `connection_store::data_dir()`) and uses synthetic data:
  - `src/automation/tests.rs`: the engine through a scripted driver that
    answers as the test says (pages, errors, hangs, a lost connection) and
    synthetic SQLite files. They cover the completion rule, truncation, the
    deadline, Stop, arbitration, the queue and its limits, durable admission,
    a history that cannot be written (nothing sent; a terminal state never
    written means no replay), recovery, a second engine on the same folder,
    the quit, declared outputs, chains (a global limit of 1, a failed step,
    `plan_changed`, input mapping, published versions) and CA-13: sentinel
    values are searched for in every byte of `automation.db` and in the log.
  - `crates/storage/src/automation_tests.rs`: each migration keeps every row
    (seeded at each earlier version), a newer file is refused untouched, a
    corrupt one is set aside and recreated, the lock is exclusive and a
    process killed while holding it releases it (a child re-executes the test
    binary), recovery marks only `running` rows, pruning expires values and
    deletes old rows, outputs are stored all or none, history merges by
    keyset, deleted values are overwritten in the file (`secure_delete`), and
    no input value reaches the file.
  - `src/automation/process/tests.rs`: programs. The test binary **starts
    itself again as the child program**: `AUTOMATION_TEST_CHILD` (passed
    through the job's `environment.set`) selects a mode in
    `process/test_child.rs` (exit code, print, spam, `sleep`, a process
    tree, an orphan that holds the pipes, the `jjp/1` modes), so the same
    tests run on every platform without `sh`. They cover the environment
    denylist, no shell, `.bat`/`.cmd` refusal, the pinned SHA-256, Stop, the
    deadline and the 5 s grace with shortened timings, grandchildren killed,
    the bounded logs and a left-over group found by its start time.
  - `src/automation/jjp/tests.rs`: the protocol over a real loopback
    connection, no child needed: framing across chunks and UTF-8
    boundaries, the line cap, invalid and reused tokens, `hello` once,
    conflicting terminals, disconnects, `cancel` while the program reads
    nothing, the method allowlist, quotas, and tokens and values absent from
    logs and `Debug`.
  - The Node.js example (`examples/jjp/export.js`) has an end-to-end test,
    `automation::process::tests::the_node_example_exports_and_completes`.
    It is `#[ignore]`d with the reason "needs node on PATH" because CI
    runners are not required to have Node.js; run it with
    `cargo test --lib automation::process::tests::the_node_example -- --ignored`.
    Not running it is reported, not hidden.
  - Windows (Job Objects, no console window, `PATHEXT`, killing grandchildren,
    the lock after a kill) and the visual flows are checked by the
    maintainer with the
    [Windows checklist](../plans/releases/0.3.0-windows-checklist.md); their
    results are recorded there, and a Windows behaviour is not claimed before.
  - `examples/automation_drain.rs` measures the completion rule on a driver
    (SQLite by default, the local fixtures with `JYKRE_CONNECTION`); it is a
    measurement, not part of the gate.
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

## Flaky tests

A test that fails without a change to what it covers is recorded here when
it is seen, with an owner, and keeps its record after the fix, with the cause
and the evidence.

### `live_interrupting_a_half_read_row_keeps_the_session` (MySQL)

- **Seen:** 2026-11-08, while gating a MySQL driver fix: 5 failures in 12
  runs against MySQL 8.4 on the unchanged driver, on a loaded machine.
- **Owner:** the maintainer.
- **Status:** Fixed on 2026-11-08.
- **Cause:** every failure was `unwrap_err()` on an `Ok`: the statement read
  its whole 128 MiB result, neither the cancellation nor the 50 ms time limit
  landed, and recovery was never reached. SQLx reads with `try_read`, which
  skips Tokio's cooperative budget, so while the socket holds more of the
  result the session's task never gives its thread up. Unloaded, the client
  outruns the server and its reads wait inside a row, where the interruption
  lands; under load the server, behind Docker's port forwarding, stays ahead.
  Two things then went wrong:
  - In the test: on its single-threaded runtime, the sink that cancels runs
    only when the session's task waits, so the cancellation never landed.
  - In the driver: the time limit was a Tokio timer, which fires only when a
    worker turns Tokio's time driver, and with the session's task holding
    its thread none did, on the application's multi-threaded runtime too.
    The same test on that runtime read all 32 rows after its 50 ms limit had
    passed and returned `Ok`, after up to 1.06 s.
- **Fix:**
  - The MySQL driver reads a passed deadline from the clock before every
    read and every row it hands on (`Limit::reached`), as it already did for
    the cancellation, so a time limit stops the statement at its next row.
    `a_passed_deadline_stops_the_statement_without_the_timer` covers it
    without a database.
  - The test reaches the fixture through a relay on `127.0.0.1` (`SlowLink`)
    that hands the server's bytes on at 32 MiB/s, so the client's reads wait
    inside a row and both interruptions land there whatever the load. The
    test still proves what it did, a session kept after an interruption
    halfway through a row, now under load too; it takes 5 to 9 s instead of
    1 to 3 s.
  - The PostgreSQL driver had a wider form of the same gap: it saw the
    cancellation and the time limit only when a page's reads waited. It now
    checks both before every read (`Limit::passed`), and
    `a_page_that_never_waits_still_stops_at_its_limit` covers it. A scratch
    test with the same 32 rows on the multi-threaded runtime, 30 statements
    of each kind: under the heaviest load, 8 cancellations (a median of 31
    rows read after them) and no 50 ms time limit landed before the fix, and
    all of both after it, the cancellations within one row; unloaded, 29
    and 10 landed before, all after.
  - The SQLite driver's time limit was the same timer, and SQLx's row
    channel does not make the reading wait while the connection's thread
    is ahead. It reads the clock as MySQL does now, covered by its own
    `a_passed_deadline_stops_the_statement_without_the_timer`. A scratch
    test with 32 rows of 4 MiB, generated or stored: unloaded, on the
    single-threaded runtime, no 50 ms time limit of 60 landed before the
    fix and all after it; on the multi-threaded runtime all had landed
    before too, since the reading waits for SQLite's thread often enough.
- **Evidence:** a manual MySQL 8.4 fixture on 8 cores. The unchanged test
  and the fixed one ran alternately, so both saw the same load: a release
  build of the workspace, started again as it ended, alone or with 8 busy
  loops.

  | Load | Unchanged | Fixed |
  | --- | --- | --- |
  | None | 24 of 24 passed | 12 of 12 passed |
  | Build (load average 6 to 14) | 51 of 60 passed | 30 of 30 passed |
  | Build and busy loops (load average 16 to 18) | 1 of 60 passed | 30 of 30 passed |

  Under the heaviest load, with the driver temporarily instrumented, the
  read was waiting in all 50 interruptions of the fixed test, and recovery
  ended with at least 4.3 s of its 5 s left. The unchanged test with only
  the driver fix still failed 10 of 10 times, at the cancellation. Run on
  the multi-threaded runtime, it passed 10 of 10 times with the driver fix
  and 0 of 15 without it; the time-limited statement then ended in 140 to
  373 ms.

## Lightweight hook

The `.githooks/pre-commit` hook only checks formatting. Enable it per checkout:

```bash
git config core.hooksPath .githooks
```

Passing the hook does not replace the gate or the review. When validating
another operating system, record the platform and the result; a Windows run
does not prove that a Linux graphical session works.
