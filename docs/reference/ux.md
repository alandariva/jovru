# User experience

Jykre Studio should feel immediate: every action answers at once, every state
is visible, and the keyboard reaches everything.

## Layout

```text
┌────────────────────────────────────────────────────────────────────────┐
│ Title bar: workspace · search (Ctrl Shift P) · New query               │
├──┬──────────────┬──────────────────────────────────────────────────────┤
│▣ │ Explorer     │ Tab groups, per connection or report: [PG · Local]   │
│☆ │ ▾ connection │ Toolbar: Run · Explain · Format        save · panel  │
│▤ │   ▾ database │ Parameters (when the SQL has :name): :id [42] …      │
│  │              │ ┌──────────────────────────────────────────────────┐ │
│◷ │     ▸ schema │ │ SQL editor, status per statement in the gutter   │ │
│⚙ │              │ │                                                  │ │
│  │              │ └──────────────────────────────────────────────────┘ │
│  │              │ Results · Statistics · Messages ⚙·filter·copy ▾·CSV·⤓│
│  │              │ Export bar (when open): format, options, progress    │
│  │              │ ┌──────────────────────────────────────────────────┐ │
│  │              │ │ Result grid                                      │ │
│  │              │ └──────────────────────────────────────────────────┘ │
│  │              │ Footer: 1,000 rows · row limit reached · 18 ms       │
│  │              │ Notice bar (user actions: copied, saved, formatted)  │
├──┴──────────────┴──────────────────────────────────────────────────────┤
│ Status bar: connections · update · version                             │
└────────────────────────────────────────────────────────────────────────┘
```

A connection's tab group appears with its first tab and goes away with its
last: the tab strip never shows a group without tabs, at start or later. A
report's tab has a group of its own (below).

The rail holds, from the top: Explorer, Saved queries, Reports, Automation,
History and, at the bottom, Preferences.

**Preferences** is a dialog over the window (the rail's gear or `Ctrl ,`),
with its categories listed on the left: **Appearance** (the theme and
**Reset widths**), **SQL editor** (the SQL font size from 12 to 24 px, with
a preview in the editor's colours, and the SQL catalogs read), **Assistant**,
**Diagnostics** (the log folder and telemetry), **Shortcuts** and **About**
(the version, the data folder and **Updates**). `Esc` or its close button closes it and
gives the keyboard back to the tab; Tab stays inside while it is open.

**Updates** ([ADR-0033](../adr/0033-update-check-and-signed-self-install.md)):
**Check for updates automatically** (on until turned off) asks GitHub once a
day, never in the first 30 s after the window opens; **Check now** asks at
once. The section says how the last check ended, in one line. A newer
version (pre-releases included) shows as a badge in the status bar, before
the version: **Version 0.2.1 available**, then **Downloading update · 42 %**,
**Installing update…** and **Restart to update**; clicking it opens About,
and nothing else announces an update (a failed check stays in About). In
About, **Download and install** downloads, checks the signature and
installs, with progress and **Cancel** until the swap starts; then **Restart
now** closes the window with the same questions as closing it (files not
saved, open transactions) and starts the new version, while **Release
notes** opens the release's page. Where the copy cannot replace itself (a
development build, another system, a folder it cannot write to), About
says why and offers **Download manually**, which opens the release's page.
A failure says what failed and that nothing was installed.

The **theme** is **System** until the user chooses: it follows the
operating system's light or dark appearance, also when it changes while the
application runs. **Light** or **Dark** stay chosen across restarts, and
choosing **System** again follows the system once more. The choice applies
at once to every part of the window.

A **report** ([ADR-0024](../adr/0024-reports.md),
[ADR-0025](../adr/0025-report-panels.md)) opens in a tab of its own, in a tab
group of its own named after the report and drawn without a colour (grey),
since it belongs to no connection: the group offers no new tab, takes
no other tab and goes away with the report's tab, since the report's panels
may run on several connections. The tab has a toolbar (title, **Edit**,
**Add a panel**, save, **Export for printing or PDF…**, **Run**), the
definition while editing (description and one row per `:name` parameter the
panels share, with its label, type, default, whether it is required and its
**List of choices**), the parameter form, its **panels** in two columns and a
footer that counts the panels, those running and those that failed. Each
panel has a header (title, where it runs as "connection · database", its
state, run or stop, the edit button while the report is edited, and a menu to
copy its result, move it, make it half or full width, duplicate or remove
it), its chart, number or table, and a footer with its rows, time and **Show
the SQL that ran**. While the report is edited, the header starts with a grip
that drags the panel onto another to take its place (or onto **Drop here to
move the panel to the end**, shown while dragging), and where it runs is a
menu: one submenu per connection, each with **Session default** and the
connection's namespaces. **Add a panel** asks the same way. One panel's
definition is open at a time, at full width and scrolled into view: its
title, its SQL, with the same completion, hover and go to definition as a
query tab for the panel's connection, and **Show as** (table, bars, lines,
pie or number), the column that labels a chart, its values and **Table too**.
A report is created with **+** in the Reports section or from a query tab's
report menu: **Save as a new report** takes the selection, the statement at
the cursor or the whole script, and **Add to a report…** adds it as a new
panel of any saved report, chosen in a dialog with a search; both keep the
tab's connection and database. Nothing is stored until **Save report**.
Report tabs are not reopened after a restart.

**Automation** ([specification](../plans/automation-engine-specification.md),
section 8.1; [ADR-0034](../adr/0034-automation-database-service-and-lock.md) to
[ADR-0038](../adr/0038-external-program-runs.md)) is a section of the rail.
It lists the jobs and chains (kind icon, name and kind, or for a scheduled
job its next run in the schedule's zone, **Suspended** in amber or
**Schedule off**, virtualized), with a
**+** menu (**New SQL Job**, **New program**, **New chain**) and a **Runs**
entry, with the number of runs active, that opens the global runs list. A
job opens in a tab of its own, in a grey tab group of its own named after
the job, as a report's does; the tab's title ends with `●` while the job is
not saved. Its toolbar holds the kind, save, **Publish** (not for a chain),
**Duplicate**, **Delete** and **Run**, which turns into **Stop** while the
job's run is active; under it, three inner tabs, and a fourth,
**Schedule**, once the job is saved:

- **Editor**, a form. A SQL Job: name, description, the connection and where
  unqualified names resolve (a menu of the connections, each with
  **Session default** and its namespaces, as report panels offer), the SQL
  editor with the same completion as a query tab (statements beyond the
  first are outlined in amber and named by their line before saving), one
  row per `:name` parameter (label, type, default, **Required**, **List of
  choices**), the timeout in seconds and the row limit with the effective
  timeout, the declared outputs (name, type, result column by position),
  which row the outputs take with what each rule does with zero, one and
  several rows, **Store outputs**, and the published versions (number and
  date, each with a delete button). A program: the program, its arguments
  one per row, the entrypoint, the working folder, the environment
  (variables removed, those set, and which of Studio's variables are
  removed now), the protocol (**No protocol** or `jjp/1`; inputs and
  outputs only with `jjp/1`), the timeout, outputs, and the versions, each
  with the path and SHA-256 it expects. A chain: its inputs, its timeout
  (3,600 s by default) and 2 to 16 steps, each a job (SQL Jobs and
  programs; a chain is never offered) and one of its published versions,
  marked when the job changed since that version, with moves up and down;
  each input of the version takes its source from a list (its default or
  NULL, a chain input, a declared output of an earlier step, or a constant
  typed beside it). What `chain_problems` finds is said under its step and
  input, read again after each change and once typing pauses.
