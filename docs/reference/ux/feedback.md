# User experience: feedback and language

Part of [User experience](README.md).

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
  session or results to lose. A report's, a job's or an MCP tab never leaves its kind's group
  (it can be reordered inside it), and no other kind of tab is dropped into it.
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
  first, naming each such statement of the run (`jovru::guard`). Nothing
  runs until the user confirms.
- Writing a result's edits (**Write…**) always asks first, in one dialog
  that lists every change, the statements with their placeholders, whether
  they are committed or stay in the open transaction, the table's triggers
  and cascades, and that the last write wins. On a production connection
  the same dialog warns so and its button reads **Write to production**.
  Editing a result whose rest is still open asks first, since reading the
  table closes it. Discarding pending edits asks first. Running a statement
  while edits are pending asks first (**Run and lose N pending changes?**),
  and so does closing the tab ([Results](../results/editing.md#editing)).
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

## Error messages

Every error follows the same shape:

1. A **short human title** chosen from the `ErrorCode` (for example, "Could
   not sign in").
2. The **database's own message**, verbatim, in a monospaced font when there
   is one.
3. A **suggested action** when one exists ("Check the user and password",
   "Run `databricks auth login --profile DEV`", "Set a SQL Warehouse on the
   connection").
4. **Copy details** ([Reliability](../../standards/reliability.md#diagnostics-for-users)).

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
  `JOVRU_LOCALE` or the OS.
- Use the same words everywhere: **Connection**, **Query** (a tab),
  **Run**, **Cancel**, **Refresh**, **Explorer**, **Results**, **Messages**,
  **Row limit**, **Timeout**, **Remember password**.
- Values from the database are shown as the database returned them; only the
  optional **pt-BR numbers** presentation changes number formatting.

## Theme and motion

- Colour themes are JSON files with one set of tokens
  ([Themes](../themes.md), [UI guidelines](../../standards/ui-guidelines.md#colours-and-theme));
  result values use the colours in [Results](../results/model.md#value-presentation).
- The SQL font size is adjustable (12 to 24 px) and remembered.
- Motion is reserved for work in progress (spinners). No decorative
  animation delays an action.

## Accessibility

Every essential flow works with the keyboard, focus is always visible, and
interactive elements expose a role, a label and their state. Details are in
[UI guidelines](../../standards/ui-guidelines.md#accessibility-and-keyboard).

A dialog over the window (Preferences, the connection form, **Import from
file…**, the command palette and the others) is modal: its backdrop takes
every click, hover and wheel, so nothing behind it (the explorer, the tabs)
reacts until it closes. A menu that is open is never covered by a tooltip:
the result grid draws none while its menu is open.
