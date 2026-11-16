# Results: the grid

Part of [Results](README.md).

## Grid

- A row-number column, then one column per result column.
- Selection: click a cell; Shift+click or Shift+arrows extend a rectangular
  block; pressing a cell and dragging selects the block up to the cell under
  the pointer, and dragging past an edge of the grid scrolls (faster the
  further the pointer goes) while the block grows; click a row number to
  select the row, Shift+click another row number to extend, or drag over row
  numbers to select those rows; `Ctrl`+click a header selects its column over
  the displayed rows (`Ctrl Shift`+click extends to it from the anchor's
  column); `Ctrl A` selects everything visible; `Esc` clears. Arrows move the
  focused cell and scroll it into view, rows and columns. `Home` and `End` go to the first and
  last cell of the row, `Ctrl Home` and `Ctrl End` to the first and last cell
  of the page, `Page Up` and `Page Down` a screen of rows; `Shift` extends the
  block from its anchor.
- A click on a column header sorts the rows of the page by that column's
  values: ascending, then descending, then the server's order again, with an
  arrow on the header. Numbers compare exactly, as decimal text, also in exponent form (never as
  floats); other values by their text; NULLs come last either way and equal
  values keep the server's order. Like the filter, it sorts only the rows
  held for the page; another result tab starts unsorted.
- The footer shows the number of selected cells when more than one is
  selected, and, when number columns are among them, the **sum, average,
  median, minimum and maximum** of their numbers (`jovru::stats`), in the
  number format on screen; its tooltip says how many numbers they come from.
  They are exact: added and divided as decimal text (`jovru::decimal`), never
  as floats. The average and a median between two values keep six decimals
  past the values' own, rounded half away from zero, without trailing zeros.
  Text such as `NaN` is skipped, NULL is not a value, and a number too far
  from the decimal point to add (`1e500`) shows the sum and average as `—`.
  Up to 20,000 cells are summarized at once; a larger selection shows
  "Calculating…" while it is summarized off the UI thread, which reads the
  shared page in place (D-0022). A drag is summarized when it ends.
