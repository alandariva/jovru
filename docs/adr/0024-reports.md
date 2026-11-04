# ADR-0024: Reports, with stored definitions, tabs of their own and a print HTML the user saves

- **Status:** Proposed
- **Date:** 2026-11-04
- **Authors:** AI agent, at the maintainer's request (plan approved 2026-11-04)

## Context

Users run the same queries again and again with different filters, and want
the result in a form they can hand on: a table, sometimes a chart, pasted into
a message or a spreadsheet, or printed to PDF. Today the application has SQL
tabs, saved queries and the history, all kept in the `workspace.state`
setting ([Persistence](../persistence.md#schema-configdb)).

A new "Reports" section in the left rail builds reports from saved SQL with
named parameters ([ADR-0023](0023-named-parameters-bound-by-drivers.md)). It
runs into these constraints:

- `WorkspaceState::to_value` (`src/workspace_state.rs`) writes back only the
  fields it knows. A version that does not know a field would erase it on its
  next save.
- Query results are never written to disk ([Persistence](../persistence.md)).
- Each tab owns one session ([ADR-0014](0014-a-session-per-tab.md)), and every
  place in `AppView` that handles tabs assumes they are SQL `Workspace`s.
- A frame costs what is on screen (golden rule 7), and exact numbers never
  pass through floating point (golden rule 9) ([golden rules](../README.md#golden-rules)).
- Adding a dependency needs the maintainer's approval
  ([Stack and dependencies](../stack-and-dependencies.md)).

## Decision

### Storage

- Reports are stored under a new `reports` key of the `settings` table in
  `config.db`, as `{"version": 1, "next_id": n, "reports": [...]}`. `settings`
  already holds JSON values by key, so no migration is needed.
- Each report holds a stable numeric `id` (its title is only a label), title,
  description, connection, SQL, parameters (name, label, kind, default,
  required), an optional chart, and when it was created and last changed.
  - The connection is stored as `{"id": …, "name": …}`: the ID survives a
    rename, and the name is the one last seen.
  - Keys this version does not know are kept and written back, in the
    library and in each report.
- Reports do not live inside `workspace.state`, because its writer keeps only
  the fields it knows: an older version opening the same configuration would
  erase every report on its next save. Under a key of their own, only the
  reports library writes them.
- A missing value is an empty library. A value that cannot be read is
  reported and never written over.
- Only definitions are stored. Never results, and never the values typed for
  a run; each parameter keeps only the default its author chose.
- The library is written half a second after changes settle and when the
  application quits. The demo writes nothing.

### Report tabs

- A report opens in a tab of its own kind: a `ReportView` entity, tracked in
  `AppView::report_tabs: HashMap<TabId, ReportTab>`, apart from the SQL
  workspaces in `panels`. Each place that assumes every tab is a `Workspace`
  (closing, moving, the run shortcut, focus, catalogs, the password prompt)
  gains a branch for reports.
- A report tab sits in its connection's group, like a SQL tab.
- Each report tab has its own session ([ADR-0014](0014-a-session-per-tab.md)),
  opened on its first run and closed with the tab. A report never runs in a
  SQL tab's session, so the values bound for its parameters
  ([ADR-0023](0023-named-parameters-bound-by-drivers.md)) stay in its own,
  after the run too, until its next run overwrites them.
- A run follows a SQL tab's flow: the password prompt, the risk confirmation on
  a production connection (`guard::risks`), `backend::query` with the
  parameters, and results of a superseded run dropped.
- The result is not paged: it runs up to the connection's row limit, under the
  memory ceiling of [ADR-0008](0008-compact-result-storage.md).
- In this first version, report tabs are not reopened after a restart. The
  definitions are kept; the open tabs are not.
- The report can show the SQL that ran, markers replaced.

### Chart

- A report may show a bar, line or pie chart built from one label column and
  one or more value columns, which must be numbers.
- It is drawn with `gpui_kit::component::chart`, already a dependency:
  - bars with `BarChart`, as small multiples, one per value column;
  - lines with `AreaChart` and a transparent fill, one series per value column;
  - pies with `PieChart`.
- Every chart is built with `.appear(false)`. A new result then paints once,
  in place, instead of drawing in over several frames.
- The chart's data is derived outside the view (`reports::chart`) when the
  result changes, never per frame, and is capped: 50 bars, 1,000 points per
  line and 10 slices plus "Others". The chart says how many it shows of how
  many.
- `f64` is used only for geometry: bar lengths, point positions and slice
  angles. Every value the user reads (tooltips, the table, the print HTML) is
  the database's text. The "Others" slice is summed exactly, as a decimal.

### Output

- **Copy.** The result is copied to the clipboard as CSV or TSV, reusing
  `cell_format::result_csv` and `cell_format::copy_cells`, with the existing
  confirmation above 50 MiB (`grid::COPY_CONFIRM_BYTES`). Reports write no CSV
  file.
- **Print HTML.** This is the one exception to "query results are never written
  to disk":
  - "Export for print…" opens the system's save dialog
    (`cx.prompt_for_new_path`). The application writes one HTML file, only at
    the path the user picked, and appends `.html` when the name lacks it,
    because the Windows dialog does not.
  - The file is then opened with the system's browser
    (`cx.open_with_system`), where the user prints it or saves it as a PDF.
  - The file is self-contained. A Content-Security-Policy meta allows no
    external resource, every text from the database or the user is escaped,
    and the chart is inline SVG. A print stylesheet sets A4, repeats the
    header row on each page and keeps rows whole. The file says when it was
    generated, in UTC, and its print button is hidden from the printout.
  - It is written through `tokio::fs` on the runtime (`backend::run`), never on
    the UI thread. No temporary file is written, and no PDF library is added.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Reports inside `workspace.state`, next to the saved queries | One value and one writer | Its writer keeps only the fields it knows | An older version would erase every report |
| A new table in `config.db` | Typed columns | A migration and repository changes for data that is always read whole | A JSON value under its own key is how preferences and the workspace already live |
| A report as a mode of the SQL tab (`Workspace`) | No new tab kind | Every SQL tab carries form, chart and export state | A separate entity leaves SQL tabs as they are |
| Reopen report tabs after a restart now | Continuity | Needs a new field in `workspace.state`, which older versions drop | Deferred; recorded as technical debt |
| A charting or PDF crate | Direct PDF output; more chart kinds | A new dependency, fonts and layout code | `gpui-component` already draws charts, and browsers already print to PDF |
| Print HTML in a temporary file, opened at once | One click | Results on disk where the user did not choose; a file left behind | The user picks where the file goes, each time |

## Consequences

- Positive:
  - Reports survive older versions of the application.
  - `config.db` holds report definitions, never results or typed values.
  - SQL tabs are unchanged, and no dependency is added.
- Negative or costs:
  - One more settings key, with its own writer and debounce.
  - Two kinds of tabs: every action over all tabs needs a branch for reports.
  - Open report tabs are lost on restart, and a report's result is not paged.
  - A chart shows at most 50 bars, 1,000 points per line or 10 slices.
  - The print HTML holds a result on disk, where the user put it. The
    application neither tracks nor deletes it.
- What becomes required or forbidden:
  - The `reports` value never holds results or the values typed for a run.
  - The print HTML is written only to a path chosen in the save dialog, and
    never as a temporary file.
  - Chart values are read as text. `f64` is used for geometry only, and every
    chart is built with `.appear(false)`.
  - The demo writes no report.

## How to validate

- Library tests:
  - JSON round trip: unknown keys are kept, and a value that cannot be read
    is not written over.
  - Parameters: reconciling them with the markers in the SQL, and validating
    their values.
  - Chart data: the caps, NULL, a missing or non-numeric column, and an exact
    "Others" slice.
  - Print HTML: a `<script>` in a cell comes out escaped, no URL points
    outside the file, the print stylesheet is present and the SVG appears only
    with a chart.
  - The suggested file name, and the `.html` extension added when it is
    missing.
- A visual check in the application, recorded apart:
  - create a report from the section and another from a SQL tab;
  - run with text, number and date parameters, and see a field's error;
  - see bar, line and pie charts;
  - copy as CSV;
  - export the HTML and open it in the browser;
  - close a tab with unsaved edits;
  - use Tab, Enter and Ctrl Enter, and check that focus stays visible.
