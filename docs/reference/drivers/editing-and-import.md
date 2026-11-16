# Drivers: editing and import

Part of [Database drivers](README.md).

## Editing results

D-0039 lets a
query tab's result be written back to its table. The contract
(`jovru_core::write`):

- `ColumnInfo::origin` says where each column comes from, from what the
  protocol said when the statement was prepared, with no extra I/O:
  `Stored(SourceColumn)` (the driver's own identifiers of the relation and
  the column, compared for equality only), `Expression`, or `Unknown`, the
  default.
- `DriverCapabilities::row_writes` (serialized with a default of `false`):
  the driver implements `Session::edit_target`, and `Session::write` or
  `Session::write_script` as `EditableTable::writes` says. PostgreSQL, MySQL
  and SQLite write directly (`WriteMode::Direct`); Databricks generates
  statements (`WriteMode::Script`).
- `Session::edit_target(statement, columns, cancel)` reads, from the
  catalog in the session, what the result can be written back to:
  `EditTarget::Editable(EditableTable)` with the table's path, one entry per
  result column (`None` for a computed one; `writable` false for a generated
  column or an identity the table always generates), the key's columns and
  kind, and what a write can change beyond the rows written (triggers, and
  foreign keys of other tables that cascade, set NULL or set the default),
  or `EditTarget::ReadOnly(ReadOnlyReason)`. Like `context`, asking
  discards a result still open for paging. The default answers
  `ReadOnly(DriverCannotTell)`.
- `Session::write(request, cancel)` writes a `WriteRequest` atomically and
  answers `WriteOutcome::Written { rows, committed }`, or
  `RolledBack { index, matched }` when change `index` affected other than
  one row. The default reports `NotImplemented`.
- `Session::write_script(request)` returns the statements with `:name`
  markers and their values apart (`WriteScript`). The default reports
  `NotImplemented`.
- `write::statement` builds every statement's text from the driver's
  quoting and a placeholder the driver chooses; `write::resolve` picks the
  key (the primary key, else the first unique key on NOT NULL columns
  without expressions or a predicate whose columns are all in the result)
  and refuses a key column whose text does not identify one value;
  `write::still_matches` compares a target with the catalog read again
  before a write. At most `write::MAX_CHANGES` (10,000) changes per request.

What every driver with `Direct` writes does:

