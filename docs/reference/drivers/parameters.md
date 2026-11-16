# Drivers: named parameters

Part of [Database drivers](README.md).

## Named parameters

Report panels and query tabs run SQL with values the user types for each run
(D-0023, D-0026). Values are always bound parameters, never part of the SQL
text, and the SQL the user wrote changes only by replacing its `:name` markers
with fixed references.

**Contract (`jovru_core`).**

- `QueryRequest::parameters: Vec<QueryParameter>` is empty unless the caller
  sets it (`QueryRequest::new` builds a request without parameters).
  `QueryParameter { name, kind, value: Option<String>, markers:
  Vec<Range<usize>> }`:
  - `ParameterKind` is `Text`, `Number` (an exact decimal: `-12`, `3.50`,
    `1e6`), `Boolean` (`true` or `false`), `Date` (`YYYY-MM-DD`) or
    `Timestamp` (`YYYY-MM-DD HH:MM[:SS[.fraction]]`, a space or `T`, no time
    zone).
  - `value` is the value's exact text; `None` is a NULL of the parameter's
    kind.
  - `markers` are the byte ranges of each of the parameter's `:name` markers
    in `QueryRequest::sql`. The caller fills them with
    `jovru_sql::named_markers`, because drivers may depend only on
    `jovru-core` (enforced by the architecture check in `cargo xtask check`).
    Drivers do not parse SQL.
- `QueryRequest::validate` checks that names are unique and valid (an ASCII
  letter or `_`, then ASCII letters, digits or `_`, at most 64 characters),
  that each value has the shape of its kind and holds no NUL character (only
  the shape: the server checks the rest, such as February 30th), and that
  each parameter has at least one marker, every marker spells `:name` at its
  range and no two markers overlap.
- `QueryRequest::with_references` does the replacement the drivers share: it
  copies the SQL and puts the reference a driver gives for each parameter in
  place of each of its markers, touching nothing else. Each driver supplies
  only the reference text, so the replacement is tested once, without a
  server.
- `DriverCapabilities::named_parameters` is `false` by default; a driver
  without it rejects a request that carries parameters. All four built-in
  drivers set it.

