# User experience: keyboard shortcuts

Part of [User experience](README.md).

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
| In a job's run form (**Run and result**): run the job with the values typed, as **Run** does. `Ctrl Enter` never runs a job | `Enter` (fixed) |
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
| Explorer: previous, next, first, last line, the connections included, so the arrows cross from one connection's tree to the next connection | `↑`, `↓`, `Home`, `End` |
| Explorer: expand or go to the first child, collapse or go to the parent (from a first-level node, its connection); on a connection, expand it or go to its first node, collapse it | `→`, `←` |
| Explorer: the selected node's context menu | `Shift F10` |
| Explorer: typing a name while the tree has the keyboard fills the search field without leaving the tree: the next or previous match (wrapping; the explorer's next and previous node keys), open the selected match as `Enter` does, take back a character, clear the text | Letters, `↓`, `↑`, `Enter`, `Backspace`, `Esc` (fixed, but the arrows) |
| Explorer, while the tree has the keyboard: the search marks its matches or shows only them | `Ctrl F` |
| Explorer search field: clear the text, then give the keyboard to the tree; go to the tree; open the selected match | `Esc`, `↓`, `Enter` (fixed) |
| Explorer: the table's Data view, which reads its rows at once (`SELECT * FROM` it, without a `LIMIT`, page by page); a Data view already open for the table is shown instead (**New query for table** in its menu opens another tab with `SELECT * … LIMIT` the row limit, not run). On any other node, expand or collapse. A click only selects; a double click does the same as `Enter`, and the chevron expands or collapses | `Enter` (fixed: it activates the node) |
| Grid: move, extend the selection | Arrows, `Shift` + arrows |
| Grid: first or last cell of the row, first or last cell of the page, a screen up or down (`Shift` extends) | `Home`, `End`, `Ctrl Home`, `Ctrl End`, `Page Up`, `Page Down` |
| Grid: select all, clear | `Ctrl A`; `Esc` (fixed) |
| Grid: copy in the format of the copy menu, the same with the column names | `Ctrl C`, `Ctrl Shift C` |
| Grid or report panel: cancel a pending copy, before closing the inspector/records view or clearing the selection | `Esc` (fixed) |
| Grid: show the focused cell's full value in the side panel | `Enter` (or a double click) |
| Grid of a query tab's editable result ([Results](../results/editing.md#editing)): edit the focused cell in a field over it (also the result toolbar's pencil); `Enter` keeps what was typed as pending, `Esc` drops it | `F2`; `Enter`, `Esc` in the field (fixed) |
| Grid of an editable result: set the selected cells to NULL | `Ctrl Shift N` |
| Grid of an editable result: add a row to insert | `Alt Insert` |
| Grid of an editable result: mark the selected rows for deletion, or take the mark away (an inserted row goes) | `Delete` |
| Grid of an editable result: write the pending edits after one confirmation (**Write…**), or generate their statements into a new tab (**Generate SQL…**) | `Ctrl Alt S` |
| Grid of an editable result: discard the pending edits, asking first | `Ctrl Alt Z` |
| Grid: close the field of the cell being edited, then the side panel, then the records view, then clear the selection | `Esc` (fixed) |
| Grid: select a column, extend to it | `Ctrl` + click, `Ctrl Shift` + click on its header |
| Grid (a query tab's, a report panel's or a job's): the menu on the focused header's column, or the focused cell's, to hide, freeze at the left or move the column, show hidden columns or reset the layout. With the mouse, a header dragged onto another takes its place | `Shift F10` |
| Find a column by name, ignoring case and accents, in the grid that has the keyboard, else in the query tab's result on screen (also the result toolbar's button); while the dialog is open with text, the records view lists only the columns it finds | `Ctrl Shift F` |
| Query tab's result grid, while it has the keyboard: open the result search and focus its field (selects the text when the bar is already open). The toolbar's button opens it the same way, and a second click closes it | `Ctrl F` |
| Result search field: the next or previous occurrence while highlighting, close the bar and give the keyboard back to the grid | `Enter`, `↓`, `Shift Enter`, `↑`, `Esc` (fixed) |
| Column search dialog: choose a column, go to it (showing it when hidden) and close the dialog, or close it; a click outside the card closes it too | `↑`, `↓`, `Enter`, `Esc`, click outside (fixed) |
| Result tabs: move the focused tab left or right | `Ctrl Shift ←`, `Ctrl Shift →` (or drag it) |
| Tab bar: move between tabs | Arrows, `Home`, `End` (fixed) |
| A job's tab: reach **Editor**, **Run and result** or **History**, open it (the keyboard goes to its first field, or the history); reach a folded section of the Editor and fold or unfold it | `Tab`, then `Enter` or `Space` (fixed) |

Shortcuts are GPUI actions in one keymap ([UI guidelines](../../standards/ui-guidelines.md#actions-and-shortcuts)).
A new shortcut updates this table, avoids keys the editor already uses, and
works without the mouse.

By default `Ctrl F` acts where the keyboard is: in a SQL editor it opens
that editor's search, on the explorer's tree it marks or filters the
explorer's search, in a query tab's result grid it opens the result search, and
elsewhere (a report or job grid, a text field, a dialog) it does nothing.
Search, replace and multiple cursors belong to the editor component
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
  `config.db` ([Persistence](../persistence/config-db.md#schema-configdb)). Tooltips,
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
