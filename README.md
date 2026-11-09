<img src="assets/icon/jykre.svg" alt="" width="96">

# Jykre Studio

[![CI](../../actions/workflows/ci.yml/badge.svg)](../../actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A native desktop workspace written in Rust and [GPUI](https://www.gpui.rs/) to
register PostgreSQL, MySQL, Databricks and SQLite connections, explore metadata
and run real SQL.

> **Status:** early prototype; `0.3.0` adds automation (SQL Jobs, external
> programs, chains and schedules while the application is open), editing data
> in the result grid, importing CSV and Excel files into a table, shortcuts
> you choose, and grid columns that can be hidden, moved, frozen and found.
> `0.2.0` made daily use reliable, with updates from signed releases. Windows is the primary platform; Linux is validated
> since 0.2.0 (Ubuntu 24.04 on X11). The AI assistant talks to Anthropic, OpenAI, an
> OpenAI-compatible server or Ollama, once set up in Preferences; it sees
> SQL, errors, row counts and structure, and values only from a query it
> asked for, after you approve the query and send its result.

## Requirements

- Rust stable 1.95 or newer (`rustup update stable`).
- Windows 10/11, or Linux (validated on Ubuntu 24.04 with X11; Wayland and
  macOS builds go through GPUI but are not validated). On Linux install the
  GPUI system packages first: `pkg-config libxkbcommon-dev
  libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libxcb1-dev
  libfontconfig-dev libfreetype-dev libvulkan-dev libasound2-dev
  libdbus-1-dev libssl-dev` on Debian and Ubuntu (as the CI workflow does),
  plus a Vulkan driver; the file dialogs need `xdg-desktop-portal` with a
  backend such as `xdg-desktop-portal-gtk`, which desktops ship. The
  published Linux binary is built on Ubuntu 24.04 and needs glibc 2.39 or
  newer.
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

### Linux applications menu and icon

The Linux release archive includes `install.sh`, which adds Jykre Studio
to the desktop's applications, with its icon, for the current user (under
`~/.local/share`). The window then shows the icon on Wayland and X11: the
desktop matches it to the entry by its app ID,
`io.github.alandariva.JykreStudio`. Run the script from the extracted folder,
and again after moving it; `./install.sh --uninstall` removes the entry.
Started without the entry, the window keeps the desktop's generic icon.

### Updates

Once a day Jykre Studio asks GitHub whether a newer version exists,
pre-releases included; a newer one shows in the status bar. **Preferences ›
About › Updates** turns the check off, checks at once (**Check now**) and
installs (**Download and install**, then **Restart now**). The request is
anonymous: GitHub sees your address and the version, nothing else. Nothing
is downloaded without your click, and only an archive signed with the
release key (`assets/update/release-key.pub`) is installed, by replacing the
files of the folder it ships in; your own files there and the data folder
are left alone. A development build, a folder you cannot write to or
another system is offered the release's page instead. The folder keeps its
name; on Linux run `./install.sh` again only if you moved it.

To check a downloaded archive by hand:

```bash
cargo install --locked zipsign --version 0.2.2
zipsign verify tar jykre-studio-v0.2.1-linux-x86_64.tar.gz release-key.pub
```

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
  chosen file as it was. See [Results](docs/reference/results.md#export).
- The metadata cache is stored locally, read page by page and browsable offline
  after a restart. Search also queries the local cache.
- Double-click a table or view (or press Enter) for a new query on it, with
  escaped identifiers and the connection's row limit; run it when ready.
  `Ctrl Shift P` searches real tables and `Ctrl N` opens a query on the active
  connection. `Ctrl Enter` runs the selection, or the statement at the cursor
  when nothing is selected, in place of the result tab on screen (pin a
  result tab to keep it); `Ctrl Shift Enter` does the same in a new result tab and `Ctrl Alt Enter` runs every
  statement, one result tab each. Results are read page by page, and the
  statement timeout is chosen in the editor toolbar. Open tabs and history are
  kept between sessions; a tab's unsaved text survives a crash in a recovery
  file. `Ctrl F2` stops a run, as the Stop button does.
  Each tab keeps its own session between runs, so `USE`, `SET`, temporary
  tables and transactions carry over; a failed statement keeps it. The
  toolbar shows the session's transaction as PostgreSQL, MySQL and SQLite
  report it, including one opened in a procedure or rolled back by the
  server; whether a MySQL or SQLite transaction changed data is inferred
  from the statements the tab ran, and Databricks shows no transaction
  state. Nothing is committed or rolled back without saying so: a tab whose
  session was lost while holding a transaction runs nothing until you run
  again, closing a tab or the application with open transactions asks
  first, and a report panel that leaves a transaction open has it rolled
  back and says so. Limits: a row limit per
  statement set in each connection (1,000 by default, up to 100,000), 256 MB
  per result and a 300 s timeout. The run button can cancel.
- Tabs keep their SQL. Moving a tab to another group changes its connection and
  cancels a running execution. Review the context before running.
- **Saved queries are plain `.sql` files.** `Ctrl S` saves a tab to its file
  (or asks where, starting in `Documents/Jykre Studio/Queries`), `Ctrl Shift
  S` saves elsewhere and `Ctrl O` opens any file; the file name is the title,
  and nothing but SQL is written into it, so the folder can be backed up,
  edited elsewhere or kept in Git. The library lists the folder's files (the
  folder is chosen in **Preferences › Editor**); tags and each file's
  connection stay on this computer. A file changed on disk is noticed before
  it is overwritten, and closing a tab or the application with unsaved edits
  offers to save them. Saved queries of `0.1.0` move to files at the first
  start, with a backup of the originals.
- **Reopen closed query tab** (`Ctrl Shift T`, or the title bar's button)
  brings back the tabs closed in this session with their SQL, connection,
  context and `:name` fields; it runs nothing.
- **Favorites**: **Add to favorites** in the explorer's menu of a connection,
  table or view keeps it above the connections; a table or view opens as
  **View data**.
- Beside **Connected**, a lock shows that the session is encrypted (with the
  TLS version) and an amber **Not encrypted** that it is not, as the server
  reports it, even when the profile only preferred TLS.
- **Format** runs off the UI thread; past 2 s its button shows the time and
  cancels it.
- Result headers show `name : type`, with the SQL type in a neutral tone.
  Values use colours and alignment by type; NULL, empty text and whitespace
  are distinct. Click a cell, then Shift+click or Shift+arrows to select a
  block (click the row number for a whole row, `Ctrl A` for everything).
  `Ctrl C` copies the block as tab-separated text that pastes into
  spreadsheets; the context menu also copies it with headers. **pt-BR
  numbers** groups digits without converting decimals or integers to floating
  point. CSV copy keeps the original values regardless of presentation.
- **Editing results.** In a query tab, a result read from one table with a
  primary key (or a unique key on NOT NULL columns) can be edited: `F2`
  edits a cell, `Ctrl Shift N` sets it to NULL, `Alt Insert` adds a row and
  `Delete` marks rows to delete. Nothing is written until **Write…**
  (`Ctrl Alt S`) shows every change with its statements; values are bound by
  the driver, each change must touch exactly one row or the whole write rolls
  back, and inside an open transaction the changes stay uncommitted. On a
  connection marked Production it asks first. On Databricks, **Generate
  SQL…** opens the statements in a new tab instead. Any other result says why
  it is read-only. See [Results](docs/reference/results.md#editing).
- **Import from file…** in the explorer's menu of a table reads a CSV or an
  `.xlsx` sheet: choose the delimiter, encoding and header row, map the
  file's columns to the table's with a preview, and run it, all or nothing,
  with progress and **Cancel**. **New table from this file…** drafts a
  `CREATE TABLE` in a new tab. Not offered on Databricks.
- **Grid columns.** `Shift F10` on a header (or a right click) hides, moves
  or freezes a column at the left edge; dragging a header moves it, and a
  line says how many columns are hidden. `Ctrl Shift F` finds a column by
  name, ignoring case and accents. Copying, exporting, the records view and
  the statistics follow the visible columns in their order; the layout
  belongs to the result tab.
- **SQL editor.** `Ctrl F` searches, `Ctrl H` replaces, `Ctrl Alt ↑`/`↓`
  (`Shift Alt ↑`/`↓` on Linux) adds a cursor, and `Ctrl G` goes to a line
  (`12` or `12:5`). Errors are marked where PostgreSQL, MySQL or Databricks
  say they are.
- **Shortcuts you choose.** **Preferences › Shortcuts** changes the key of
  any action by pressing it, names the action that already uses a key before
  taking it, and resets one action or all of them.
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

## Automation

Automation (a section of the rail) saves work you run by hand and keeps a
history of what ran and how it ended. It runs only while Jykre Studio is
open, and **nothing runs when the application is closed**. Each job opens in
a tab of its own with its **Editor**, its run form under **Inputs** and its
history under **Runs**; **Runs** in the section lists every run, with the
ones active or queued on top.

- **SQL Jobs.** One statement on a connection, with typed inputs filled in a
  form before each run (bound by the database driver, never written into the
  SQL), a timeout and a row limit. A job that writes is read to the end of
  its result before it counts as done. If it cannot be confirmed, the run
  says `effect_unknown`; a run is **never replayed** by the application.
- **External programs.** A program you name, started without a shell, with
  its arguments, its working folder and environment. **Stop** and the timeout
  end the program and everything it started. `.bat` and `.cmd` files are not
  accepted.
- **`jjp/1`.** A small protocol over a local connection for a program that
  takes inputs and returns outputs. The Node.js program in
  [`examples/jjp/`](examples/jjp/README.md) shows it in under 200
  lines; there is no SDK.
- **Chains.** A SQL Job can feed an external program: a chain runs published
  versions of jobs in order, passes declared outputs on to the next step and
  stops at the first step that fails. Outputs are kept only for jobs you
  allow it for, small, for 7 days.
- **History.** Every run records its state, cause, counters and times in
  `automation.db`, per job and in a list of all runs, for 90 days. It never
  stores input values, result cells or server messages.
- **Schedules.** A job or chain can run every few minutes or hours, or at
  times of the day on chosen days, in the time zone you choose, after you
  consent to what it runs, on which connection and environment; any change
  to them suspends it until you confirm again. Times missed while the
  application was closed are not run unless you choose to catch them up
  once or up to N times, and a time that finds the previous run still going
  is skipped unless you choose otherwise. A scheduled run never asks for a
  password: one that is not remembered fails it.
- Up to 2 runs are active at a time (1 per connection); more wait in a queue
  of at most 8 for up to 60 s.

What to know before using it:

- **There is no sandbox.** An external program is code you trust: it runs
  with your privileges and can read your files and use your network. Studio
  supervises it; it does not contain it. It inherits Studio's environment
  (secrets in it included) except `JYKRE_*`, `OTEL_*`, `NODE_OPTIONS` and the
  `PYTHON*` variables that load code.
- **A program's output is kept on this computer.** stdout, stderr and its
  `jjp/1` log messages are written to files under `automation/logs/` in the
  data folder, at most 1 MiB each, for 7 days. Never print passwords, tokens
  or personal data from a program. **Delete logs** removes them sooner.
- **SQL Jobs on Databricks are not validated against a workspace**; they are
  covered by local tests only.
- **macOS is not validated** for Automation. Windows and Linux are the
  validated platforms, and Windows is checked by a checklist.
- Only one Jykre Studio window runs automation on a data folder; a second one
  shows it read-only until the first closes. Keep the data folder on a local
  disk.

See [Persistence](docs/reference/persistence.md#automation-automationdb),
[Security](docs/standards/security.md#automation-external-programs-and-jjp1)
and [Reliability](docs/standards/reliability.md#automation).

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
| Connections, preferences, report definitions, saved queries' tags and connections, explorer favorites | `config.db` in the data folder |
| Saved queries | `.sql` files in `Documents/Jykre Studio/Queries`, or the folder chosen in Preferences (`<JYKRE_CONFIG_DIR>/Queries` when that variable is set) |
| Unsaved text of open tabs | `recovery/` in the data folder, until saved, discarded or closed |
| A report's print document | Only where you save it with **Export for printing or PDF…** |
| Metadata cache | `metadata/<connection-id>.db` in the data folder |
| Automation jobs, versions and run history (90 days) | `automation.db` in the data folder |
| An external program's stdout, stderr and `jjp/1` log (7 days, at most 1 MiB each) | `automation/logs/<run-id>/` in the data folder |
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

See [architecture, configuration and adding drivers](docs/reference/drivers.md).

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

See the [testing policy](docs/standards/testing.md) for prerequisites, offline runs, the
pre-commit hook and manual UI/Databricks checks. The same gate runs in GitHub
Actions on every push and pull request.

The [development documentation](docs/README.md) covers architecture, coding
and UI guidelines, performance budgets, persistence, security, reliability,
the workflow and the architecture decision records.

The official icon is [`assets/icon/jykre.svg`](assets/icon/jykre.svg); the
title bar draws it. [`assets/icon/jykre.ico`](assets/icon/jykre.ico) is the
same drawing rasterized with resvg at 16 to 256 px, one image per size, that
`build.rs` embeds in the Windows executable for its window, the taskbar and
Explorer, with the Windows SDK's resource compiler (part of the MSVC build
tools Rust already needs). On Linux, the release job ships the SVG with the
desktop entry and the script in [`assets/linux/`](assets/linux).

## Security

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE).
