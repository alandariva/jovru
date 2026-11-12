# Results: copying, export and import

Part of [Results](README.md).

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
  and are remembered ([Persistence](../persistence/README.md)).
- **Copy as**, in the context menu, copies the selection once in any
  format, with the same settings, without changing the format of `Ctrl C`.
- **Formats** (`jovru::copy_as`):
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
  strings (`jovru::sql::string_literal`: PostgreSQL doubles the quote, MySQL
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
system's save dialog (`jovru::export`, `backend::export`).

- **Formats:** CSV, Excel (XLSX), JSON (an array laid out as **Copy as
  JSON**), JSON Lines (one object per line), SQL `INSERT` (one per row, as
  **Copy as INSERT**) and Markdown. CSV starts as the CSV copy writes:
  `;` between fields, every value quoted, the column names first, no BOM,
  one line per record ending in `\n`. Its options are the separator (`;`,
  `,` or tab), **Quote every value** (off: only values holding the
  separator, a quote or a line break, and empty text), **Column names
  first** and a **Byte order mark (BOM)**. XLSX also takes **Column names
  first**. The bar remembers the last choices and folder
  ([Persistence](../persistence/README.md)).
- **Values** follow the copies' rules, so a copy and an export of the same
  cells write the same text: NULL is an empty CSV field and empty text is
  `""`, numbers are written as the database wrote them (a JSON number only
  when valid), strings are escaped by the dialect, nothing is converted
  between time zones or changed to defend spreadsheets from formulas
  ([Exported files](#exported-files)).
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
    `jovru::sql::reads_only`) runs again; any other exports the rows held.
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
  (D-0022).

### Exported files

- An export writes only to the file the user chose in the system's save
  dialog. A table or tab name only suggests the file's name: characters a
  file name cannot hold become `_`, names Windows reserves (`CON`, `NUL`,
  `COM1`) get a `_` in front, and no folder comes from it
  (`jovru::export::suggested_file_name`).
- Values are written as the database returned them. Nothing is changed to
  stop a spreadsheet from reading a value that starts with `=`, `+`, `-` or
  `@` as a formula, since changing values would break
  [golden rule 9](../../README.md#golden-rules) (approved 2026-11-04). Quotes in a
  CSV do not stop a spreadsheet from evaluating such a value: a file exported
  from data others wrote is as untrusted as that data when opened in one. In
  XLSX every value is a string, a number or a boolean cell, never a formula.
- Messages about the file never name its folder, which may hold the user's
  name; logs record the format, rows, bytes and duration, never the path.
- Temporary files (the export being written and Excel's constant-memory
  rows) live next to the chosen file and are removed when the export ends,
  fails or is cancelled.

## Import

**Import from file…** adds the rows of a CSV or an `.xlsx` sheet to a table
(D-0040, `jovru::import`),
through the driver's write path ([Drivers](../drivers/editing-and-import.md#importing-rows)).

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
  type, names quoted by the driver) in a new query tab, not run. The table
  is named after the file, followed by `_2`, `_3`… when a relation the
  metadata cache knows in that namespace has the name (ignoring case); the
  draft's comment then says which name was taken. The cache may be behind
  the database, so the draft stays the user's to check and edit.
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