- **Inputs**: the run form, as a report's (a list input shows a menu),
  **Run** or **Stop**, then the run's state and, for a SQL Job, the latest
  result in a grid while the tab is open. A job that differs from what is
  saved runs as a draft, and the form says so.
- **Runs**: the job's history (below).
- **Schedule** ([specification](../plans/automation-engine-specification.md),
  section 9.5; [ADR-0041](../adr/0041-schedules-and-grants.md)): the
  schedule's state (off, on with its **next run** in its zone, abbreviation
  and offset and this computer's time when it differs, or **Suspended** with
  the reason and **Confirm again**), its last outcome, its missed count and
  last missed time, then the form: the published version it runs (not for a
  chain), **At times of day** (times `HH:MM`, every day, days of the week or
  days of the month with `last`, and months) or **Every N minutes** (with the
  note that an interval counts absolute time), the zone as an IANA name
  (default the system's), the rule in one sentence and the next three times
  with what daylight saving does to each; the missed-time policy (**Do not
  run them**, **Run once for all of them**, **Run up to N of the most
  recent** with N from 2 to 24) and the overlap policy (**Skip it and record
  it**, **Wait for the run to end**, **Run beside it**), each with what it
  does; **Notify me when a scheduled run ends badly**; and **Save the
  schedule**, which opens the **consent dialog**. **Turn on**, **Turn off**
  and **Delete the schedule** follow its state. The tab says that schedules
  run only while Jykre Studio is open, in the window holding automation (a
  second window says the schedule runs in the other one), and a job with a
  required input without a default says why it cannot be scheduled.
  The consent dialog states what runs, the rule and the next times, that it
  runs only while the application is open, both policies, each connection
  with its environment label and a warning when it does not remember its
  password, each Production step's risks (it is the production
  confirmation), each program's file and hash, and the defaults; confirming
  again first says why it was suspended and which connections changed.

The rules the engine follows are stated where they apply (section 8.1.1):
the limits and the queue beside the timeout, one statement per job, the
4 KiB, 7-day and 90-day limits of stored outputs, the 7-day and 1 MiB
limits of program logs and not to print secrets, that stdin is reserved
for `jjp/1`'s token, what the environment loses (`ENV_DENYLIST`, the job's
own names) and that anything else in it reaches the program, that
external code runs with the user's privileges and is not sandboxed, that
only the entrypoint is checked by hash, how Stop ends a program (and that
on Windows a program without `jjp/1` is killed after the 5 s), and that a
chain runs published versions, never nests and stops at the first step
that fails. Shortcuts that run SQL in a query tab never run a job.

The history (a job's **Runs**, and the global **Runs** list) lists runs and
chain runs newest first, virtualized and read 50 at a time
(**Load older runs**), each with its state icon, job, kind, how long ago and
how long it took (a run without a job reads as a draft or a deleted job);
a scheduled run or chain run has a **Scheduled** or **Scheduled, catch-up**
badge and its due time in the schedule's zone.
Choosing one shows what ran and how it ended: its state and cause in words,
**Timed out after …** apart from **Stopped**, `effect_unknown`
("The change may or may not have been applied; it was not run again"), the
rows kept and the rows read and discarded or left unread, the rows
affected, SQLite's one writer per file, the server's code, a program's
exit code or signal and `fail` code, whether its outputs were stored and
why not (their names, sizes and whether a value expired; never the
values), and a program run's logs: **stdout**, **stderr** and **jjp** with
their sizes, read from disk only when opened, with the 7-day and 1 MiB
note and **Delete logs**. A chain run lists each step with its version and
its run, which opens under it, and **Not run** for the steps after the one
that stopped it, naming that step and the cause; **Run this step again…**
opens the step's job with its form filled from what earlier steps stored
(constants, and outputs whose values were stored and have not expired)
and says which inputs are left to fill in; it never runs by itself. The
history says that refused runs are not recorded and how long runs, values
and logs are kept. The global list has a **State** filter and, above the
history, the runs and chain runs active or queued now, each with its state
and **Stop**.

Dividers are adjustable with the mouse and the arrow keys. The assistant panel opens to the
right of the query area ([ADR-0028](../adr/0028-agentic-ai-assistant-over-the-metadata-cache.md)):

- **Telemetry** in Preferences › Diagnostics: **Send logs** and **Send
  traces** (both off by default), the protocol (HTTP/protobuf or gRPC), the
  collector's address and headers for its authentication (kept in the
  system vault and never shown again). **Save** applies at once; turning a
  signal off stops capturing it immediately. **Test** sends one span and
  says how the collector answered, with a way to cancel. A note lists what
  is sent and never sent ([ADR-0029](../adr/0029-otlp-export-of-logs-and-traces.md)).
- **Set up** in Preferences › Assistant: provider (Anthropic, OpenAI,
  OpenAI-compatible, Ollama or off), model, address, API key (kept in the
  system vault and never shown again), when to review requests, whether
  the model may ask to run queries (**Ask me each time** or **Off**), and
  prices per million tokens for the costs shown. **Test** asks the model a tiny
  question with one tool declared. Until it is set up, the panel offers to
  open Preferences.
- **Fix with AI**, next to **Copy details** on a failed statement, sends the
  statement and the server's error. **Ask** sends a question, with the row
  counts of the tab's last result; **Write SQL** sends a description, with
  the selection or the statement at the cursor.
