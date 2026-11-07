# ADR-0025: Report panels, each with its connection, database, SQL and chart, run on a session per connection and database

- **Status:** Proposed
- **Date:** 2026-11-05 (revised the same day, before acceptance: panels may
  run on different connections, and a report tab has a group of its own;
  revised on 2026-11-07, at the maintainer's request: a number panel shows
  several numbers, each optionally green or red by a condition)
- **Authors:** AI agent, at the maintainer's request (plans approved on
  2026-11-05)
- **Amends:** [ADR-0024](0024-reports.md) (a report is a set of panels, not
  one SQL and one chart on one connection; its tab sits in a group of its
  own; the stored format) and, for report tabs,
  [ADR-0014](0014-a-session-per-tab.md) (a session per connection and
  database used, not one per tab)

## Context

A report of [ADR-0024](0024-reports.md) runs one SQL on one connection and
draws one chart. Users build reports that look at several things at once:
revenue by region next to the costs of the quarter, last year's sales from an
archive database or another server, a single total. Those queries live in
different databases (schemas, catalogs), sometimes on different connections,
and each wants its own chart.

What constrains the change:

- Where unqualified names resolve is a `SessionContext` carried by each
  request (`ExecutionOptions::context`). MySQL changes database with `USE` and
  Databricks keeps the catalog and schema itself, but a PostgreSQL session is
  bound to the database it connected to: a request naming another one fails.
- A session applies the requested context only when it differs from the one
  it last applied or reported, so a session shared by panels of different
  databases would leave a panel on "session default" in whatever database the
  previous panel used.
- Every session is a connection (PostgreSQL, MySQL) or an execution context
  (classic Databricks clusters), so the number of sessions a report opens
  matters.
- Tabs are grouped by connection, and a tab moved to another connection's
  group runs on that connection. A report on several connections belongs to
  none of those groups.
- Results are bounded (golden rule 8), a frame costs what is on screen
  (golden rule 7), and views decide nothing (golden rule 1).

## Decision

### Panels

- A report holds up to 12 **panels**, in order. Each panel has a stable
  identity within its report, a title, its **connection**, its SQL, its
  **context** (the database, schema or catalog of that connection it runs in;
  empty is the session default), how it shows its result (a table, bars,
  lines, a pie or **numbers**, the first row's value of up to eight columns
  shown large, side by side), whether the table shows under its chart, and
  its width (half or the whole row). Panels of one report may run on
  different connections.
- Each number may turn **green** or **red** when it meets a condition the
  user sets: a comparison (`>`, `>=`, `<`, `<=`, `=`, `<>`) with a target
  number, one condition per colour, both optional. The comparison is exact
  (golden rule 9); NULL, text or a target that is not a number never
  colour; when both hold, red wins so an alert is never hidden. A coloured
  number also shows a mark, so colour is not its only sign.
- While the report is edited, a panel's header has a menu of the connections,
  each with a submenu of its databases, to choose where it runs, and a grip
  that drags the panel onto another to take its place (or after the last
  one). **Move up** and **Move down** stay in the panel's menu for the
  keyboard. **Add a panel** asks for the connection and database the same
  way.
- The `:name` **parameters are the report's**, shared by the panels: one form
  field per name, whatever panel uses it, each panel's markers read by its
  connection's dialect. A run checks only the parameters of the panels it
  runs.
- A parameter may offer a **list of choices** (at most 50): the form shows a
  menu, and a value must be one of them and valid for the parameter's kind.
  The choices are part of the definition; the values a run binds still never
  are.
- A panel is its own entity, embedded with `.cached`, so a panel's grid or
  chart redraws without laying the others out. Only one panel's definition
  (and SQL editor) is open at a time. The editor gets the name index of the
  panel's connection and the panel's context, so completion, hover and go to
  definition work as in a query tab.

### Tabs

- A report tab has a **tab group of its own**, named after the report and
  renamed when it is saved. The group offers no new tab, no other tab is
  dropped into it, and the report tab is not moved out of it; the group goes
  away with the tab.

### Sessions

- A report keeps **one session per connection and context its panels use**.
  Panels of one connection and context run one after the other on that
  session, in the report's order; others run at the same time, **at most four
  at once**. A session only ever runs in its own connection and context, so a
  panel never inherits another one's database.
- At most **four idle sessions** are kept between runs; the least recently
  used closes first, and a session no panel uses any more closes when it is
  idle. A connection whose settings change, or that is removed, stops its
  panels and closes its sessions; a new password keeps them.
- Each connection that needs a password asks for it before the run; a panel
  whose connection cannot run fails alone, saying why. On production
  connections, the statements of each are checked and one confirmation names
  them all.
- The decisions (which panels run, how they group, which group starts, which
  outcome is stale, which sessions close) live in `jykre::reports::run`, a
  module without GPUI. The view only starts each group's loop and hands the
  outcomes to the panels.
- A run of a report is always **settled**: after each panel the session is
  asked its context and transaction even when the last result has more rows
  (a report does not page), so a `USE` in one panel's SQL cannot leak into
  the next panel of the same session.
