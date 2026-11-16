# Results: model, values and limits

Part of [Results](README.md).

## Value types

Drivers deliver each value as a `Cell`. PostgreSQL and MySQL use the text
protocol, so most of their values arrive as `Text` with the column's type
alongside:

| `Cell` | Rule |
| --- | --- |
| `Null` | NULL, always distinct from the text `NULL` and from an empty string |
| `Boolean` | Shown as `true` / `false`; for boolean columns delivered as text, `cell_format` maps `t`/`f` and `1`/`0` |
| `Integer`, `UnsignedInteger` | Exact |
| `Decimal` | The database's exact text; **never** parsed into a float |
| `Float` | Approximate types (`real`, `double`) only |
| `Text` | As returned |
| `Temporal` | The database's text, **without time-zone conversion** |
| `Binary` | Shown as hexadecimal with a `0x` prefix |

JSON, arrays, structs and unknown types arrive as text.

Classic Databricks Command Execution preserves numeric JSON lexemes and
structured cell text before general JSON decoding. Its returned result is
bounded by the API's server cap and paged locally; a server-truncated result is
shown as truncated rather than being rerun to obtain additional rows.

## Value presentation

`cell_format` groups column types into families that decide colour and
alignment only; the value itself is never converted.

| Family | Colour | Alignment |
| --- | --- | --- |
| Text, JSON, arrays, unknown | `#E2E8F0` | Left |
| Boolean | `#E2E8F0` | Centre |
| Number | `#60A5FA` | Right |
| Temporal | `#C4B5FD` | Left |
| Binary | `#94A3B8` | Left |
| NULL, in any column | `#64748B`, italic | That of the column |

- Empty text shows `(empty)` and whitespace-only text shows `␠ N spaces`, in
  the neutral colour, so neither is mistaken for NULL.
- A cell is one line: line breaks show as `↵`. The full value stays in the
  tooltip and in copies.
- A tooltip appears only when the value does not fit its column or has line
  breaks, so it never covers neighbours or the context menu needlessly.
- **pt-BR numbers**, a toggle in the gear of the result toolbar, groups
  digits and swaps the decimal separator as text, without passing through
  floating point.
- Headers read `name : type`, the type in a neutral tone.

## Model in memory

- `jovru::result::ResultData` holds the columns (name and database type) once,
  all cell text in one UTF-8 buffer, the end offset of each cell and a NULL
  bitmap (D-0008). Cells are read through `value(row, column)`, which returns
  `None` for NULL; the grid and the copy code use that accessor, and the
  representation never alters a value. Its size in memory is exact
  (`memory_bytes`).
- `ResultBuilder` is the `RowSink` that `backend::query` hands the driver: it
  converts each row as it arrives, on the Tokio runtime, and drops the driver
  cells there. It stops reading at the memory ceiling and records which limit
  applied.
- A tab shares its result as `Arc<ResultData>` (D-0022): `Page::data` in
  `result_tabs`, the pages a tab lets go of (`ResultTabs::take_dropped`) and
  the grid's current result. One shared value serves the grid, the chart,
  Messages and the exports, and it is never copied. A new run releases the
  tab's previous result when it starts (so a cancelled run leaves the grid
  empty); a result that is no longer held is freed on the runtime's blocking
  pool (`backend::release`, which takes the `Arc` itself), never on the UI
  thread. Closing a tab releases everything.
- Work that reads a whole page (a large copy, an export, the grid's
  statistics) takes a clone of the `Arc` to the database runtime or GPUI's
  background executor, never the UI thread. A task that must not keep a
  released page alive holds a `Weak` and stops when it is gone; any task
  holding a page is bounded by the work it does and cancellable. No
  `Rc<ResultData>` comes back.
- `jovru::grid::GridView` holds what the grid derives from the result: the rows in display
  order (after the open filter and the sort), the column widths (sampled from
  the first 200 rows) and the value families. `GridCache` rebuilds it only
  when its key changes: the result version, the filter text, the sort or the
  number format. Hover, scroll and selection never rebuild it.
- The grid on screen is `ResultGrid` (`src/app/result_grid.rs`), its own
  entity embedded in the workspace with `.cached(...)` (D-0012). It shares the
  tab's `Arc<ResultData>`, owns the selection, focus and scroll, and reports
  to the workspace through `ResultGridEvent` (the selected count, the search's
  counts, notices, statistics). Selection and copy rules live in
  `jovru::grid`.
  - **Who owns what.** The grid owns the current result, `GridCache`, the
    selection, the focus and scroll handles, row rendering, mouse and keyboard
    handling, the context menu and copy. The workspace owns the run (SQL,
    statement statuses, execution state), the filter input, the number-format
    toggle, the result tabs, the footer and the notice bar, and the other
    views of the same result (chart, Messages, export).
  - **How they talk.** The workspace calls methods on the grid that update it
    and notify only it (`set_result`, `set_filter`, `set_number_format`); the
    grid answers with events, and the workspace keeps only the small state they
    carry. It never reaches into the grid's fields.
  - **Rules.** Hover, scroll, selection and copy notify only the grid, never
    `Workspace`; a change in the workspace (typing, toolbar, footer) reuses the
    grid's last frame through `.cached(...)`, whose style gives the grid its
    size (`size_full`). Everything that needs no GPUI (the grid view,
    selection blocks, copy formatting) stays in plain functions with unit
    tests. New grid features live in `ResultGrid`.