- Each request appears as a card (**Request N**: model, estimated tokens,
  the estimated tokens that are new, and cost) with its messages, tools and
  JSON. Messages show what is new since the last request sent; a **…** row
  says what is the same (the system prompt, N messages) and shows everything
  when chosen. When the review mode asks, it
  waits with **Send** and **Cancel**, saying nothing has been sent yet.
  Each tool the model calls is a step showing its arguments and the answer
  the next request carries. After 300 ms a round shows **Thinking…**, after
  2 s its elapsed time, and **Stop** (or the composer's stop button)
  cancels it. After 8 rounds the request stops until **Continue**.
- When the model asks to run a query, a card says why (its purpose), shows
  the statement, marks one that changes or deletes data and a production
  connection, and says where it runs: **Decline** or **Run** (**Run on
  production** for a risky statement there). It runs in the assistant's own
  session, with **Running…** and **Stop**. Then the card shows a toggle per
  column (focusable buttons that wrap, so every column is reachable in a
  narrow panel), up to 50 rows and, below,
  the exact text the model would receive, updated as columns are hidden;
  it says nothing of the result has been sent: **Don't send** or **Send
  result**. A decided card folds to one line (sent, with rows and hidden
  columns; ran, not sent; declined) that opens on the statement and what
  the model was told. Columns hidden once start hidden in the next result
  of the conversation.
- Answers are plain text; each SQL block has Copy, **Insert at the cursor**
  and, for a fix, **Replace the failed statement**, which replaces it only
  where it still is unchanged, as one undo step. A block that changes or
  deletes every row is marked. Nothing runs without the user's approval.
- Each answer shows its tokens (input, output, cached), cost and time.
  Below the header, the **Session** summary shows three tiles (spent in the
  session, tokens in the context, the share of input from the provider's
  cache with a bar) and a line with rounds, input and output, and cache
  hits and misses; its chevron folds it to one line (spent · tokens ·
  cache), and it stays as left. The gear (**Assistant options**) says what
  is sent and holds **Compact the context**: **Prune long tool results**
  and **Summarize with the model** (a request reviewed like any other),
  available when nothing is on its way. Each compaction shows in the
  conversation with the context's estimated size before and after, and
  that the next request starts the provider's cache again. A request the
  provider rejects suggests compacting. **Copy the whole trace** exports
  every request, answer and tool step as JSON. **New conversation** starts
  over; choosing another connection does too. A failure shows inline with
  **Try again**, never in a dialog.
- The demo (`JYKRE_DEMO`) answers with canned text marked **Simulated**.

## Always-visible states

Databricks connections show their selected warehouse/classic cluster name and
ID in the explorer and restore that selection immediately in the connection
dialog. **Load compute** explicitly refreshes a searchable, virtualized select,
shows loading time and can be cancelled. Refreshing never changes the selection
silently. An unavailable saved resource remains labelled until changed by the
user. Discovery and connection testing never start classic compute.
The CLI profile is a searchable select as well. **Load CLI profiles** refreshes
the names/hosts using the CLI without signing in to all profiles. Its saved
selection survives failures; choosing a new profile clears the compute selection.
New connection forms show a prompt until profiles are loaded. Initial selection
honours the CLI's declared default instead of assuming the name `DEFAULT`.

The connection dialog keeps its title, the driver and a footer with the
status, **Test connection** and **Save connection** in view, and switches
three sections between them: **Connection** (name, server, credentials, TLS
or the Databricks profile and compute), **Metadata** (what to introspect;
its tab shows how much is chosen, amber when nothing is) and **Options**
(row limit, colour and environment). Only the section scrolls, and the dialog keeps its height when
switching. A field that fails validation on save opens its section.

In **Metadata**, **Load list** reads the
namespaces of the server typed in the form, a tree with three-state boxes
(chosen, partly chosen, not chosen) opens one level at a time, a filter
narrows it, and **Everything, including what is created later** chooses all.
A namespace a saved choice names but the server no longer has shows
**Not found**. Saving without a choice is allowed; nothing is read until the
user chooses. When the connection settings changed after the list was loaded,
a new choice cannot be saved until the list is loaded again, so a choice made
for another server is never saved. For PostgreSQL, **Connection** also takes
the search path sessions start with and whether to offer the server's other
databases to introspect.

Each state has one consistent representation.

| State | Where | Representation |
| --- | --- | --- |
| Query running | Footer, results area, Run button | Spinner, "Running…" and, after 2 s, how long it has run ("Running… 12.4 s", its tooltip names the statement timeout); the Run button turns into Stop |
| Statement status | Editor gutter | Waiting, running, success, error or cancelled per statement; only the statement on the server shows as running |
| Row limit reached | Footer | Amber warning with the limit in its tooltip |
| Run cancelling | Footer | Spinner and "Cancelling…" until the driver stops; after 5 s "Still cancelling…" with the time waited, and its tooltip says the driver gives up and closes the session. The previous result stays and a new run may start once it ends |
| Run cancelled / failed | Footer (and the results area for errors) | Cancel or error icon with a short text |
| Metadata refreshing | The node being refreshed | Its `↻` spins; clicking it cancels |
| Metadata refresh waiting | The node (or the connection) whose refresh was asked for while another runs | "queued" after its name; it starts when the running one ends, in order. Asking again (F5, the menu or the connection's `↻`) takes it out of the queue |
| Reading the chosen metadata | Below the connection in the explorer | A bar with "Reading n of total: name", the percentage and the time left (estimated after the first namespace); the connection's `↻` spins and cancels |
| Nothing chosen to introspect | The connection in the explorer | **Choose what to introspect** under its name, which opens the connection settings |
| A connection in the explorer | Its line | Its name, colour and environment, with **Configure connection** and **Refresh** at the right edge; where it connects (host, or profile and compute) is its tooltip. Under it only what needs attention or is running: a warning, the last failure (until a refresh succeeds), a read's progress, **Choose what to introspect**, or that nothing is saved yet. How an action ended (saved, refreshed, taken out of the queue) goes to the notice above the connections |
| Metadata state per scope | Explorer node and its tooltip | Last success time, failure message, permission warnings |
| Age of a chosen namespace | After the namespace's name in the explorer | How long ago it was last read in full ("3 days ago"), as of the moment it is drawn |
| A tab's context | The editor header, after the connection | Where unqualified names resolve: the schema (PostgreSQL), database (MySQL) or catalog and schema (Databricks), as the session reports it after each run ("Session default" before the first). Choosing another from the namespaces in the cache applies it from the next run; it comes back with the tab after a restart and is applied again to a session that reopened |
| No access | After a node's name in the explorer | "no access" when the last refresh could not read it for lack of permission; its last good read stays and its menu says when it was taken |
| Inspection | **Inspect** in a relation's or a schema's menu | A panel with indexes, constraints and triggers (relations) or functions, procedures with their parameters, sequences and types (schemas), read from the server when asked; `Esc` closes it |
| Definition | **Show definition** in a relation's menu | A new tab with the view's source or the table's DDL, read from the server |
| Revalidation | Connection dialog, **Options** | Hours after which opening the application reads the chosen namespaces again, once; empty or 0 never does |
| Editor suggestions | The SQL editor | After `FROM` or `JOIN`, the relations of the tab's context (or of the namespace written before the dot), its CTEs and namespaces: a catalog or database first, and the schemas below it once the context gives it or the cache holds only one (without a catalog chosen, a Databricks connection offers its catalogs, and `catalog.` its schemas). A level the tab's context does not give yet (no run reported it) matches any name of the cache, so `sales.` still finds the `sales` schema; after `alias.`, the columns of that relation (inside a subquery, an alias of the enclosing query too); elsewhere, the columns of the relations of the query around the cursor: the statement, a subquery, a CTE body or one side of a `UNION`, never those of a subquery the cursor is not in. A CTE or a derived table offers the columns its `SELECT` names (renamed by a column list written after it; `*` brings the columns of what it reads), with the type of the column each one reads when that is a plain column. Without a qualifier, an expression also suggests the dialect's functions (with their signature and description; choosing one inserts `name(`) and keywords (`WHERE`, `CASE`), after the columns; after `::`, the dialect's types. Names that start exactly as typed come first. Keywords, functions and types come from the SQL catalogs, so they show even before the cache is read. Only the tab's own connection is suggested. Columns not read yet show "Loading columns…" and are read in the background; typing never waits. A name the dialect reserves (`order`, `user`) is inserted quoted. Hovering a column shows its type, nullability, key, default and comment (for a CTE's or a derived table's column, those of the column it reads), and hovering a function before its `(` its signatures, description and page; `Ctrl`-click on a relation or column shows it in the explorer |
| Structure changed by SQL | The explorer | A `CREATE`, `ALTER`, `DROP`, `RENAME` or `COMMENT` that ran queues a refresh of the namespaces it touched, when they are chosen to be introspected, about 300 ms later |
| Quick search in the explorer | A field over the tree, opened by typing while the tree has the keyboard | The text typed, how many shown nodes hold it (amber "No matches"), **Highlight** or **Filter**, and a button that closes it. It searches only the nodes shown, in their order, ignoring case and without reading the cache: the selection stays on a node that still matches, otherwise goes to the next match, scrolled into view. The part of each name that matches is marked (`MATCH_TEXT`, `MATCH_BG`), as the explorer search's text is. **Filter** hides the nodes that do not match, keeping their ancestors and what is expanded under a match. The keyboard stays on the tree, so the arrows move the selection without changing the text and typing goes on after `Enter` opens a table. Closing it shows the whole tree again with the node found selected; the mode is kept for the next search |
| Explorer search | The field above the connections | Searches the whole local cache and shows the matches with their ancestors until cleared, also while other sections are shown. What is expanded does not change; clearing it gives back the selection from before when the node chosen meanwhile is not in the tree |
| Explorer favorites | **Favorites**, above the connections, shown while there is one | Connections, tables and views added with **Add to favorites** in their explorer menu (the context menu, also `Shift F10`), in the order added, at most six rows high before the list scrolls (virtualized). Each row shows the kind, the name and its connection and namespace; Enter, Space or a click opens a table or view as **View data** does and expands and scrolls to a connection; its `×` (or **Remove from favorites** in the explorer menu) removes it. A favorite whose connection was deleted, or whose object the connection's cache no longer holds, is greyed with an amber **Unavailable** whose tooltip says why; it is never pointed at another object. Nothing is read from a server to list them |
| Explorer expansion | The explorer | The nodes left expanded come back after a restart, read from the local cache; "Load more" after a node reads its next page only |
| What a node is | Explorer node | Icon by kind (table, view, materialized view, column, key column, foreign key, partition), the column's type or what a key references after the name, and a tooltip with nullability, primary key position, default and comment |
| Demo versus server data | Footer badge | `MOCK` or `SQL`; a tab without a connection outside the demo shows "No connection" and runs nothing |
| Run of a query tab | Before the tab's title in the tab bar | A spinner while a run is active (stopping included), then how the last run ended: a check (succeeded), a red cross (failed) or a ban sign (cancelled). Nothing before the first run (`RunState::tab_status`) |
| The tab's session | Editor header, at the right | **Connected** while the tab holds a session; a click (or Enter) disconnects: the session closes, results stay, and the next run opens a new one. With an open transaction it asks first, since closing rolls it back. While a run or a page read has the session out it shows **Connected** without the action, and **Disconnected** when there is no session. Beside **Connected**, a lock when the server says the session is encrypted (its tooltip names the TLS version) or an amber **Not encrypted** warning, whose tooltip says the SQL, results and password can be read on the network and to require TLS in the connection's settings ([ADR-0032](../adr/0032-session-encryption-reported-by-drivers.md)); a SQLite file, or a driver that cannot tell, shows neither. A tab without a connection shows "No connection" (or "Demo") |
| Transaction of the tab's session | Editor toolbar, for drivers with transactions | **Auto-commit**, **Transaction open** (or **Manual commit** after MySQL's `SET autocommit = 0`), **Uncommitted changes** (amber) or **Transaction failed** (red, PostgreSQL), with **Commit** and **Rollback** while one is open. Reported by the driver after each run from the session's real state (PostgreSQL, MySQL, SQLite), including a transaction opened in a procedure or rolled back by the server; whether a MySQL or SQLite transaction changed data, and Databricks, are inferred from the statements the tab ran (`jykre::transaction`) |
| Pinned result | Result tab | Its pin icon is mint; no run replaces it |
| Selection statistics | Footer, after the selected count | Sum, average, median, minimum and maximum of the numbers selected, exact; "Calculating…" while a large selection is read |
| Full value, statistics | Side panel of the grid; the toolbar's **Statistics** button opens it on its Statistics tab | The focused cell's full value (JSON laid out), or the statistics of the selection, of the rows the filter shows, or of one column (a column's line in the first two opens it), with small charts: values against NULLs, a number column's distribution and the most frequent values' shares; a spinner while they are computed |
| Records | Result grid | The selected rows side by side, one line per column; the toolbar's records button is highlighted |
| A connection's colour and environment | Explorer entry (a stripe on its left), its tab group and the editor header | The colour chosen in the connection dialog, or the driver's; a `DEV`, `STAGING` or `PROD` badge after the name |
| What ran in the tab | Messages | A log of every statement that ended, independent of the result tabs |
| Run duration | Footer, Messages, history | In the scale that reads best: `850 ms`, `12.4 s`, `3 min 3 s`, `1 h 2 min` |
| Filtered rows | Footer | While the filter is open, the rows it shows of the page: "3 of 1,000 rows". The grid filters once typing pauses for 150 ms, not on every keystroke |
| Format | Editor toolbar | While a Format runs, its button reads **Cancel format** (with the time it has run after 2 s) and stops it: the formatter ends before its next statement and the SQL is left as it was |
| Explorer search | The search field over the connections | A spinner at the end of the field while the search reads the local caches, off the UI thread (one or two characters scan the whole name index) |
| Sorted column | Grid header | An arrow after the name; a click on a header sorts the page's rows by that column's values, ascending, then descending, then in the server's order. NULLs come last and numbers compare exactly |
| Saved query (a `.sql` file, [ADR-0031](../adr/0031-saved-queries-as-sql-files.md)) | Editor toolbar, library, tab strip | **Save** (`Ctrl S`; a filled save icon while the tab holds what its file holds) writes the tab's file (the tab says **Saving to …** until it is written: syncing to disk can take seconds on a busy disk); a tab without one, and **Save as…** (`Ctrl Shift S`), ask where in the system dialog, starting in the library's folder with the tab's title as the name (`.sql` added when no extension is typed). The title is the file's name; a file tab with unsaved edits shows `●` after it. Before writing, a file changed on disk offers **Overwrite**, **Reload from disk** or **Save as…**, and a missing file **Locate…** or **Save as…**; a failed save keeps the tab's SQL, unsaved. The library lists the `.sql` files of its folder and subfolders (a file in a subfolder shows that folder before its connection; hidden folders skipped; at most 10,000 files, 8 deep), read again when it is shown, when the window comes back, after a save and with its refresh button; **Open…** (`Ctrl O`, or the library's folder button) opens any file. Its pencil renames the file (never over another; when the name has to be made safe, the dialog shows the file name it will get before the rename) and edits its tags, which stay on this computer; its folder button shows the file in the system's file manager; a file no longer found is marked and offers **Locate…** or **Forget**. Saved queries of an earlier version not moved to files yet are listed apart, with **Move to files** |
| Tags of saved queries | Library, under the search | Each tag once, as `#tag`; choosing one lists only the queries with it, choosing it again lists them all. The search finds tags too, and `#word` asks for a tag that starts with `word` |
| Saved queries in the palette | **Ctrl+Shift+P** | Saved queries that match the text (name, tags, SQL, connection; `#tag` filters) are listed above the tables and views; Enter or a click opens the one highlighted. A search of tags alone lists no tables. Without text it lists at most 10 rows, and a search at most 50; when more matched, a line under the list says to type to search or to narrow it. In each row, the text typed (and each `#tag` filter, in the tags) is marked where it appears, ignoring case: brighter text on a faint mint wash (`MATCH_TEXT`, `MATCH_BG`) |
| History | The history list | Every statement that ended (succeeded, failed or cancelled), newest first: the start of its SQL, its outcome, its connection, how long ago and how long it took; a search filters by SQL, title or connection |
| Long lines of a large script | The SQL editor | Lines wrap at the editor's width, except in a tab whose text exceeds 256 KiB: there long lines scroll sideways, so a large file opens without freezing the window. Wrapping comes back once the text is 192 KiB or less ([Large `.sql` files in the editor](../plans/editor-large-files.md)) |
| Query parameters | Above a query tab's editor, under its header | One field per `:name` of the SQL (none without markers): the name, its type (**Automatic**, which shows the type it reads from the value, or one chosen) and **NULL**, which disables the field. A missing or malformed value of a statement being run is outlined in red with what is expected, and nothing runs. **Run again** binds the current values; an export that runs a statement again binds those its result ran with. The fields come back with the tab after a restart. The values are bound by the driver ([ADR-0026](../adr/0026-named-parameters-in-sql-tabs.md)) |
| Saved report | Report toolbar, Reports list, tab group | The save icon is a filled bookmark while the tab holds the stored definition, and saving renames the report's tab group; the list shows each report's title, its connections (by name, or "N connections" past two), its number of panels and the start of its SQL, with its chart type as icon (a dashboard for several panels); a search filters by title, description, or a panel's title, connection or SQL |
| Report parameters | The report's form | One field per `:name` of any panel, labelled as the definition says (`*` when required) with a placeholder for its type; a list parameter shows a menu of its choices (and **No value** when optional). A missing, malformed or unlisted value of a panel that runs is outlined in red with what is expected, and nothing runs. The values are bound by the driver, never written into the SQL ([ADR-0023](../adr/0023-named-parameters-bound-by-drivers.md)) |
| Run of a report | Report toolbar, panel headers, footer, tab bar | **Run** turns into **Stop** while any panel runs; a connection without its password asks for it first, one after another; each panel shows **Waiting** (for its connection and database's session or a free place: at most four run at once), a spinner while it runs, **Failed** or **Stopped**, and its error in place of its result (a panel whose connection is gone says so and the others run); the tab shows the worst outcome; a panel's footer shows its rows, its time and "Stopped at the row limit" when more rows exist, and a panel whose session was lost says it was opened again |
| Where a report panel runs | Panel header | A badge "connection · database" (just the connection for the session default), amber when the connection or database is no longer listed; while the report is edited, a menu of the connections with their namespaces, with a warning icon for one no longer listed |
| Report chart | Above a panel's table | Bars (one small chart per value column), lines (one per value column on a shared scale), a pie or numbers (the first row's value of up to eight columns, large and side by side, two to a line in a half-width panel and four in a whole one, as the database wrote them; while editing, each may turn green or red when it meets a comparison with a target number, red winning when both hold, with a check or cross mark beside its name); at most 50 bars, 1,000 points or 10 slices plus "Other", and a line says how many rows it shows when it leaves some out. A chart that cannot be drawn says why (a column missing, a column without numbers, no rows). A new panel takes the chart its first result suggests (numbers for a single row of up to four number columns) |
| Connection test | Connection dialog footer | **Test connection** turns into **Cancel test** while it runs and shows the elapsed time after 2 s |
| Export of a result | The export bar, under the result toolbar | Its options and where the rows come from (the rows held, the rest from the server, or a statement run again); while writing, a spinner, the rows and bytes written (of the total when known), the elapsed time after 2 s and **Cancel export**; at the end, how many rows went to which file, its size and time, with **Show in folder**, or why nothing was written. The toolbar's export button stays highlighted while the bar is open ([Results](results.md#export)) |
| Copy of a grid selection, a query page, the inspector's value or a report panel's result | Status over the grid after 100 ms; for a report panel, in its notice line | **Preparing copy…** while estimating; **Copying…** with rows completed and total while encoding, elapsed time after 2 s, and **Cancel copy** (also `Esc`). Above 50 MiB, a confirmation comes before encoding. The clipboard changes only after a successful, still-current copy |
| Data of a table | A query tab opened for the table (a double click or `Enter` in the explorer, **View data** in its menu, `Enter` in the search) | The editor holds `SELECT * FROM` the table, without a `LIMIT`, and the tab reads it at once into a result tab called **Data**: the first page has the connection's row limit and the footer's arrows read the next ones from the session, never running the statement again. Opening the table again shows that tab, and reads the rows again only when none of its result tabs holds them: in place of a read that failed or was cancelled, otherwise in a new result tab, never over another result (a restored tab offers **View data**). Running something else in the tab closes the open result, as for any query; `↻` reads the table again |
| Automation available | The Automation section | Nothing while it works. **Starting automation…** while the engine opens its file; **Read-only** when another Jykre Studio holds the data folder's automation lock (its jobs and history can be read, nothing runs or is saved), with **Try again**, also tried each time the section opens; **Automation is off** when `automation.db` comes from a newer version or the file or the lock cannot be opened, the rest of the application working. At start, what recovery found is said once: runs marked interrupted, and a damaged file set aside (job definitions lost, also in the explorer's notice) |
| A job's run | The job's Inputs, toolbar and tab | **Queued, place N**: which limit it waits for (2 runs at once, 1 per connection), the runs that hold it and the time left before it leaves the queue (redrawn each second); then its phase (opening a session, running the statement, reading the rest of a write's result, the program running with its percent and stage), rows read, **Stopping…**, and how it ended: state and cause, `effect_unknown`, the database's message, a transaction rolled back, rows kept and discarded or left unread, whether the program ended on its own after Stop or was killed, outputs stored or why not. A run that did not start says why in place, with that refused runs are not recorded; a program refused because its file changed names it and offers **Publish**. A chain shows the step running ("step 2 of 3") with that step's state. The tab shows a spinner, then a check, a cross or a ban sign, as a query tab's |
| Active runs | The Automation section's **Runs** entry | "N active" while runs or chains are active or queued |
| A schedule | The job's **Schedule** tab, the sidebar's job entry | Off, on with its next run in the schedule's zone (abbreviation, offset, and this computer's time when it differs), or **Suspended** with the reason and **Confirm again**; the last outcome (started, a catch-up started, waiting for the run still going, skipped, missed, refused with the reason, suspended) and how many times were missed, with the last |
| Scheduled runs that ended badly | The rail's Automation icon and the status bar | A red count of scheduled runs that failed or did not run since the Automation section or **Runs** was last opened; the status bar names the latest (the job, the outcome and the cause) and opens its job. A schedule that asked for it also shows an operating-system notification with the same words, never a value or a server message |
| Plan of a statement | **Explain** in the editor toolbar | The dialect's `EXPLAIN` of the statement at the cursor (or of the one selected) runs into a new result tab; the statement itself does not run. Only queries and data changes are explained |

