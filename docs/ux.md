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

Each state has one consistent representation.

| State | Where | Representation |
| --- | --- | --- |
| Query running | Footer, results area, Run button | Spinner, "Running…", the Run button turns into Cancel |
| Statement status | Editor gutter | Waiting, running, success, error or cancelled per statement; only the statement on the server shows as running |
| Row limit reached | Footer | Amber warning with the limit in its tooltip |
| Run cancelling | Footer | Spinner and "Cancelling…" until the driver stops; the previous result stays and a new run may start |
| Run cancelled / failed | Footer (and the results area for errors) | Cancel or error icon with a short text |
| Metadata refreshing | The node being refreshed | Its `↻` spins; clicking it cancels |
| Metadata state per scope | Explorer node and its tooltip | Last success time, failure message, permission warnings |
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
