# Performance

Being fast and light is a core promise of Jykre Studio. Performance therefore
has **numeric budgets**: a regression beyond a budget is a bug, and a task that
touches a measured path reports its numbers.

## Budgets

Measured on the reference machine (the maintainer's Windows 11 workstation) in
a **release** build, with the synthetic data from `docker-compose.dev.yml`. A
frame is the time from the start of `render` to the end of paint for the
window. Debug builds are several times slower and are not held to these
numbers.

| Metric | Budget | Baseline (2026-11-02) | How to measure |
| --- | --- | --- | --- |
| Frame while scrolling, hovering or selecting in the result grid, any row count up to 100,000 | ≤ 8 ms p95 | 2.6 ms median, ≤ 6.4 ms p95 (1,000 × 15 columns to 100,000 × 5) | Frame probe |
| Frame for any other interaction in the main window | ≤ 8 ms p95 | Not measured | Frame probe |
| Longest UI-thread stall while a query runs | ≤ 16 ms | 12.8 ms during a 4 s `pg_sleep` (the Windows timer resolution is about 10–15 ms) | Heartbeat |
| UI-thread work to publish a finished result | ≤ 2 ms at any size | 0.01 ms at 100,000 rows | Timing around the publish |
| First frame after a result arrives | ≤ 16 ms | 7–10 ms | Frame probe |
| Rebuilding the grid view after a filter keystroke or a header sort, 100,000 rows | ≤ 50 ms | In the app (release, Windows, 2026-11-07, 100,000 × 3 SQLite rows, `grid view` timing at `JYKRE_LOG=jykre::ui_thread=debug`): 13.4 ms and 14.1 ms per filter rebuild; the filter applies after a 150 ms pause, so six keystrokes typed together rebuilt once. The unit test builds it in under 80 ms in a debug build. Benchmark (release, 2026-11-04): 14 ms filtered, 5.0 ms sorted by a number, 3.3 ms sorted by text, on rows already in order | Unit test budget, `grid view` timing, frame probe |
| Copy of a selection | ≤ 100 ms for 500,000 cells; above that, show progress | About 85 ms for 100,000 × 5 cells | Heartbeat |
| Cold start to an interactive window | ≤ 1.5 s | 835 ms to a window that accepts input (865 ms to the main window) on the first launch from an empty data folder; 241–254 ms on the next four launches (release, Windows, 2026-11-07; see [Startup, memory and export](#startup-memory-and-export-measurements)). Not measured after a reboot | Stopwatch and a startup timestamp |
| Idle memory, no connection | ≤ 200 MB | 79.0 MB working set, 106.4 MB private bytes 15 s after start (release, Windows, 2026-11-07) | Task manager |
| Expand an explorer node from the local cache | ≤ 50 ms p95 | 0.65 ms for a first page and 0.68 ms for the page after 4,800 children, in a cache of 500,000 objects kept open, with paths rebuilt from compact keys (benchmark, release, 2026-11-03); opening the file costs about 680 ms, once per connection at start. Not measured in the app | Timing around the cache read, benchmark |
| Longest UI-thread stall while an export runs | ≤ 16 ms | 6.3 ms at most while a 100,000-row CSV export (53.3 MB) is written; 15.8 ms at most over the whole export, native save dialog included (release, Windows, 2026-11-08, `WM_NULL` round trips; see [Startup, memory and export](#startup-memory-and-export-measurements)) | `WM_NULL` round trips on Windows; heartbeat elsewhere |
| Encoding an export part, off the UI thread | No budget; it bounds how fast an export writes | 13 ms as CSV, 26 ms as JSON Lines, 30 ms as `INSERT` for 100,000 × 5 cells (benchmark, release, 2026-11-04); a unit test keeps 100,000 × 10 CSV cells under 3 s in debug | Benchmark, unit test budget |
| Metadata search (`Ctrl Shift P`) over the local cache | ≤ 50 ms p95 | 7.4 ms for a name of three characters or more among 500,000 objects (trigram index); 93 ms for one or two characters (benchmark, release, 2026-11-03, after the compact keys of [ADR-0019](../adr/0019-compact-metadata-cache-keys.md)) | Timing around the search, benchmark |

The numbers are initial targets. Changing a budget is a maintainer decision
recorded in this table. "Not measured" rows are measured the first time a task
touches that path.

## Scalability targets

| Dimension | Minimum supported |
| --- | --- |
| Saved connections | 200 |
| Metadata objects per connection | 500,000 (paged from the local cache) |
| Open query tabs | 50 |
| Rows per result | The connection's row limit (up to 100,000), within the memory ceiling in [Results](../reference/results.md#limits) |

## Principles

1. **Nothing remote on the interactive path.** The explorer and search read
   the local metadata cache. The network is used only for explicit actions:
   test, run, refresh; only when the user turns it on, for telemetry,
   sent in batches by background threads, never by the UI thread
   ([ADR-0029](../adr/0029-otlp-export-of-logs-and-traces.md)); and, unless
   the user turns it off, for the update check: one request to GitHub's
   release list at most once a day, from the Tokio runtime, never sooner
   than 30 s after the window opens
   ([ADR-0033](../adr/0033-update-check-and-signed-self-install.md)). High-frequency
   UI events (mouse, selection, frames) are never exported.
2. **Load on demand.** The explorer reads children page by page; results are
   bounded by the row limit while they are read.
3. **Stream, do not buffer everything.** Drivers deliver rows to a `RowSink`
   and stop reading at the limit.
4. **A frame costs what is on screen.** Long collections are virtualized and
   derived data is cached by its inputs ([UI guidelines](ui-guidelines.md#how-gpui-renders-and-what-follows-from-it)).
5. **The UI thread only does UI.** Network, disk, conversion and dropping of
   large data happen on the Tokio runtime or a background executor.
6. **Compact data in memory.** Results use a compact representation
   ([ADR-0008](../adr/0008-compact-result-storage.md)).

## Rejected patterns

A change that introduces one of these is rejected in review:

- `.children(...)` over a collection that grows with the data.
- Filtering, sorting, measuring or formatting every row inside `render`.
- Rebuilding a view on every notification of an unrelated entity (an
  `observe` that only calls `cx.notify()` on a large parent).
- A unique `format!`-built element ID or an eagerly built tooltip string for
  every element of a large view.
- Converting, cloning or dropping a large result on the UI thread.
- N+1 introspection (one query per table to read its columns); use set-based
  queries per database or schema.
- Reading a whole metadata tree into the UI instead of paging it.
- Polling timers to discover state that an event or subscription can deliver.
- Cloning large vectors to get past the borrow checker.
- Recomputing a data-sized filter on every keystroke without a debounce.

## How to measure

### Automated synthetic grid run

Run `cargo run --release --locked -- --test-performance` (or pass
`--test-performance` to the packaged executable). This opens an isolated
window containing the production result grid, without loading connections,
workspace persistence, a vault or a database. It generates 1,000 and 100,000
rows with five columns, then 1,000 rows with 400 columns of which the first
three are frozen, off the UI thread. After a two-second warmup, each size
runs idle (75 ticks), rectangular selection (250 ticks), selection near the
final row (250 ticks), scrolling (125 ticks) and panning sideways back and
forth over the whole width (`pan`, 125 ticks), with a nominal 40 ms timer per
tick. Closing the window cancels the run at its next update. With
`JYKRE_PROBE=1` the window also runs the frame probe below, so
`JYKRE_PROBE=1 cargo run --release --locked -- --test-performance` logs
`frame probe` lines for each phase; the status line names the phase, and the
wide size's `pan` is the sideways scrolling with frozen columns that the
grid's frame budget covers.

The finished window shows the report path. A uniquely named
`jykre-performance-<timestamp>-<pid>.json` is saved to the working directory.
It contains elapsed wall time per phase, CPU percent (100% = one logical
CPU), endpoint resident memory in bytes, dimensions, step counts, build
profile and platform. Linux uses process CPU ticks and VmRSS; Windows uses
background PowerShell process snapshots (hidden window). Snapshot overhead
can affect short intervals, particularly on Windows. Counter failures are
explicit errors and null measurements, not zero. Other platforms currently
report counters unavailable. Memory is sampled at phase boundaries, not a
peak-memory measurement. Time is phase duration, not frame latency.

This calls the same selection/navigation methods as keyboard handling, but
does not inject OS input, measure input delivery, or exercise the editor,
explorer and workspace ancestors. Its numbers are not directly interchangeable
with the earlier whole-workspace external-input measurements. Keep window
size, graphics adapter, build and machine identical when comparing runs.
The existing frame probe below remains the tool for the frame budget.

- **Frame probe.** Run the application with `JYKRE_PROBE=1` (a release build
  for budgets: `JYKRE_PROBE=1 cargo run --release`). `src/app/probe.rs`
  records an `Instant` at the start of the root view's `render` and reads it
  in a `canvas` painted last; that is render, layout and paint for the frame.
  Every 5 s the log gets a `frame probe` line (target `jykre::probe`) with the
  frame count, median, p95 and maximum, and how many times each view rendered
  (`AppView`, `Workspace`, `ResultGrid`), which shows whether a cached entity
  was redrawn. Drive the scenario (scroll, hover, selection) and read the
  lines in the log file or, in debug builds, the console.
- **Heartbeat.** With the probe on, a UI-thread task sleeps 5 ms in a loop;
  any extra delay is time the UI thread was busy, reported as `stall_max_ms`
  in the same line. On Windows the timer resolution sets a floor of about
  10–15 ms.
- **Data.** Use the dev databases (`sales.event_log` has 100,000 rows; see
  [Testing](testing.md#local-databases-for-manual-testing)). Raise the row
  limit only in the measurement build, never in a shipped default.
- **Unit test budgets.** Data-sized algorithms get a test with a generous time
  budget (ten times the measured time or more), such as
  `a_large_result_builds_its_grid_view_within_budget`. They catch complexity
  regressions, not small slowdowns.
- **Benchmarks.** `benches/hot_paths.rs` ([ADR-0010](../adr/0010-benchmarks-with-criterion.md))
  measures building the grid view at 100,000 rows (unfiltered, filtered and sorted by a header), converting 100,000 × 30
  driver rows to result data, splitting a 10,000-statement script and, in a
  cache of 500,000 objects, opening the file to read a first page (what every
  expansion cost before the cache stayed open) and reading the first page and
  the page after 4,800 children through the cache kept open, encoding
  100,000 rows for an export as CSV, JSON Lines and `INSERT`s, the
  editor's tokens of a 10 MB script (whole, and after a keystroke, also in
  place) and a query tab's outline of a 4 MiB script.
  `cargo xtask check --full` runs
  them in quick mode, only to prove they work. A task that touches one of
  these paths compares against a baseline and reports the result:

  ```bash
  cargo bench --bench hot_paths -- --save-baseline main
  ```

  ```bash
  cargo bench --bench hot_paths -- --baseline main
  ```
- **Report.** A task that touches a measured path reports before and after
  numbers and the build profile in its report ([Workflow](workflow.md#task-report)).

### Startup, memory and export measurements

Taken for 0.2.0 M10 on 2026-11-07, on the reference machine, with a copy of
`target/release/jykre.exe` and `JYKRE_CONFIG_DIR` pointing at a folder used
only for the measurement.

- **Startup.** A PowerShell script starts the executable with
  `Process.Start` on an empty data folder (no connection) and times
  `WaitForInputIdle` (the window accepts input) and the first non-zero
  `MainWindowHandle`, five launches in a row, killing the process between
  them. First launch: 835 ms and 865 ms. Launches 2–5: 241–254 ms and
  264–277 ms. The executable had just been copied, so the first launch read
  it from the file cache: it is the first start of a new data folder, not a
  start after a reboot.
- **Idle memory.** Read from the process 15 s after the fifth launch, with
  no connection and no tab run: 79.0 MB working set, 106.4 MB private bytes.
- **Export.** `JYKRE_PROBE=1`, a SQLite file with 100,000 synthetic rows of
  three columns, `SELECT * FROM big` in a query tab, and **Export…** as CSV
  through the application's own save dialog (62 MB, written in 53–93 ms off
  the UI thread). A script posts a mouse move every 200 ms from one process
  so that the probe keeps reporting, and repeats three cycles of: no dialog,
  a dialog cancelled, an export. Longest stall per 5 s probe line: the lines
  that held an export read 14.7, 17.6 and 17.6 ms; lines without a dialog
  12.2–18.8 ms; lines in which the save dialog opened 17.4–24.3 ms. The log
  had no `jykre::ui_thread` timing during an export. A first series that
  started a new process for every mouse move read 16–24 ms in every line,
  export or not. The export adds no stall the heartbeat can tell from its
  floor, but the heartbeat cannot show that the budget holds: on Windows
  its 5 ms sleep wakes on the system timer (about 15.6 ms), so idle lines
  already reach 16–19 ms.
- **Export, measured again on 2026-11-08** (release 0.2.0-rc.1, same
  machine, isolated data folder, SQLite file with 100,000 synthetic rows of
  three columns, about 530 bytes a row). A separate process sends
  `WM_NULL` to the main window with `SendMessageTimeout`, waits 1 ms by
  spinning and sends again, timing each round trip with
  `QueryPerformanceCounter`: a round trip lasts as long as the window's
  thread is busy, so its length is the stall, without the system timer's
  floor. The longest gap between a reply and the next send was 3.4 ms, so
  no longer stall can hide between samples. Idle: 0.03 ms median, 1.7 ms
  at most. The same process clicks **Exportar…** and fills the native
  save dialog with `PostMessage`, four exports in a row (53.3 MB each,
  written in 64–76 ms off the UI thread per the log's `export finished`).
  Longest round trip per export, from the click to 5 s after the file was
  chosen: 15.8, 6.0, 6.3 and 6.7 ms. The longest ones fall within 40 ms
  of the click, while the native dialog opens (15.8 ms the first time),
  and when it closes; while the file is written the longest was 6.3 ms.
  The budget holds. Opening the export bar itself took up to 10.9 ms.

### The editor with a large `.sql` file

Taken for ADR-0031's limit (D7) on 2026-11-07, release build, Windows,
`JYKRE_PROBE=1`, isolated data folder. Files of numbered one-line `SELECT`s
(about 89 bytes a line) are opened from the library; a script then posts
60 wheel scrolls of three notches, 100 ms apart, and types 24 characters,
150 ms apart. Longest frame and stall per phase, from the 5 s probe lines
(the machine was running other work; a one-line file shows the floor):

| File | Lines | Opening: longest frame / stall | Scrolling: p95 / longest frame | Typing: p95 / longest stall |
| --- | --- | --- | --- | --- |
| One line | 1 | 16.3 ms / 33.4 ms | 11.0 ms / 12.7 ms | 8.7 ms / 14.4 ms |
| 64 KiB | 784 | 131.5 ms / 134.7 ms | 12.9–14.6 ms / 20.7 ms | 14.0–17.1 ms / 28.1 ms |
| 256 KiB | 3,051 | 357.7 ms / 390.7 ms | 15.0–18.1 ms / 29.9 ms | 12.8–16.9 ms / 39.3 ms |
| 1 MiB | 12,021 | 1,426.7 ms / 1,423.7 ms | 8.5–13.4 ms / 17.3 ms | 7.6–8.1 ms / 53.0 ms |
| 4 MiB (4.18 MB) | 46,815 | 5,734 ms / 6,093 ms | 12.2–18.6 ms / 33.0 ms | 14.0 ms / 676.7 ms |

Opening grows by about 1.4 ms per KiB on the UI thread. At the time, on
every change the workspace copied the editor's whole text into a `String`,
compared it, and then found every statement's line again and scanned the
parameters on the UI thread; every notification of the editor (a scroll, a
cursor move, the caret blinking) also copied and compared the text.

#### Since 2026-11-08

What the tab does with its text now (`src/app/workspace.rs`,
`jykre::script_outline`):

- The editor's notifications only redraw; nothing reads the text.
- Each edit arrives with its range and the text after it
  (`InputEditsEvent`), and the tab's copy of the text takes only that range,
  so a keystroke moves the bytes after it and copies nothing else.
- The statements, their lines, the line count and the `:name` parameters
  (the *outline*) are read on the background executor, at once for a text
  up to 64 KiB and after a 150 ms pause in typing above that. Each change of
  the text or the dialect takes a new revision, and an outline read from an
  older one is never applied. A run started before the latest outline
  arrived reads it first, on the UI thread, so its parameters are those of
  the text it runs.
- Whether the tab has unsaved changes is compared once per revision.
- The highlighter relexes in place (`jykre::sql::relex_in_place`): the tokens
  around the edit are replaced and the rest moved, instead of a new list of
  every token on each keystroke.

Measured on 2026-11-08 on Linux, before and after the change, release build,
an 8-vCPU KVM virtual machine with no GPU (Xvfb with Mesa's software Vulkan,
lavapipe), `JYKRE_PROBE=1`, an isolated data folder and a SQLite connection
to a synthetic file, so that the tab shows the editor. Files of numbered
one-line `SELECT`s (about 89 bytes a line) were opened from the library, then
a script posted 180 wheel notches 33 ms apart, typed 24 characters 150 ms
apart and moved the cursor down 30 lines, 100 ms apart, each from one
`xdotool` process. *Ours* is the UI-thread time of our code, from
`UiTimer` timings at `jykre::ui_thread=debug` (temporary ones, for the
before column). The heartbeat's floor on this machine is 30–50 ms, as the
one-line file shows; scrolling a large file stalls up to about 180 ms before
and after the change, in the software renderer's compiled shaders (`perf`),
which a GPU does not have.

| File | Opening: longest frame / stall / ours, before | After | Typing: frame p95 / longest stall / ours per key, before | After |
| --- | --- | --- | --- | --- |
| One line | 10.5 ms / 43.6 ms / 0.3 ms | 9.9 ms / 40.0 ms / 0.3 ms | 4.6 ms / 30.5 ms / 0.01 ms | 7.3 ms / 49.0 ms / 0.01 ms |
| 64 KiB | 86.0 ms / 117.0 ms / 5.7 ms | 81.2 ms / 108.7 ms / 0.4 ms | 5.4 ms / 41.2 ms / 2.4 ms (4.7 ms max) | 6.0 ms / 36.2 ms / 0.01 ms (0.02 ms max) |
| 256 KiB | Not captured / 350.7 ms / not captured | 316.2 ms / 342.3 ms / 0.8 ms | 7.1 ms / 47.6 ms / 9.6 ms (12.8 ms max) | 7.2 ms / 42.8 ms / 0.04 ms (0.07 ms max) |
| 1 MiB | 1,230 ms / 1,334 ms / 71.4 ms | 1,220 ms / 1,248 ms / 3.4 ms | 6.7 ms / 142.1 ms / 36.0 ms (38.2 ms max) | 7.1 ms / 51.2 ms / 0.13 ms (0.30 ms max) |
| 4 MiB | 4,988 ms / 5,306 ms / 285.6 ms | 5,074 ms / 5,113 ms / 13.1 ms | 7.5 ms / 3,697 ms / 140.9 ms (145.1 ms max) | 7.2 ms / 56.3 ms / 0.49 ms (1.12 ms max) |

- In the 4 MiB file the outline takes 151 ms off the UI thread and 2.7 ms to
  apply on it (0.8 ms at 1 MiB). Scrolling and moving the cursor cost our
  code nothing; before, every notification copied and compared the text,
  2.5 ms at 4 MiB.
- Of the 13.1 ms of ours on opening 4 MiB, 6.5 ms is creating the editor
  with its text (the component builds its rope).
- A run started within the pause after an edit reads the outline on the UI
  thread first: 42.7 ms in a 1 MiB file (about 150 ms at 4 MiB, from the
  outline's time). A run started later reads nothing more than before.
- The rest of opening is the editor component's: `gpui-kit` 0.9.0 soft-wraps
  by default, and its first layout measures every line of the document with
  the text shaper (`TextWrapper::update_wrapped`, `perf`). The maintainer
  chose on 2026-11-08 to keep the 4 MiB limit and turn soft wrap off for a
  text above 256 KiB (back on at 192 KiB or less,
  `jykre::script_outline::soft_wrap`;
  [Large `.sql` files in the editor](../plans/editor-large-files.md)).
  Measured again with it, same machine and script, longest frame / longest
  stall on opening: 64 KiB (still wrapped) 84.6 ms / 115.8 ms; 256 KiB
  13.5 ms / 43.3 ms; 1 MiB 29.3 ms / 56.2 ms; 4 MiB 62.0 ms / 100.1 ms.
  Typing kept a frame p95 of 5.8–6.5 ms and a longest stall of 40–51 ms
  (the floor) at every size; our code took 0.56 ms a key in the 4 MiB file
  (1.22 ms at most). At 4 MiB the rest of opening is reading the
  highlighter's tokens of the whole text and building the rope, once.
- Benchmarks (`benches/hot_paths.rs`, release, Linux, the machine above):
  reading the outline of a 4 MiB script takes 145 ms (off the UI thread);
  relexing a 10 MB script after a keystroke takes 3.8 ms in place, against
  52.7 ms for building a new list of its tokens as the highlighter did.
- **Go to line** (`Ctrl G`, 0.3.0 A9) computes nothing per frame: its
  prompt is an entity drawn only while open, and going reads the rope's
  line count and the target line's start (`lines_len`,
  `position_to_offset`, logarithmic in the text) once, then moves the
  cursor with the editor's own `set_cursor_position`, which scrolls it into
  view. The text is never copied. Not yet measured in a file above
  256 KiB; the move may meet the editor component's click cost noted in
  *Known deviations* below.

#### On the reference machine, 2026-11-08

Release 0.2.0-rc.1 on Windows, `JYKRE_PROBE=1`, isolated data folder, the
same kind of files opened from the library, each in a new tab. Stalls are
`WM_NULL` round trips (see [Startup, memory and
export](#startup-memory-and-export-measurements); gaps between samples of
6.7 ms at most), frames are the probe's. Per file: open it and wait 7 s,
60 wheel steps of three notches 100 ms apart, then click into the text and
type 24 characters 150 ms apart.

| File | Opening: longest frame / stall | Scrolling: longest stall | Typing: longest stall after the click; ours per key | A click into the text: longest stall (4 clicks) |
| --- | --- | --- | --- | --- |
| One line | Not measured (opened before the probe) | Not measured | 10.8 ms; under 0.01 ms | 9.6 ms |
| 64 KiB (wrapped) | 114.6 ms / 115.3 ms | 20.5 ms | 15.9 ms; under 0.1 ms | 15.4 ms |
| 256 KiB (wrapped) | 184.9 ms / 185.4 ms | 25.2 ms | 16.8 ms | 13.4 ms |
| 1 MiB (not wrapped) | 21.0 ms / 21.0 ms | 17.2 ms | 11.1 ms | 23.4–29.2 ms |
| 4 MiB (not wrapped) | 54.1 ms / 54.2 ms | 26.5 ms | 21.2 ms; 1.6–2.3 ms, 3.8 ms to apply an outline | 97.4–117.3 ms |

- Opening follows the Linux measurements: what is wrapped costs the editor
  component's first layout (185 ms just under 256 KiB), what is not opens
  in one frame of 21–54 ms. The 4 MiB limit of D7 is therefore within one
  frame of about 54 ms on this machine; files just under the 256 KiB wrap
  threshold are the slowest to open.
- A click into a text that is not wrapped stalls the UI thread in
  proportion to its size, about 25 ms per MiB (four clicks each at 1 and
  4 MiB, and five more at 4 MiB of 89–131 ms). None of our `UiTimer`
  timings ran during them; the time is the editor component's handling of
  the click, not yet located. Arrow keys and the wheel in the same 4 MiB
  tab stall 15–18 ms.

## Known deviations

M2 clipboard encoding measurements (release, Windows, 2026-11-07): 49 ms for
500,000 text cells and 57 ms for CSV, plus 4–5 ms to estimate the size, all
off the UI thread. These measure encoding, not the OS clipboard handoff.

| Standard | Current code |
| --- | --- |
| A file the library opens stays within the frame budget ([ADR-0031](../adr/0031-saved-queries-as-sql-files.md) D7) | Typing and scrolling meet it since 2026-11-08. Opening does not: up to 256 KiB the editor component's soft wrap measures every line on the first layout, about 1.2–1.4 ms per KiB in one frame (85 ms at 64 KiB, about 310 ms just under 256 KiB); above it the tab does not wrap, and the longest frame on opening is 13.5–62 ms up to 4 MiB ([measurements](#since-2026-11-08)). On the reference machine (2026-11-08): 115 ms at 64 KiB and 185 ms at 256 KiB, wrapped; 21 ms at 1 MiB and 54 ms at 4 MiB ([measurements](#on-the-reference-machine-2026-11-08)). Lazy wrapping upstream would remove the rest ([Large `.sql` files in the editor](../plans/editor-large-files.md)) |
| A click stays within the frame budget | A click into a text that is not wrapped (above 256 KiB) stalls the UI thread about 25 ms per MiB, 97–131 ms at 4 MiB, inside the editor component; not yet located ([measurements](#on-the-reference-machine-2026-11-08)) |
| Long collections are virtualized | The command palette builds its rows without a virtual list, but lists at most 10 without text and 50 for a search (`jykre::palette::limit`). Saved queries and history are virtualized |
| Metadata search within budget at any length | A query of one or two characters, which the trigram index cannot match, scans the name index: about 93 ms at 500,000 objects. Three characters or more take about 7.4 ms. Every search runs off the UI thread after a 150 ms pause, and since 0.2.0 M3 both the palette and the explorer's search field show a spinner while one runs |
| Frequently changing regions are cached entities | The result grid is (`ResultGrid`, [ADR-0012](../adr/0012-result-grid-as-a-cached-entity.md)); so is the assistant panel (`src/app/assistant_panel.rs`, [ADR-0028](../adr/0028-agentic-ai-assistant-over-the-metadata-cache.md)). The editor still renders inside `Workspace` |
| A grid interaction renders only the grid ([ADR-0012](../adr/0012-result-grid-as-a-cached-entity.md#how-to-validate)) | Not possible in GPUI 0.3.11: a notified view renders with all its ancestors, so hover, scroll and selection also render `AppView`, `GroupedTabs` and `Workspace`, about 1.5 ms of a 3.6–4.4 ms p50 grid frame. `.cached(...)` spares the grid when only the workspace changes: typing 15 characters rendered `Workspace` 15 times and `ResultGrid` never (frame probe, release, Windows, 2026-11-02). [ADR-0013](../adr/0013-thin-ancestors-of-frequently-notified-views.md) (Accepted, not implemented) replaces this validation with the frame budget and keeps the ancestors thin. The editor gutter no longer splits the script on every frame, which took grid frames to 57 ms with 1,000 statements |
