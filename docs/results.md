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
- **pt-BR numbers**, a toggle in the gear of the result toolbar, groups
  digits and swaps the decimal separator as text, without passing through
  floating point.
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
  block; pressing a cell and dragging selects the block up to the cell under
  the pointer, and dragging past an edge of the grid scrolls (faster the
  further the pointer goes) while the block grows; click a row number to
  select the row, Shift+click another row number to extend, or drag over row
  numbers to select those rows; `Ctrl`+click a header selects its column over
  the displayed rows (`Ctrl Shift`+click extends to it from the anchor's
  column); `Ctrl A` selects everything visible; `Esc` clears. Arrows move the
  focused cell and scroll it into view, rows and columns. `Home` and `End` go to the first and
  last cell of the row, `Ctrl Home` and `Ctrl End` to the first and last cell
  of the page, `Page Up` and `Page Down` a screen of rows; `Shift` extends the
  block from its anchor.
- A click on a column header sorts the rows of the page by that column's
  values: ascending, then descending, then the server's order again, with an
  arrow on the header. Numbers compare exactly, as decimal text, also in exponent form (never as
  floats); other values by their text; NULLs come last either way and equal
  values keep the server's order. Like the filter, it sorts only the rows
  held for the page; another result tab starts unsorted.
- The footer shows the number of selected cells when more than one is
  selected, and, when number columns are among them, the **sum, average,
  median, minimum and maximum** of their numbers (`jykre::stats`), in the
  number format on screen; its tooltip says how many numbers they come from.
  They are exact: added and divided as decimal text (`jykre::decimal`), never
  as floats. The average and a median between two values keep six decimals
  past the values' own, rounded half away from zero, without trailing zeros.
  Text such as `NaN` is skipped, NULL is not a value, and a number too far
  from the decimal point to add (`1e500`) shows the sum and average as `—`.
  Up to 20,000 cells are summarized at once; a larger selection shows
  "Calculating…" while it is summarized off the UI thread, which reads the
  shared page in place
  ([ADR-0022](adr/0022-result-pages-shared-across-threads.md)). A drag is
  summarized when it ends.
- A header's right edge resizes its column (48 to 1,600 px); a double click on
  it, or **Fit the column to its values** in the menu, fits the column to its
  header and the values of the first 5,000 displayed rows. **Reset column
  widths** goes back to the measured ones. Widths belong to the result tab:
  its pages share them and switching tabs restores them.
- A double click on a cell, or `Enter`, opens the side panel on the cell's
  **full value**: read-only text that can be selected, JSON laid out with
  indentation without changing any token, up to the first 1 MB (**Copy**
  copies all of it), NULL shown as NULL. The panel follows the focused cell.
  Its other tab, **Statistics**, summarizes one of three things, chosen at
  its top:
  - **Selection**: the selected cells (two or more): how many rows and
    columns, values and NULLs, the sum, average, median, minimum and maximum
    of the numbers of number columns together (as the footer shows), and one
    line per selected column.
  - **Result**: every row of the page the filter shows, with one line per
    column. Numbers of different columns are not added together.
  - **Column**: the focused column (or the one a line or the menu chose),
    over the rows of the page the filter shows: NULLs, distinct values
    (numbers by value), empty texts, minimum and maximum in the order the
    header sorts by, sum, average and median for numbers, the shortest and
    longest text, and the five most frequent values.

  A column's line shows its type and, for numbers, the sum and average,
  otherwise its distinct values, its NULLs, and a thin meter of how much of
  it holds a value; a click (or `Enter`) opens that column's statistics.
- **Small charts** in the Statistics tab, each with its figures written
  beside it, so no value is shown by colour alone: a thin bar splits values
  from NULLs (and, for a text column, from empty texts), with a swatch on
  each of those lines; a number column's **Distribution** draws up to 20
  equal slices of its range between its smallest and largest value, whose
  tooltip says how many values a bar holds and its own smallest and
  largest, as the database wrote them (the value is turned into floating
  point only to choose its bar); each of the most frequent values has a
  meter of its share of the rows, with the percentage. The colours are the
  `CHART_*` tokens: the accent for values, neutrals for NULL and empty
  text. Up to 100 columns are listed; a line says how
  many more there are. The selection's and the page's statistics are
  computed off the UI thread, reading the shared page in place; a column's
  are read a chunk per frame and computed off the UI thread. `Esc` closes the
  panel.
