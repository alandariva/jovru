# ADR-0025: Report panels, each with its SQL, its database and its chart, run on a session per database

- **Status:** Proposed
- **Date:** 2026-11-05
- **Authors:** AI agent, at the maintainer's request (plan approved on 2026-11-05)
- **Amends:** [ADR-0024](0024-reports.md) (a report is a set of panels, not
  one SQL and one chart; the stored format) and, for report tabs,
  [ADR-0014](0014-a-session-per-tab.md) (a session per database used, not
  one per tab)

## Context

A report of [ADR-0024](0024-reports.md) runs one SQL on one connection and
draws one chart. Users build reports that look at several things at once:
revenue by region next to the costs of the quarter, last year's sales from an
archive database, a single total. Those queries live in different databases
(schemas, catalogs) of the same connection, and each wants its own chart.

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
- Results are bounded (golden rule 8), a frame costs what is on screen
  (golden rule 7), and views decide nothing (golden rule 1).

## Decision

### Panels

- A report holds up to 12 **panels**, in order. Each panel has a stable
  identity within its report, a title, its SQL, its **context** (the
  database, schema or catalog of the report's connection it runs in; empty is
  the session default), how it shows its result (a table, bars, lines, a pie
  or a **number**, the first row's value of one column shown large), whether
  the table shows under its chart, and its width (half or the whole row).
- The `:name` **parameters are the report's**, shared by the panels: one form
  field per name, whatever panel uses it. A run checks only the parameters of
  the panels it runs.
- A parameter may offer a **list of choices** (at most 50): the form shows a
  menu, and a value must be one of them and valid for the parameter's kind.
  The choices are part of the definition; the values a run binds still never
  are.
- A panel is its own entity, embedded with `.cached`, so a panel's grid or
  chart redraws without laying the others out. Only one panel's definition
  (and SQL editor) is open at a time. The editor gets the connection's name
  index and the panel's context, so completion, hover and go to definition
  work as in a query tab.

### Sessions

- A report keeps **one session per context its panels use**. Panels of one
  context run one after the other on that session, in the report's order;
  different contexts run at the same time, **at most four at once**. A
  session only ever runs in its own context, so a panel never inherits
  another one's database.
- At most **four idle sessions** are kept between runs; the least recently
  used closes first, and a session whose context no panel uses any more
  closes when it is idle.
- The decisions (which panels run, how they group, which group starts, which
  outcome is stale, which sessions close) live in `jykre::reports::run`, a
  module without GPUI. The view only starts each group's loop and hands the
  outcomes to the panels.
- A run of a report is always **settled**: after each panel the session is
  asked its context and transaction even when the last result has more rows
  (a report does not page), so a `USE` in one panel's SQL cannot leak into
  the next panel of the same session.
- Each panel's result may take the memory ceiling of a query tab divided by
  the number of panels, so a report holds no more than one query tab.

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

- The `reports` setting goes to `"version": 2`. Each report holds
  `parameters` (with `choices`) and `panels` (`id`, `title`, `sql`, `context`
  as `{"catalog", "schema"}`, `chart`, `table`, `width`).
- A version 1 report (one top-level `sql` and `chart`) is read as one panel
  and written back as version 2; version 1 was never released. Keys a version
  does not know are kept at every level: library, report, parameter, panel.

### Output

- **Print HTML** shows every panel in a two-column grid like the report, each
  with its chart or number and, when asked, its table. The values the panels
  ran with appear once at the top; a panel run again with other values lists
  its own.
- **Copy** is per panel (CSV or for a spreadsheet).
- A query tab's SQL becomes a report with **Save as a new report**, or a new
  panel of a saved report of the same connection with **Add to a report…**;
  either keeps the tab's context. Nothing is stored until the report is saved.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| One session per report tab, switching the context before each panel (ADR-0014 as it is) | Fewest sessions | Panels run in sequence; a "session default" panel inherits the previous panel's database; a PostgreSQL session cannot change database | Wrong results and no other PostgreSQL database |
| One session per panel | Simplest scheduling; every panel independent | As many connections as panels (12) for every report, and as many execution contexts on a classic cluster | Too many sessions for what a report needs |
| Several connections per report (one per panel) | Any database of any server | Mixes credentials, row limits and production checks within one report | The user asked for databases of the same connection |
| A dashboard of independent saved queries | Reuses saved queries | No shared parameters; a second library of layouts | Reports already own parameters and output |
| Infer the PostgreSQL database switch in the application | No contract change | `match` on the driver kind outside the drivers (golden rule 3) | The driver knows how its sessions bind |

## Consequences

- Positive:
  - A report shows several results from several databases of one connection,
    in parallel, each with its own chart, and prints them together.
  - The report SQL editor completes names like a query tab.
  - Drivers that can switch database within a session need no change.
- Negative or costs:
  - A report may open up to four sessions at once (and keep four idle) where
    it opened one.
  - One more method in the `Driver` contract.
  - The stored format changes; version 1 is read but no longer written.
  - The report view coordinates panels, sessions and runs, which is more
    code than one SQL on one session.
- What becomes required or forbidden:
  - A report session runs only in the context it was opened for.
  - A panel's result is bounded by the row limit and its share of the memory
    ceiling.
  - Another PostgreSQL database is reached only with `allDatabases` and the
    profile's own credentials.
  - Report decisions stay in `jykre::reports::run`; the views only run them.

## How to validate

- Library tests: the version 1 to 2 reading, round trips with unknown keys at
  every level and repaired panel identities; parameters across panels, list
  choices and per-panel checking; the indicator's exact text; the
  scheduler's grouping, limit of four, joining a running group, stopping,
  removing and stale outcomes; the sessions' leases, least recently used
  closing and closing of unused contexts; the print HTML with several panels.
- Contract test: `session_connection` returns the connection by default.
- PostgreSQL: a unit test for the database switch and its refusal, and a live
  test that opens a session in another database and refuses a request for
  the home one.
- Backend test: a lost report session reopens in its context's database, and
  the session is asked its context even when the last result has more rows.
- A visual check: a report with panels in two schemas and another database,
  completion in a panel's SQL, Ctrl Enter in a panel and outside it, a list
  parameter, the indicator, half-width panels, Add to a report, and the
  keyboard and focus.
