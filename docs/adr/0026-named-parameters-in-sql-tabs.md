# ADR-0026: Named parameters in SQL tabs, and typed literals on classic Databricks clusters

- **Status:** Proposed
- **Date:** 2026-11-05
- **Authors:** AI agent, at the maintainer's request (plan approved 2026-11-05)
- **Amends:** [ADR-0023](0023-named-parameters-bound-by-drivers.md), which
  limited parameters to reports and had classic Databricks clusters refuse
  them; [ADR-0005](0005-databricks-through-cli-and-rest.md), whose "user SQL is
  never rewritten" gains the same marker exception as PostgreSQL and MySQL

## Context

[ADR-0023](0023-named-parameters-bound-by-drivers.md) made the drivers bind
`:name` parameters, but only reports send them: every request of a SQL tab
carries no parameters, so its markers reach the server as written. Running a
`SELECT` again with another id means editing a literal in the SQL, or writing
the value into the text, which is what the bound-values rule
([Rust guidelines](../standards/rust-guidelines.md#sql-generated-by-the-application))
exists to avoid. DataGrip and DBeaver bind variables in their consoles;
Beekeeper Studio has query parameters.

A SQL tab has no report definition: nothing says a parameter's type, and on
PostgreSQL `id = :id` fails when the value is bound as text. Users also expect
the values to come back with the tab, like its SQL and its context.

Classic Databricks clusters run SQL through the Command Execution API
(`/api/1.2/commands/execute`), whose body is `{clusterId, contextId, language,
command}`. It has no field for values, so ADR-0023 had them refuse every
request with parameters, for reports too.

## Decision

### SQL tabs

- A query tab finds the `:name` markers of its SQL with
  `jykre_sql::named_markers` (through `jykre::parameters::names`) each time the
  SQL or its dialect changes, and shows one field per name above the editor.
  A tab whose SQL has none shows no form.
- Each field has a type: **Automatic** (the default) or Text, Number, True or
  false, Date, Date and time. Automatic reads the type from the value's shape
  (`jykre::parameters::infer_kind`): `true`/`false` is a boolean, an exact
  number a number, `YYYY-MM-DD` a date, a date and time a timestamp, anything
  else text. The field shows the type it reads (`Auto · Number`).
- A field binds NULL only when **NULL** is chosen; an empty field is a missing
  value, and nothing runs.
- A run builds one request per statement it runs
  (`jykre::parameters::tab_request`), and only the markers of those statements
  need a value. Values go through the same `QueryRequest::parameters`, the
  same validation and the same drivers as a report's.
- A result tab keeps the parameters bound for it. **Run again** (`↻`) binds
  the fields' current values, which is how the same query runs with another
  value; an export that runs the statement again binds the values its result
  was read with, so the file holds the result on screen.
- The fields (name, chosen type, text and NULL) are kept with the tab in
  `workspace.state`, like its SQL. Report definitions still never keep the
  values typed for a run ([ADR-0024](0024-reports.md)).
- The history and the run log keep the SQL with its markers, never the
  values.
- The shared rules (names, value shapes, binding by statement) live in
  `jykre::parameters`; `jykre::reports::params` keeps only what reports add
  (specifications, required fields, defaults).

### Classic Databricks clusters

The cluster driver replaces each marker, through
`QueryRequest::with_references`, with a typed literal that never holds the
text of a value that could change the statement:

| Kind | Literal | NULL |
| --- | --- | --- |
| Text | `decode(X'<hex of the UTF-8 bytes>', 'UTF-8')`, or `''` when empty | `CAST(NULL AS STRING)` |
| Number | `CAST('<value>' AS DECIMAL(p,s))` with the warehouse's precision and scale, `DOUBLE` with an exponent; more than 38 digits is refused before sending | `CAST(NULL AS DECIMAL(38,18))` |
| Boolean | `TRUE` or `FALSE` | `CAST(NULL AS BOOLEAN)` |
| Date | `DATE'YYYY-MM-DD'` | `CAST(NULL AS DATE)` |
| Timestamp | `TIMESTAMP'…'`, in the session's time zone as on a warehouse | `CAST(NULL AS TIMESTAMP)` |

- Text reaches the SQL only as hexadecimal digits, so no quote, backslash,
  comment or `$` of the value is ever in the command, whatever the cluster's
  escaping settings (`spark.sql.parser.escapedStringLiterals`).
- The other kinds are written only after `QueryRequest::validate` checked
  their shape, and the driver checks again that they hold nothing but their
  kind's characters (digits, sign, `.`, `e`; digits and `-`; digits, `-`, `:`,
  `.`, space and `T`), refusing anything else.
- SQL warehouses keep binding through the Statement API's `parameters`, and
  PostgreSQL and MySQL keep ADR-0023's bound session values.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Keep the SQL tab without parameters | Nothing changes | Editing literals, or interpolating, to run with another value | The problem this solves |
| A type chosen for every field, text by default | No inference | `id = :id` fails on PostgreSQL until the user picks Number | Automatic with an override serves both |
| Keep the values only in memory | Nothing typed on disk | Lost on restart, unlike the SQL | The maintainer chose to keep them with the tab |
| Classic clusters through a Python context and `spark.sql(sql, args=…)` | Real binding by Spark | A second context per tab, another result shape, values encoded inside Python source, ADR-0005 keeps Python out of scope | More moving parts than the literals, and not verifiable without a workspace |
| Databricks session variables (`DECLARE VARIABLE`, `SET VAR`) on classic clusters | Mirrors PostgreSQL's settings | The value is still in the SQL text of the `SET VAR` | Same exposure as a literal, with an extra command |
| Classic clusters keep refusing | No exception to ADR-0023 | Reports and SQL tabs with parameters cannot run on classic compute | The maintainer chose typed literals |

## Consequences

- Positive:
  - The same mechanism as reports in everyday SQL: values never edited into
    the statement by hand.
  - Reports and SQL tabs with parameters run on classic clusters.
- Negative or costs:
  - On PostgreSQL and MySQL a SQL tab's statement with markers is preceded by
    the binding round trip of ADR-0023, and its settings or variables stay in
    the tab's session. In a transaction the tab opened, the PostgreSQL
    `set_config` can take the snapshot just before the statement would
    ([TD-002](../plans/technical-debt.md#td-002--binding-named-parameters-in-postgresql-runs-a-query-first)).
  - On PostgreSQL and MySQL a statement with markers is not sent byte for byte
    as written: its markers are replaced by references.
  - On classic clusters values are written into the command as typed
    literals: an exception to ADR-0023's "never interpolate", limited to
    hexadecimal text and checked characters for the other kinds.
  - An inferred type can differ from what the column needs (`00123` reads as
    a number); the user then chooses Text.
  - The values typed in a SQL tab are stored in `config.db` with the tab. An
    older version drops them on its next save; the SQL stays.
- What becomes required or forbidden:
  - A SQL tab's request carries parameters only for markers that
    `jykre_sql::named_markers` finds.
  - On classic clusters, a value's text never reaches the command; a value
    with a character outside its kind's set is refused.
  - Parameter values are never logged nor kept in the history.

## How to validate

- Library tests: inferred types, a field's value or NULL, only the run
  statements' markers needing values, problems reported once per name, kept
  fields by name, and the tab state round trip.
- Driver tests: the literal of every kind and NULL, a hostile text that comes
  out as hexadecimal only, a number past 38 digits refused without sending,
  and a command body with the markers replaced and no value in it.
- An export run again binds the parameters its result ran with.
- A Databricks check with an authenticated CLI on a classic cluster running a
  statement with each kind, recorded apart (needs a workspace).
- A visual check: the form appears and disappears with the markers, Enter in
  a field runs, a missing or malformed value is outlined, NULL disables the
  field, and the values come back after a restart.
