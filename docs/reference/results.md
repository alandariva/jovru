# Results

How query results are read, kept, shown and copied.

## Principle

A result is **bounded** while it is read, **converted off the UI thread**, and
**rendered only where it is visible**. The UI thread receives display-ready
data and builds the rows on screen, so a frame costs the same for ten rows or
the largest result the limits allow ([Performance](../standards/performance.md)).

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
  bitmap ([ADR-0008](../adr/0008-compact-result-storage.md)). Cells are read
  through `value(row, column)`, which returns `None` for NULL. Its size in
  memory is exact (`memory_bytes`).
- `ResultBuilder` is the `RowSink` that `backend::query` hands the driver: it
  converts each row as it arrives, on the Tokio runtime, and drops the driver
  cells there. It stops reading at the memory ceiling and records which limit
  applied.
- A tab shares its result as `Arc<ResultData>`. A new run releases the previous
  result before the next one arrives; a result that is no longer held is freed
  on the runtime's blocking pool (`backend::release`), never on the UI thread.
  Closing a tab releases everything.
- `jykre::grid::GridView` holds what the grid derives from the result: the rows in display
  order (after the open filter and the sort), the column widths (sampled from
  the first 200 rows) and the value families. `GridCache` rebuilds it only
  when its key changes: the result version, the filter text, the sort or the
  number format. Hover, scroll and selection never rebuild it.
- The grid on screen is `ResultGrid`, its own entity embedded in the workspace
  with `.cached(...)` ([ADR-0012](../adr/0012-result-grid-as-a-cached-entity.md)).
  It shares the tab's `Rc<ResultData>`, owns the selection, focus and scroll,
  and reports the selected count and copy notices to the workspace through
  `ResultGridEvent`. Selection and copy rules live in `jykre::grid`.
- Selections are stored as data-row indices, so filtering or sorting the view
  does not change what is selected; typing in the filter keeps the selection,
  a selection whose ends are hidden is not shown (and counts as no cells), and
  it shows again when they are. Its columns are result columns too: a block
  spans the display positions between its ends, so moving or hiding a column
  between them changes which columns it covers, and an end in a hidden column
  hides the selection until that column is shown.
- `jykre::column_layout::ColumnLayout` is how the grid lays out a result's
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
- A table's Data view ([User experience](ux.md#always-visible-states))
  calls the result tab of its `SELECT * FROM` the table **Data**
  (`result_tabs::ResultTab::title`), wherever that statement ran from;
  opening the table again shows it, or reads it into a new tab, never over
  another result (`ResultTabs::data_view`).
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
  ([ADR-0022](../adr/0022-result-pages-shared-across-threads.md)). A drag is
  summarized when it ends.
