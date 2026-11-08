# ADR-0039: Editing result data through the driver, which reports each column's source and the row key (amends the `Session` contract)

- **Status:** Proposed
- **Date:** 2026-11-08
- **Authors:** Claude (draft), for the maintainer's review (milestone A10 of
  [Version 0.3.0](../plans/releases/0.3.0.md))

## Context

Milestone A10 asks that a cell of a result can be changed or set to NULL,
and rows inserted and deleted, in the results this ADR deems editable. Every
other result must say why it is read-only. The grid is read-only today. The
only SQL it generates is **Copy as `INSERT`** ([Results](../reference/results.md)).

The milestone fixes these requirements:

- The ADR says which results can be edited. It says how the driver tells:
  the table each column comes from, and a key that identifies each row.
- It says how changes are written, in which session and in which
  transaction.
- It says what a driver that cannot tell does.
- The driver-contract change is part of it.
- Nothing is written before the user has seen what will be written and
  confirmed it.
- Values are bound by the driver and never written into the SQL. They are
  not reinterpreted (golden rule 9).
- An open transaction in the tab is honoured. Outside one, a write that
  affects other rows than the ones edited is not committed silently.
- **Production** asks first, as `jykre::guard` does.

The milestone leaves three questions open: whether an edit writes directly
or generates statements into the editor, whether pending edits are written
together or one by one, and whether Databricks is included. They are
[decisions for the maintainer](#decisions-for-the-maintainer) below.

What the code offers today (read on 2026-11-08 at `release/0.3.0`, `f85ac9b`):

- `ColumnInfo` has `name`, `data_type` and `nullable` only
  (`crates/core/src/result.rs`). No driver says where a column comes from.
- `ResultData` keeps, for each column, the name and the database type. It
  keeps each cell's text and a NULL bitmap
  ([ADR-0008](0008-compact-result-storage.md)). A `Cell::Binary` becomes
  `0x…` text (`src/result.rs`, `cell_text`), and `Cell::Float` becomes Rust's
  shortest round-trip text.
- The relational drivers take a statement's columns from preparing it with
  SQLx (PostgreSQL and MySQL `connection.prepare`, SQLite the same), then run
  it as text (`RawSql`) ([ADR-0002](0002-sqlx-for-postgres-and-mysql.md)).
- The pinned SQLx fork ([ADR-0030](0030-transaction-state-through-a-pinned-sqlx-fork.md),
  revision `e538af0`) already exposes what the protocols say about each
  column's origin:
  - **PostgreSQL:** `PgColumn::relation_id()` and
    `PgColumn::relation_attribute_no()`. These are public and come from
    `RowDescription` with no extra I/O. They are `None` for an expression.
  - **MySQL:** `Column::origin()` returns `ColumnOrigin::Table` built from
    the column definition packet's schema, physical table and physical
    column name. The schema and the table are **joined by a dot** into one
    string, `schema.table`. A column without a table is `Expression`. The
    column flags (`PRIMARY_KEY`, `UNIQUE_KEY`, `NOT_NULL`) are `pub(crate)`
    (`sqlx-mysql/src/column.rs`, `connection/executor.rs`).
  - **SQLite:** `Column::origin()` reads `sqlite3_column_database_name`,
    `sqlite3_column_table_name` and `sqlite3_column_origin_name`. It returns
    `db.table`, or `table` for `main`, or `Expression`. These functions need
    `SQLITE_ENABLE_COLUMN_METADATA`. The bundled build of `libsqlite3-sys`
    0.37.0 (the version in `Cargo.lock`) always defines it (`build.rs`,
    line 128). SQLx notes that these functions give the base table's column
    even through a view.
  - **Databricks:** the Statement Execution manifest describes a column by
    `name`, `type_text`, `type_name` and position. The Command Execution
    schema gives `name` and `type` (`crates/driver-databricks`). Neither says
    where a column comes from.
- Each query tab keeps one session ([ADR-0014](0014-a-session-per-tab.md)).
  The PostgreSQL, MySQL and SQLite drivers read the transaction state from
  the protocol (ADR-0030), so a driver knows whether its session is inside a
  transaction.

Changing a core contract needs an ADR and the maintainer's approval
([Workflow](../standards/workflow.md#architecture-decisions)).

## Decision

### 1. Which results can be edited

A result is **editable** when all of the following hold:

1. It is a query tab's result (the **Data** view included), read by the
   tab's current session. Report panels, a job's result and the assistant's
   results stay read-only. Their sessions belong to the report, the engine
   or the assistant, not to a tab.
2. Its driver has `DriverCapabilities::row_writes` (section 2).
3. Each column whose origin is a stored relation comes from the **same
   table**. That table is an ordinary table: a PostgreSQL ordinary or
   partitioned table, a MySQL base table in a transactional engine, or a
   SQLite table. Computed columns (expressions, aggregates, literals) may sit
   beside it and stay read-only.
4. The table has a **row key**, and every column of that key is in the
   result with that table as its origin. The row key is the primary key, or
   failing that a unique index whose columns are all `NOT NULL`, with no
   expression and no partial predicate (decision E-4).
5. No key column has a type whose text does not identify one value:
   - approximate numbers: PostgreSQL `real` and `double precision`, MySQL
     `FLOAT` and `DOUBLE`, SQLite `REAL` affinity;
   - MySQL `TIMESTAMP`, which the session's time zone converts, so a value
     in a repeated hour reads the same for two instants.

Within an editable result:

- A row whose key holds a NULL (an outer join's empty side) cannot be
  changed or deleted.
- A column that cannot be written stays read-only: generated columns,
  PostgreSQL `GENERATED ALWAYS AS IDENTITY`, and computed columns.
- A column whose cells arrived both as binary and as text (possible in
  SQLite) is read-only and cannot be part of the key.

Every result that is not editable states **why**, with the first rule it
failed, in words. Examples: "Computed columns only", "Columns from two
tables", "`orders` has no primary key or unique key on NOT NULL columns",
"The key column `id` is not in the result", "Databricks results are
read-only", "Read by a report panel".

### 2. The contract change

In `jykre-core`. Every new method has a default that keeps today's
behaviour, so a driver that cannot tell needs no change.

```rust
// result.rs
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: Option<bool>,
    /// Where the column's values come from, as the protocol described it
    /// when the statement was prepared. No extra I/O.
    pub origin: ColumnOrigin,
}

#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ColumnOrigin {
    /// The driver cannot tell (the default; Databricks).
    #[default]
    Unknown,
    /// Computed: an expression, a function, a literal, an aggregate, or a
    /// column the protocol does not tie to a relation.
    Expression,
    /// A column of a stored relation, named the way the driver identifies it.
    Stored(SourceColumn),
}

/// The driver's own identifiers for a column's relation and the column in
/// it. Elsewhere they are compared for equality only: never parsed, shown or
/// written into SQL. PostgreSQL: the relation's OID and the attribute
/// number. MySQL: schema, table and column. SQLite: database, table and
/// column.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceColumn {
    pub relation: String,
    pub column: String,
}

// driver.rs
pub struct DriverCapabilities {
    // … existing fields …
    /// Implements `Session::edit_target` and `Session::write` (ADR-0039).
    #[serde(default)]
    pub row_writes: bool,
}

#[async_trait]
pub trait Session {
    // … existing methods …

    /// What the rows of a result with these columns can be written back to,
    /// read now from the server's catalog in this session. Like `context`,
    /// asking discards a result still open for paging. The default answers
    /// `ReadOnly(DriverCannotTell)`.
    async fn edit_target(
        &mut self,
        columns: &[ColumnInfo],
        cancel: CancellationToken,
    ) -> DriverResult<EditTarget> {
        let _ = (columns, cancel);
        Ok(EditTarget::ReadOnly(ReadOnlyReason::DriverCannotTell))
    }

    /// Writes `request` atomically (section 4) and reports what happened.
    /// The default reports `NotImplemented`.
    async fn write(
        &mut self,
        request: &WriteRequest,
        cancel: CancellationToken,
    ) -> DriverResult<WriteOutcome> {
        let _ = (request, cancel);
        Err(DriverError::not_implemented("this driver", "writing rows"))
    }
}
```

```rust
// write.rs (new module of jykre-core)
#[non_exhaustive]
pub enum EditTarget {
    Editable(EditableTable),
    ReadOnly(ReadOnlyReason),
}

pub struct EditableTable {
    /// The table as a structural path, resolved from the catalog.
    pub table: ObjectPath,
    /// One entry per result column: `Some` for a column of `table` that
    /// can be written, `None` for one that cannot (computed, generated).
    pub columns: Vec<Option<EditableColumn>>,
    /// The result columns, by position, that form the row key, in key order.
    pub key: Vec<usize>,
    pub key_kind: KeyKind, // PrimaryKey | UniqueNotNull
    /// What a write here can change beyond the rows written, named in the
    /// confirmation: triggers, and foreign keys of other tables whose
    /// ON DELETE or ON UPDATE cascades, sets NULL or sets the default.
    pub side_effects: Vec<SideEffect>,
}

pub struct EditableColumn {
    /// The table column's own name, not the result's alias.
    pub name: String,
    /// The type as the catalog writes it (`numeric(12,2)`, `varchar(80)`).
    /// Only the driver reads it, to bind values.
    pub declared_type: String,
    pub nullable: bool,
    /// An insert may leave it out and get the table's default.
    pub has_default: bool,
    /// Its values are bound as bytes, not text.
    pub binary: bool,
}

#[non_exhaustive]
pub enum ReadOnlyReason {
    DriverCannotTell,
    ComputedOnly,
    SeveralTables,
    NotATable { kind: String },
    NotTransactional,
    NoKey,
    KeyNotInResult { missing: Vec<String> },
    KeyType { column: String },
}

pub struct WriteRequest {
    pub target: EditableTable,
    pub changes: Vec<RowChange>,
    /// The tab's statement timeout, for the whole write.
    pub timeout: Option<Duration>,
}

pub enum RowChange {
    /// `key` holds one value per key column, in key order. `set` pairs a
    /// result column's position with its new value.
    Update { key: Vec<WriteValue>, set: Vec<(usize, WriteValue)> },
    /// Columns left out take their default.
    Insert { values: Vec<(usize, WriteValue)> },
    Delete { key: Vec<WriteValue> },
}

pub enum WriteValue {
    Null,
    /// The exact text: a cell's text as read, or what the user typed. The
    /// database converts it to the column's type.
    Text(String),
    Bytes(Vec<u8>),
}

#[non_exhaustive]
pub enum WriteOutcome {
    /// Every change affected exactly one row. `committed` is false when
    /// the session's transaction holds them uncommitted.
    Written { rows: u64, committed: bool },
    /// Change `index` affected `matched` rows, not one. Everything this
    /// request did was rolled back, and nothing of it remains.
    RolledBack { index: usize, matched: u64 },
}
```

The result model keeps the origins. `ResultColumn` gains `origin:
ColumnOrigin`. It also gains `binary: Option<bool>`, which the
`ResultBuilder` sets from the cells it converts: `Some(true)` when they
arrived as `Cell::Binary`, `Some(false)` as text, `None` when mixed. This
adds a few bytes per column and nothing per cell.

### 3. How each driver tells

| | PostgreSQL | MySQL | SQLite | Databricks |
| --- | --- | --- | --- | --- |
| Origin at execute, no extra I/O | `relation_id()` and `relation_attribute_no()` of the prepared statement's columns, kept as decimal text. `None` is `Expression` | `Column::origin()`: the relation is the `schema.table` text, the column is the physical column name | `Column::origin()`: the relation is `db.table` or `table`, the column is the origin name | None: `Unknown` for every column |
| Resolution (`edit_target`, catalog queries in the session, with bound arguments) | `pg_class` and `pg_namespace` for the OID: path, `relkind` (`r` or `p`). `pg_attribute`: the name, `format_type(atttypid, atttypmod)`, `attnotnull`, `atthasdef`, `attgenerated`, `attidentity`. `pg_index`: `indisprimary`, else `indisunique` with every column `attnotnull`, `indpred IS NULL` and no expression column. Side effects: `pg_trigger` without `tgisinternal`, and `pg_constraint` (`contype = 'f'`) that references the table with `confdeltype` or `confupdtype` among `c`, `n` and `d` | The relation is split at its only dot. When it has several dots, each split is checked against `information_schema.TABLES` and the one that exists is used. If none or more than one exists, the result is read-only (decision E-6). `TABLES`: `TABLE_TYPE = 'BASE TABLE'` and `ENGINE` transactional (InnoDB). `COLUMNS`: `COLUMN_TYPE`, `IS_NULLABLE`, `COLUMN_DEFAULT`, `EXTRA` (generated). `STATISTICS`: `PRIMARY`, else `NON_UNIQUE = 0` with every column `NOT NULL` and no functional part. Side effects: `TRIGGERS` and `REFERENTIAL_CONSTRAINTS` | Same split rule against `pragma_database_list`. `pragma_table_xinfo` (`pk`, `notnull`, `dflt_value`, `hidden` for generated columns). `pragma_index_list` and `pragma_index_info` for unique keys. Side effects: `sqlite_schema` triggers, and `pragma_foreign_key_list` of the other tables (`on_delete`, `on_update`). The `rowid` is not used as a key | — |
| A view in the query | Parse-time origin. A plain view's columns are expected to name the view, which is then not a table and so read-only. **To be confirmed by the live tests** | Depends on the server's view algorithm; `TABLE_TYPE` decides. **To be confirmed by the live tests** | Names the base table even through a view, so the write goes to that table. The confirmation names it | — |
| Atomic writes | `BEGIN`/`COMMIT`, or `SAVEPOINT` inside a transaction | The same, InnoDB only. A non-transactional engine (MyISAM) is read-only | The same. One writer per file, 4 s busy wait | No multi-statement transactions. Each statement is its own Delta commit |
| Feasible in 0.3.0 | **Yes** | **Yes**, for InnoDB tables | **Yes** | **No**: recommended read-only (decision E-3) |

The resolution runs in the tab's own session, so it sees what the session
sees, including a table its open transaction created. It runs when the user
first starts editing a result, never on every run. Runs therefore cost
nothing extra. The answer is kept with the result tab. `write` checks the
target again in the same transaction, before its first statement. If the
table, its key or a column changed meanwhile, the write fails with
`Validation` ("The table changed since this result was read; run it again")
and writes nothing.

### 4. How changes are written

- **Generated by the driver, with its quoting.** One statement per change,
  each naming the table and its columns through `quote_identifier` and
  `relation_name`:
  - `UPDATE t SET c1 = <v>, … WHERE k1 = <v> AND …`
  - `DELETE FROM t WHERE k1 = <v> AND …`
  - `INSERT INTO t (c1, …) VALUES (<v>, …)`

  A driver may group consecutive inserts into multi-row statements within
  its parameter limit: 65,535 for PostgreSQL and MySQL, and SQLite's
  `SQLITE_MAX_VARIABLE_NUMBER`. The statements run through SQLx's prepared
  path. They return no rows, so the single decoding path of ADR-0023 is
  untouched.
- **Every value is bound**, never written into the SQL. `<v>` is a
  placeholder plus the conversion that keeps the value exact:
  - PostgreSQL: `CAST($n AS <declared_type>)`, with the text bound as
    `text`. The server's input function reads it. The text is what the
    server printed or what the user typed, so nothing passes through a
    client-side type. Since PostgreSQL 12, `real` and `double precision`
    print text that reads back exactly. `bytea` arrives and returns as its
    `\x…` text.
  - MySQL: by the declared type's family:
    - integers: `CAST(? AS SIGNED)` or `CAST(? AS UNSIGNED)`;
    - decimals: `CAST(? AS DECIMAL(p,s))`, with `p` and `s` from the
      declared type;
    - `DATE`, `DATETIME(fsp)` and `TIME(fsp)`: the matching `CAST`;
    - text: no cast, so the column's collation applies;
    - binary columns: the bytes decoded from the cell's `0x…` text, bound as
      bytes.

    This avoids MySQL comparing a string to a number as floating point.
    After each statement the driver reads `@@warning_count`. A warning is a
    value MySQL truncated or adjusted, so it rolls the request back like a
    row-count mismatch and names the value.
  - SQLite: the text bound as text. The column's affinity converts it when
    it is stored, and in the key comparison. Cells that arrived as binary
    are bound as bytes.
- **NULL is `WriteValue::Null`**, chosen with an explicit **Set to NULL**.
  Typing `NULL` gives the four-letter text. Clearing a cell gives empty text.
  An inserted row's untouched columns are left out of the `INSERT`, so they
  take the table's default, which is not NULL.
- **Temporal values are never converted.** A value is the text the
  database printed or the text the user typed, read by the server in the
  same session, exactly as a statement typed in the tab would be. A
  PostgreSQL `timestamptz` keeps its printed offset.
- **The key identifies the row; nothing else is compared** (decision E-5).
  A change made by someone else to the same row in the meantime is
  overwritten. The confirmation says so.
- **Exactly one row per change.** After each statement the driver checks
  the rows affected. SQLx's MySQL connections ask for `CLIENT_FOUND_ROWS`, so
  an `UPDATE` reports the rows it matched. A count other than one rolls the
  whole request back and returns `RolledBack { index, matched }`. The grid
  then marks that row and says how many rows matched.
- **Bounded.** At most 10,000 changed rows per write (decision E-7). The tab's
  statement timeout applies to the whole write. **Stop** cancels it like a
  run. Cancellation, the timeout or an error roll the request back. If the
  connection is lost during `COMMIT`, the tab says the change may or may not
  have been written. It is never written again by itself.

### 5. Session and transaction

- **The tab's session.** The write runs in the session that read the
  result, as a run of the tab: it takes the session as a run does, counts as
  a running query for Stop, for closing the tab and for saving the
  connection, and is logged in **Messages** with its counts but without
  values. Writing, like running anything else in the tab, closes the
  result's pages not read yet. The grid says so before the first edit when
  the result still has more rows.
- **Outside a transaction:** the driver wraps the request in its own
  transaction (`BEGIN … COMMIT`) and commits only when every change affected
  exactly one row. Otherwise, it rolls back. The confirmation also names the
  table's triggers and cascading foreign keys before anything is written
  (`side_effects`). So a write that may touch other rows is never committed
  without the user having been told.
- **Inside an open transaction** (a `BEGIN` the user ran, or MySQL with
  `autocommit = 0`): the driver uses `SAVEPOINT jykre_edit`. A mismatch rolls
  back to the savepoint, and the user's transaction keeps what it had. On
  success the savepoint is released and the changes stay **uncommitted**:
  `WriteOutcome::Written { committed: false }`. The workspace asks
  `Session::transaction` as after a run, so the toolbar shows a transaction
  with changes and **Commit** or **Rollback** decide. A PostgreSQL
  transaction that already failed refuses edits until it is rolled back.
- The driver knows which case applies from the transaction state it reads
  from the protocol (ADR-0030). The application does not infer it.

### 6. Confirmation and Production

- Edits are **pending** until the user writes them (decision E-2):
  - changed cells, NULLs, new rows and rows marked for deletion are shown
    in the grid, distinct from the values read;
  - **Discard** drops them;
  - nothing is written on leaving a cell.
- **Write…** opens one confirmation before anything is sent. It shows:
  - the table, by its qualified name;
  - each change: the key, and for each changed column the value read and
    the new value. NULL is shown as NULL, apart from the text `NULL`;
  - the statements with their placeholders, the values listed apart;
  - whether the changes will be committed, or left in the tab's open
    transaction;
  - the table's triggers and cascades;
  - that a concurrent change to the same row would be overwritten.
- On a connection marked **Production** the same dialog opens with the
  production warning and a confirming button that names it. That is the
  single question, as `jykre::guard` asks once for a run's risky statements.
  Every write asks, even a single cell. A change to every row cannot happen
  here, since every statement names one key.
- Pending edits are held in memory only. They are never written to
  `config.db`, logs or telemetry, and they are lost with the tab or the
  application. Closing a tab or running another statement with pending
  edits asks first.

### 7. A driver that cannot tell

A driver without `row_writes` keeps the defaults: every column is
`ColumnOrigin::Unknown`, and `edit_target` answers
`ReadOnly(DriverCannotTell)`. The grid stays read-only and says so: "This
connection's results cannot be edited: its driver does not report which
table a column comes from." No editing control is offered. The application
never guesses a table from the SQL text.

### 8. In the application

- The pending edits and the request that turns them into `RowChange`s live
  in `jykre::grid_edit`, outside the views (golden rule 1). That code is
  tested without a window. Writing goes through `backend::write`, on the
  runtime, with the tab's session.
- The grid draws pending values over the cached `GridView`. The cache key
  gains the edits' version, so hovering and scrolling still rebuild nothing,
  and a frame costs what is on screen ([Performance](../standards/performance.md)).
- After a successful write the grid shows each written cell **as written**,
  marked. The database may store a value differently: a `numeric(10,1)`
  rounds, and a default or a trigger fills columns. So the tab offers ↻ to
  read the rows back. It never re-runs the statement by itself.
- The editing keys (edit, set to NULL, insert a row, delete rows, write,
  discard) are GPUI actions in the keymap, listed in User experience and
  **Preferences › Shortcuts** (milestone A12).

## Decisions for the maintainer

| ID | Question | Options | Recommendation |
| --- | --- | --- | --- |
| E-1 | Does an edit write directly, or generate the statements into the editor? | **(a) Write directly** after the confirmation of section 6. **(b) Generate into the editor:** the user reviews and runs the SQL. Values written as literals break A10's "never written into the SQL". With `:name` markers ([ADR-0026](0026-named-parameters-in-sql-tabs.md)), every value becomes a parameter field, only five kinds exist (no binary, interval or JSON), and the driver could no longer check exactly one row per change or wrap the changes in one transaction or savepoint | **(a)**. The dialog shows the statements with placeholders, so the user still sees what runs, and the safety checks stay in the driver |
| E-2 | Are pending edits written together, or one by one? | **(a) Together:** pending in the grid, one confirmation, one atomic write. **(b) One by one:** each cell written on leaving it. That needs a confirmation per cell (tiring), or none (breaks A10), and leaves half of a multi-cell correction committed when the second cell fails | **(a)** |
| E-3 | Is Databricks included? | **(a) Read-only**, with its reason. **(b) Warehouses only, with key columns the user names:** the API reports no origin, Delta keys are informational and not enforced, there is no multi-statement transaction (each statement is a separate Delta commit, so a failed fifth change leaves four written), and classic clusters cannot bind values (ADR-0026) | **(a)** for 0.3.0 |
| E-4 | Which keys identify a row? | (a) The primary key only. **(b) The primary key, else a unique index whose columns are all NOT NULL**, with no expression and no partial predicate. (c) Also the SQLite `rowid` when selected | **(b)**. (c) can come later |
| E-5 | Does a write detect a concurrent change to the row? | **(a) Key only:** last write wins, and the dialog says so. (b) Also compare the read values of the edited columns. This fails for types without equality (PostgreSQL `json`, `xml`, `point`) and for approximate numbers, so some edits could never be written | **(a)** |
| E-6 | MySQL and SQLite join the schema and the table with a dot in SQLx's `origin()` | **(a)** Split at the only dot. With several dots, check each split against the catalog. Read-only when not exactly one exists. (b) Widen the pinned SQLx fork to expose the schema, table and column flags separately. That is a dependency change under ADR-0030, kept until upstream has it | **(a)**. Names with dots are rare, and the result is then read-only, never misattributed |
| E-7 | How many changed rows may one write hold? | 1,000, **10,000**, or no limit | **10,000**, refused above with the reason. The changes are held in memory and listed in the dialog |

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Guess the table from the SQL text (`SELECT … FROM t`) | No contract change; works on Databricks | Wrong for aliases, joins, views, CTEs and `search_path`; parsing SQL in the application | Golden rule 3; the protocol already tells PostgreSQL, MySQL and SQLite |
| Use the metadata cache for keys | No catalog query | The cache may be stale or not introspected; it misses what the session's transaction created | The session's catalog is the truth at the moment of the write |
| Resolve the source on every run | The grid knows at once whether a result is editable | A catalog query per `SELECT` (budgets, and a PostgreSQL snapshot taken in a transaction) | Asked only when the user starts editing |
| Use SQLx's `ColumnOrigin` text for PostgreSQL | No OID handling | `attrelid::regclass::text` depends on `search_path` and is cached per connection by OID | The OID and attribute number are exact |
| MySQL key from the column flags | No catalog query | `flags` is private in SQLx; `PRIMARY_KEY` marks columns, not which index; needs the fork widened | The catalog gives the key and its columns' order |
| Write in a new session | Never touches the tab's transaction | A10 asks that the tab's transaction is honoured; a new session would not see the rows the transaction changed | The tab's session, with a savepoint |
| Compare every read column in `WHERE` | Detects any concurrent change | Types without equality; approximate numbers; long values | E-5 |

## Consequences

- Positive:
  - Editing rests on what the server says, in the session that read the
    result. A write never touches a row other than the one keyed, unless
    the confirmation said why (triggers, cascades).
  - The values travel apart from the SQL, and the server converts them.
  - [ADR-0040](0040-importing-files-into-a-table.md) reuses the same write
    path for imports.
- Negative or costs:
  - A field in `ColumnInfo`, so every `ColumnInfo` literal changes.
  - Two methods and a capability on `Session`.
  - A write module in core.
  - Catalog queries per driver, with live tests on each.
  - The edited cells show what was written, not what was stored, until the
    rows are read again.
  - Last write wins.
- What becomes required:
  - A driver with `row_writes` binds every value and checks exactly one row
    per change.
  - It wraps a request in a transaction or a savepoint, and checks the
    target again before writing.
  - Each driver documents its catalog queries in
    [Drivers](../reference/drivers.md).
- What becomes forbidden:
  - Writing a value into generated SQL.
  - Writing without the confirmation.
  - Editing a result whose driver cannot tell, or guessing its table.
  - Committing a write in which a change affected other than one row.

## How to validate

- Core tests: defaults (`Unknown`, `ReadOnly(DriverCannotTell)`,
  `NotImplemented`). `jykre::grid_edit` turns pending edits into changes,
  with NULL, empty text and the text `NULL` kept apart. Deleting a row
  dropped its pending updates.
- Live tests under `cargo xtask check --full` on PostgreSQL 14/18 and MySQL
  8.4/9.7, and SQLite on synthetic files:
  - a single-table result is editable, and a join, a computed column, a
    view, a table without a key, a MyISAM table, a `FLOAT` key and a MySQL
    `TIMESTAMP` key are each read-only with the right reason;
  - an update, an insert with a default and a delete are written in
    autocommit;
  - inside `BEGIN` they stay uncommitted until `ROLLBACK` undoes them;
  - a duplicated key (a unique index dropped meanwhile) rolls back with
    `RolledBack`;
  - `numeric(38,10)`, `bigint` above 2^53, a MySQL `DECIMAL(65,30)`,
    `bytea` and `VARBINARY`, `timestamptz` in another session time zone,
    empty text, the text `NULL` and NULL all keep their values exactly when
    read back;
  - a MySQL value that would be truncated is refused (`@@warning_count`);
  - a table changed after the result was read fails with `Validation`;
  - Stop in a write of 10,000 rows leaves the table as it was.
- Databricks: a local HTTP test shows its results read-only.
- In the application: edit, set to NULL, insert, delete, write and discard
  with the keyboard alone, the Production dialog, and the grid's budgets
  with a result at the row limit and edits pending
  ([Performance](../standards/performance.md)).
