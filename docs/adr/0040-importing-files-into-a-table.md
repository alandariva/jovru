# ADR-0040: Importing CSV and XLSX files into a table, streamed through the driver's write path (amends the `Session` contract and the stack)

- **Status:** Accepted on 2026-11-08 by the maintainer, with every
  recommendation of the decisions below (I-0 to I-5)
- **Date:** 2026-11-08
- **Authors:** Claude (draft), for the maintainer's review (milestone A11 of
  [Version 0.3.0](../plans/releases/0.3.0.md))

## Context

Milestone A11 asks that the user chooses:

- a CSV or Excel file (and its sheet);
- the header row;
- the CSV's delimiter and encoding;
- the table;
- how the file's columns map to the table's, with a preview of the first
  rows.

The user then runs the import. The milestone also requires that:

- values are bound by the driver, never written into the SQL, and converted
  by the database;
- text from a CSV reaches the table as written;
- the dialog says where an XLSX stores numbers as floating point;
- the import streams off the UI thread with bounded memory;
- it shows progress after 300 ms, and its elapsed time and a way to cancel
  past 2 s;
- it has a timeout;
- a failure or a cancellation says in words what it left;
- **Production** asks first.

Reading the files needs dependencies: the
[proposal](../plans/csv-and-xlsx-reading-dependencies.md) asks for `csv`,
`calamine` and `encoding_rs`. That is a stack change, so it needs an ADR
([Workflow](../standards/workflow.md#architecture-decisions)), and so does
writing rows through the driver. [ADR-0039](0039-editing-result-data-through-the-driver.md)
proposes `Session::write` for A10. This ADR reuses it and adds what an
import needs on top.

## Decision

### 1. Where it lives

- `jykre::import`, in the root crate beside `jykre::export`, holds:
  - the readers (`csv` with the `encoding_rs` decoder; `calamine`'s
    `XlsxCellReader`);
  - the mapping;
  - batching;
  - the pipeline.

  It knows no GPUI and is tested without a window (golden rule 1). The
  dialog lives in `src/app/import/`. The dependencies are declared only for
  the root crate.
- Reading and decoding run on the runtime's blocking pool. Writing runs on
  the Tokio runtime through `backend`. A bounded channel of **2 batches**
  joins them, so a slow database slows reading down and memory stays at
  about two batches plus the reader's buffers. The XLSX shared strings
  table is the exception, checked and bounded before opening (proposal,
  [memory](../plans/csv-and-xlsx-reading-dependencies.md#memory-and-streaming)).
- A batch is at most **1,000 rows or 8 MiB** of values, whichever comes
  first. The driver splits it into statements within its parameter limit
  (ADR-0039, section 4).

### 2. The table and its columns

The user picks the target in the dialog from the connection's tree; the
metadata cache only fills the list. When the import starts, the driver
describes the table in the import's session. The contract gains:

```rust
#[async_trait]
pub trait Session {
    // … ADR-0039's edit_target and write …

    /// The table at `path` as a write target: every column, in table order,
    /// with its declared type, nullability, default and whether it can be
    /// written. Read from the catalog in this session. `key` is empty: an
    /// import only inserts. The default answers `ReadOnly(DriverCannotTell)`.
    async fn table_target(
        &mut self,
        path: &ObjectPath,
        cancel: CancellationToken,
    ) -> DriverResult<EditTarget> {
        let _ = (path, cancel);
        Ok(EditTarget::ReadOnly(ReadOnlyReason::DriverCannotTell))
    }

    /// Opens a transaction that the following `write`s with
    /// `WriteScope::Caller` join, and `end_writes` ends. The default
    /// reports `NotImplemented`.
    async fn begin_writes(&mut self, cancel: CancellationToken) -> DriverResult<()>;

    /// Commits (`commit = true`) or rolls back the transaction
    /// `begin_writes` opened.
    async fn end_writes(&mut self, commit: bool, cancel: CancellationToken) -> DriverResult<()>;
}

// ADR-0039's WriteRequest gains:
pub enum WriteScope {
    /// ADR-0039: the request is atomic on its own (its own transaction, or a
    /// savepoint inside an open one).
    Atomic,
    /// Inside the transaction `begin_writes` opened; nothing is committed.
    Caller,
}
```

- **Mapping.** Each table column is filled from one file column, or left
  out so it takes its default. A required column (`NOT NULL` without a
  default) left out is refused before anything runs, and so is a generated
  column given a value. By default columns are matched by header name,
  ignoring case; otherwise by position. The user changes either.
- `RowChange::Insert` carries the values. A driver may group them into
  multi-row statements. The rows affected must equal the rows sent, or the
  request fails as in ADR-0039.
- `WriteValue` gains `Boolean(bool)` for an XLSX boolean, bound as each
  database's boolean: PostgreSQL `boolean`, MySQL and SQLite `1` or `0`
  through the driver's binding.

### 3. Values

- **CSV:** every field is `WriteValue::Text` exactly as decoded. The
  database converts it (ADR-0039, section 4: PostgreSQL's `CAST($n AS
  type)`, MySQL's family casts and `@@warning_count`, SQLite's affinity). A
  value the database rejects stops the import with the file's line and
  column and the server's message. An empty field is NULL or empty text,
  per the import's choice (decision I-4).
- **XLSX:**
  - a string is `Text`;
  - a number is the shortest decimal text that reads back as the stored
    double (`f64`'s `{}`);
  - a boolean is `Boolean`;
  - a date or time is `YYYY-MM-DD HH:MM:SS[.mmm]` or `HH:MM:SS[.mmm]`, from
    `ExcelDateTime::to_ymd_hms_milli`, without a time zone. An ISO cell
    (`t="d"`) is its own text;
  - an empty cell is NULL;
  - an error cell stops the import at its address.

  Where the file has numbers, the dialog says that Excel stores numbers as
  floating point, so a long number was rounded when the file was written.
- No value is reinterpreted on the way (golden rule 9). The preview shows
  the texts that will be bound, with NULL in its own style.

### 4. Session, transaction and outcome

- The import opens **its own session** through `Driver::session_connection`,
  in the chosen table's context, as a SQL Job does. It never uses a tab's
  session, so no tab's transaction is committed, rolled back or changed. The
  session is closed at the end.
- **All or nothing** (decision I-1):
  - `begin_writes`;
  - every batch with `WriteScope::Caller`;
  - `end_writes(true)` once the file is read to its end.

  A failure, a cancellation or the timeout calls `end_writes(false)`.
  Afterwards the dialog says one of:
  - "Imported N rows into `schema.table`."
  - "Nothing was imported: `schema.table` is as it was."
  - "The connection was lost while committing: the rows may or may not have
    been imported. Check the table before importing again."

  Nothing is ever retried.
- SQLite has one writer per file. The import holds the write lock for the
  whole transaction, so a tab that writes the same file waits 4 s and fails,
  and its error names the cause.
- **Timeout:** each batch has the connection's statement timeout. The whole
  import has a limit chosen in the dialog, like the tab's options: 15 min,
  1 h (default), 4 h or none. **Cancel** fires the token. The driver
  interrupts the statement as for a run and the transaction rolls back.
- **Progress:** after 300 ms the dialog shows the rows imported and the
  share of the file read (bytes for CSV, the sheet's declared dimension for
  XLSX when present). Past 2 s it shows the elapsed time and **Cancel**
  ([Performance](../standards/performance.md)).
- **Production:** on a connection marked Production, **Import** asks first.
  The question names the connection, the table, the file and that the rows
  are added to the table.

### 5. What is never stored

The file's values are never written to this computer (files, logs,
telemetry). Only the import's outcome is logged: the counts, the table's
path, the elapsed time and the error code. The dialog may remember the last
delimiter, encoding and header choice in `config.db`'s `settings`, which
holds no value.

### 6. Dependencies

`csv` 1.4, `calamine` 0.38 without default features, and `encoding_rs` 0.8
as a direct dependency, as in the
[proposal](../plans/csv-and-xlsx-reading-dependencies.md). They are used
only by `jykre::import`. Only `.xlsx` is read (decision I-5). Malformed
input in the chosen encoding stops the import; nothing is replaced.

## Decisions for the maintainer

The [proposal](../plans/csv-and-xlsx-reading-dependencies.md#open-decisions-of-a11)
gives each decision's options and trade-offs:

| ID | Question | Recommendation |
| --- | --- | --- |
| I-0 | Approve `csv`, `calamine` and `encoding_rs` as proposed? | Approve |
| I-1 | All or nothing, or commits in batches? | All or nothing |
| I-2 | May the import create the table? | No; "New table from this file…" drafts a `CREATE TABLE` in a new query tab |
| I-3 | Is Databricks included? | No, for 0.3.0 |
| I-4 | An empty CSV field? | A choice per import, NULL by default |
| I-5 | Which workbook formats? | `.xlsx` only |

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| PostgreSQL `COPY FROM STDIN`, MySQL `LOAD DATA LOCAL INFILE` | Much faster for large files | Per-database formats and escaping. `LOCAL INFILE` must be enabled on both client and server and lets a server read client files. Values are not bound by our driver | Bound inserts first. `COPY` may come later as a PostgreSQL driver optimisation behind the same contract |
| Import in the tab's session | Joins the user's open transaction | A long import blocks the tab; a failure mid-way interacts with the user's transaction | A session of its own, as a job's |
| Generate `INSERT` statements into the editor | The user sees the SQL | Values as literals break A11; millions of statements in an editor | Contradicts the milestone |
| Read the whole file first | Simpler; exact row count for progress | Memory grows with the file | A11 asks for bounded memory |
| Commit every batch | Short transactions | Half-imported tables after a failure | Decision I-1 |

## Consequences

- Positive:
  - One write path for edits and imports, with the same binding and
    exactness rules.
  - The table is untouched by a failure.
  - Memory does not grow with the file, except for an XLSX's shared
    strings, which are bounded and checked first.
- Negative or costs:
  - Three dependencies (seven new crates, one duplicated on Linux).
  - Three more `Session` methods.
  - A transaction as long as the file.
  - Only `.xlsx` workbooks.
- What becomes required:
  - The drivers that implement `row_writes` also implement `table_target`,
    `begin_writes` and `end_writes`.
  - Live tests per driver.
  - The stack table row when the dependencies land.
- What becomes forbidden:
  - Replacing undecodable bytes.
  - Writing file values to logs.
  - Importing through a tab's session.
  - Creating tables inside the import.

## How to validate

- Unit tests (`jykre::import`, no window) on dedicated synthetic files:
  - quoted fields with line breaks and doubled quotes, `;` `,` and tab
    delimiters, a BOM, Windows-1252 and UTF-16LE;
  - malformed UTF-8 refused at its offset;
  - a ragged row refused at its line, a record over 64 MiB refused;
  - XLSX with shared and inline strings, the 1900 and 1904 epochs, dates,
    times, booleans, an error cell, a formula's cached value, a backward
    row refused;
  - a shared strings table over the limit refused before `calamine` opens
    it;
  - the mapping's refusals;
  - the pipeline's memory staying within two batches;
  - cancellation and the timeout.
- Live tests under `cargo xtask check --full` (PostgreSQL 14/18, MySQL
  8.4/9.7) and on SQLite files:
  - `numeric(38,10)`, `bigint` above 2^53, dates, text with the text
    `NULL`, empty fields under both choices, a value the database rejects
    (nothing imported);
  - a cancelled import of 100,000 rows leaves the table as it was;
  - a MySQL value that would be truncated is refused.
- In the application: the whole flow with the keyboard, the preview, the
  300 ms and 2 s rules, the Production question and the messages for
  success, rollback and an unknown commit.

## Implementation notes

2026-11-08, implementing A11:

- The contract is as decided: `Session::table_target`, `begin_writes`,
  `end_writes`, `WriteScope` on `WriteRequest` and `WriteValue::Boolean`.
  Two additions carry it: `write::insert_target` builds the key-less target
  from a driver's catalog read, and `DriverError` keeps the index of the
  change a write failed at (`at_change`, `change()`), which is how the
  import names the file's line. `WriteRequest::validate` accepts inserts
  without a key.
- A rejected value names the file's line and the server's message; the
  column is named only when the server's message does, since a statement
  inserts a whole row.
- `calamine` was kept, with debug assertions on for `debug_unsafe` in
  release builds, by the maintainer's decision
  ([proposal](../plans/csv-and-xlsx-reading-dependencies.md#residual-risks)).