- **Column layout.** Any column can be hidden and shown again, moved to
  another place and frozen at the left edge, in every result grid (query
  tabs, report panels and a job's result). With the mouse, a header dragged
  onto another takes its place (onto a frozen header it is frozen there;
  out of the frozen ones it scrolls again); with the keyboard, `Shift F10`
  opens the menu on the focused header's column or the focused cell's. The
  menu has **Hide the column**, **Freeze the column at the left** (checked
  while frozen; choosing it again unfreezes), **Move the column left**,
  **right**, **to the start** and **to the end** (within the frozen columns
  or within the others), **Show a hidden column** (the first 20 by name;
  finding a column by name reaches the rest), **Show all columns** and
  **Reset the column layout**. The last column shown cannot be hidden.
- **Finding a column.** `Ctrl Shift F`, or the toolbar's **Find a column**
  button, opens a field above the grid that finds columns by name,
  ignoring case and accents (`jykre::column_search`: `cao` finds `Ação`,
  `strasse` finds `Straße`), with the matching part marked in the
  `MATCH_TEXT` and `MATCH_BG` colours. It lists the shown columns in display
  order, then the hidden ones, each saying it is hidden and that choosing it
  shows it. `↑` and `↓` choose a match, `Enter` (or a click) goes to it:
  the grid pans to the column and selects it over the displayed rows, after
  showing it when it was hidden, and takes the keyboard; `Esc` closes the
  field. The field says how many columns match. The matches are computed
  when the text or the layout changes, never per frame, and only the lines
  on screen are drawn, so thousands of columns cost a frame what eight do.
  The shortcut works in the grid that has the keyboard (a query tab's, a
  report panel's or a job's), the field included; elsewhere it opens the
  field in the query tab's result on screen. While the field is open with
  text, the records view lists only the shown columns it finds; closing
  the field lists them all again.
  While any column is hidden, a line above the table (and the records)
  says how many, with **Show all columns**. The values, their types and
  NULLs are untouched; only where a column is drawn changes.
- Frozen columns keep their place at the left edge, after the row numbers,
  which stay with them, while the other columns pan under them; without
  frozen columns the row numbers pan with the rest, as before. Moving the
  focused cell into a scrolling column pans it into the room the frozen
  ones leave.
- Only the columns in view are built: a row draws its frozen columns and
  the scrolling ones that cross the viewport, not those panned wholly under
  the frozen ones, with the width of the others kept as room
  (`column_layout::ColumnWidths::shown`), so a frame costs the columns on
  screen, not the result's. The widths in display order and where each
  column starts are kept from one frame to the next and computed again only
  when the view, the layout or a width changes (`grid::WidthsCache`). Each
  row, and its frozen part over it, is painted as one layer of the scene,
  so ordering a row's borders, backgrounds and values costs one look-up
  instead of one per border strip and background; the row with a cell
  being typed in is not, so the field's caret stays over its text. The
  demo result, whose five columns share the width in proportion, draws them
  all.
- A header's right edge resizes its column (48 to 1,600 px); a double click on
  it, or **Fit the column to its values** in the menu, fits the column to its
  header and the values of the first 5,000 displayed rows. **Reset column
  widths** goes back to the measured ones. Widths belong to the result tab:
  its pages share them and switching tabs restores them.
- A double click on a cell, or `Enter`, opens the side panel on the cell's
  **full value**: read-only text that can be selected, JSON laid out with
  indentation without changing any token, up to the first 1 MB (**Copy**
  copies all of it, exactly as stored, like the other copies below), NULL
  shown as NULL. The panel follows the focused cell.
  Its other tab, **Statistics**, summarizes one of three things, chosen at
  its top:
  The columns of each scope are the ones shown, in display order; hidden
  columns are left out.
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
  the selection side by side: one line per column shown, in display order,
  one column per row, up to 50 rows. A click on a value shows it in the side
  panel. `Esc` goes back to the table.
- Result tabs can be dragged to another place in their bar, or moved with
  `Ctrl Shift ←` and `Ctrl Shift →` on the focused tab. The tab on screen,
  numbers, pins and what a run replaces do not change.
- Right-clicking outside the selection selects the cell under the pointer
  first, so the context menu acts on what is under it.
- The filter bar matches text in any column, ignoring case, and stops hiding
  rows when it is closed. It applies once typing pauses for 150 ms, so a
  burst of keystrokes rebuilds the grid's view once. It matches values, not how they are drawn: a NULL
  never matches, even a filter for "null", while the text 'NULL' does.
- The filter searches only the rows held for the page on screen. Below the
  field it says so ("Filters this page only (1,000 rows)…") whenever the
  result has other pages or stopped at a limit, and an empty match reads "No
  rows on this page match the filter", never "no results".
- Each result tab keeps its own selection and scroll: switching tabs and
  coming back to the same page restores them (`ResultGrid` keeps them by
  `ResultId`); another page starts at the top with nothing selected.

## Editing

A query tab's result can be written back to its table
([ADR-0039](../adr/0039-editing-result-data-through-the-driver.md)); report
panels, jobs and the assistant's results stay read-only.

- **Which results.** The first editing key (or the toolbar's pencil) asks the
  tab's session what the result can be written back to
  (`Session::edit_target`, see [Drivers](drivers.md#editing-results)), once
  per result tab; runs cost nothing extra. When the result still has rows
  to read, the grid says first that asking closes them, so only the rows
  read so far can be edited. A result is editable when its stored columns
  come from one table with a primary key, or a unique key on NOT NULL
  columns, all of whose columns are in the result; every other result shows
  in a bar under the toolbar why it is read-only. A driver that cannot tell
  where a column comes from keeps its results read-only.
- **Pending edits.** `jykre::grid_edit::EditSession` holds them, outside the
  views: changed cells, NULLs, rows marked for deletion and inserted rows.
  The grid draws them over the cached view: the cache key gains the edits'
  version and the inserted rows, which follow the result's rows whatever the
  filter or sort, numbered `+`. A pending cell has an amber border, a written
  one a mint border; a row marked for deletion is struck through in red; an
  inserted row's untouched columns read `DEFAULT`. Rows are addressed in the
  whole result, so every page's edits are kept, and the key's values are
  taken when a row is first edited. Edits live in memory only: never in
  `config.db`, logs or telemetry, and they go with the result tab.
- **Values.** `F2` (or the pencil) opens a field over the focused cell with
  its value; `Enter` or leaving the field keeps what was typed as pending,
  `Esc` drops it. **Set to NULL** (`Ctrl Shift N`) sets the selected cells
  to NULL; typing `NULL` gives the four-letter text and clearing a cell the
  empty text. A binary column takes `0x` followed by hexadecimal digits.
  A value equal to the one read is no change. Computed, generated and
  identity-always columns, and a column whose cells arrived both as bytes and
  as text, cannot be edited and say so.
- **Rows.** **Insert a row** (`Alt Insert`) adds a row whose columns take the
  table's default until given a value. `Delete` marks the selected rows for
  deletion, which drops their pending values, or takes the mark away; an
  inserted row goes. A row whose key is NULL cannot be changed.
- **Write…** (`Ctrl Alt S`) opens one confirmation before anything is sent.
  It lists the table, each change with its key and, per column, the value
  read and the new one (NULL as NULL, every text quoted, so the text `NULL`
  reads `“NULL”`), the statements with their placeholders (the values bound
  apart), whether the changes are committed or stay in the tab's open
  transaction, the table's triggers and the foreign keys that cascade, and
  that a change made meanwhile to the same row is overwritten. On a
  connection marked Production the same dialog says so and its button reads
  **Write to production**. Changes are written in this order: deletions,
  updates, inserts. At most 10,000 changed rows are written at once.
- **The write** runs in the tab's session as a run of the tab: it takes the
  session, answers Stop (the edit bar's **Stop**, `Ctrl F2`), makes closing
  the tab ask, and is logged in Messages with its counts, never its values.
  Afterwards the toolbar shows the session's transaction. A written cell
  shows what was written, marked, since the database may store it otherwise
  (a rounded `numeric`, a default, a trigger); `↻` reads the rows back. A
  change that matched other than one row rolls everything back and its row
  is marked red.
- **Databricks** has no write: **Generate SQL…** opens a new query tab on the
  same connection and context with one statement per change and their
  values in its parameter fields, after a header comment saying that each
  statement commits on its own and nothing checks it changes one row. The
  table is the one the result's single-table `SELECT` names; a table that
  declares no key takes the columns the user selects (**Use as key**) before
  the first edit.
- **Discard** (`Ctrl Alt Z`) drops the pending edits, asking first. Running
  a statement in the tab while edits are pending, or closing the tab, asks
  first: a run loses the edits of the result tabs it replaces.

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
- Every copy of cells follows the column layout: the selection's columns,
  or the page's, are the ones shown, in display order; hidden columns are
  never copied. The inspector's **Copy** copies its one value.
- **Copy this page as CSV** copies the page on screen with headers, `;` as the
  separator and every value quoted, using the original values regardless of
  presentation; NULL becomes an empty field. The copy settings do not apply.
- Every copy of a result runs the same way (`copy_as::CopyRequest`, run by
  `src/app/copying.rs`): grid selections (every menu format), **Copy this
  page as CSV**, the inspector's **Copy** (the whole value, never the part
  the inspector shows) and a report panel's **Copy as CSV** and **Copy for a
  spreadsheet**. Each estimates its size and generates its text on the
  runtime's blocking pool, sharing the immutable page and display order
  rather than cloning the cells. Copies estimated above 50 MiB ask for
  confirmation before generating the text. The confirmation keeps the
  selection, page or value that was requested.
- Past the 100 ms copy budget, the grid shows progress and **Cancel copy**
  (a report panel, in its notice line); `Esc` cancels too. Encoding reports
  rows at most every 100 ms. Estimation and encoding each have a 60 s
  deadline, checked between rows/cells. Cancellation, closing the grid or
  panel, changing its page or result, or requesting a newer copy anywhere in
  the window prevents the old text from reaching the clipboard. Partial text
  is never published; large discarded text and snapshots are freed off the UI
  thread. Only handing the completed text to the OS clipboard stays on the UI
  thread.

## Export

**Export the result to a file** (the toolbar's download button, or
`Ctrl Shift E`) opens a bar under the result toolbar for the result tab on
screen. It writes the whole result, not the page, with the columns the
grid shows, in display order (hidden columns are not written; with the
result's own layout every column is, in its order), to a file chosen in the
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
  ([Security](../standards/security.md#exported-files)).
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
    holds, and the bar says so. An export that fails or is cancelled before
    it asks the session for its first part leaves the rest open for the tab;
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
  temporary file and leaves the chosen file as it was; the bar says so.
  Writing and syncing that outlive the time limit go on in the background
  and then remove the file, never renaming it; only the rename replaces the
  chosen file. A rename that itself outlives the limit may still happen, and
  the bar then says the chosen file may be replaced instead of saying it was
  left as it was. A
  finished export says how many rows, the size and the time, with **Show in
  folder**.
- Each part is converted and written on the runtime's blocking pool, never
  the UI thread; the held pages are shared with the export, not copied
  ([ADR-0022](../adr/0022-result-pages-shared-across-threads.md)).

## Import

**Import from file…** adds the rows of a CSV or an `.xlsx` sheet to a table
([ADR-0040](../adr/0040-importing-files-into-a-table.md), `jykre::import`),
through the driver's write path ([Drivers](drivers.md#importing-rows)).

- **CSV:** the delimiter (`,`, `;`, tab or `|`; the first line suggests
  one), the encoding (UTF-8 by default, any of `encoding_rs`'s; a
  byte-order mark overrides it), the header row (records above it are
  skipped; empty for none) and what an empty field means: NULL (the
  default, as an export writes NULL) or empty text. `""` reads as an empty
  field too. Every field is text exactly as decoded, the text `NULL`
  included. Bytes not valid in the encoding stop the read at their offset
  and nothing is replaced; a row with another number of fields stops it at
  its line; a record over 64 MiB (an unclosed quote) stops it rather than
  grow. Blank lines are skipped.
- **XLSX** (`.xlsx` only; `.xls`, `.xlsb`, `.xlsm` and `.ods` are refused
  with a reason): the sheet and the header row. A string is text, a number
  the shortest decimal that reads back as the stored double, a boolean a
  boolean, a date `YYYY-MM-DD HH:MM:SS[.mmm]` and a time or duration
  `HH:MM:SS[.mmm]` without a time zone (1900 and 1904 epochs), an ISO cell
  its own text, an empty cell NULL and a formula its cached value. An error
  cell, a date outside 1900–9999, a value right of the header's columns and
  cells out of order stop the read at their address. A row without values
  is skipped. Where the file has numbers, the dialog says that Excel stores
  them as floating point. A workbook whose shared strings table is over
  256 MiB uncompressed is refused before it is opened, suggesting CSV, as is
  one whose table declares more strings (`uniqueCount`) than its size can
  hold, or whose count cannot be read in its first 64 KiB the way an XML
  reader reads it (the `sst` element first, after the declaration and
  comments; the exact attribute; a plain number), since `calamine` reserves
  room for the count it reads.
- **The mapping** fills each table column from one file column, or leaves it
  out so it takes its default: by header name ignoring case, or by position
  without a header; the user changes either. A required column (not null,
  no default, not a key the cache cannot tell) left out, a generated
  column given a value or no column at all is said before anything runs.
- **The preview** shows the first 50 rows as they would be bound, for the
  mapped columns, NULL in its own style. **New table from this file…** opens
  a `CREATE TABLE` drafted from the header (every column the dialect's text
  type, names quoted by the driver) in a new query tab, not run.
- **Writing.** The import opens a session of its own in the table's
  context, never a tab's, reads the table from the driver (which decides
  what is required and generated) and writes every row in **one
  transaction**: batches of 1,000 rows or 8 MiB, each value bound by the
  driver and converted by the database (PostgreSQL's casts, MySQL's family
  casts with `@@warning_count`, SQLite's affinity); an XLSX boolean is the
  database's boolean. A value the database rejects stops the import, naming
  the file's line and the server's message; MySQL's adjusted values are
  refused the same way. The whole import has a time limit (15 min, 1 h by
  default, 4 h or none); each batch has the default statement timeout.
- **Outcome.** "Imported N rows into `schema.table`.", "Nothing was
  imported: `schema.table` is as it was." (a failure, **Cancel** or the
  time limit, which roll back; the reason under it), or "The connection was
  lost while committing: the rows may or may not have been imported. Check
  the table before importing again." Nothing is ever retried. SQLite holds
  the file's write lock from the first row to the commit.
- File values are never logged or stored; the log has the counts, the
  table's path, the time and the error's code.

## Reports

A report ([ADR-0024](../adr/0024-reports.md),
[ADR-0025](../adr/0025-report-panels.md)) shows, in each panel, the last result
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
  whole result with the column names, as **Copy this page as CSV** and as
  tab-separated text, whatever the copy menu says, with the columns the
  panel's grid shows, in display order. They run off the UI thread like
  every copy ([Copying](#copying)), asking first above 50 MiB.
- **Export for printing or PDF…** (`jykre::reports::export`) writes one HTML
  file where the user chooses: the title, description, the connections, the
  values the panels ran with, and each panel with its connection and
  database, its chart (as SVG) or number and, when it shows one, its table, in two columns like the report, with every text
  escaped, nothing loaded from elsewhere and print styles (A4, the header
  repeated on each page). Its tables keep every column in the result's
  order: the grid's column layout is not part of a report. Like an export,
  it reaches the disk only at the user's request, at the path they chose
  ([Persistence](persistence.md)).

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
- [ ] Edits written to PostgreSQL, MySQL and SQLite read back exactly
      (exact numbers, NULL apart from the text `NULL`, temporal values),
      a write whose change matches other than one row leaves the table as
      it was, and inside an open transaction they stay uncommitted.

## Known deviations

None at the moment. Copies off the UI thread when large, the last one
listed here, were completed in 0.2.0 M2.
