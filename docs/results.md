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

- `ResultData` holds the columns (name and database type), the rows as display
  text and the positions of NULL cells. It is built from the driver's cells on
  the Tokio runtime, where the driver cells are also dropped.
- `GridView` holds what the grid derives from the result: the rows in display
  order (after the open filter and the sort), the column widths (sampled from
  the first 200 rows) and the value families. `GridCache` rebuilds it only
  when its key changes: the result version, the filter text, the sort or the
  number format. Hover, scroll and selection never rebuild it.
- Selections are stored as data-row indices, so filtering or sorting the view
  does not change what is selected; a selection whose ends are hidden is not
  shown.

## Limits

| Limit | Value |
| --- | --- |
| Rows per statement | 1,000 (`ExecutionOptions.max_rows`); the driver stops reading at the limit |
| Statement timeout | 300 s (`ExecutionOptions.timeout`) |
| Column width | 110 to 360 px, from the header and the first 200 rows |

The standard ([ADR-0008](adr/0008-compact-result-storage.md), proposed):

- The row limit is configurable per connection, up to 100,000 rows.
- A **memory ceiling** per result (default 256 MB) stops reading even below
  the row limit, and the footer says which limit applied.
- Results use a compact representation: one UTF-8 buffer, cell offsets and a
  NULL bitmap, instead of one `String` per cell and a hash set of NULLs.
- Starting a new run in a tab releases that tab's previous result; closing the
  tab releases everything.

## Statements and results

The standard:

- Each statement that returns rows produces its own result, shown as
  `Result 1`, `Result 2`, ...; **Messages** lists every statement with its
  status, time, rows affected and error.
- Statements run in order and stop at the first failure. The message says how
  many completed and that they were not rolled back automatically.
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
  rows when it is closed.

## Copying

- `Ctrl C` copies the selection as tab-separated text that pastes into
  spreadsheets. `Ctrl Shift C`, or **Copy with headers** in the context menu,
  adds the column names first.
- A single cell copies its exact value. Fields with tabs, line breaks or
  quotes are quoted with doubled quotes; NULL becomes an empty field.
- **Copy results as CSV** copies the whole result with headers, `;` as the
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
| Configurable row limit, memory ceiling, compact storage | Fixed 1,000-row limit; `Vec<Vec<String>>` plus a `HashSet` of NULL positions ([ADR-0008](adr/0008-compact-result-storage.md), proposed) |
| One result per statement | A run with several statements shows only the last statement's result; **Messages** summarises the run |
| Copy confirmation above 50 MiB, off the UI thread when large | Copies always run on the UI thread, without confirmation (about 85 ms for 500,000 cells) |