**Finding markers.** `jovru_sql::named_markers(sql, dialect)` is the single
place that decides what a marker is. It returns the markers in order, repeats
included, from the editor tokenizer's tokens for the dialect, so only code
counts: string literals (escape strings included), comments (`--`, `/* */` and
MySQL's `#`), quoted names and PostgreSQL's dollar-quoted bodies are skipped
whole. A marker is a `Parameter` token that starts with `:` (how the
Databricks tokenizer reads `:name`), or a `:` symbol followed, with no space,
by a word that starts with an ASCII letter or `_`, where the `:` does not come
right after a letter, a digit, `_`, `)`, `]`, a closing quote or another `:`.
A marker right after `[` is not one, and the name must be a valid parameter
name. So none of `x::int`, `y :: text`, `x:=1`, `raw:owner`, `arr[1:2]`,
`arr[lo:hi]`, `arr[:hi]`, `: a`, `:1`, `:ação`, `':a'`, `-- :a`, `/* :a */`
and `$$ :a $$` holds a marker.

**Binding, per driver.** PostgreSQL and MySQL have no named parameters and
bind only through the extended or prepared protocol. Binding the user's
statement itself would put it on SQLx's prepared path, whose rows arrive in
binary and would need a decoder for every type of each database (see
[SQLx and the pinned fork](configuration.md#sqlx-and-the-pinned-fork) on the text protocol).
Instead the values are bound by a separate statement that returns no rows the
user sees, and the user's SQL refers to them; the statement itself still runs
through `RawSql` and its rows are decoded as in any SQL tab.

| Driver | How the value is bound | What a marker becomes |
| --- | --- | --- |
| PostgreSQL | One round trip before the statement, in the same session, over the extended protocol: `SELECT set_config(n, v, false) FROM unnest($1::text[], $2::text[]) AS p(n, v)`, with the names `jovru.p_<i>` and the values bound as two text arrays. `is_local` is `false`, since in autocommit a local setting would end with that statement's own transaction | `(current_setting('jovru.p_<i>')::<type>)`, `<type>` being `text`, `numeric`, `boolean`, `date` or `timestamp`; a NULL becomes `CAST(NULL AS <type>)` and sets no variable |
| MySQL | One prepared `SET @jovru_p_0 = ?, @jovru_p_1 = ?, …` over the binary protocol; it returns no rows. Booleans are bound as booleans, the other kinds as text | Text: `@jovru_p_<i>`, with no cast, so it keeps the connection's collation. Number: `CAST(@jovru_p_<i> AS DECIMAL(65,s))`, `s` being the value's fraction digits (at most 30; 30 for a value written with an exponent). Boolean: `@jovru_p_<i>`. Date: `CAST(… AS DATE)`. Timestamp: `CAST(… AS DATETIME(6))`. A NULL becomes `CAST(NULL AS <type>)` and sets no variable |
| SQLite | `:name` becomes `?N`, bound in order; numbers are bound as text and read as `CAST(?N AS NUMERIC)`, booleans as `1`/`0`, dates and timestamps as text | `?N` |
| Databricks, SQL warehouse | The `parameters` field of the statement request, `[{name, value, type}]`, with `STRING`, `DECIMAL(p,s)` (computed from the text, `p` at most 38), `DOUBLE` for a number written with an exponent, `BOOLEAN`, `DATE` or `TIMESTAMP`; a NULL omits `value`. A number of more than 38 digits is rejected before anything is sent | Nothing: the SQL is sent unchanged, since the server reads `:name` itself |
| Databricks, classic cluster | The command API has no binding, so each marker becomes a typed literal (below) | The literal |

- On PostgreSQL and MySQL nothing is cleared after the run. A result can stay
  open to be read page by page (`DriverOutcome::has_more`) and no other
  statement can run on the connection meanwhile. The settings or variables
  stay in the tab's own session (D-0014) until its next run overwrites them
  or it closes; no other session sees them, but SQL that reads or sets them
  sees or changes the application's values.
- PostgreSQL's planner sees `current_setting(…)`, a stable function, instead
  of a constant: row estimates can differ from a literal's, and partition
  pruning happens when the statement starts. In a transaction the tab opened,
  the `set_config` can take the snapshot just before the statement would
  ([TD-002](../../plans/technical-debt.md#td-002--binding-named-parameters-in-postgresql-runs-a-query-first)).
- Numbers are bounded by each server's exact type. MySQL's `DECIMAL(65,s)`
  rounds a value with more than 30 fraction digits and holds at most `65 - s`
  integer digits. Databricks sends an exponent number (`1e6`) as `DOUBLE`,
  which is approximate; written out in full it stays exact. A Databricks
  timestamp parameter is read in the session's time zone, as a
  `TIMESTAMP '…'` literal would be.
- A driver that cannot bind refuses the request; it never falls back to
  interpolating a value. Parameter values are never logged nor kept in the
  history ([security](../../standards/security.md#logs-and-diagnostics)).

**Typed literals on classic clusters.** The driver replaces each marker,
through `with_references`, with a literal that never holds the text of a value
that could change the statement:

| Kind | Literal | NULL |
| --- | --- | --- |
| Text | `decode(X'<hex of the UTF-8 bytes>', 'UTF-8')`, or `''` when empty | `CAST(NULL AS STRING)` |
| Number | `CAST('<value>' AS DECIMAL(p,s))` with the warehouse's precision and scale, `DOUBLE` with an exponent; more than 38 digits is refused before sending | `CAST(NULL AS DECIMAL(38,18))` |
| Boolean | `TRUE` or `FALSE` | `CAST(NULL AS BOOLEAN)` |
| Date | `DATE'YYYY-MM-DD'` | `CAST(NULL AS DATE)` |
| Timestamp | `TIMESTAMP'…'`, in the session's time zone as on a warehouse | `CAST(NULL AS TIMESTAMP)` |

Text reaches the command only as hexadecimal digits, so no quote, backslash,
comment or `$` of the value is ever in it, whatever the cluster's escaping
settings (`spark.sql.parser.escapedStringLiterals`). The other kinds are
written only after `validate` checked their shape, and the driver checks again
that they hold nothing but their kind's characters (digits, sign, `.`, `e`;
digits and `-`; digits, `-`, `:`, `.`, space and `T`), refusing anything else.

**SQL tabs.** A query tab finds the markers of its SQL with `named_markers`
(through `jovru::parameters::names`) each time the SQL or its dialect changes
and shows one field per name above the editor ([UX](../ux/README.md)); a tab without
markers shows no form.

- Each field has a type: **Automatic** (the default) or Text, Number, True or
  false, Date, Date and time. Automatic reads it from the value's shape
  (`jovru::parameters::infer_kind`): `true`/`false` a boolean, an exact
  number a number, `YYYY-MM-DD` a date, a date and time a timestamp, anything
  else text (`00123` reads as a number; the user then chooses Text).
- A field binds NULL only when **NULL** is chosen; an empty field is a
  missing value and nothing runs.
- A run builds one request per statement it runs
  (`jovru::parameters::tab_request`) and only the markers of those
  statements need a value. Requests, validation and drivers are the
  report's.
- A result tab keeps the parameters bound for it. **Run again** binds the
  fields' current values; an export that runs the statement again binds the
  values its result was read with.
- The fields (name, chosen type, text, NULL) are kept with the tab in
  `workspace.state` (`config.db`); an older version drops them on its next
  save. Report definitions never keep the values typed for a run. The
  history and the run log keep the SQL with its markers, never the values.
- The shared rules live in `jovru::parameters`; `jovru::reports::params`
  keeps only what reports add (specifications, required fields, defaults).