- Selections are stored as data-row indices, so filtering or sorting the view
  does not change what is selected; typing in the filter keeps the selection,
  a selection whose ends are hidden is not shown (and counts as no cells), and
  it shows again when they are. Its columns are result columns too: a block
  spans the display positions between its ends, so moving or hiding a column
  between them changes which columns it covers, and an end in a hidden column
  hides the selection until that column is shown.
- `jovru::column_layout::ColumnLayout` is how the grid lays out a result's
  columns: every column in display order, which are hidden, and how many
  lead the order frozen. It maps display positions to result columns and
  back, and belongs to the result tab: kept in memory while the tab is open
  (its pages share it and switching tabs restores it, as the widths), never
  saved, and started again when the tab's result has other columns. The
  grid shares it (`Arc`) with what reads the result as shown: copies,
  exports, the records view and the statistics.

## Limits

| Limit | Value |
| --- | --- |
| Rows per page | Per connection, 1 to 100,000, default 1,000 (`ExecutionOptions.max_rows`). The driver reads one row ahead, so a result with exactly the limit has no further page. A table's **Data** view reads `SELECT * FROM` the table without a `LIMIT` (`backend::data_sql`), so its pages are these; **New query for table** writes the same limit into its `LIMIT` (`backend::preview_sql`) |
| Memory per page | 256 MB (`DEFAULT_MEMORY_CEILING`); `ResultBuilder` stops reading even below the row limit, and the rest of the result is not read |
| Memory per result tab | 256 MB of pages; the oldest pages are released first and can no longer be shown |
| Statement timeout | Chosen per tab in the editor toolbar: 30 s, 1 min, 5 min (default), 15 min, 1 h or none (`ExecutionOptions.timeout`) |
| Column width | 110 to 360 px, from the header and the first 200 rows |

- The row limit is set in the connection dialog (**Row limit per statement**)
  and stored with the connection's preferences
  (`connection.<connection-id>.row_limit`, [Persistence](../persistence/README.md));
  a missing or out-of-range value reads as the default, 1,000. A 100,000 ×
  30 result takes about 38 MB (D-0008).
- A result is read page by page. The first page arrives with the run; the
  footer's arrows show the previous or next page, and a page not read yet is
  read from the result the session keeps open, never by running the
  statement again. Running anything else in the tab closes that result, and
  its tab then says that more rows were not read.
- The footer shows the rows of the page ("Rows 1,001–2,000") and says when
  the memory limit stopped reading, with the limit in its tooltip. While the
  filter is open it counts the rows the grid shows: "3 of 1,000 rows". The
  elapsed time reads in its own scale (`850 ms`, `12.4 s`, `3 min 3 s`),
  and a run in progress shows its time after 2 s.

## Statements and results

- Each statement a run executes gets a result tab, `Result 1`, `Result 2`,
  ... `Ctrl Enter` puts its result in the tab on screen, `Ctrl Shift Enter`
  opens a new tab and `Ctrl Alt Enter` replaces every tab with one per
  statement. A **pinned** tab (its pin icon) is never replaced by a run:
  `Ctrl Enter` with a pinned tab on screen replaces the last unpinned tab
  created, or opens one, and `Ctrl Alt Enter` keeps pinned tabs and numbers
  the new ones after them. Result tabs can be closed.
- A table's Data view ([User experience](../ux/states.md#always-visible-states))
  calls the result tab of its `SELECT * FROM` the table **Data**
  (`result_tabs::ResultTab::title`), wherever that statement ran from;
  opening the table again shows it, or reads it into a new tab, never over
  another result (`ResultTabs::data_view`).
- **Run this result's statement again** (`↻`) runs the statement stored in
  the tab on screen, as it ran, into that same tab; the editor's cursor plays
  no part.
- **Messages** is the tab's log (`jovru::run_log`): every statement that
  ended, newest first, with how long ago, its outcome (rows, rows affected,
  error or cancellation) and how long it took. It is independent of the
  result tabs, so replacing a tab does not lose an `UPDATE`'s outcome. The
  log keeps the last 500 entries; times are relative ("5 min ago") because
  showing the local clock time needs a date library, which is a dependency
  decision.
- `COMMIT` and `ROLLBACK` from the toolbar's transaction controls open no
  result tab; they appear only in Messages, and an error goes to the notice
  bar.
- A statement that fails shows its error in its own tab; the tabs of the
  statements before it keep their results, and the statements after it do
  not run.
- A statement without rows shows the rows it affected.
- Statements run in order and stop at the first failure. The message says how
  many completed; whether they are committed depends on the session's
  transaction, which the toolbar shows ([User experience](../ux/states.md#always-visible-states)).
- The editor gutter shows each statement's status (running, success, error,
  cancelled); a status resets when its statement's text changes.