Rules:

- Anything that may take more than **300 ms** shows progress. Anything that
  may take more than **2 s** also shows the elapsed time and a way to cancel.
- No action leaves the UI without an answer: a click gives immediate visual
  feedback even when the result takes longer.
- Dangerous actions ask for confirmation: deleting a connection, closing a tab
  with a running query or an open transaction, closing the application with
  open transactions, copying more than 50 MiB.
- Work is never committed or rolled back without the user being told:
  - A run whose tab holds an open transaction does not start when the
    session no longer answers: the server already rolled the transaction
    back, and a new session would commit each statement on its own. The
    notice bar says so and that nothing ran; the next run opens a new
    session.
  - When a run, a page read or an export loses the session while a
    transaction was open, the notice bar says the server rolled it back.
  - A report panel whose SQL leaves a transaction open (or failed, or
    MySQL's autocommit off) has its session closed right after it, which
    rolls the transaction back, and the panel says so: reports keep no
    transactions between runs or panels.
- A run shortcut never cancels by itself. Pressing `Ctrl Enter`,
  `Ctrl Shift Enter` or `Ctrl Alt Enter` while a run is active asks "Cancel
  the running query?" (for `Ctrl Alt Enter`, "Stop the whole run?") with
  **Yes, cancel** focused, so Enter confirms. The Run button, which shows
  **Stop** meanwhile, cancels on click.
- With the cursor on no statement (a blank or comment line, before the first
  or after the last), `Ctrl Enter` runs nothing and says to put the cursor on
  the statement or select it. On a statement's first line before its text,
  or its last line after its `;`, it runs that statement.
- A connection whose password is not remembered asks for it in a prompt when
  a run or a refresh opens a session. The password stays in memory until the
  application closes, or goes to the vault with **Remember password**.

## Feedback

- **The footer reports how a run ended**, once: rows, rows affected, row limit,
  cancellation or error, and the elapsed time. Run outcomes do not also appear
  in the notice bar.
- **The notice bar reports the outcome of user actions**: copy, save, format,
  connection changes. It can be dismissed.
- Errors of a run fill the results area with the message; they never open a
  modal dialog.

## Confirmations

- Closing a tab with a running query asks first: closing cancels the query.
  Closing one with an open transaction asks too: closing rolls it back.
  Closing one whose SQL is not in the saved queries asks too: closing loses it.
- Closing the application (the title bar's X, `Alt F4`, the window manager)
  while query tabs hold open transactions asks first, naming how many:
  closing rolls them back (**Close and roll back** or **Keep open**).
- Moving a tab to another connection (dragging it to another group, or
  choosing a connection in the editor toolbar) asks first: its results and
  session belong to the previous connection, and an open transaction is
  rolled back. A tab without a connection moves without asking: it has no
  session or results to lose. A report's tab never leaves its own group, and no other tab is
  dropped into it.
- Saving changed connection settings asks first when a tab of that
  connection runs a query or holds an open transaction: applying them cancels
  the query and closes the session. Otherwise tabs keep their results and
  session, say that the settings changed, and the next run opens a session
  with the new settings.
- A saved query or history entry whose connection no longer exists opens
  without a connection, never on another one, and says so. Running a tab
  without a connection offers the connections in the results area.
- Deleting a connection (**Delete connection…** in its menu, or `Delete` on
  it) asks first and says what goes with it: its remembered password and
  its metadata cache. Its open tabs keep their SQL and results in their
  group, now without a connection, and say so; a query running in them is
  cancelled and an open transaction rolled back, which the question names.
- Closing a tab with unsaved SQL (a file tab's edits, or a draft) offers
  **Save** (it closes once written), **Don't save** or **Cancel**, after the
  questions about a running query, a transaction or an export. Closing the
  application asks once about the file tabs with unsaved edits (**Save all**,
  **Don't save**, **Keep open**); drafts are not asked about, since they
  reopen at the next start. Removing a saved query of an earlier version
  (its button in the library list) asks first.
- Copying a selection, a page or the inspector's value estimated above
  50 MiB asks first. So does copying a report panel's result.
- Exporting a result the tab no longer holds whole runs its statement again
  only after asking (**Run and export**), and only for a statement that
  only reads. Closing a tab while it exports asks first: closing cancels the
  export, which leaves no file. An export that reads from the session counts
  as a running query when the tab is moved or its connection saved.
- Closing a report tab asks first when it is running (closing stops it) or
  its definition is not saved (closing loses it). Removing a saved report
  (its button in the Reports list) asks first; a tab that has it open keeps
  it as an unsaved draft.
- **Export for printing or PDF…** asks where to save the document; nothing is
  written until a path is chosen, and the file opens in the browser, which
  prints it or saves it as PDF.
- On a connection marked **Production**, running a statement that changes
  every row of a table (`UPDATE` or `DELETE` without a `WHERE` of its own) or
  drops data (`DROP`, `TRUNCATE`, an `ALTER` that drops something) asks
  first, naming each such statement of the run (`jykre::guard`). Nothing
  runs until the user confirms.
- Writing a result's edits (**Write…**) always asks first, in one dialog
  that lists every change, the statements with their placeholders, whether
  they are committed or stay in the open transaction, the table's triggers
  and cascades, and that the last write wins. On a production connection
  the same dialog warns so and its button reads **Write to production**.
  Editing a result whose rest is still open asks first, since reading the
  table closes it. Discarding pending edits asks first. Running a statement
  while edits are pending asks first (**Run and lose N pending changes?**),
  and so does closing the tab ([Results](results.md#editing)).
- Running a SQL Job goes: the form's values (a missing or malformed value
  is outlined in red and nothing runs), the password of its connection when
  it is not remembered, then, on a production connection, one question
  naming its risky statement and, when the job stores its outputs, that
  every run stores them for 7 days. A program's first run at its revision
  warns that external code runs with the user's privileges, is not
  sandboxed, and that its logs stay on disk for 7 days.
- Running a chain goes: the chain's form, the password of every connection
  its SQL steps use, one after another, then **one** dialog for the whole
  plan: every step with its job, version and kind, where each SQL step runs
  (and whether on production, with its risky statement), whether the job
  changed since that version, and the trusted-code warning for each program
  version that has not run before. Exactly that plan runs.
- Turning **Store outputs** on for a job whose connection is marked
  **Production** asks first, stating the limits. Deleting a job or a
  published version asks first; the engine refuses it while a chain uses it,
  and the editor names the chains. Closing a job's tab with unsaved changes
  asks first; a run it started keeps going, since runs belong to the engine.
- Closing the application while runs or chains are active or queued asks
  last, after the questions about files and transactions: it names them and
  says they will be stopped and the queue emptied (**Stop the runs and
  close** or **Keep open**). Confirming stops them and waits for the engine
  (up to about 11 s; the status bar says so) before the window closes.
- At start, when the previous session left programs running (Unix), a
  dialog lists them and offers **End them** (each process group whose
  leader still has its recorded start time is killed) or **Leave them**.

## Keyboard shortcuts

The keys below are the defaults. **Preferences › Shortcuts** changes them
([below](#shortcuts-the-user-chooses)), except the keys that type or move
inside one focused control, marked *fixed*.

| Action | Default shortcut |
| --- | --- |
| Run the selection, or the statement at the cursor, in place of the result tab on screen (asks before cancelling while a run is active) | `Ctrl Enter` |
| The same, in a new result tab | `Ctrl Shift Enter` |
| Run every statement in the tab, one result tab each | `Ctrl Alt Enter` |
| Stop the run of the tab on screen, as Stop does (a query tab's statements, a report's panels, or a job's or chain's run, which leaves the queue when it waits), without asking | `Ctrl F2` |
| In a job's run form (**Inputs**): run the job with the values typed, as **Run** does. `Ctrl Enter` never runs a job | `Enter` (fixed) |
| Run the panel whose SQL has the keyboard; elsewhere in a report tab, every panel | `Ctrl Enter` (or `Enter` in a parameter field, which runs every panel; fixed) |
| Run every panel of the report on screen | `Ctrl Alt Enter` |
| In a query tab's parameter field: run what `Ctrl Enter` runs (the selection or the statement at the cursor) | `Enter` (fixed) |
| Show the editor's suggestions at the cursor, also with nothing typed | `Ctrl Space` |
| SQL editor (a query tab's, a report panel's or a SQL Job's): comment the selected lines, or the cursor's line, with `--`, or take the comment out when all of them are comments (one step for `Ctrl Z`) | `Ctrl /` |
| SQL editor, while it has the keyboard: search the SQL in the editor's search bar, which starts from the selected text; the matches are marked in the text | `Ctrl F` |
| SQL editor: the same search bar with its replace field (replace the current match or all of them; `Tab` moves between the two fields, `Ctrl H` in the bar shows or hides the replace field) | `Ctrl H` |
| SQL editor's search bar: the next or previous match, close it and give the keyboard back to the editor | `Enter`, `Shift Enter`, `Esc` (fixed) |
| SQL editor: go to a line, typed as `12` or with a column as `12:5` (or `12,5`) in a small field over the editor's top right corner (it closes the editor's search bar, which sits there); `Enter` puts the cursor there, scrolled into view, and gives the keyboard back to the editor; a column past the line's end stops at its end; a line past the text goes to the start of the last line and the notice bar says so; text that is not a line is named under the field. `Esc` closes the field and gives the keyboard back to the editor; a click elsewhere closes it | `Ctrl G` |
| SQL editor: add a cursor on the line above or below; what is typed then goes to every cursor, and `Esc` leaves a single one | `Ctrl Alt ↑`, `Ctrl Alt ↓` on Windows; `Shift Alt ↑`, `Shift Alt ↓` on Linux |
| Open or close the export bar of the result on screen | `Ctrl Shift E` |
| Search tables in the local cache | `Ctrl Shift P` |
| Search: choose a match, open it | `↑`, `↓`, `Enter` (fixed) |
| New query on the active connection | `Ctrl N` |
| New query tab on the connection of the query tab on screen; without one (no tab, a report or a tab without a connection), a tab without a connection whose results area offers the connections to choose from | `Ctrl T` |
| Open or close Preferences | `Ctrl ,` |
| Close the query tab (asks when it runs or has unsaved SQL) | `Ctrl W`, `Ctrl F4`, or a middle click on the tab |
| Open `.sql` files | `Ctrl O` |
| Save the query tab to its file, or to a new one | `Ctrl S` |
| Save the query tab to a file chosen now | `Ctrl Shift S` |
| Reopen the query tab closed last in this session, then the one before, up to 20 (also the title bar's reopen button, shown while there is one): its SQL, title, connection, context, timeout and `:name` fields, never its results, session or transaction; nothing runs. A tab whose connection is gone reopens without one and says what to choose | `Ctrl Shift T` |
| Next or previous query tab, from anywhere in the window | `Ctrl Tab`, `Ctrl Shift Tab` (or `Ctrl Page Down`, `Ctrl Page Up`) |
| Show or hide the explorer | `Ctrl B` |
| Show or hide the assistant | `Ctrl Shift B` |
| SQL font size: larger, smaller, reset | `Ctrl +`, `Ctrl -`, `Ctrl 0` |
| Close what is on top: the password prompt, a dialog, Preferences, the inspector, the search, the connection dialog. The key then goes on to what has the keyboard, so the grid and a report panel still close their own layers with `Esc` | `Esc` |
| Refresh the focused explorer node | `F5` |
| Delete the focused connection (asks first) | `Delete` |
| Explorer: previous, next, first, last node | `↑`, `↓`, `Home`, `End` |
| Explorer: expand or go to the first child, collapse or go to the parent | `→`, `←` |
| Explorer: the selected node's context menu | `Shift F10` |
| Explorer: quick search, typing a name while the tree has the keyboard: the next or previous match (wrapping; every node while nothing is typed; the explorer's next and previous node keys), open the selected match as `Enter` does, take back a character, close | Letters, `↓`, `↑`, `Enter`, `Backspace`, `Esc` (fixed, but the arrows) |
| Explorer, while the tree has the keyboard: the quick search marks its matches or shows only them (opens it when closed) | `Ctrl F` |
| Explorer search field: clear the text, then give the keyboard to the tree; go to the tree | `Esc`, `↓` (fixed) |
| Explorer: the table's Data view, which reads its rows at once (`SELECT * FROM` it, without a `LIMIT`, page by page); a Data view already open for the table is shown instead (**New query for table** in its menu opens another tab with `SELECT * … LIMIT` the row limit, not run). On any other node, expand or collapse. A click only selects; a double click does the same as `Enter`, and the chevron expands or collapses | `Enter` (fixed: it activates the node) |
| Grid: move, extend the selection | Arrows, `Shift` + arrows |
| Grid: first or last cell of the row, first or last cell of the page, a screen up or down (`Shift` extends) | `Home`, `End`, `Ctrl Home`, `Ctrl End`, `Page Up`, `Page Down` |
| Grid: select all, clear | `Ctrl A`; `Esc` (fixed) |
| Grid: copy in the format of the copy menu, the same with the column names | `Ctrl C`, `Ctrl Shift C` |
| Grid or report panel: cancel a pending copy, before closing the inspector/records view or clearing the selection | `Esc` (fixed) |
| Grid: show the focused cell's full value in the side panel | `Enter` (or a double click) |
| Grid of a query tab's editable result ([Results](results.md#editing)): edit the focused cell in a field over it (also the result toolbar's pencil); `Enter` keeps what was typed as pending, `Esc` drops it | `F2`; `Enter`, `Esc` in the field (fixed) |
| Grid of an editable result: set the selected cells to NULL | `Ctrl Shift N` |
| Grid of an editable result: add a row to insert | `Alt Insert` |
| Grid of an editable result: mark the selected rows for deletion, or take the mark away (an inserted row goes) | `Delete` |
| Grid of an editable result: write the pending edits after one confirmation (**Write…**), or generate their statements into a new tab (**Generate SQL…**) | `Ctrl Alt S` |
| Grid of an editable result: discard the pending edits, asking first | `Ctrl Alt Z` |
| Grid: close the field of the cell being edited, then the side panel, then the records view, then clear the selection | `Esc` (fixed) |
| Grid: select a column, extend to it | `Ctrl` + click, `Ctrl Shift` + click on its header |
| Grid (a query tab's, a report panel's or a job's): the menu on the focused header's column, or the focused cell's, to hide, freeze at the left or move the column, show hidden columns or reset the layout. With the mouse, a header dragged onto another takes its place | `Shift F10` |
| Find a column by name, ignoring case and accents, in the grid that has the keyboard, else in the query tab's result on screen (also the result toolbar's button); while its text is there, the records view lists only the columns it finds | `Ctrl Shift F` |
| Column search field: choose a match, go to it (showing it when hidden), close the field | `↑`, `↓`, `Enter`, `Esc` (fixed) |
| Result tabs: move the focused tab left or right | `Ctrl Shift ←`, `Ctrl Shift →` (or drag it) |
| Tab bar: move between tabs | Arrows, `Home`, `End` (fixed) |
| A job's tab: reach **Editor**, **Inputs** or **Runs**, open it (the keyboard goes to its first field, or the history) | `Tab`, then `Enter` or `Space` (fixed) |

Shortcuts are GPUI actions in one keymap ([UI guidelines](../standards/ui-guidelines.md#actions-and-shortcuts)).
A new shortcut updates this table, avoids keys the editor already uses, and
works without the mouse.

By default `Ctrl F` acts where the keyboard is: in a SQL editor it opens
that editor's search, on the explorer's tree it marks or filters the quick
search, and elsewhere (the result grid, a text field, a dialog) it does
nothing. Search, replace and multiple cursors belong to the editor component
(`EditorState` of `gpui-base`), which binds their keys itself, in every SQL
editor alike: a query tab, a report panel and a SQL Job's editor.

### Shortcuts the user chooses

**Preferences › Shortcuts** lists every action whose keys can be changed,
by where it works (window, running SQL, SQL editor, moving and editing
text, explorer, results, result grid), with the keys it has now:

- **Change** listens for the next keys pressed, with the keyboard alone;
  `Esc` cancels, so `Esc` itself is given back with **Reset**. One
  keystroke, with any modifiers; sequences are not offered. The keys
  chosen replace all the action's default keys (`Ctrl W` and `Ctrl F4`
  become the one key chosen).
- A key that another action already has where this one works (the window
  reaches every place; a SQL editor is a text field) is named first:
  **Take it** takes it from them, **Cancel** keeps everything. A typing key
  of the text fields (Backspace, Delete, Enter, Esc, Tab) cannot be taken.
  Nothing is bound twice without saying so.
- **Reset** gives one action its default keys back, **Reset all shortcuts**
  every action.
- A change applies at once, without a restart, and is saved in
  `config.db` ([Persistence](persistence.md#schema-configdb)). Tooltips,
  menus, the editor's hint and the notices name the key in use.
- The editor component's own keys (moving, selecting, undo, copy and
  paste, indenting) work in every text field, so changing one changes it in
  the fields too. The keys marked *fixed* in the table are not offered: they
  type or move inside one focused control (a field's `Enter`, a list's
  arrows, the tab bar, `Tab`) or close a layer of the grid or a report
  panel with `Esc` after the window's own `Esc`.
- A stored choice for an action this version does not offer, or one that
  cannot be read, is set back to the default and said once, in the window's
  notice, never a failure to start.

## Error messages

Every error follows the same shape:

1. A **short human title** chosen from the `ErrorCode` (for example, "Could
   not sign in").
2. The **database's own message**, verbatim, in a monospaced font when there
   is one.
3. A **suggested action** when one exists ("Check the user and password",
   "Run `databricks auth login --profile DEV`", "Set a SQL Warehouse on the
   connection").
4. **Copy details** ([Reliability](../standards/reliability.md#diagnostics-for-users)).

When the database reports the position of an error, the editor marks it
and moves the cursor there, and the error names its line and column in the
statement. PostgreSQL sends the position itself. MySQL and Databricks write
it only into the message, so their drivers read it from there: MySQL's
syntax errors quote the statement from the token they stopped at (`near
'…' at line n`), found in the statement on that line; Databricks gives a
line and a character (`line 1, pos 9`, `line 1 pos 14`, or a query context
`== SQL(line 1, position 15) ==`, but not a view's). A statement whose
`:name` parameters the driver wrote into its text before sending it (MySQL,
a Databricks cluster) is not marked, since the position would be in a text
the user did not write. Avoid generic messages ("Unknown error"),
stack traces and modal dialogs for query errors.

## Text and terminology

- English is the source language; every string goes through `tr!`, and
  catalogs live in `locales/` (pt-BR today). The locale comes from
  `JYKRE_LOCALE` or the OS.
- Use the same words everywhere: **Connection**, **Query** (a tab),
  **Run**, **Cancel**, **Refresh**, **Explorer**, **Results**, **Messages**,
  **Row limit**, **Timeout**, **Remember password**.
- Values from the database are shown as the database returned them; only the
  optional **pt-BR numbers** presentation changes number formatting.

## Theme and motion

- A light and a dark theme with the same colour tokens of `ui.rs`
  ([UI guidelines](../standards/ui-guidelines.md#colours-and-theme)); result values use the
  colours in [Results](results.md#value-presentation).
- The SQL font size is adjustable (12 to 24 px) and remembered.
- Motion is reserved for work in progress (spinners). No decorative
  animation delays an action.

## Accessibility

Every essential flow works with the keyboard, focus is always visible, and
interactive elements expose a role, a label and their state. Details are in
[UI guidelines](../standards/ui-guidelines.md#accessibility-and-keyboard).

## Known deviations

| Standard | Current code |
| --- | --- |
| Error position marked in the editor | PostgreSQL reports a position; MySQL and Databricks give it only inside the message, and their drivers read it from there (since 0.3.0 A9). MySQL quotes only syntax errors, so its other errors (an unknown column, a missing table) are not marked. Not marked either: a MySQL or Databricks cluster statement with `:name` parameters, whose text the driver changed. PostgreSQL's position with `:name` parameters is in the text sent, with each marker replaced, so it lands off by the difference in length when a parameter comes before the error |
