# Results: editing

Part of [Results](README.md).

## Editing

A query tab's result can be written back to its table (D-0039); report
panels, jobs and the assistant's results stay read-only, since their
sessions belong to the report, the engine or the assistant, not to a tab.
The driver contract and each driver's catalog queries are in
[Drivers](../drivers/editing-and-import.md#editing-results).

- **Which results.** The first editing key (or the toolbar's pencil) asks the
  tab's session what the result can be written back to
  (`Session::edit_target`, see [Drivers](../drivers/editing-and-import.md#editing-results)), once
  per result tab and read from the session's own catalog (so it sees a table
  its open transaction created, not the metadata cache); runs cost nothing
  extra. When the result still has rows
  to read, the grid says first that asking closes them, so only the rows
  read so far can be edited. A result is editable when its driver can write
  rows and its stored columns come from the **same ordinary table** (a
  PostgreSQL ordinary or partitioned table, a MySQL base table in a
  transactional engine, a SQLite table) with a **row key** all of whose
  columns are in the result: the primary key, else a unique index on NOT NULL
  columns without an expression or a partial predicate. Computed columns
  (expressions, aggregates, literals) may sit beside them and stay read-only.
  A key column whose text does not identify one value is refused: approximate
  numbers (PostgreSQL `real` and `double precision`, MySQL `FLOAT` and
  `DOUBLE`, SQLite `REAL` affinity) and MySQL `TIMESTAMP`, which the session's
  time zone converts. Every other result shows in a bar under the toolbar why
  it is read-only, in words, with the first rule it failed ("Computed
  columns only", "Columns from two tables", "`orders` has no primary key or
  unique key on NOT NULL columns", "The key column `id` is not in the result",
  "Read by a report panel"). A driver that cannot tell where a column comes
  from keeps its results read-only ("This connection's results cannot be
  edited: its driver does not report which table a column comes from"), offers
  no editing control and never guesses a table from the SQL text; Databricks
  is the one deliberate exception, inferred inside its driver (see
  **Databricks** below).
- **Pending edits.** `jovru::grid_edit::EditSession` holds them, outside the
  views: changed cells, NULLs, rows marked for deletion and inserted rows.
  The grid draws them over the cached view: the cache key gains the edits'
  version and the inserted rows, which follow the result's rows whatever the
  filter or sort, numbered `+`. A pending cell has an amber border, a written
  one a mint border; a row marked for deletion is struck through in red; an
  inserted row's untouched columns read `DEFAULT`. Rows are addressed in the
  whole result, so every page's edits are kept, and the key's values are
  taken when a row is first edited. Edits live in memory only: never in
  `config.db`, logs or telemetry, and they go with the result tab. Nothing is
  written on leaving a cell; pending edits are written together after one
  confirmation (**Write…**), never cell by cell. The editing keys (edit, set to
  NULL, insert a row, delete rows, write, discard) are keymap actions, listed
  in **Preferences › Shortcuts**.
- **Values.** `F2` (or the pencil) opens a field over the focused cell with
  its value; `Enter` or leaving the field keeps what was typed as pending,
  `Esc` drops it. The field keeps the keys it lets through: `Enter` there
  never opens the side panel, and an arrow at the end of the text never
  moves the selection; **Write…** keeps what was typed first. The side
  panel shows a cell's pending (or written) value, as the cell does. **Set to NULL** (`Ctrl Shift N`) sets the selected cells
  to NULL; typing `NULL` gives the four-letter text and clearing a cell the
  empty text. A binary column takes `0x` followed by hexadecimal digits.
  A value equal to the one read is no change. Computed, generated and
  identity-always columns, and a column whose cells arrived both as bytes and
  as text, cannot be edited and say so.
- **Where.** Only a query tab's result can be edited. The grid of a report
  panel or of a job's result shows the menu's editing items disabled, under
  a line that says so; a query tab's result known to be read-only shows its
  reason there the same way.
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
  that a change made meanwhile to the same row is overwritten (the key alone
  identifies the row; no read value is compared, since some types have no
  equality and last write wins). On a
  connection marked Production the same dialog says so and its button reads
  **Write to production**; it is the single question, every write asks, even
  one cell. Changes are written in this order: deletions,
  updates, inserts. At most 10,000 changed rows are written at once, and more
  are refused with the reason. Every value is bound by the driver, never
  written into the SQL and never reinterpreted; the server converts the text
  (the exact text read, or what the user typed), temporal values included.
- **The write** runs in the tab's session as a run of the tab: it takes the
  session, answers Stop (the edit bar's **Stop**, `Ctrl F2`), makes closing
  the tab ask, and is logged in Messages with its counts, never its values.
  Afterwards the toolbar shows the session's transaction. A written cell
  shows what was written, marked, since the database may store it otherwise
  (a rounded `numeric`, a default, a trigger); `↻` reads the rows back and is
  never run by itself. Each change must affect exactly one row: one that
  matched another number rolls everything back and its row is marked red,
  saying how many rows matched. Outside a transaction the driver wraps the
  request in its own and commits only when every change matched one row;
  inside one the toolbar's transaction (a `BEGIN` the user ran, MySQL with
  autocommit off) keeps the changes **uncommitted** until **Commit** or
  **Rollback**, and a mismatch undoes only the write. A PostgreSQL transaction
  that already failed refuses edits until it is rolled back. Before its first
  statement the driver checks the target again in the same transaction; if the
  table, its key or a column changed meanwhile, the write fails with "The table
  changed since this result was read; run it again" and writes nothing.
  Cancellation, the timeout (the tab's statement timeout, for the whole write)
  and an error roll the write back; a connection lost during `COMMIT` is
  reported as "may or may not have been written" and nothing is sent again.
  The triggers and cascading foreign keys named in the confirmation are the
  only way a write may touch other rows.
- **Databricks** has no write: **Generate SQL…** opens a new query tab on the
  same connection and context with one statement per change and their
  values in its parameter fields, after a header comment saying that each
  statement commits on its own and nothing checks it changes one row. The
  table is the one the result's single-table `SELECT` names; a table that
  declares no key takes the columns the user selects (**Use as key**) before
  the first edit. Nothing is run by generating, so it asks nothing; running
  the generated tab goes through `jovru::guard` like any run, and the
  10,000-change limit applies. Values are `:name` parameters, never literals;
  a column whose type has no parameter kind (`BINARY`, `INTERVAL`, `ARRAY`,
  `MAP`, `STRUCT`, `VARIANT`), approximate numbers and `TIMESTAMP` as a key
  are refused as [Drivers](../drivers/editing-and-import.md#editing-results) says. Databricks
  changes are never written without the user running them.
- **Discard** (`Ctrl Alt Z`) drops the pending edits, asking first. Running
  a statement in the tab while edits are pending, or closing the tab, asks
  first: a run loses the edits of the result tabs it replaces.
