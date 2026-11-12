# User experience: layout

Part of [User experience](README.md).

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
last: the tab strip never shows a group without tabs, at start or later. Tabs
that belong to no connection share one grey group per kind: **Reports**,
**Automation** and **MCP** (below).

The rail holds, from the top: Explorer, Saved queries, Reports, Automation,
History and, at the bottom, Preferences.

**Preferences** is a dialog over the window (the rail's gear or `Ctrl ,`),
with its categories listed on the left: **Appearance** (the theme and
**Reset widths**), **SQL editor** (the SQL font size from 12 to 24 px, with
a preview in the editor's colours, and the SQL catalogs read), **Assistant**,
**Diagnostics** (the log folder and telemetry), **Shortcuts** and **About**
(the version, the data folder and **Updates**). `Esc` or its close button closes it and
gives the keyboard back to the tab; Tab stays inside while it is open.

**Updates** ([D-0033](../../decisions/security.md#d-0033)):
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
choosing **System** again follows the system once more. A theme file in a
themes directory appears as another choice. The choice applies at once to
every part of the window. How a file is written and where it is placed is
in [Themes](../themes.md).

**Import from file…**, in the explorer's menu of a table or of the namespace
that holds tables (a schema, or a MySQL database), on connections whose driver has transactions,
opens a dialog over the window ([Results](../results/transfer.md#import)): **Choose
file…**, then the file's options (CSV: delimiter, encoding, header row,
empty fields; XLSX: sheet, header row), the table (a searchable list of the
connection's cached tables, the one the menu was opened on chosen) with
**New table from this file…**, one row per table column with the file
column that fills it (**Match by name** or **Match by position**,
**Required** marked) and the preview. Reading shows its progress after
300 ms, then its elapsed time and **Cancel** past 2 s. What stops the
mapping is listed above the footer, which holds the time limit (15 min,
1 h by default, 4 h, no limit), **Cancel** and **Import**. **Import** asks
for the connection's password when it is not in memory and, on a
connection marked **Production**, asks first, naming the file, the table
and the connection. While it runs, the footer shows the rows written and
the share of the file read after 300 ms, then the elapsed time and
**Cancel import** past 2 s; closing the dialog or `Esc` cancels it too.
Then it says, in words, what the table holds: every row imported, nothing
imported (with the reason, a rejected value naming its line), or that the
commit's answer was lost. That outcome goes as soon as the file, its
options, the sheet, the table or the mapping change: it was about another
import. `Esc` (or the key chosen to close a dialog) closes it; Tab stays inside while it is open.

A **report** ([D-0024](../../decisions/results.md#d-0024),
[D-0025](../../decisions/results.md#d-0025)) opens in a tab of its own, in the **Reports** tab group, drawn without a colour
(grey), since it belongs to no connection: every report tab shares the group,
which offers no new tab, takes no other kind of tab and goes away with its
last tab, since a report's panels may run on several connections. The tab has a toolbar (title, **Edit**,
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

**Automation** ([specification](../../plans/automation-engine-specification.md),
section 8.1; [D-0034](../../decisions/automation.md#d-0034) to
[D-0038](../../decisions/automation.md#d-0038)) is a section of the rail.
It lists the jobs and chains (kind icon, name and kind, or for a scheduled
job its next run in the schedule's zone, **Suspended** in amber or
**Schedule off**, virtualized), with a
**+** menu (**New SQL Job**, **New program**, **New chain**) and a **Runs**
entry, with the number of runs active, that opens the global runs list. A
job opens in a tab of its own, in the grey **Automation** tab group that the
**Runs** tab shares, as reports share theirs; the tab's title ends with `●` while the job is
not saved. Its toolbar holds the kind, **Save** (**Saved**, muted, when nothing changed), **Publish** (not for a chain),
**Duplicate**, **Delete** and **Run**, which turns into **Stop** while the
job's run is active; under it, three inner tabs, and a fourth,
**Schedule**, once the job is saved:

- **Editor**, a form that opens on what a job needs first. Its other
  sections (**Limits**, **Declared outputs**, **Published versions** and,
  for a program, **Entrypoint and working folder**, **Environment**,
  **Protocol** and **Logs, stdin and Stop**) fold to their title and one
  line saying what they hold (`logic::Section`, the summaries beside it):
  a section opens unfolded when the job sets something in it, and its
  header is a button that `Enter` or `Space` folds and unfolds. The rules
  of section 8.1.1 stay inside the section they apply to. A SQL Job: name, description, a **Connection**
  section of its own (a menu of the connections and, once one is chosen, a
  **Database** menu with **Session default** and its namespaces, where
  unqualified names resolve; a job without a connection, or whose connection
  was deleted or cannot run, says so under the menu before **Run** refuses
  it; a new job takes the only saved connection, and none when there are
  several to choose from), the SQL
  editor with the same completion as a query tab (statements beyond the
  first are outlined in amber and named by their line before saving), one
  row per `:name` parameter (label, type, default, **Required**, **List of
  choices**), the timeout in seconds and the row limit with the effective
  timeout, the declared outputs (name, type, result column by position),
  which row the outputs take with what each rule does with zero, one and
  several rows, **Store outputs**, and the published versions (number and
  date, each with a delete button). A program: its **Command**, the
  program and its arguments typed as one line (`php -r "print('x');"`) and
  read without a shell (`jovru::automation::command_line`): the first word
  is the program, each other word one argument, quotes keep several words
  as one, and a backslash outside quotes is an ordinary character, so
  Windows paths need no escaping. Under the field, the program and each
  argument are drawn as separate pieces, a quote left open is named, and a
  word a shell would act on (`|`, `>`, `&&`, `$NAME`) says that it reaches
  the program as an argument; then the entrypoint, the working folder, the environment
  (variables removed, those set, and which of Jovru's variables are
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
- **Run and result**: one line saying what **Run** starts (the connection
  and database a SQL Job runs on, a program's command, or a chain's number
  of steps; a job with none says to choose it in the Editor), the run
  form, as a report's (a list input shows a menu),
  **Run** or **Stop**, then the run's state and, under it, what the run
  left to look at while the tab is open: **Result**, a SQL Job's rows in a
  grid with their count of rows and columns, or **Output**, what a program
  printed (**stdout** and **stderr** with their sizes, read from its logs
  when it ends; a run that did not succeed opens on stderr). A chain shows
  what its last step left, naming the step. Before the first run the area
  says what will appear in it; a queued or running run shows neither. A job that differs from what is
  saved runs as a draft, and the form says so.
- **History**: the job's past runs (below).
- **Schedule** ([specification](../../plans/automation-engine-specification.md),
  section 9.5; [D-0041](../../decisions/automation.md#d-0041)): the
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
  run only while Jovru is open, in the window holding automation (a
  second window says the schedule runs in the other one), and a job with a
  required input without a default says why it cannot be scheduled.
  The consent dialog states what runs, the rule and the next times, that it
  runs only while the application is open, both policies, each connection
  with its environment label and a warning when it does not remember its
  password, each Production step's risks (it is the production
  confirmation), each program's file and hash, and the defaults; confirming
  again first says why it was suspended and which connections changed.

Where this differs from the specification's section 8.1: the inner tabs it
names **Inputs** and **Runs** are **Run and result** and **History** (the
first holds what a run returns, the second is apart from the global **Runs**
list), and a program's arguments are typed with its program, as one command
line, rather than one per row (0.3.1).

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

The history (a job's **History**, and the global **Runs** list) lists runs and
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
and logs are kept. A SQL run says that the rows it returned are
never stored, only their count: they are in the job's tab right after the
run. The global list has a **State** filter and, above the
history, the runs and chain runs active or queued now, each with its state
and **Stop**.

Dividers are adjustable with the mouse and the arrow keys. The assistant panel opens to the
right of the query area ([D-0028](../../decisions/assistant.md#d-0028)):

- **Telemetry** in Preferences › Diagnostics: **Send logs** and **Send
  traces** (both off by default), the protocol (HTTP/protobuf or gRPC), the
  collector's address and headers for its authentication (kept in the
  system vault and never shown again). **Save** applies at once; turning a
  signal off stops capturing it immediately. **Test** sends one span and
  says how the collector answered, with a way to cancel. A note lists what
  is sent and never sent ([D-0029](../../decisions/reliability.md#d-0029)).
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
- The demo (`JOVRU_DEMO`) answers with canned text marked **Simulated**.
