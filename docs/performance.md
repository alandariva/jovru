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
| Expand an explorer node from the local cache | ≤ 50 ms p95 | Not measured | Timing around the cache read |
| Metadata search (`Ctrl Shift P`) over the local cache | ≤ 50 ms p95 | Not measured | Timing around the search |

The numbers are initial targets. Changing a budget is a maintainer decision
recorded in this table. "Not measured" rows are measured the first time a task
touches that path.

## Scalability targets

| Dimension | Minimum supported |
| --- | --- |
| Saved connections | 200 |
| Metadata objects per connection | 500,000 (paged from the local cache) |
| Open query tabs | 50 |
| Rows per result | The row limit (1,000 today; up to 100,000 once configurable, within the memory ceiling in [Results](results.md#limits)) |

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
6. **Compact data in memory.** Large results use a compact representation
   ([ADR-0008](adr/0008-compact-result-storage.md), proposed).

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

- **Frame probe.** Record an `Instant` at the start of the root view's
  `render` and read it in a `canvas` painted last; that is render, layout and
  paint for the frame. Drive the scenario (scroll offsets, hover, selection
  changes) programmatically and report median, p95 and maximum.
- **Heartbeat.** A foreground task that sleeps 5 ms in a loop; any extra delay
  is time the UI thread was busy. On Windows the timer resolution sets a floor
  of about 10–15 ms.
- **Data.** Use the dev databases (`sales.event_log` has 100,000 rows; see
  [Testing](testing.md#local-databases-for-manual-testing)). Raise the row
  limit only in the measurement build, never in a shipped default.
- **Unit test budgets.** Data-sized algorithms get a test with a generous time
  budget (ten times the measured time or more), such as
  `a_large_result_builds_its_grid_view_within_budget`. They catch complexity
  regressions, not small slowdowns.
- **Benchmarks.** [ADR-0010](adr/0010-benchmarks-with-criterion.md) proposes
  `criterion` benchmarks for the grid view, result conversion, statement
  splitting and metadata paging, run in quick mode by
  `cargo xtask check --full`.
- **Report.** A task that touches a measured path reports before and after
  numbers and the build profile in its report ([Workflow](workflow.md#task-report)).

## Known deviations

| Standard | Current code |
| --- | --- |
| A reusable frame probe and heartbeat in the repository | The 2026-11-02 measurements used a temporary harness that was not committed |
| Benchmarks for hot paths | None yet ([ADR-0010](adr/0010-benchmarks-with-criterion.md), proposed) |
| Long collections are virtualized | The explorer tree, saved queries and history build all their children |
| Frequently changing regions are cached entities | The result grid renders inside `Workspace`; [ADR-0012](adr/0012-result-grid-as-a-cached-entity.md) (accepted) moves it to its own cached entity |
| Filters are debounced | The result filter rebuilds the grid view on every keystroke |
| Compact result storage | Results are `Vec<Vec<String>>` plus a `HashSet` of NULL positions ([ADR-0008](adr/0008-compact-result-storage.md), proposed) |
| Large data dropped off the UI thread | Replacing a result drops the previous one on the UI thread (negligible at the 1,000-row limit) |