- The toolbar's **Statistics** button (beside **Results**) shows the table
  with this tab open: on the selection when two or more cells are selected,
  otherwise on the result. It stays highlighted while the tab is open, and a
  second click closes the panel. The menu has **Statistics** and **Column
  statistics** too.
- **Show the selected rows as records** (toolbar or menu) shows the rows of
  the selection side by side: one line per column, one column per row, up to
  50 rows. A click on a value shows it in the side panel. `Esc` goes back to
  the table.
- Result tabs can be dragged to another place in their bar, or moved with
  `Ctrl Shift ←` and `Ctrl Shift →` on the focused tab. The tab on screen,
  numbers, pins and what a run replaces do not change.
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

- `Ctrl C` copies the selection in the format chosen in the **copy menu**
  (the copy button of the result toolbar, labelled with that format);
  `Ctrl Shift C`, or **Copy with headers** in the context menu, adds the
  column names first when the format can leave them out. The format starts
  as tab-separated text, which pastes into spreadsheets.
- The copy menu copies the selection (with or without the column names),
  chooses the format `Ctrl C` copies in, and holds the **Copy settings**:
  **Column names first** (text and CSV; off to start), **Quote every CSV
  value** (on to start; off quotes only values holding the separator, a
  quote or a line break, and empty text), the **CSV separator** (`;` to
  start, `,` or tab) and how text and CSV write **NULL** (an empty field to
  start, or the word `NULL`, never quoted). The choices apply to every tab
  and are remembered ([Persistence](persistence.md)).
- **Copy as**, in the context menu, copies the selection once in any
  format, with the same settings, without changing the format of `Ctrl C`.
- **Formats** (`jykre::copy_as`):
  - **Text (tab-separated)**: a single cell copies its exact value. Fields
    with tabs, line breaks or quotes are quoted with doubled quotes.
  - **CSV**: fields as the CSV export writes them with the same options,
    one line per row and no line break after the last.
  - **JSON** (an array with an object per row; repeated names get `_2`,
    `_3`) and **JSON Lines** (one object per line), as the export writes
    them.
  - **Markdown table** (`|` escaped, line breaks as `<br>`, NULL as `NULL`)
    and **HTML table** (`<table>` with the names in `<thead>`, every text
    escaped, line breaks as `<br>`, NULL as `NULL`, numbers aligned to the
    right). The clipboard holds text, so the HTML pastes as code.
  - **INSERT statements** (one per row) and **One INSERT for all rows**
    (`INSERT INTO … VALUES` with one line per row), into the table the tab
    was opened for, otherwise `table_name`.
  - **IN list** (its distinct values in parentheses, without NULLs) and
    **Values separated by commas** (every value, NULL included, row by row).
  - **Column names**, separated by commas.
- Values are never reinterpreted: a number of a number column is written
  as the database wrote it (in JSON only when it is a valid JSON number),
  booleans as `TRUE`/`FALSE` (`true`/`false`), NULL as `NULL` (`null`),
  anything else as a string escaped as the connection's dialect reads
  strings (`jykre::sql::string_literal`: PostgreSQL doubles the quote, MySQL
  and Databricks escape with a backslash). Names are quoted only when the
  dialect needs it, as the editor's suggestions quote them.
- **Copy this page as CSV** copies the page on screen with headers, `;` as the
  separator and every value quoted, using the original values regardless of
  presentation; NULL becomes an empty field. The copy settings do not apply.
- Copies estimated above 50 MiB (from the length of the cells, before they
  are built) ask for confirmation, and copying runs off the UI thread
  when it would exceed a frame.

## Export

**Export the result to a file** (the toolbar's download button, or
`Ctrl Shift E`) opens a bar under the result toolbar for the result tab on
screen. It writes the whole result, not the page, to a file chosen in the
system's save dialog (`jykre::export`, `backend::export`).

- **Formats:** CSV, Excel (XLSX), JSON (an array laid out as **Copy as
  JSON**), JSON Lines (one object per line), SQL `INSERT` (one per row, as
  **Copy as INSERT**) and Markdown. CSV starts as the CSV copy writes:
  `;` between fields, every value quoted, the column names first, no BOM,
  one line per record ending in `\n`. Its options are the separator (`;`,
  `,` or tab), **Quote every value** (off: only values holding the
  separator, a quote or a line break, and empty text), **Column names
  first** and a **Byte order mark (BOM)**. XLSX also takes **Column names
  first**. The bar remembers the last choices and folder
  ([Persistence](persistence.md)).