- Each panel's result is bounded by its connection's row limit and may take
  the memory ceiling of a query tab divided by the number of panels, so a
  report holds no more than one query tab.

### Another database of a PostgreSQL server

- The `Driver` contract gains `session_connection(connection, context)`: the
  connection a session opens with so that names resolve in `context` from its
  first statement. The default returns the connection unchanged.
- PostgreSQL returns, for a context in another database of the server, the
  same connection with that database, only when the profile offers the
  server's other databases (`allDatabases`); otherwise a validation error. The
  user, the credentials and TLS are the profile's.
- The backend opens and reopens a report's session with that connection, so a
  session lost and reopened stays in its database.

### Storage

- The `reports` setting goes to `"version": 3`. Each report holds
  `parameters` (with `choices`) and `panels` (`id`, `title`, `connection` as
  `{"id", "name"}`, `sql`, `context` as `{"catalog", "schema"}`, `chart`,
  `table`, `width`). A number panel's `chart` may hold `colors`, by column:
  `{"<column>": {"green": {"op": ">=", "value": "100"}, "red": {…}}}`,
  written only when some number has a condition; a version that does not
  know it reads the panel without colours.
- A version 1 report (one top-level `sql` and `chart`) is read as one panel,
  and the report-level `connection` of versions 1 and 2 is given to each of
  its panels; they are written back as version 3. Neither was released. Keys
  a version does not know are kept at every level: library, report,
  parameter, panel.

### Output

- **Print HTML** names the connections and shows every panel in a two-column
  grid like the report, each with its connection and database, its chart or
  numbers (in their colours, with their marks) and, when asked, its table. The values the panels ran with appear
  once at the top; a panel run again with other values lists its own.
- **Copy** is per panel (CSV or for a spreadsheet).
- A query tab's SQL becomes a report with **Save as a new report**, or a new
  panel of any saved report with **Add to a report…**; either keeps the tab's
  connection and context. Nothing is stored until the report is saved.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| One connection per report (the first version of this ADR) | One password, one production check, the tab stays in its connection's group | A report cannot put an archive server next to the live one | The user needs reports across connections |
| One session per report tab, switching the context before each panel (ADR-0014 as it is) | Fewest sessions | Panels run in sequence; a "session default" panel inherits the previous panel's database; a PostgreSQL session cannot change database; one session cannot span connections | Wrong results and no other database or connection |
| One session per panel | Simplest scheduling; every panel independent | As many connections as panels (12) for every report, and as many execution contexts on a classic cluster | Too many sessions for what a report needs |
| All report tabs in one "Reports" group | One group in the strip whatever the number of reports | A busy group mixing unrelated reports | The maintainer chose a group per report |
| A dashboard of independent saved queries | Reuses saved queries | No shared parameters; a second library of layouts | Reports already own parameters and output |
| Infer the PostgreSQL database switch in the application | No contract change | `match` on the driver kind outside the drivers (golden rule 3) | The driver knows how its sessions bind |

## Consequences

- Positive:
  - A report shows several results from several databases and connections,
    in parallel, each with its own chart, and prints them together.
  - The report SQL editor completes names like a query tab, for each panel's
    connection.
  - Drivers that can switch database within a session need no change.
- Negative or costs:
  - A report may open up to four sessions at once (and keep four idle) where
    it opened one, on several connections.
  - A run may ask for several passwords, one connection after another.
  - One more method in the `Driver` contract, and a `GroupedTabs` option to
    hide a group's new-tab button.
  - The stored format changes; versions 1 and 2 are read but no longer
    written.
  - The report view coordinates panels, connections, sessions and runs,
    which is more code than one SQL on one session.
- What becomes required or forbidden:
  - A report session runs only in the connection and context it was opened
    for.
  - A panel's result is bounded by its connection's row limit and its share
    of the memory ceiling.
  - Another PostgreSQL database is reached only with `allDatabases` and the
    profile's own credentials.
  - A report tab stays in its own group.
  - Report decisions stay in `jykre::reports::run`; the views only run them.

## How to validate

- Library tests: the version 1 and 2 reading (the connection given to each
  panel), round trips with unknown keys at every level and repaired panel
  identities; the connections of a report; parameters across panels in their
  dialects, list choices and per-panel checking; the numbers' exact text and their colours, compared exactly, red winning;
  where a dropped panel lands; the scheduler's grouping by connection and
  context, limit of four, joining a running group, stopping a panel or a
  connection, removing and stale outcomes; the sessions' leases, least
  recently used closing, closing of unused or changed connections; the print
  HTML with several panels and connections; a tab group without a new-tab
  button.
- Contract test: `session_connection` returns the connection by default.
- PostgreSQL: a unit test for the database switch and its refusal, and a live
  test that opens a session in another database and refuses a request for
  the home one.
- Backend test: a lost report session reopens in its context's database, and
  the session is asked its context even when the last result has more rows.
- A visual check: a report with panels on two connections and in several
  databases, the source menu and **Add a panel**, completion in a panel's
  SQL for its connection, a password per connection, dragging panels (onto
  another and to the end), the report's own tab group, Add to a report from
  another connection's tab, and the keyboard and focus.