- **Atomic.** Outside a transaction it opens one (`BEGIN`, `START
  TRANSACTION`, SQLite's `BEGIN IMMEDIATE`) and commits only when every
  change affected exactly one row; inside an open transaction (MySQL's
  autocommit off included) it uses `SAVEPOINT jovru_edit`, released on
  success (`committed: false`) and rolled back to on a mismatch, so the
  user's transaction keeps what it had. A failure, a cancellation or the
  timeout rolls back what the write did. A `COMMIT` that gets no answer is
  reported as a `Connection` error that says the changes may or may not
  have been written; nothing is sent again.
- **Checked again.** Before its first statement, inside the same
  transaction, the driver reads the catalog again and fails with
  `Validation` ("The table changed since this result was read") when the
  table, a column's type or its key changed.
- **Bound.** Every value is a bound parameter; NULL is bound as a typed
  NULL. Each statement runs through SQLx's prepared path, unsaved
  (`persistent(false)`).

| | PostgreSQL | MySQL | SQLite |
| --- | --- | --- | --- |
| Origin | `PgColumn::relation_id()` and `relation_attribute_no()` as decimal text; none (or a system column) is `Expression` | `Column::origin()`: `schema.table` and the physical column name | `Column::origin()`: `db.table` (`table` for `main`) and the column's own name |
| Relation | `pg_class` joined to `pg_namespace` by the OID: path and `relkind` (`r` or `p`; a view, a materialized view or a foreign table is `NotATable`) | Split at its only dot; with several, each split is checked against `information_schema.TABLES` and used only when exactly one exists (E-6). `TABLES` joined to `ENGINES`: `BASE TABLE` and `TRANSACTIONS = 'YES'`, else `NotATable` or `NotTransactional` | Read as `main.<name>` or split at a dot; with dots, each reading is checked against `pragma_database_list` and the database's `sqlite_schema`, and used only when exactly one exists. A view or a virtual table is `NotATable`. A view in the query reports its base table, which is written |
| Columns | `pg_attribute` joined to `pg_type`: `format_type(atttypid, NULL)` (the type without its modifier, so an explicit cast never cuts a `varchar(n)` text that the column's own assignment refuses), `attnotnull`, `atthasdef`, `attgenerated`, `attidentity` (`a` is not writable); `float4` and `float8` cannot be a key | `COLUMNS`: `COLUMN_TYPE`, `DATA_TYPE`, `IS_NULLABLE`, `COLUMN_DEFAULT`, `EXTRA` (generated columns are not writable); binary types and `BIT` are bound as bytes; spatial types are left out; `FLOAT`, `DOUBLE` and `TIMESTAMP` cannot be a key | `pragma_table_xinfo(table, schema)`: `type`, `notnull`, `dflt_value`, `pk`, `hidden` (2 and 3 are generated); a declared `BLOB` is bound as bytes; `REAL` affinity cannot be a key |
| Key | `pg_index` (`indisunique`, `indisvalid`, `indimmediate`): `indisprimary`, else a unique index without `indexprs` or `indpred` whose key columns (`indkey`, first `indnkeyatts`) are all `attnotnull` | `STATISTICS` with `NON_UNIQUE = 0`: `PRIMARY`, else a unique index without a functional part whose columns are all NOT NULL | `pk` of `pragma_table_xinfo`, else `pragma_index_list(table, schema)` (`unique`, not `partial`) and `pragma_index_info(index, schema)` without an expression (`cid = -2`), every column NOT NULL. The `rowid` is not used |
| Side effects | `pg_trigger` without `tgisinternal`; `pg_constraint` with `contype = 'f'` and `confrelid` the table, whose `confdeltype` or `confupdtype` is `c`, `n` or `d` | `TRIGGERS` of the table; `REFERENTIAL_CONSTRAINTS` that reference it with `CASCADE`, `SET NULL` or `SET DEFAULT` | Triggers in `sqlite_schema`; `pragma_foreign_key_list` of every table of the schema that references it, with its `on_delete` and `on_update` |
| Values | `CAST($n AS <type>)` with the text bound as `text`: the server's input function reads it; `bytea` travels as its `\x` text | By the type's family: `CAST(? AS SIGNED)` or `UNSIGNED` for integers (an `UNSIGNED` column's text that is a whole number up to 2^64 - 1 is bound as an unsigned integer, since a prepared `CAST(? AS UNSIGNED)` reads a text above 2^63 - 1 as 2^63 - 1), `CAST(? AS DECIMAL(65,30))` for every decimal (a cast to the column's `DECIMAL(p,s)` rounds a longer fraction silently, while storing the wider value warns), `CAST(? AS DATETIME(6))` for `DATE` (`CAST(? AS DATE)` drops a time of day silently, while storing a time other than midnight in the column warns), `CAST(? AS DATETIME(fsp))` (also for `TIMESTAMP`), `CAST(? AS TIME(fsp))`; text without a cast, so the column's collation applies; bytes for binary columns. After each statement `SELECT @@warning_count` over the text protocol: a warning (a value MySQL adjusted) rolls the request back and names `SHOW WARNINGS`' message | `?N`: text bound as text (the column's affinity converts it), bytes as bytes |
| Rows matched | `rows_affected` | `rows_affected`, which counts the rows an `UPDATE` matched (`CLIENT_FOUND_ROWS`) | `changes()` of the statement |
| Inside a transaction | The catalog is read in a savepoint, rolled back afterwards; a failed transaction refuses edits until it is rolled back | The catalog reads cannot fail the transaction | An interrupted write inside an open transaction makes SQLite roll that whole transaction back; the toolbar shows it |

Databricks reports no column's origin, so its driver infers the table from
the result's SQL text, by the maintainer's decision (D-0039):
only a `SELECT` from one table, with no join, subquery, table-valued
function, `LATERAL VIEW`, `PIVOT`, time travel or CTE, no `UNION`,
`INTERSECT`, `EXCEPT` or `MINUS`, no `GROUP BY`, `HAVING`, `QUALIFY`,
`WINDOW` or `DISTINCT`, and result columns that are plain column references
(optionally renamed) or `*`; `WHERE`, `ORDER BY`, `LIMIT` and `OFFSET` may
follow (`runtime/edit.rs`, unit-tested on fixtures). A one- or two-part name
takes the session's catalog and schema; the table is read from Unity
Catalog's tables endpoint (`/api/2.1/unity-catalog/tables/{full name}`, the
catalog `information_schema` reflects), without compute: `MANAGED` or
`EXTERNAL` tables only, its columns' `type_text`, `nullable` and
`position`, and its declared primary key (`table_constraints`). The key is
that primary key when all its columns are in the result
(`KeyKind::Informational`); otherwise the user names it
(`KeyKind::Chosen`). Each column gets a parameter kind from its type
(numbers, text, booleans, dates, timestamps); `BINARY`, `INTERVAL`, complex
types and `VARIANT` have none and are not editable; `FLOAT`, `DOUBLE` and
`TIMESTAMP` cannot be a key. `write_script` writes one statement per change
with `:column_n` markers and checks each value against its kind, refusing
one that does not fit rather than rewriting it.

### Importing rows

D-0040 reuses the same
write path for an import into a table, in a session of its own:

- `Session::table_target(path, cancel)` reads the table at `path` from the
  catalog, as `edit_target` does, and answers `write::insert_target`: every
  column in table order and no key, since an import only inserts. A table
  that is not there is `ReadOnly(TableNotFound)`; a view, a foreign table or
  (MySQL) an engine without transactions is read-only as for an edit. The
  default answers `ReadOnly(DriverCannotTell)`.
- `Session::begin_writes(cancel)` opens a transaction (`BEGIN`,
  `START TRANSACTION`, SQLite's `BEGIN IMMEDIATE`, which holds the file's
  write lock to the end), refused while one is open.
  `Session::end_writes(commit, cancel)` commits or rolls it back; it is
  never cancelled. A `COMMIT` the server answers with an error rolled it
  back; one without an answer is a `Connection` error, and the session
  closes. Both default to `NotImplemented`.
- `WriteRequest::scope` is `WriteScope::Atomic` for an edit and
  `WriteScope::Caller` for an import's batch: `write` then runs
  `edit::apply` inside that transaction, with nothing committed or rolled
  back. A change that fails, affects other than one row or (MySQL) has a
  value the server would adjust fails the write with
  `DriverError::change()` naming its index, and the transaction is left
  for `end_writes`. `validate` accepts a request of inserts without a key.
- `WriteValue::Boolean` is an XLSX boolean: bound as `boolean` in
  PostgreSQL (`CAST($n AS <type>)` converts it), `1` or `0` in MySQL and
  SQLite. Databricks writes no import.
- The import sends batches of at most 1,000 rows or 8 MiB of values;
  `RowChange::Insert` carries the values and a driver may group them into
  multi-row statements within its parameter limit, with rows affected equal to
  rows sent. Every driver that implements `row_writes` with `Direct` also
  implements `table_target`, `begin_writes` and `end_writes`. SQLite has one
  writer per file: the import holds the write lock for its whole
  transaction, so a tab that writes the same file waits 4 seconds and fails,
  and its error names the cause. The pipeline, mapping and dialog are in
  [results](../results/transfer.md#import).

#### Import encodings

How a CSV's bytes become text (`jovru::import`, `delimited.rs`):

- The user chooses the encoding from `encoding_rs`'s list, without its
  `replacement` encoding; UTF-8 is first and the default. A byte-order mark
  (UTF-8, UTF-16LE or UTF-16BE) overrides the choice.
- Decoding uses `Decoder::decode_to_utf8_without_replacement` over fixed
  buffers. Bytes not valid in the encoding **stop the import** at their byte
  offset, naming the encoding so the user can pick another. Nothing is
  replaced with `U+FFFD`: text from a CSV reaches the table as written.
- `encoding_rs_io` is not used, because it always replaces malformed bytes;
  the `Read` adapter around the decoder is ours.
- The encoding is never guessed (no `chardetng`): a wrong guess would change
  the text silently, and the preview of the first rows shows the effect of
  the chosen one. Only the delimiter is suggested, from the first 64 KiB.

#### Import memory and streaming

Reading runs on the blocking pool and writing on the runtime, joined by a
bounded channel of 2 batches, so memory does not grow with the file.

- **CSV:** one record at a time, in a reused buffer, behind an 8 KiB
  decoder buffer. Memory is bounded by those buffers plus the longest record.
  A record over 64 MiB stops the import instead of growing: `csv` reads a
  quoted field to its closing quote, so a missing quote would swallow the
  file.
- **XLSX:** `calamine`'s cells reader streams the sheet one cell at a time.
  `worksheet_range`, which collects every cell, is never used. Opening the
  workbook reads three things whole: the shared strings table, the styles and
  the list of sheets. So the import checks the table's uncompressed size in
  the zip, before `calamine` opens the file, and refuses a workbook over
  256 MiB (the per-page memory ceiling of [Results](../results/model.md#limits)),
  suggesting CSV; the further checks on its declared count are in
  [Results](../results/transfer.md#import).
- Cells arrive in file order, which is row-major in files written by Excel
  and the usual libraries. A cell whose row or column goes backwards stops the
  import at its address; it is never reordered. A position past 1,048,576
  rows or 16,384 columns is refused.
- Only `.xlsx` is read. `calamine` loads `.xls` and `.ods` whole when it
  opens them, so they are refused with a reason.
- A shared string index that `calamine` cannot parse reads as index 0
  (`calamine` 0.38), so a damaged cell can take the table's first shared
  string. Only a damaged or hand-made file holds one, and the import cannot
  tell.