- **Values** follow the copies' rules, so a copy and an export of the same
  cells write the same text: NULL is an empty CSV field and empty text is
  `""`, numbers are written as the database wrote them (a JSON number only
  when valid), strings are escaped by the dialect, nothing is converted
  between time zones or changed to defend spreadsheets from formulas
  ([Security](security.md#exported-files)).
- **XLSX** writes one sheet, `Result`, in constant memory (each row goes to a
  temporary file as soon as the next starts). A number column's value is a
  number cell only when it has at most 15 significant digits, which Excel
  keeps; any other value of the column is text, so no digit changes. Boolean
  columns give boolean cells, everything else (dates included) text. Excel
  has no empty text: NULL and empty text both leave the cell empty. A sheet
  holds 1,048,576 rows and 16,384 columns, and a cell 32,767 characters; an
  export past them fails and suggests CSV or JSON, never cuts a value.
- **Where the rows come from** (`result_tabs::ExportSource`):
  - the tab holds the whole result: its pages are written;
  - the session keeps the rest open: the pages held, then the rest, read in
    parts of about 16 MB (whatever the row limit). What the export reads goes
    to the file only, so afterwards the tab no longer pages past what it
    holds, and the bar says so;
  - part of the result can no longer be read (pages released for memory,
    the rest closed by another statement, a limit or the server): the bar
    offers **Run again and export**, which runs the statement again in the
    tab's session after asking and writes everything it returns, and
    **Export the rows held**. Only a single statement that reads (a query
    without `INTO`, `FOR UPDATE`, a data change or a call;
    `jykre::sql::reads_only`) runs again; any other exports the rows held.
    The statement run again is logged in Messages.
- **While it runs** the bar shows the rows and bytes written (out of the
  total when the tab holds them all), the elapsed time after 2 s and
  **Cancel export**. An export that reads from the session holds it: runs,
  page reads and transaction controls wait, and closing the tab, moving it
  or saving its connection asks first. One that writes only the rows held
  leaves the session free.
- **The file appears only once it is complete.** Rows are written to a
  temporary file next to the chosen one, which replaces it after every row is
  written, flushed and synced. A failure or a cancellation removes the
  temporary file and leaves the chosen file as it was; the bar says so. A
  finished export says how many rows, the size and the time, with **Show in
  folder**.
- Each part is converted and written on the runtime's blocking pool, never
  the UI thread; the held pages are shared with the export, not copied
  ([ADR-0022](adr/0022-result-pages-shared-across-threads.md)).

## Reports

A report ([ADR-0024](adr/0024-reports.md),
[ADR-0025](adr/0025-report-panels.md)) shows, in each panel, the last result
of the panel's SQL up to its connection's row limit, without paging, in the
same grid. A panel's result may also take the memory ceiling divided by the
report's number of panels (`jykre::reports::run::memory_ceiling`), so a
report holds no more than one query tab.

- A panel's chart (`jykre::reports::chart`) finds its columns by name and
  draws at most 50 bars, 1,000 line points or 10 pie slices plus an "Other"
  slice that sums the rest. Numbers become floating point only for the
  drawing: the tooltips, the table and the print document keep the
  database's text, and the "Other" slice is summed exactly
  (`jykre::decimal::Sum`). A **number** panel shows the first row's value of
  one column exactly as the database wrote it, NULL in the NULL style.
- What a panel draws is computed off the UI thread, with its result, and
  again when its chart changes.
- **Copy as CSV** and **Copy for a spreadsheet**, in a panel's menu, copy its
  result with the rules above, asking first above 50 MiB.
- **Export for printing or PDF…** (`jykre::reports::export`) writes one HTML
  file where the user chooses: the title, description, the connections, the
  values the panels ran with, and each panel with its connection and
  database, its chart (as SVG) or number and, when it shows one, its table, in two columns like the report, with every text
  escaped, nothing loaded from elsewhere and print styles (A4, the header
  repeated on each page). Like an export, it reaches the disk only at the
  user's request, at the path they chose ([Persistence](persistence.md)).

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
- [ ] Sums, averages and medians of `numeric(38,10)` values are exact.
- [ ] An export of a result larger than the tab holds writes every row in
      order, in each format, and a cancelled or failed export leaves the
      chosen file as it was.

## Known deviations

| Standard | Current code |
| --- | --- |
| Copy off the UI thread when large | Copies above 50 MiB ask first, but the text is still built on the UI thread (about 85 ms for 500,000 cells). Pages are now shared as `Arc<ResultData>` ([ADR-0022](adr/0022-result-pages-shared-across-threads.md), Proposed), which lets the copy move off the UI thread; that change is not made yet |
