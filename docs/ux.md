# User experience

Jykre Studio should feel immediate: every action answers at once, every state
is visible, and the keyboard reaches everything.

## Layout

```text
┌────────────────────────────────────────────────────────────────────────┐
│ Title bar: workspace · search (Ctrl Shift P) · New query               │
├──┬──────────────┬──────────────────────────────────────────────────────┤
│▣ │ Explorer     │ Tab groups, one per connection: [PostgreSQL · Local] │
│☆ │ ▾ connection │ Toolbar: Run · Explain · Format        save · panel  │
│◷ │   ▾ database │ ┌──────────────────────────────────────────────────┐ │
│⚙ │     ▸ schema │ │ SQL editor, status per statement in the gutter   │ │
│  │              │ └──────────────────────────────────────────────────┘ │
│  │              │ Results · Chart · Messages     pt-BR · filter · CSV  │
│  │              │ ┌──────────────────────────────────────────────────┐ │
│  │              │ │ Result grid                                      │ │
│  │              │ └──────────────────────────────────────────────────┘ │
│  │              │ Footer: 1,000 rows · row limit reached · 18 ms       │
│  │              │ Notice bar (user actions: copied, saved, formatted)  │
├──┴──────────────┴──────────────────────────────────────────────────────┤
│ Status bar: workspace · connections · local data · version             │
└────────────────────────────────────────────────────────────────────────┘
```

Dividers are adjustable with the mouse and the arrow keys. The assistant panel (simulated today)
opens to the right of the query area.

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
(row limit). Only the section scrolls, and the dialog keeps its height when
switching. A field that fails validation on save opens its section.

In **Metadata**, **Load list** reads the
namespaces of the server typed in the form, a tree with three-state boxes
(chosen, partly chosen, not chosen) opens one level at a time, a filter
narrows it, and **Everything, including what is created later** chooses all.
A namespace a saved choice names but the server no longer has shows
**Not found**. Saving without a choice is allowed; nothing is read until the
user chooses.

Each state has one consistent representation.

| State | Where | Representation |
| --- | --- | --- |
| Query running | Footer, results area, Run button | Spinner, "Running…", the Run button turns into Cancel |
| Statement status | Editor gutter | Waiting, running, success, error or cancelled per statement; only the statement on the server shows as running |
| Row limit reached | Footer | Amber warning with the limit in its tooltip |
| Run cancelling | Footer | Spinner and "Cancelling…" until the driver stops; the previous result stays and a new run may start |
| Run cancelled / failed | Footer (and the results area for errors) | Cancel or error icon with a short text |
| Metadata refreshing | The node being refreshed | Its `↻` spins; clicking it cancels |
| Metadata refresh waiting | The node (or the connection) whose refresh was asked for while another runs | "queued" after its name; it starts when the running one ends, in order. Asking again (F5, the menu or the connection's `↻`) takes it out of the queue |
| Reading the chosen metadata | Below the connection in the explorer | A bar with "Reading n of total: name", the percentage and the time left (estimated after the first namespace); the connection's `↻` spins and cancels |
| Nothing chosen to introspect | The connection in the explorer | "Nothing is introspected yet." and **Choose what to introspect**, which opens the connection settings |
| Metadata state per scope | Explorer node and its tooltip | Last success time, failure message, permission warnings |
| What a node is | Explorer node | Icon by kind (table, view, materialized view, column, key column, foreign key, partition), the column's type or what a key references after the name, and a tooltip with nullability, primary key position, default and comment |
| Demo versus server data | Footer badge | `MOCK` or `SQL` |

Rules:

- Anything that may take more than **300 ms** shows progress. Anything that
  may take more than **2 s** also shows the elapsed time and a way to cancel.
- No action leaves the UI without an answer: a click gives immediate visual
  feedback even when the result takes longer.
- Dangerous actions ask for confirmation: deleting a connection, closing a tab
  with a running query, copying more than 50 MiB.

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
  Closing one whose SQL is not in the saved queries asks too: closing loses it.
- Moving a tab to another connection (dragging it to another group, or
  choosing a connection in the editor toolbar) asks first: its results and
  session belong to the previous connection.

## Keyboard shortcuts

| Action | Shortcut |
| --- | --- |
| Run the selection, or the statement at the cursor, in place of the last result tab created | `Ctrl Enter` |
| The same, in a new result tab | `Ctrl Shift Enter` |
| Run every statement in the tab, one result tab each | `Ctrl Alt Enter` |
| Search tables in the local cache | `Ctrl Shift P` |
| Search: choose a match, open it | `↑`, `↓`, `Enter` |
| New query on the active connection | `Ctrl N` |
| Close the query tab (asks when it runs or has unsaved SQL) | `Ctrl W`, `Ctrl F4` |
| Show or hide the explorer | `Ctrl B` |
| Show or hide the assistant | `Ctrl Shift B` |
| SQL font size: larger, smaller, reset | `Ctrl +`, `Ctrl -`, `Ctrl 0` |
| Close the search or the connection dialog | `Esc` |
| Refresh the focused explorer node | `F5` |
| Explorer: previous, next, first, last node | `↑`, `↓`, `Home`, `End` |
| Explorer: expand or go to the first child, collapse or go to the parent | `→`, `←` |
| Explorer: open the table (a click only selects it; a double click opens) | `Enter` |
| Grid: move, extend the selection | Arrows, `Shift` + arrows |
| Grid: select all, clear | `Ctrl A`, `Esc` |
| Grid: copy, copy with headers | `Ctrl C`, `Ctrl Shift C` |
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
| Elapsed time and cancel for runs over 2 s | The Run button cancels, but the footer shows "Running…" without the elapsed time |
| Syntax error position marked in the editor | PostgreSQL reports a position; MySQL and Databricks give it only inside the message |
| Shortcuts as actions in one keymap | Only the three run shortcuts are actions ([UI guidelines](ui-guidelines.md#known-deviations)) |
