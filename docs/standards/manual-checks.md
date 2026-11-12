# Manual checks

What the gate cannot prove (the operating system, the native window, the
keyboard) is checked by hand in the application on Windows and Linux; macOS
is not validated. Run the groups a release touches. Record the commit, the
operating system, the scaling and the date, and mark each check **Passed**,
**Failed** (with what happened) or **Not run** (with why). A behaviour that
did not pass is not claimed for that platform. A fix made after a check is
reported as not checked again unless it was.

Setup: a release build; `JOVRU_CONFIG_DIR` on an empty folder, `JOVRU_LOCALE=en`,
synthetic SQLite files and the [dev fixtures](testing.md#local-databases-for-manual-testing).
Programs are harmless: `ping -n 300 127.0.0.1` runs long, and a grandchild is a
program that starts another, such as
`powershell.exe -NoProfile -Command "Start-Process ping -ArgumentList '-n','300','127.0.0.1' -NoNewWindow; Start-Sleep 300"`.
Never `cmd.exe /c` (`.bat` and `.cmd` are refused). Keys sent through the
window's message queue (an agent driving the window) are not the physical
keyboard.

## Windows: gate and programs

| Check | What to do | Expected result |
| --- | --- | --- |
| Gate | `cargo xtask check` (`--full` for a release) | Passes, including the programs that re-run the test binary, `jjp/1` and the lock test |
| Node example | `cargo test --lib automation::process::tests::the_node_example -- --ignored` with Node.js 18+ | Passes, or **Not run** with the reason |
| Stop, process tree | Run a program with a grandchild, press **Stop** | Both gone from Task Manager within about 5 s; the run is `cancelled` with `effect_unknown` |
| Timeout | The same with a 10 s timeout | Both gone after the timeout plus the 5 s grace; `failed` with `timeout` |
| Jovru killed | Run it, end `jovru.exe` in Task Manager | Program and grandchild end at once (Job Object) |
| No `jjp/1`, Stop | Run a program without `jjp/1`, press **Stop** | Killed after the 5 s grace; `cancelled`; record how long Stop took |
| `jjp/1`, Stop | Run a `jjp/1` program slowed down (the example finishes in 0.5 s), press **Stop** | It gets `cancel`; one that obeys ends alone, one that ignores it is killed after 5 s |
| Console | Run any program with the desktop visible | No console window shows (a flash shorter than the polling interval cannot be excluded) |
| `.bat` / `.cmd` | Save a program job whose program is a `.bat`, a `.cmd`, then a bare name that `PATH` resolves to one | Each refused before the run, with the reason, leaving no history |
| `PATHEXT` | Program `ping` with no extension | Resolves to `ping.exe` and runs |
| Log limit | Print more than 1 MiB to stdout; open the run's **stdout** log | `automation\logs\<run-id>\stdout.log` is at most 1 MiB and ends with the truncation marker; **Delete logs** removes the folder |
| Code page | Run a console program that prints accented words | They read correctly (OEM code page, D-0037) |

## Windows: `jjp/1`, lock and second instance

| Check | What to do | Expected result |
| --- | --- | --- |
| Node example | Create the job in `examples/jjp/README.md` (outputs `rows` and `file`), run with 25 rows | `succeeded`; **Inputs** shows the percent and stage while it runs; the CSV has 25 rows; `log` lines are in the **jjp** log; the outputs are listed with sizes, never values, only with **Store outputs** on (D-0035) |
| Chain | Run it as the step after a SQL Job whose output feeds `rows` | The chain succeeds and the output reached the program |
| Token | Read the running program's command line and environment | No token in either; the only `JOVRU_` variable is `JOVRU_JJP_ADDRESS` |
| Lock after a kill | Open Automation, end `jovru.exe`, start it again | Automation is ready at once and says nothing about the lock; runs left `running` show **Interrupted**, with a singular or plural notice |
| Second instance | Start a second one on the same data folder | Automation is read-only and says another window runs it, with **Try again** |
| Takeover | Close the first, press **Try again** | The second takes over, runs jobs without a restart, and no "still holds" notice remains |

## Automation interface

For each flow, also check **Tab** and **Shift+Tab** reach every control,
**Enter** and **Esc** do what the dialog says, focus returns to where it was
when a dialog closes, and nothing is clipped at 100%, 125% and 150% scaling.

| Check | What to do | Expected result |
| --- | --- | --- |
| SQL Job | Create, edit, save and run one on SQLite and on a PostgreSQL or MySQL fixture | The form asks only for declared inputs and refuses a wrong type before the run; the run shows state, cause and counters; the result is not stored; a password not remembered is asked first |
| Stop and queue | **Stop** a long run; with two active, run a third | Stop ends it within about a second; the third says it is queued and which limit holds it, and starts when a slot frees |
| Trusted code | First run of a program job revision | The warning names the user's privileges, shows once per revision (kept in `automation.db`, across instances and restarts) and again after every save |
| Chain | Create a chain of a SQL Job and a program, read the plan, confirm, run, **Stop** midway | The plan lists versions and connections; the stopped step is **Stopped** and later steps say they did not run |
| History | Open a job's history and the global list | Each run shows state, cause, `effect_unknown`, counters and time; the global list pages without freezing (needs a long history) |
| Quit | Quit with a run active | It asks; **Keep open** keeps the run; confirming stops it, writes `cancelled` and exits within about 11 s |

## Schedules

Use a SQL Job on a SQLite file and a published version. Windows ships no
time-zone database: `jiff` uses the one bundled with the build.

| Check | What to do | Expected result |
| --- | --- | --- |
| Zones | **Schedule** tab: every 1 minute, zone `America/Sao_Paulo`, then `Europe/Berlin`, save | The next three times show in each zone with abbreviation, offset and this computer's time; the consent lists times, policies and connection; a run starts within a minute and **Runs** shows **Scheduled** with its due time |
| Notification | Turn on **Notify me when a scheduled run ends badly**, make the SQL fail, consent | A toast names the job, "Failed" and the cause, never a value; the rail shows a red count and the status bar names the run and cause. Whether the toast shows for an unpackaged build (AppUserModelID not verified, Q-40) has never been captured |
| Missed times | With a schedule on, close for three due times, start; repeat with each missed policy (N = 2) | **Do not run them**: nothing catches up. **Run once**: one run marked catch-up. **Up to 2**: two catch-up runs one after another. A due time reached at start runs on time in all |
| Suspension | Save the job's connection (any save, even unchanged) | **Suspended** with the reason and **Confirm again**; nothing runs until confirmed |

## Editor, grid and shortcuts

Run on SQLite, the PostgreSQL fixture and the MySQL fixture; the MySQL fixture
needs TLS (`sslMode` `required`).

| Check | What to do | Expected result |
| --- | --- | --- |
| Search and cursors | In a query tab, a report panel and a SQL Job: `Ctrl F`, a word, `Enter`; `Ctrl H`, replacement, `Tab`, **Replace All**, `Esc`; add cursors (`Ctrl Alt ↓` on Windows, `Shift Alt ↓` on Linux), type, `Esc` | Matches marked and counted; `Tab` moves between fields; `Esc` returns the keyboard to the editor; several cursors type at once and `Esc` leaves one |
| `Ctrl F` by focus | `Ctrl F` in the editor, on the explorer tree, on a grid | The editor's search; the tree's quick search; nothing in the grid |
| Go to a line | `Ctrl G` in the three editors: `3:8`, a line past the end, `abc`, `Esc` | Cursor to line 3, column 8; past the end goes to the last line and the notice says so; invalid text is named under the field; `Esc` returns the keyboard |
| Error position | On MySQL run `SELECT 1 FROM dual WHERE 1 = = 2;` | The error is marked in the gutter, line and column named, **Go to the position** moves the cursor |
| Grid edit | On a SQLite table with a key: `F2`, type, `Enter`; `Ctrl Shift N`; `Delete`; `Alt Insert`; `Ctrl Alt S`. Repeat on PostgreSQL with the keyboard alone, with exact numbers and NULL | Pending changes counted; the dialog lists each change and its bound statement; `Esc` cancels; `Enter` writes in one transaction and the database holds the values; `Enter` in a cell's field does not open the value panel |
| Not editable | Query a table without a key, a `GROUP BY`, a report panel's and a job's grid | The reason is stated; the editing items are disabled |
| Transaction | `BEGIN;` then edit and write a cell, on SQLite and PostgreSQL | The write stays in the open transaction; toolbar and notice say uncommitted; another connection does not see it; **Rollback** undoes it |
| Production | On a **Production** connection edit a cell, `Ctrl Alt S`, then `Ctrl Alt Z` | Asks first and says production; discarding asks first |
| Import CSV | **Import from file…** (explorer menu, `Shift F10`) on a SQLite table with a synthetic CSV; map by name and by position | Delimiter and encoding detected; the preview shows the rows as they would be written; they are imported; choosing another file clears the last outcome |
| Import XLSX | The same with an XLSX | The sheet can be chosen; the floating-point note shows; the preview shows the number as stored |
| Large import | Import 3,000,000 synthetic rows (108 MB) and **Cancel import** | Progress after 300 ms; elapsed time and **Cancel import** past 2 s; after cancelling it says nothing was imported and the table is as it was |
| Dialog backdrops | `Esc` and **Cancel** in the import dialog; click the explorer behind it and behind Preferences | `Esc` closes it, also after **Cancel import**; clicks do not reach what is behind |
| New table from file | **New table from this file…** | A query tab with a draft `CREATE TABLE` from the header; the name gets `_2` when the table exists |
| Shortcuts | **Preferences › Shortcuts**: **Change** for **Go to a line**, press `Ctrl B`, **Take it**; `Esc` while listening; restart; **Reset**, **Reset all shortcuts** | The collision is named; **Take it** moves the key; `Esc` keeps it; the change survives the restart; Reset gives back the defaults |
| Column layout | Hide, move and freeze columns with the mouse in a query tab, a report panel and a job's result | The hidden count and **Show all columns** show; a dragged header takes the other's place; a frozen column stays left while scrolling; no tooltip over an open menu |
| Column menu keys | `Shift F10` on a cell or header, arrows and `Enter` | The menu opens and works; after **Hide** the grid keeps the keyboard (`Ctrl Shift F` works at once) |
| Find a column | `Ctrl Shift F` or **Find a column**, `acao`, `↓`, `Enter` | Ignores case and accents (finds `Ação`) and marks the match; `Enter` scrolls to and selects the column; a hidden match says so and shows it; the records view lists only matching columns |

## Linux

A debug build under a private `Xvfb` display (for example `:99`, 1600×1000, no
window manager) with Mesa software Vulkan, `xdg-desktop-portal-gtk`, an
isolated configuration, synthetic data and keys sent by `xdotool`; the
fixtures are reached on `127.0.0.1`. Run the editor, grid and automation
checks above, plus:

| Check | What to do | Expected result |
| --- | --- | --- |
| Start and connect | Start; connect to SQLite, the PostgreSQL fixture and the MySQL fixture, with the password prompt | PostgreSQL shows **Not encrypted**, MySQL the TLS lock |
| SQL and grid | Run SQL, page the grid; `Ctrl F2` on a long query; `Ctrl C` of a cell and of a selection | Paging works; the query is cancelled; the clipboard holds the copy |
| Files | `Ctrl S`, `Ctrl Shift S`, `Ctrl O` | The portal's dialogs open and the file is read or written |
| Tabs | Close a tab with unsaved SQL (`Esc` cancels); `Ctrl Shift T` | It asks; the tab reopens with its SQL |
| Favorites | Add a table from `Shift F10` and the arrow keys, open it as **View data**, restart | The favorite and the file tabs survive |
| Install | `install.sh` adds and removes the desktop entry and icon | `StartupWMClass` matches the window's class |

Not covered: Wayland, a real desktop session (panel icon, window-manager
focus), Linux performance (the software renderer is not comparable to the
budgets) and the X11 window's own icon (`_NET_WM_ICON` is not set; the icon
comes from the installed desktop entry). Never checked on a physical Windows
keyboard: `Ctrl Alt ↑`/`↓` where `AltGr` is `Ctrl Alt` (ABNT2) must add a
cursor and type nothing; the visible focus ring of a full `Tab` walk; 125% and
150% scaling.

## Before a release

- `cargo xtask check --full` passes on the release tree after the last fix.
  Databricks against a workspace is **Not run** unless a profile is available.
- The manual groups the release touches pass on Windows and on Linux, with
  keyboard and focus.
- Updates: with signed release candidates, on both platforms, from a new data
  folder: the older one starts without a warning; its automatic check finds
  the newer after about 30 s; the badge opens About; **Download and install**
  shows progress and installs in the release folder; **Restart now** starts
  the new version (the log's `started` carries it, the badge is gone). An
  unsigned archive is refused with the folder unchanged
  ([D-0033](../decisions/security.md#d-0033)).
- The Windows executable carries the product icon and the version (the title
  bar and taskbar show it); the Linux release ships the desktop entry and the
  SVG with `install.sh`; the root README names the official icon and the
  minimum glibc of the Linux binary, built on `ubuntu-24.04`.
- Start on an empty data folder: the explorer loads and the update and
  telemetry preferences are readable (three parts of the application once
  created `config.db` at the same time and failed with `SQLITE_BUSY`).
- Performance numbers of a touched path are measured in a release build and
  reported against [Performance](performance.md).
