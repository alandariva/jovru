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
│ Status bar: workspace · connections · local data · version             │
└────────────────────────────────────────────────────────────────────────┘
```

A connection's tab group appears with its first tab and goes away with its
last: the tab strip never shows a group without tabs, at start or later. A
report's tab has a group of its own (below).

The rail holds, from the top: Explorer, Saved queries, Reports, History and,
at the bottom, Preferences.

A **report** ([ADR-0024](adr/0024-reports.md),
[ADR-0025](adr/0025-report-panels.md)) opens in a tab of its own, in a tab
group of its own named after the report: the group offers no new tab, takes
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

Dividers are adjustable with the mouse and the arrow keys. The assistant panel (simulated today)
opens to the right of the query area, and says at its top that it is pending
implementation and its answers are simulated.

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
| Editor suggestions | The SQL editor | After `FROM` or `JOIN`, the relations of the tab's context (or of the schema written before the dot), its CTEs and namespaces; after `alias.`, the columns of that relation (inside a subquery, an alias of the enclosing query too); elsewhere, the columns of the relations of the query around the cursor: the statement, a subquery, a CTE body or one side of a `UNION`, never those of a subquery the cursor is not in. A CTE or a derived table offers the columns its `SELECT` names (renamed by a column list written after it; `*` brings the columns of what it reads), with the type of the column each one reads when that is a plain column. Without a qualifier, an expression also suggests the dialect's functions (with their signature and description; choosing one inserts `name(`) and keywords (`WHERE`, `CASE`), after the columns; after `::`, the dialect's types. Names that start exactly as typed come first. Keywords, functions and types come from the SQL catalogs, so they show even before the cache is read. Only the tab's own connection is suggested. Columns not read yet show "Loading columns…" and are read in the background; typing never waits. A name the dialect reserves (`order`, `user`) is inserted quoted. Hovering a column shows its type, nullability, key, default and comment (for a CTE's or a derived table's column, those of the column it reads), and hovering a function before its `(` its signatures, description and page; `Ctrl`-click on a relation or column shows it in the explorer |
| Structure changed by SQL | The explorer | A `CREATE`, `ALTER`, `DROP`, `RENAME` or `COMMENT` that ran queues a refresh of the namespaces it touched, when they are chosen to be introspected, about 300 ms later |
| Explorer expansion | The explorer | The nodes left expanded come back after a restart, read from the local cache; "Load more" after a node reads its next page only |
| What a node is | Explorer node | Icon by kind (table, view, materialized view, column, key column, foreign key, partition), the column's type or what a key references after the name, and a tooltip with nullability, primary key position, default and comment |
| Demo versus server data | Footer badge | `MOCK` or `SQL`; a tab without a connection outside the demo shows "No connection" and runs nothing |
| Run of a query tab | Before the tab's title in the tab bar | A spinner while a run is active (stopping included), then how the last run ended: a check (succeeded), a red cross (failed) or a ban sign (cancelled). Nothing before the first run (`RunState::tab_status`) |
| The tab's session | Editor header, at the right | **Connected** while the tab holds a session; a click (or Enter) disconnects: the session closes, results stay, and the next run opens a new one. With an open transaction it asks first, since closing rolls it back. While a run or a page read has the session out it shows **Connected** without the action, and **Disconnected** when there is no session. A tab without a connection shows "No connection" (or "Demo") |
| Transaction of the tab's session | Editor toolbar, for drivers with transactions | **Auto-commit**, **Transaction open** (or **Manual commit** after MySQL's `SET autocommit = 0`), **Uncommitted changes** (amber) or **Transaction failed** (red, PostgreSQL), with **Commit** and **Rollback** while one is open. Reported by the driver after each run, inferred from the statements the tab ran when it cannot tell (`jykre::transaction`) |
| Pinned result | Result tab | Its pin icon is mint; no run replaces it |
| Selection statistics | Footer, after the selected count | Sum, average, median, minimum and maximum of the numbers selected, exact; "Calculating…" while a large selection is read |
| Full value, statistics | Side panel of the grid; the toolbar's **Statistics** button opens it on its Statistics tab | The focused cell's full value (JSON laid out), or the statistics of the selection, of the rows the filter shows, or of one column (a column's line in the first two opens it); a spinner while they are computed |
| Records | Result grid | The selected rows side by side, one line per column; the toolbar's records button is highlighted |
| A connection's colour and environment | Explorer entry (a stripe on its left), its tab group and the editor header | The colour chosen in the connection dialog, or the driver's; a `DEV`, `STAGING` or `PROD` badge after the name |
| What ran in the tab | Messages | A log of every statement that ended, independent of the result tabs |
| Run duration | Footer, Messages, history | In the scale that reads best: `850 ms`, `12.4 s`, `3 min 3 s`, `1 h 2 min` |
| Filtered rows | Footer | While the filter is open, the rows it shows of the page: "3 of 1,000 rows" |
| Sorted column | Grid header | An arrow after the name; a click on a header sorts the page's rows by that column's values, ascending, then descending, then in the server's order. NULLs come last and numbers compare exactly |
| Saved query | Editor toolbar, library | The bookmark is filled while the tab holds the saved version. The first save asks a name (the tab's title to start with) and tags separated by commas; later saves only update the SQL, never remove. A saved query opens linked to its tab (an open one is shown again). In the library list its pencil renames it or changes its tags (an open tab takes the new name) and its bin removes it, after a confirmation |
| Tags of saved queries | Library, under the search | Each tag once, as `#tag`; choosing one lists only the queries with it, choosing it again lists them all. The search finds tags too, and `#word` asks for a tag that starts with `word` |
| Saved queries in the palette | **Ctrl+Shift+P** | Saved queries that match the text (name, tags, SQL, connection; `#tag` filters) are listed above the tables and views; Enter or a click opens the one highlighted. A search of tags alone lists no tables. Without text it lists at most 10 rows, and a search at most 50; when more matched, a line under the list says to type to search or to narrow it. In each row, the text typed (and each `#tag` filter, in the tags) is marked where it appears, ignoring case: brighter text on a faint mint wash (`MATCH_TEXT`, `MATCH_BG`) |
| History | The history list | Every statement that ended (succeeded, failed or cancelled), newest first: the start of its SQL, its outcome, its connection, how long ago and how long it took; a search filters by SQL, title or connection |
| Query parameters | Above a query tab's editor, under its header | One field per `:name` of the SQL (none without markers): the name, its type (**Automatic**, which shows the type it reads from the value, or one chosen) and **NULL**, which disables the field. A missing or malformed value of a statement being run is outlined in red with what is expected, and nothing runs. **Run again** binds the current values; an export that runs a statement again binds those its result ran with. The fields come back with the tab after a restart. The values are bound by the driver ([ADR-0026](adr/0026-named-parameters-in-sql-tabs.md)) |
| Saved report | Report toolbar, Reports list, tab group | The save icon is a filled bookmark while the tab holds the stored definition, and saving renames the report's tab group; the list shows each report's title, its connections (by name, or "N connections" past two), its number of panels and the start of its SQL, with its chart type as icon (a dashboard for several panels); a search filters by title, description, or a panel's title, connection or SQL |
| Report parameters | The report's form | One field per `:name` of any panel, labelled as the definition says (`*` when required) with a placeholder for its type; a list parameter shows a menu of its choices (and **No value** when optional). A missing, malformed or unlisted value of a panel that runs is outlined in red with what is expected, and nothing runs. The values are bound by the driver, never written into the SQL ([ADR-0023](adr/0023-named-parameters-bound-by-drivers.md)) |
| Run of a report | Report toolbar, panel headers, footer, tab bar | **Run** turns into **Stop** while any panel runs; a connection without its password asks for it first, one after another; each panel shows **Waiting** (for its connection and database's session or a free place: at most four run at once), a spinner while it runs, **Failed** or **Stopped**, and its error in place of its result (a panel whose connection is gone says so and the others run); the tab shows the worst outcome; a panel's footer shows its rows, its time and "Stopped at the row limit" when more rows exist, and a panel whose session was lost says it was opened again |
| Where a report panel runs | Panel header | A badge "connection · database" (just the connection for the session default), amber when the connection or database is no longer listed; while the report is edited, a menu of the connections with their namespaces, with a warning icon for one no longer listed |
| Report chart | Above a panel's table | Bars (one small chart per value column), lines (one per value column on a shared scale), a pie or a number (the first row's value of one column, large, as the database wrote it); at most 50 bars, 1,000 points or 10 slices plus "Other", and a line says how many rows it shows when it leaves some out. A chart that cannot be drawn says why (a column missing, a column without numbers, no rows). A new panel takes the chart its first result suggests |
| Connection test | Connection dialog footer | **Test connection** turns into **Cancel test** while it runs and shows the elapsed time after 2 s |
| Export of a result | The export bar, under the result toolbar | Its options and where the rows come from (the rows held, the rest from the server, or a statement run again); while writing, a spinner, the rows and bytes written (of the total when known), the elapsed time after 2 s and **Cancel export**; at the end, how many rows went to which file, its size and time, with **Show in folder**, or why nothing was written. The toolbar's export button stays highlighted while the bar is open ([Results](results.md#export)) |
| Plan of a statement | **Explain** in the editor toolbar | The dialect's `EXPLAIN` of the statement at the cursor (or of the one selected) runs into a new result tab; the statement itself does not run. Only queries and data changes are explained |

Rules:

- Anything that may take more than **300 ms** shows progress. Anything that
  may take more than **2 s** also shows the elapsed time and a way to cancel.
- No action leaves the UI without an answer: a click gives immediate visual
  feedback even when the result takes longer.
- Dangerous actions ask for confirmation: deleting a connection, closing a tab
  with a running query or an open transaction, copying more than 50 MiB.
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
- Moving a tab to another connection (dragging it to another group, or
  choosing a connection in the editor toolbar) asks first: its results and
  session belong to the previous connection, and an open transaction is
  rolled back. A report's tab never leaves its own group, and no other tab is
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
- Removing a saved query (its button in the library list) asks first; a tab
  that has it open keeps its SQL, which closing then asks about. The
  editor's save button only saves or updates.
- Copying a selection or a page estimated above 50 MiB asks first. So does
  copying a report panel's result.
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

## Keyboard shortcuts

| Action | Shortcut |
| --- | --- |
| Run the selection, or the statement at the cursor, in place of the result tab on screen (asks before cancelling while a run is active) | `Ctrl Enter` |
| The same, in a new result tab | `Ctrl Shift Enter` |
| Run every statement in the tab, one result tab each | `Ctrl Alt Enter` |
| Run the panel whose SQL has the keyboard; elsewhere in a report tab, every panel | `Ctrl Enter` (or `Enter` in a parameter field, which runs every panel) |
| Run every panel of the report on screen | `Ctrl Alt Enter` |
| In a query tab's parameter field: run what `Ctrl Enter` runs (the selection or the statement at the cursor) | `Enter` |
| Show the editor's suggestions at the cursor, also with nothing typed | `Ctrl Space` |
| Open or close the export bar of the result on screen | `Ctrl Shift E` |
| Search tables in the local cache | `Ctrl Shift P` |
| Search: choose a match, open it | `↑`, `↓`, `Enter` |
| New query on the active connection | `Ctrl N` |
| Close the query tab (asks when it runs or has unsaved SQL) | `Ctrl W`, `Ctrl F4`, or a middle click on the tab |
| Next or previous query tab, from anywhere in the window | `Ctrl Tab`, `Ctrl Shift Tab` (or `Ctrl Page Down`, `Ctrl Page Up`) |
| Show or hide the explorer | `Ctrl B` |
| Show or hide the assistant | `Ctrl Shift B` |
| SQL font size: larger, smaller, reset | `Ctrl +`, `Ctrl -`, `Ctrl 0` |
| Close the search or the connection dialog | `Esc` |
| Refresh the focused explorer node | `F5` |
| Delete the focused connection (asks first) | `Delete` |
| Explorer: previous, next, first, last node | `↑`, `↓`, `Home`, `End` |
| Explorer: expand or go to the first child, collapse or go to the parent | `→`, `←` |
| Explorer: new query for the table, `SELECT * … LIMIT` the row limit, not run; a tab already opened for the table is shown instead (**New query for table in another tab** in its menu opens another). On any other node, expand or collapse. A click only selects; a double click does the same as `Enter`, and the chevron expands or collapses | `Enter` |
| Grid: move, extend the selection | Arrows, `Shift` + arrows |
| Grid: first or last cell of the row, first or last cell of the page, a screen up or down (`Shift` extends) | `Home`, `End`, `Ctrl Home`, `Ctrl End`, `Page Up`, `Page Down` |
| Grid: select all, clear | `Ctrl A`, `Esc` |
| Grid: copy in the format of the copy menu, the same with the column names | `Ctrl C`, `Ctrl Shift C` |
| Grid: show the focused cell's full value in the side panel | `Enter` (or a double click) |
| Grid: close the side panel, then the records view, then clear the selection | `Esc` |
| Grid: select a column, extend to it | `Ctrl` + click, `Ctrl Shift` + click on its header |
| Result tabs: move the focused tab left or right | `Ctrl Shift ←`, `Ctrl Shift →` (or drag it) |
| Tab bar: move between tabs | Arrows, `Home`, `End` |

Shortcuts are GPUI actions in one keymap ([UI guidelines](ui-guidelines.md#actions-and-shortcuts)).
A new shortcut updates this table, avoids keys the editor already uses, and
works without the mouse.

## Error messages

Every error follows the same shape:

1. A **short human title** chosen from the `ErrorCode` (for example, "Could
   not sign in").
2. The **database's own message**, verbatim, in a monospaced font when there
   is one.
3. A **suggested action** when one exists ("Check the user and password",
   "Run `databricks auth login --profile DEV`", "Set a SQL Warehouse on the
   connection").
4. **Copy details** ([Reliability](reliability.md#diagnostics-for-users)).

When the database reports the position of a syntax error, the editor marks
it and moves the cursor there. Avoid generic messages ("Unknown error"),
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

- A dark theme with the colour tokens of `ui.rs`; result values use the
  colours in [Results](results.md#value-presentation).
- The SQL font size is adjustable (12 to 24 px) and remembered.
- Motion is reserved for work in progress (spinners). No decorative
  animation delays an action.

## Accessibility

Every essential flow works with the keyboard, focus is always visible, and
interactive elements expose a role, a label and their state. Details are in
[UI guidelines](ui-guidelines.md#accessibility-and-keyboard).

## Known deviations

| Standard | Current code |
| --- | --- |
| Syntax error position marked in the editor | PostgreSQL reports a position; MySQL and Databricks give it only inside the message |
| Elapsed time and cancel for work over 2 s | **Format** runs off the UI thread and the editor stays usable, but the first format per dialect (building its grammar) or a long script can take seconds, and it cannot be cancelled; a result for SQL edited meanwhile is dropped |
| Shortcuts as actions in one keymap | Only the three run shortcuts, closing a tab, moving to the next or previous tab and the export bar are actions ([UI guidelines](ui-guidelines.md#known-deviations)). There is no shortcut of its own for cancelling a run; the Run button (Stop) and the confirmation after a run shortcut cancel it |