- **Column layout.** Any column can be hidden and shown again, moved to
  another place and frozen at the left edge, in every result grid (query
  tabs, report panels and a job's result). With the mouse, a header dragged
  onto another takes its place (onto a frozen header it is frozen there;
  out of the frozen ones it scrolls again); with the keyboard, `Shift F10`
  opens the menu on the focused header's column or the focused cell's. The
  menu has **Hide the column**, **Freeze the column at the left** (checked
  while frozen; choosing it again unfreezes), **Move the column left**,
  **right**, **to the start** and **to the end** (within the frozen columns
  or within the others), **Show a hidden column** (the first 20 by name;
  finding a column by name reaches the rest), **Show all columns** and
  **Reset the column layout**. The last column shown cannot be hidden.
- **Finding a column.** `Ctrl Shift F`, or the toolbar's **Find a column**
  button, opens a dialog over the grid that finds columns by name,
  ignoring case and accents (`jovru::column_search`: `cao` finds `Ação`,
  `strasse` finds `Straße`), with the matching part marked in the
  `MATCH_TEXT` and `MATCH_BG` colours. With nothing typed it lists every
  column; otherwise only the ones whose name holds the text. They are in
  alphabetical order, ignoring case and accents, with the hidden ones among
  the shown ones, each saying it is hidden and that choosing it shows it. `↑` and `↓` choose a column,
  `Enter` (or a click) goes to it and closes the dialog: the grid pans to
  the column and selects it over the displayed rows, after showing it when
  it was hidden, and takes the keyboard. `Esc`, the close button or a click
  on the dimmed area outside the card closes the dialog and gives the grid
  the keyboard. Opening it again selects the text it already holds. The
  dialog says how many columns match, or how many there are when the text
  is empty. The matches are computed when the text, the layout or the
  result changes, never per frame, and only the lines on screen are drawn,
  so thousands of columns cost a frame what about ten do. The shortcut
  works in the grid that has the keyboard (a query tab's, a report panel's
  or a job's), the dialog included; elsewhere it opens the dialog in the
  query tab's result on screen. While the dialog is open with text, the
  records view lists only the shown columns it finds, still in display
  order; closing it lists them all again.
  While any column is hidden, a line above the table (and the records)
  says how many, with **Show all columns**. The values, their types and
  NULLs are untouched; only where a column is drawn changes.
- Frozen columns keep their place at the left edge, after the row numbers,
  which stay with them, while the other columns pan under them; without
  frozen columns the row numbers pan with the rest, as before. Moving the
  focused cell into a scrolling column pans it into the room the frozen
  ones leave.
- Only the columns in view are built: a row draws its frozen columns and
  the scrolling ones that cross the viewport, not those panned wholly under
  the frozen ones, with the width of the others kept as room
  (`column_layout::ColumnWidths::shown`), so a frame costs the columns on
  screen, not the result's. The widths in display order and where each
  column starts are kept from one frame to the next and computed again only
  when the view, the layout or a width changes (`grid::WidthsCache`). Each
  row, and its frozen part over it, is painted as one layer of the scene,
  so ordering a row's borders, backgrounds and values costs one look-up
  instead of one per border strip and background; the row with a cell
  being typed in is not, so the field's caret stays over its text. The
  demo result, whose five columns share the width in proportion, draws them
  all.
- A header's right edge resizes its column (48 to 1,600 px); a double click on
  it, or **Fit the column to its values** in the menu, fits the column to its
  header and the values of the first 5,000 displayed rows. **Reset column
  widths** goes back to the measured ones. Widths belong to the result tab:
  its pages share them and switching tabs restores them.
- A double click on a cell, or `Enter`, opens the side panel on the cell's
  **full value**: read-only text that can be selected, JSON laid out with
  indentation without changing any token, up to the first 1 MB (**Copy**
  copies all of it, exactly as stored, like the other copies below), NULL
  shown as NULL. The panel follows the focused cell.
  Its other tab, **Statistics**, summarizes one of three things, chosen at
  its top:
  The columns of each scope are the ones shown, in display order; hidden
  columns are left out.
  - **Selection**: the selected cells (two or more): how many rows and
    columns, values and NULLs, the sum, average, median, minimum and maximum
    of the numbers of number columns together (as the footer shows), and one
    line per selected column.
  - **Result**: every row of the page the filter shows, with one line per
    column. Numbers of different columns are not added together.
  - **Column**: the focused column (or the one a line or the menu chose),
    over the rows of the page the filter shows: NULLs, distinct values
    (numbers by value), empty texts, minimum and maximum in the order the
    header sorts by, sum, average and median for numbers, the shortest and
    longest text, and the five most frequent values.

  A column's line shows its type and, for numbers, the sum and average,
  otherwise its distinct values, its NULLs, and a thin meter of how much of
  it holds a value; a click (or `Enter`) opens that column's statistics.
- **Small charts** in the Statistics tab, each with its figures written
  beside it, so no value is shown by colour alone: a thin bar splits values
  from NULLs (and, for a text column, from empty texts), with a swatch on
  each of those lines; a number column's **Distribution** draws up to 20
  equal slices of its range between its smallest and largest value, whose
  tooltip says how many values a bar holds and its own smallest and
  largest, as the database wrote them (the value is turned into floating
  point only to choose its bar); each of the most frequent values has a
  meter of its share of the rows, with the percentage. The colours are the
  `CHART_*` tokens: the accent for values, neutrals for NULL and empty
  text. Up to 100 columns are listed; a line says how
  many more there are. The selection's and the page's statistics are
  computed off the UI thread, reading the shared page in place; a column's
  are read a chunk per frame and computed off the UI thread. `Esc` closes the
  panel.
- The toolbar's **Statistics** button (beside **Results**) shows the table
  with this tab open: on the selection when two or more cells are selected,
  otherwise on the result. It stays highlighted while the tab is open, and a
  second click closes the panel. The menu has **Statistics** and **Column
  statistics** too.
- **Show the selected rows as records** (toolbar or menu) shows the rows of
  the selection side by side: one line per column shown, in display order,
  one column per row, up to 50 rows. A click on a value shows it in the side
  panel. `Esc` goes back to the table.
- Result tabs can be dragged to another place in their bar, or moved with
  `Ctrl Shift ←` and `Ctrl Shift →` on the focused tab. The tab on screen,
  numbers, pins and what a run replaces do not change.
- Right-clicking outside the selection selects the cell under the pointer
  first, so the context menu acts on what is under it.
- The result search matches text in any column, ignoring case, and stops
  hiding rows and marking text when it is closed. **Highlight** (the mode it
  opens in) keeps every row and marks the matching text. **Filter** shows
  only the matching rows. The footer counts the matching rows either way
  ("3 of 1,000 rows", or "3 of rows 1–1,000" when the page is not the whole
  result). While highlighting, the bar shows the current occurrence as
  `1/3`: each visible cell that matches, left to right and top to bottom.
  A row with two matching cells counts once in the footer and twice in
  `1/3`. `Enter`, `↓`, `Shift Enter` and `↑` move that occurrence, wrapping. Hiding
  rows and both counts wait until typing pauses for 150 ms, so a burst of
  keystrokes rebuilds the grid's view once; the marks follow each keystroke.
  It matches the text drawn in the cell, ignoring case. A boolean stored
  as `t` or `1` matches "true", because that is what the cell shows; a
  number in the pt-BR format matches the grouped text on screen. NULL is
  drawn as NULL, empty text as "(empty)", and a line break as ↵, so a
  search follows those. The mark is that text (`MATCH_TEXT`, `MATCH_BG`).
  Searches run on the background executor with a ten-second scan limit;
  typing, closing the bar or changing the page cancels the previous scan,
  and superseded responses are discarded. Pending and written edits,
  inserted rows (including DEFAULT), and number-format changes participate
  in the search just as they do in the painted cells.
- The search covers only the rows held for the page on screen. Below the
  field it says so ("Filters this page only (1,000 rows)…", or "Highlights
  this page only…" in the other mode) whenever the result has other pages or
  stopped at a limit, and an empty filter reads "No rows on this page match
  the filter", never "no results". With text and no highlight hit, the
  footer reads "0 of 1,000 rows" and the bar shows no `0/0`.
- Each result tab keeps its own selection and scroll: switching tabs and
  coming back to the same page restores them (`ResultGrid` keeps them by
  `ResultId`); another page starts at the top with nothing selected.

## Footer

The footer is the single place that reports how the last run ended: rows,
rows affected, row limit reached, cancellation or error, and the elapsed time.
The notice bar is kept for the outcome of user actions such as copy, save or
format ([User experience](../ux/feedback.md#feedback)).
