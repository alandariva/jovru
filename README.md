# Jykre Studio

[![CI](../../actions/workflows/ci.yml/badge.svg)](../../actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A native desktop workspace written in Rust and [GPUI](https://www.gpui.rs/) to
register PostgreSQL, MySQL, Databricks and SQLite connections, explore metadata
and run real SQL.

> **Status:** early prototype (`0.1.0`). Windows is the primary and only
> validated platform. The AI assistant talks to Anthropic, OpenAI, an
> OpenAI-compatible server or Ollama, once set up in Preferences; it sees
> SQL, errors, row counts and structure, never values.

## Requirements

- Rust stable 1.95 or newer (`rustup update stable`).
- Windows 10/11. Linux and macOS builds go through GPUI but are not validated;
  on Linux install the GPUI system packages first (see the CI workflow).
- Optional: Docker with Compose v2 and `cargo-deny` for the full test gate.
- Optional: the [Databricks CLI](https://docs.databricks.com/dev-tools/cli/)
  with an authenticated profile for Databricks connections.

## Quick start

```bash
git clone <repository-url>
cd <repository>
cargo run --release --locked
```

The first build downloads and compiles all dependencies. Later builds can run
offline with `--offline` once the Cargo cache is populated.

To open the earlier visual prototype with simulated data only:

```bash
JYKRE_DEMO=1 cargo run --locked          # bash
```

```powershell
$env:JYKRE_DEMO='1'; cargo run --locked  # PowerShell
```

Demo connections and results are labelled as such and never reach a server.

## Features

- Add a connection with `+` in the explorer. **Test connection** reaches the
  server; saving validates the configuration and starts reading metadata.
- PostgreSQL/MySQL take host, port, database, user, password and TLS mode.
  Databricks uses an authenticated CLI profile. Choose SQL Warehouse or classic
  cluster, then **Load compute** to select a resource by name/ID. Reopening the
  connection restores the saved selection. A compute is required only for SQL;
  classic all-purpose clusters must already be running.
  **Load CLI profiles** populates the profile select from the Databricks CLI;
  profile discovery skips authentication until the selected profile is used.
  SQLite takes the full path of an existing database file (**Browse…** opens
  the system's file dialog) and an optional **Open read-only**; it needs no
  password, and a missing file is never created.
- **Remember password** is optional and uses the operating system vault
  (Windows Credential Manager, macOS Keychain, Linux Secret Service). Without
  it, the password lives only in the session. If the vault fails, the UI says
  so; there is no fallback to a password file.
- **Delete connection…** in a connection's context menu (or `Delete` on it)
  removes it, with its remembered password and metadata cache, after a
  confirmation. Its open tabs keep their SQL, without a connection.
- `↻` on a connection refreshes everything; **Refresh** in the context menu
  (right click, or `F5` on a focused node) of a database, catalog, schema,
  table, view or column refreshes only that node and its descendants. While a
  refresh runs, its `↻` spins; clicking it cancels. Cancellation and failures
  keep the last good read, and permission warnings
  keep the parts that could not be read. Each scope keeps its own state and
  timestamp.
- **Export** (the download button, or `Ctrl Shift E`) writes the whole
  result to CSV, Excel, JSON, JSON Lines, SQL `INSERT`s or Markdown, not only
  the page on screen. It reads the rest of the result from the server when
  the session still has it, or runs a read-only statement again after
  asking; the file appears only once complete, and cancelling leaves the
  chosen file as it was. See [Results](docs/results.md#export).
- The metadata cache is stored locally, read page by page and browsable offline
  after a restart. Search also queries the local cache.
- Double-click a table or view (or press Enter) for a new query on it, with
  escaped identifiers and the connection's row limit; run it when ready.
  `Ctrl Shift P` searches real tables and `Ctrl N` opens a query on the active
  connection. `Ctrl Enter` runs the selection, or the statement at the cursor
  when nothing is selected, in place of the result tab on screen (pin a
  result tab to keep it); `Ctrl Shift Enter` does the same in a new result tab and `Ctrl Alt Enter` runs every
  statement, one result tab each. Results are read page by page, and the
  statement timeout is chosen in the editor toolbar. Open tabs, saved queries
  and history are kept between sessions.
  Each tab keeps its own session between runs, so `USE`, `SET`, temporary
  tables and transactions carry over; a failed statement keeps it. Limits: a row limit per
  statement set in each connection (1,000 by default, up to 100,000), 256 MB
  per result and a 300 s timeout. The run button can cancel.
- Tabs keep their SQL. Moving a tab to another group changes its connection and
  cancels a running execution. Review the context before running.
- Result headers show `name : type`, with the SQL type in a neutral tone.
  Values use colours and alignment by type; NULL, empty text and whitespace
  are distinct. Click a cell, then Shift+click or Shift+arrows to select a
  block (click the row number for a whole row, `Ctrl A` for everything).
  `Ctrl C` copies the block as tab-separated text that pastes into
  spreadsheets; the context menu also copies it with headers. **pt-BR
  numbers** groups digits without converting decimals or integers to floating
  point. CSV copy keeps the original values regardless of presentation.
- **Reports** (a section of the rail) gather panels, each on its own
  connection and database (schema, catalog, or another PostgreSQL database
  when the connection offers them), chosen from the panel's header. A panel
  shows a table, a bar, line or pie chart, or a single number, at half or
  full width, and is dragged by its grip to another place. The panels share
  `:name` parameters filled in before each run (text, number, true or false,
  date, date and time, or one of a list; bound by the database driver, never
  written into the SQL), and the panels of different databases or
  connections run at the same time. A report tab has a tab group of its own.
  The SQL editor completes names like a query tab, for the panel's
  connection. Create a report with **+**, or from a query tab with **Save as
  a new report** or **Add to a report…**, which adds a panel to any saved
  report. Copy a panel's result as CSV or for a spreadsheet, or export a
  print document (HTML) with every panel that the browser prints or saves as
  PDF. Parameters need a SQL warehouse on Databricks.
- `Ctrl B` toggles the explorer and `Ctrl Shift B` the assistant. Dividers and
  the SQL font size are adjustable.

## Languages

The interface is available in English and Brazilian Portuguese (`pt-BR`).
Every UI string goes through the `tr!` macro (`src/i18n.rs`), so adding a
language needs no code changes beyond registering its catalog. The locale
comes from `JYKRE_LOCALE` (for example `pt-BR`) or the OS language, falling
back to English.

To add a language:

1. `cargo xtask i18n pt-BR` creates or refreshes `locales/pt-BR.json` with every
   message; run it again after UI changes to pick up new strings.
2. Fill in the values. Empty values fall back to English. Keep the positional
   placeholders (`{0}`, `{1}`), which may be reordered.
3. Add `("pt-BR", include_str!("../locales/pt-BR.json"))` to `CATALOGS` in
   `src/i18n.rs`. A unit test checks that each catalog parses and keeps the
   placeholders.

Error messages produced by the `jykre-storage` and `jykre-secrets` crates and
the demo data are not translatable yet.

## Local data and credentials

| Data | Location |
| --- | --- |
| Connections, preferences, report definitions | `config.db` in the data folder |
| A report's print document | Only where you save it with **Export for printing or PDF…** |
| Metadata cache | `metadata/<connection-id>.db` in the data folder |
| Copies of `config.db` (the five most recent, one per start) | `backups/` in the data folder; offered for restore when `config.db` cannot be opened |
| Passwords (only with **Remember password**) | OS vault, service `JykreStudio` |

The data folder follows each platform's convention:

| Platform | Data folder |
| --- | --- |
| Windows | `%LOCALAPPDATA%\JykreStudio` |
| macOS | `~/Library/Application Support/JykreStudio` |
| Linux | `$XDG_DATA_HOME/jykre-studio` (default `~/.local/share/jykre-studio`) |

Set `JYKRE_CONFIG_DIR` to use another folder (for example, an isolated test
folder). Only when no home directory can be found does the app fall back to
`.jykre/` in the working directory.

Logs are written to `logs/` inside the data folder as JSON lines, one file per
day, keeping the last seven; **Open log folder** in Preferences shows them.
`JYKRE_LOG` changes the level (for example
`JYKRE_LOG=jykre=debug`); SQL text is logged only at `trace` and passwords
never. `JYKRE_PROBE=1` adds frame-time measurements to the log.

Telemetry is off by default. **Preferences › Diagnostics › Telemetry** can
send the log and traces of every external call (databases, Databricks, the
AI provider, the system vault, files) to an OpenTelemetry collector over OTLP
(HTTP/protobuf or gRPC), logs and traces each on their own. Only metadata
leaves: operations, database names, durations, row counts and error codes;
never SQL, values, credentials, file paths or connection settings. Headers
for the collector's authentication are kept in the system vault.

Credentials are never serialized: SQLite and JSON files hold only non-sensitive
configuration. Databricks tokens are resolved by the CLI at runtime and kept in
memory. The SQLite files are not encrypted; they contain object names and types
but no query results. Their protection at rest is that of your OS account and
disk. A legacy `connections.json` is migrated in a single transaction and
deleted only after the import is verified.

## Drivers

The GPUI-independent backend implements PostgreSQL and MySQL with SQLx, SQL
execution in sessions kept per tab and metadata via `information_schema`.
Databricks uses CLI profiles, Statement Execution for warehouses, Command
Execution for classic all-purpose clusters, and Unity Catalog REST for metadata.
SQLite opens a database file through SQLx's bundled SQLite and reads metadata
from its pragma functions. `jykre::drivers::builtin_registry()` registers the
four adapters.

```bash
cargo run --example drivers --locked
```

To connect from the terminal (PowerShell):

```powershell
$env:JYKRE_CONNECTION = '{"driver":"postgres","options":{"host":"localhost","database":"commerce","user":"postgres","sslMode":"require"}}'
# Provide JYKRE_PASSWORD through the environment, never inside the JSON.
$env:JYKRE_SQL = 'SELECT 1 AS example'
cargo run --example connect --locked
```

For Databricks use `"driver":"databricks"` and `"options":{"profile":"DEV"}`.
For SQLite use `"driver":"sqlite"` and `"options":{"path":"C:\\data\\shop.sqlite"}`.
SQL also needs `warehouseId`, or `"compute":"cluster"` with `clusterId`;
metadata does not. `JYKRE_METADATA=1` enables
metadata reading in the example. Nothing is persisted.

See [architecture, configuration and adding drivers](docs/drivers.md).

## Grouped tabs component

`jykre::grouped_tabs::GroupedTabs` is a reusable GPUI component with a
continuous group bar, global selection, closing, drag and drop and independent
content per tab.

```rust,ignore
use jykre::grouped_tabs::{GroupedTabs, GroupedTabsEvent};
use gpui_kit::*;

let tabs = cx.new(GroupedTabs::new);
let group = tabs.update(cx, |tabs, cx| {
    tabs.add_group("My group", rgb(0x477de6).into(), cx)
});
let panel = cx.new(|cx| MyPanel::new(window, cx));
let tab = tabs.update(cx, |tabs, cx| {
    tabs.add_tab(group, "My tab", panel, cx)
})?;
```

Content can be any `Entity<V>` where `V: Render`, or an `AnyView`. Subscribe
to `GroupedTabsEvent::NewTabRequested(group)` to provide content when `+` or an
empty group header is clicked, and keep the `Subscription` alive while you want
events.

- `add_group` / `remove_group`: control groups from the application.
- `add_tab` / `close_tab` / `select_tab`: control tabs by stable ID.
- `move_tab(tab, group, index, cx)`: the index is the position **after removing
  the tab from its origin**; larger indices append at the end.
- `active_tab`, `group_ids`, `tab_ids`, `tab_group` and `tab_content`: query state.
- `SelectionChanged`, `TabClosed` and `TabMoved`: notify changes already applied.

Adding or moving a tab selects it. Closing the active tab selects the next tab
in the group, then the previous one and, if the group is empty, the first tab of
the other groups. Removing a group emits `TabClosed` for each removed tab. IDs
are local to an instance; drags between instances are ignored. Operations with
unknown IDs return `TabsError` without changing the layout. Tab content
entities are kept while open; switching or moving does not recreate them.

Drag a tab over the left or right half of another tab to choose the position;
dropping on a group header appends to that group. Arrows, Home and End navigate
when the bar has focus. The search button lists all open tabs, searchable by
title or group, ignoring case and accents.

```bash
cargo run --example grouped_tabs --locked
```

## Development

The local gate runs the architecture check, formatting, Clippy and the
workspace tests, including doctests. Full mode adds the dependency audit and
real integration tests against disposable Docker fixtures:

```bash
cargo xtask check
cargo xtask check --full
```

See the [testing policy](docs/testing.md) for prerequisites, offline runs, the
pre-commit hook and manual UI/Databricks checks. The same gate runs in GitHub
Actions on every push and pull request.

The [development documentation](docs/README.md) covers architecture, coding
and UI guidelines, performance budgets, persistence, security, reliability,
the workflow and the architecture decision records.

## Security

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE).
