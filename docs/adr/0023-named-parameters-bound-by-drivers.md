# ADR-0023: Named parameters bound by the drivers

- **Status:** Proposed
- **Date:** 2026-11-04
- **Authors:** AI agent, at the maintainer's request (plan approved 2026-11-04)
- **Amends:** [ADR-0002](0002-sqlx-for-postgres-and-mysql.md), for report SQL
  only: user SQL keeps the text protocol, but a report's `:name` markers are
  replaced by fixed references before it is sent

## Context

Reports ([ADR-0024](0024-reports.md)) run saved SQL with values the user types
for each run: a date range, a customer code, a status. Three written rules
apply:

- Values are always bound parameters, never interpolated into the text
  ([Rust guidelines](../rust-guidelines.md#sql-generated-by-the-application)).
- The SQL the user wrote is sent as written ([Security](../security.md#sql-sent-to-servers)).
- User SQL runs through the text protocol (`RawSql`) and every value arrives
  as text ([ADR-0002](0002-sqlx-for-postgres-and-mysql.md)). The PostgreSQL
  and MySQL drivers decode rows with `value.as_str()`
  (`crates/driver-postgres/src/runtime.rs`); SQLx's prepared queries ask for
  binary results, which those drivers do not decode.

The servers differ:

- PostgreSQL and MySQL have positional parameters (`$1`, `?`) only in their
  extended or prepared protocols, and no named parameters at all.
- A Databricks SQL warehouse's Statement Execution API
  (`POST /api/2.0/sql/statements`) binds `:name` markers itself from a
  `parameters` field.
- A classic Databricks cluster's command API (`/api/1.2/commands/execute`) has
  no binding.

`:name` is the marker users know from other tools and the one Databricks reads
natively, but a colon already means something else in each dialect: `x::int`
casts in PostgreSQL, `@a := 1` assigns in MySQL, `raw:owner` reads a JSON path
in Databricks and `arr[lo:hi]` slices an array in PostgreSQL.

## Decision

### Contract (`jykre-core`)

- `QueryRequest` gains `parameters: Vec<QueryParameter>`. It is empty for
  every SQL tab (`QueryRequest::new` builds a request without parameters), so
  nothing changes for them.
- `QueryParameter { name, kind, value: Option<String>, markers: Vec<Range<usize>> }`:
  - `kind: ParameterKind { Text, Number, Boolean, Date, Timestamp }`. Number is
    an exact decimal (`-12`, `3.50`, `1e6`), Boolean is `true` or `false`, Date
    is `YYYY-MM-DD`, and Timestamp is `YYYY-MM-DD HH:MM[:SS[.fraction]]` with a
    space or `T` and no time zone.
  - `value` is the value's exact text; `None` is a NULL of the parameter's
    kind.
  - `markers` holds the byte ranges of each of the parameter's `:name` markers
    in `QueryRequest::sql`. The caller fills them with
    `jykre_sql::named_markers`, because drivers may depend only on
    `jykre-core` (enforced by the architecture check in `cargo xtask check`).
    Drivers do not parse SQL.
- `QueryRequest::validate` checks that:
  - names are unique and valid: an ASCII letter or `_`, then ASCII letters,
    digits or `_`, at most 64 characters;
  - each value has the shape of its kind and holds no NUL character (only the
    shape is checked; the server checks the rest, such as February 30th);
  - each parameter has at least one marker, every marker spells `:name` at
    its range in the SQL, and no two markers overlap.
- `QueryRequest::with_references` does the marker replacement the drivers
  share: it copies the SQL and puts the reference a driver gives for each
  parameter in place of each of its markers, touching nothing else.
- `DriverCapabilities` gains `named_parameters` (`false` by default). A driver
  without it rejects a request that carries parameters. A driver with it may
  still refuse them for some connections, as Databricks does for classic
  clusters.

### Finding markers (`jykre-sql`)

`named_markers(sql, dialect) -> Vec<NamedMarker { name, range }>` returns the
markers in order, repeats included. It reads the tokens of the editor's
tokenizer for the dialect, so only code counts: string literals (escape
strings included), comments (`--`, `/* */` and MySQL's `#`), quoted names and
PostgreSQL's dollar-quoted bodies (`$$ … $$`) are skipped whole.

A marker is either:

- a `Parameter` token that starts with `:` (how the Databricks tokenizer reads
  `:name`); or
- a `:` symbol followed, with no space between them, by a word that starts with
  an ASCII letter or `_`, where the `:` does not come right after a letter, a
  digit, `_`, `)`, `]`, a closing quote or another `:`.

In both cases a marker right after `[` is not one, and the name must be a
valid parameter name. So none of these holds a marker: `x::int`,
`y :: text`, `x:=1`, `raw:owner`, `arr[1:2]`, `arr[lo:hi]`, `arr[:hi]`,
`: a`, `:1`, `:ação`, `':a'`, `-- :a`, `/* :a */` and `$$ :a $$`.

Finding markers changes nothing. The reports library uses the names to build
the form and passes the ranges in the request.

### Binding, per driver

On PostgreSQL and MySQL the only change to the SQL text is that each marker is
replaced by a fixed reference the driver generates from the parameter's index
and kind (and, for a MySQL number, from how many fraction digits it has). The
value travels apart from that text, bound for real.

| Driver | How the value is bound | What a marker becomes | After the run |
| --- | --- | --- | --- |
| PostgreSQL | Before the statement, one round trip in the same session with the extended protocol: `SELECT set_config(n, v, false) FROM unnest($1::text[], $2::text[]) AS p(n, v)`, with the names `jykre.p_<i>` and the values bound as two text arrays | `(current_setting('jykre.p_<i>')::<type>)`, where `<type>` is `text`, `numeric`, `boolean`, `date` or `timestamp`; a NULL value becomes `CAST(NULL AS <type>)` and sets no variable | Nothing is cleared: the settings stay in the report tab's own session until its next run overwrites them |
| MySQL | One prepared `SET @jykre_p_0 = ?, @jykre_p_1 = ?, …`, executed with the binary protocol; it returns no rows. Booleans are bound as booleans, the other kinds as text | Text: `@jykre_p_<i>`, with no cast, so it keeps the connection's collation. Number: `CAST(@jykre_p_<i> AS DECIMAL(65,s))`, where `s` is the value's number of fraction digits (at most 30), or 30 for a value written with an exponent. Boolean: `@jykre_p_<i>`. Date: `CAST(@jykre_p_<i> AS DATE)`. Timestamp: `CAST(@jykre_p_<i> AS DATETIME(6))`. A NULL value becomes `CAST(NULL AS <type>)` and sets no variable | Nothing is cleared: the variables stay in the report tab's own session until its next run overwrites them |
| Databricks, SQL warehouse | The `parameters` field of the statement request, `[{name, value, type}]`, with `STRING`, `DECIMAL(p,s)` (precision and scale computed from the text, `p` at most 38), `DOUBLE` for a number written with an exponent, `BOOLEAN`, `DATE` or `TIMESTAMP`; a NULL omits `value`. A number of more than 38 digits is rejected before anything is sent | Nothing: the SQL is sent unchanged, since the server reads `:name` itself | Nothing to clear |
| Databricks, classic cluster | Not supported: the request fails with a clear message that parameters need a SQL warehouse, before anything is sent | — | — |

- PostgreSQL sets the variables for the session (`is_local` is `false`):
  in autocommit, a local setting would end with the `set_config` statement's
  own transaction.
- The bound values are not cleared after the run. A result can stay open in
  the session to be read page by page (`DriverOutcome::has_more`), and no
  other statement can run on the connection meanwhile. Each report tab has
  its own session ([ADR-0014](0014-a-session-per-tab.md)), so the values stay
  there, never visible to another session, until the tab's next run
  overwrites them or the session closes.
- The statement itself still runs through the text protocol (`RawSql`) on
  PostgreSQL and MySQL, and its rows are decoded exactly as in a SQL tab.
- Each driver supplies only the reference text for a parameter;
  `QueryRequest::with_references` puts it in place, so the replacement is
  tested once, without a server.

### Why the text protocol of ADR-0002 is kept

Binding the user's statement directly (markers turned into `$1` or `?`) would
put it on SQLx's prepared path, whose rows arrive in binary. Each driver would
then need a decoder for every type its database has, which is what
[ADR-0002](0002-sqlx-for-postgres-and-mysql.md) chose to avoid. Reports and SQL
tabs would also decode the same value along two paths that could show it
differently. Binding the values in a separate statement that returns no rows
the user sees, and referring to them from the user's SQL, keeps one decoding
path and still keeps every value out of the SQL text.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Interpolate typed literals (`DATE '2026-01-01'`, strings quoted with the dialect's rules) | One round trip; works on every driver, classic clusters included | The value is part of the SQL text; whether the quoting is safe depends on the dialect and on server settings (`standard_conforming_strings`, `NO_BACKSLASH_ESCAPES`) | Breaks the bound-values rule; a quoting bug is an injection |
| Extended protocol with binary decoding: markers turned into `$1` or `?` and the statement prepared | Standard binding; the planner sees the values | Binary decoding per type for PostgreSQL and MySQL, two decoding paths, and MySQL cannot prepare every kind of statement | Reverses ADR-0002 for one feature |
| Plain text replacement of `:name` by the value | Trivial | Injection by construction | Rejected outright |
| Native `:name` only, sending the SQL unchanged | No rewriting at all | Only Databricks warehouses support it | PostgreSQL and MySQL have no named parameters |

## Consequences

- Positive:
  - No value is ever written into SQL text, on any driver. A value such as
    `' \ ; -- /* $$` reaches the server as data.
  - SQL tabs are unchanged: their requests carry no parameters.
  - Reports and SQL tabs share one way of decoding rows.
- Negative or costs:
  - PostgreSQL and MySQL take one extra round trip, before the statement.
  - On PostgreSQL and MySQL the SQL that runs is not byte for byte the SQL
    the user wrote: the markers are replaced. The report can show the SQL
    that ran ([ADR-0024](0024-reports.md)).
  - PostgreSQL's planner sees `current_setting(…)`, a stable function, instead
    of a constant. Row estimates can differ from those for a literal, and
    partition pruning happens when the statement starts instead of when it is
    planned.
  - The report tab's session keeps the variables `jykre.p_<i>` or
    `@jykre_p_<i>` after the run, until its next run overwrites them or the
    session closes. No other session sees them, but SQL in that report that
    reads or sets them sees or changes the application's values.
  - Numbers are bounded by each server's exact type. On MySQL,
    `DECIMAL(65,s)` rounds a value with more than 30 fraction digits and
    holds at most `65 - s` integer digits. Databricks rejects a number of more
    than 38 digits, and sends one written with an exponent (`1e6`) as
    `DOUBLE`, which is approximate; written out in full, it stays exact.
  - On Databricks, a Timestamp parameter is read in the session's time zone,
    as a `TIMESTAMP '…'` literal would be.
  - Classic Databricks clusters cannot run a report that has parameters.
- What becomes required or forbidden:
  - Drivers never write a parameter's value into SQL text, not even quoted.
  - The only change to a report's SQL is replacing markers with fixed
    references the driver generates, through `QueryRequest::with_references`.
  - `jykre_sql::named_markers` is the single place that decides what a marker
    is.
  - A driver that cannot bind refuses the request; it never falls back to
    interpolation.
  - Parameter values are never logged, like any query parameter
    ([Security](../security.md#logs-and-diagnostics)).

## How to validate

- Unit tests for `named_markers` in every dialect: markers found in order with
  repeats, and none in `::`, `:=`, `raw:owner`, `arr[1:2]`, strings,
  comments, quoted names and `$$` bodies.
- Unit tests for `QueryRequest::validate` (repeated and invalid names, values
  of the wrong shape, markers out of place or overlapping) and for
  `QueryRequest::with_references`.
- Unit tests for each driver's references, and a test of the Databricks
  request body in the existing pattern of
  `crates/driver-databricks/src/runtime.rs`: `parameters` present with their
  types, `statement` unchanged, a number of more than 38 digits rejected
  without a request, and a classic cluster refusing without a request.
- Live tests (`cargo xtask check --full`, PostgreSQL 14/18 and MySQL 8.4/9.7)
  bind the values `' \ ; -- /* $$`. Each value must come back identical in a
  column, and the row count must not change.
