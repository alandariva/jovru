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
| Rebuilding the grid view after a filter keystroke, 100,000 rows | ≤ 50 ms | Not measured in the app; the unit test builds it in under 80 ms in a debug build | Unit test budget, frame probe |
| Copy of a selection | ≤ 100 ms for 500,000 cells; above that, show progress | About 85 ms for 100,000 × 5 cells | Heartbeat |
| Cold start to an interactive window | ≤ 1.5 s | Not measured | Stopwatch and a startup timestamp |
| Idle memory, no connection | ≤ 200 MB | Not measured | Task manager |
| Expand an explorer node from the local cache | ≤ 50 ms p95 | 0.65 ms for a first page and 0.68 ms for the page after 4,800 children, in a cache of 500,000 objects kept open, with paths rebuilt from compact keys (benchmark, release, 2026-11-03); opening the file costs about 680 ms, once per connection at start. Not measured in the app | Timing around the cache read, benchmark |
| Metadata search (`Ctrl Shift P`) over the local cache | ≤ 50 ms p95 | 7.4 ms for a name of three characters or more among 500,000 objects (trigram index); 93 ms for one or two characters (benchmark, release, 2026-11-03, after the compact keys of [ADR-0019](adr/0019-compact-metadata-cache-keys.md)) | Timing around the search, benchmark |

The numbers are initial targets. Changing a budget is a maintainer decision
recorded in this table. "Not measured" rows are measured the first time a task
touches that path.

## Scalability targets

| Dimension | Minimum supported |
| --- | --- |
| Saved connections | 200 |
| Metadata objects per connection | 500,000 (paged from the local cache) |
| Open query tabs | 50 |
| Rows per result | The connection's row limit (up to 100,000), within the memory ceiling in [Results](results.md#limits) |

## Principles

1. **Nothing remote on the interactive path.** The explorer and search read
   the local metadata cache. The network is used only for explicit actions:
   test, run, refresh.
2. **Load on demand.** The explorer reads children page by page; results are
   bounded by the row limit while they are read.
3. **Stream, do not buffer everything.** Drivers deliver rows to a `RowSink`
   and stop reading at the limit.
4. **A frame costs what is on screen.** Long collections are virtualized and
   derived data is cached by its inputs ([UI guidelines](ui-guidelines.md#how-gpui-renders-and-what-follows-from-it)).
5. **The UI thread only does UI.** Network, disk, conversion and dropping of
   large data happen on the Tokio runtime or a background executor.
6. **Compact data in memory.** Results use a compact representation
   ([ADR-0008](adr/0008-compact-result-storage.md)).

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
rows with five columns off the UI thread. After a two-second warmup, each
size runs idle (75 ticks), rectangular selection (250 ticks), selection near
the final row (250 ticks) and scrolling (125 ticks), with a nominal 40 ms
timer per tick. Closing the window cancels the run at its next update.

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
- **Benchmarks.** `benches/hot_paths.rs` ([ADR-0010](adr/0010-benchmarks-with-criterion.md))
  measures building the grid view at 100,000 rows, converting 100,000 × 30
  driver rows to result data, splitting a 10,000-statement script and, in a
  cache of 500,000 objects, opening the file to read a first page (what every
  expansion cost before the cache stayed open) and reading the first page and
  the page after 4,800 children through the cache kept open.
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

## Known deviations

| Standard | Current code |
| --- | --- |
| Long collections are virtualized | Saved queries and history build all their children |
| Metadata search within budget at any length | A query of one or two characters, which the trigram index cannot match, scans the name index: about 93 ms at 500,000 objects. Three characters or more take about 7.4 ms |
| Frequently changing regions are cached entities | The result grid is (`ResultGrid`, [ADR-0012](adr/0012-result-grid-as-a-cached-entity.md)); the editor and the assistant still render inside `Workspace` |
| A grid interaction renders only the grid ([ADR-0012](adr/0012-result-grid-as-a-cached-entity.md#how-to-validate)) | Not possible in GPUI 0.3.11: a notified view renders with all its ancestors, so hover, scroll and selection also render `AppView`, `GroupedTabs` and `Workspace`, about 1.5 ms of a 3.6–4.4 ms p50 grid frame. `.cached(...)` spares the grid when only the workspace changes: typing 15 characters rendered `Workspace` 15 times and `ResultGrid` never (frame probe, release, Windows, 2026-11-02). [ADR-0013](adr/0013-thin-ancestors-of-frequently-notified-views.md) (Proposed) replaces this validation with the frame budget and keeps the ancestors thin. The editor gutter no longer splits the script on every frame, which took grid frames to 57 ms with 1,000 statements |
| Filters are debounced | The result filter rebuilds the grid view on every keystroke |
