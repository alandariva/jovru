# Results

How query results are read, kept, shown and copied.

## Principle

A result is **bounded** while it is read, **converted off the UI thread**, and
**rendered only where it is visible**. The UI thread receives display-ready
data and builds the rows on screen, so a frame costs the same for ten rows or
the largest result the limits allow ([Performance](performance.md)).

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
- **pt-BR numbers** groups digits and swaps the decimal separator as text,
  without passing through floating point.
- Headers read `name : type`, the type in a neutral tone.

## Model in memory

- `jykre::result::ResultData` holds the columns (name and database type) once,
  all cell text in one UTF-8 buffer, the end offset of each cell and a NULL
  bitmap ([ADR-0008](adr/0008-compact-result-storage.md)). Cells are read
  through `value(row, column)`, which returns `None` for NULL. Its size in
  memory is exact (`memory_bytes`).
- `ResultBuilder` is the `RowSink` that `backend::query` hands the driver: it
  converts each row as it arrives, on the Tokio runtime, and drops the driver
  cells there. It stops reading at the memory ceiling and records which limit
  applied.
- A tab shares its result as `Rc<ResultData>`. A new run releases the previous
  result before the next one arrives; a result that is no longer held is freed
  on the runtime's blocking pool (`backend::release`), never on the UI thread.
  Closing a tab releases everything.
- `jykre::grid::GridView` holds what the grid derives from the result: the rows in display
  order (after the open filter and the sort), the column widths (sampled from
  the first 200 rows) and the value families. `GridCache` rebuilds it only
  when its key changes: the result version, the filter text, the sort or the
  number format. Hover, scroll and selection never rebuild it.
- The grid on screen is `ResultGrid`, its own entity embedded in the workspace
  with `.cached(...)` ([ADR-0012](adr/0012-result-grid-as-a-cached-entity.md)).
  It shares the tab's `Rc<ResultData>`, owns the selection, focus and scroll,
  and reports the selected count and copy notices to the workspace through
  `ResultGridEvent`. Selection and copy rules live in `jykre::grid`.
- Selections are stored as data-row indices, so filtering or sorting the view
  does not change what is selected; typing in the filter keeps the selection,
  a selection whose ends are hidden is not shown (and counts as no cells), and
  it shows again when they are.

## Limits

| Limit | Value |
| --- | --- |
| Rows per page | Per connection, 1 to 100,000, default 1,000 (`ExecutionOptions.max_rows`). The driver reads one row ahead, so a result with exactly the limit has no further page. **New query for table** writes the same limit into its `LIMIT` (`backend::preview_sql`) |
| Memory per page | 256 MB (`DEFAULT_MEMORY_CEILING`); `ResultBuilder` stops reading even below the row limit, and the rest of the result is not read |
| Memory per result tab | 256 MB of pages; the oldest pages are released first and can no longer be shown |
| Statement timeout | Chosen per tab in the editor toolbar: 30 s, 1 min, 5 min (default), 15 min, 1 h or none (`ExecutionOptions.timeout`) |
| Column width | 110 to 360 px, from the header and the first 200 rows |

- The row limit is set in the connection dialog (**Row limit per statement**)
  and stored with the connection's preferences.
- A result is read page by page. The first page arrives with the run; the
  footer's arrows show the previous or next page, and a page not read yet is
  read from the result the session keeps open, never by running the
  statement again. Running anything else in the tab closes that result, and
  its tab then says that more rows were not read.
- The footer shows the rows of the page ("Rows 1,001–2,000") and says when
  the memory limit stopped reading, with the limit in its tooltip.

## Statements and results

- Each statement a run executes gets a result tab, `Result 1`, `Result 2`,
  ... `Ctrl Enter` puts its result in the tab on screen, `Ctrl Shift Enter`
  opens a new tab and `Ctrl Alt Enter` replaces every tab with one per
  statement. A **pinned** tab (its pin icon) is never replaced by a run:
  `Ctrl Enter` with a pinned tab on screen replaces the last unpinned tab
  created, or opens one, and `Ctrl Alt Enter` keeps pinned tabs and numbers
  the new ones after them. Result tabs can be closed.
- **Run this result's statement again** (`↻`) runs the statement stored in
  the tab on screen, as it ran, into that same tab; the editor's cursor plays
  no part.
- **Messages** is the tab's log (`jykre::run_log`): every statement that
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
  transaction, which the toolbar shows ([User experience](ux.md#always-visible-states)).
- The editor gutter shows each statement's status (running, success, error,
  cancelled); a status resets when its statement's text changes.

## Grid

- A row-number column, then one column per result column.
- Selection: click a cell; Shift+click or Shift+arrows extend a rectangular
  block; click a row number to select the row, Shift+click another row number
  to extend; `Ctrl A` selects everything visible; `Esc` clears. Arrows move the
  focused cell and scroll it into view.
- The footer shows the number of selected cells when more than one is
  selected.
- Right-clicking outside the selection selects the cell under the pointer
  first, so the context menu acts on what is under it.
- The filter bar matches text in any column, ignoring case, and stops hiding
  rows when it is closed. It matches values, not how they are drawn: a NULL
  never matches, even a filter for "null", while the text 'NULL' does.
- The filter searches only the rows held for the page on screen. Below the
  field it says so ("Filters this page only (1,000 rows)…") whenever the
  result has other pages or stopped at a limit, and an empty match reads "No
  rows on this page match the filter", never "no results".
- Each result tab keeps its own selection and scroll: switching tabs and
  coming back to the same page restores them (`ResultGrid` keeps them by
  `ResultId`); another page starts at the top with nothing selected.

## Copying

- `Ctrl C` copies the selection as tab-separated text that pastes into
  spreadsheets. `Ctrl Shift C`, or **Copy with headers** in the context menu,
  adds the column names first.
- A single cell copies its exact value. Fields with tabs, line breaks or
  quotes are quoted with doubled quotes; NULL becomes an empty field.
- **Copy this page as CSV** copies the page on screen with headers, `;` as the
  separator and every value quoted, using the original values regardless of
  presentation; NULL becomes an empty field.
- Copies above 50 MiB ask for confirmation, and copying runs off the UI thread
  when it would exceed a frame.

## Footer

The footer is the single place that reports how the last run ended: rows,
rows affected, row limit reached, cancellation or error, and the elapsed time.
The notice bar is kept for the outcome of user actions such as copy, save or
format ([User experience](ux.md#feedback)).

## Acceptance criteria

- [ ] Text, exact numbers (`bigint`, `numeric(38,10)`), dates, booleans, NULL,
      JSON and binary show correctly for PostgreSQL, MySQL and Databricks.
- [ ] NULL is visually distinct from the text `NULL`, from empty text and from
      whitespace.
- [ ] Scrolling, hovering and selecting stay within the frame budget at the
      maximum row limit.
- [ ] Cell, block and row selections copy with and without headers and paste
      correctly into a spreadsheet.
- [ ] Running in one tab does not change another tab's results.

## Known deviations

| Standard | Current code |
| --- | --- |
| Copy confirmation above 50 MiB, off the UI thread when large | Copies always run on the UI thread, without confirmation (about 85 ms for 500,000 cells) |
