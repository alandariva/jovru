# Results

How query results are read, kept, shown and copied.

## Files

- [`model.md`](model.md): Value types and presentation, the model in memory, limits, statements and results.
- [`grid.md`](grid.md): The grid and the footer.
- [`editing.md`](editing.md): Writing a result back to its table.
- [`transfer.md`](transfer.md): Moving results out to the clipboard and files, and rows in from files.
- [`reports.md`](reports.md): Reports and their panels.

## Principle

A result is **bounded** while it is read, **converted off the UI thread**, and
**rendered only where it is visible**. The UI thread receives display-ready
data and builds the rows on screen, so a frame costs the same for ten rows or
the largest result the limits allow ([Performance](../../standards/performance.md)).

## Acceptance criteria

- [ ] Text, exact numbers (`bigint`, `numeric(38,10)`), dates, booleans, NULL,
      JSON and binary show correctly for PostgreSQL, MySQL and Databricks.
- [ ] NULL is visually distinct from the text `NULL`, from empty text and from
      whitespace.
- [ ] Scrolling, hovering and selecting stay within the frame budget at the
      maximum row limit.
- [ ] Cell, block and row selections copy with and without headers and paste
      correctly into a spreadsheet.
- [ ] Running in one tab does not change another tab's results.
- [ ] Sums, averages and medians of `numeric(38,10)` values are exact.
- [ ] An export of a result larger than the tab holds writes every row in
      order, in each format, and a cancelled or failed export leaves the
      chosen file as it was.
- [ ] Edits written to PostgreSQL, MySQL and SQLite read back exactly
      (exact numbers, NULL apart from the text `NULL`, temporal values),
      a write whose change matches other than one row leaves the table as
      it was, and inside an open transaction they stay uncommitted.

## Decisions

The reasoning behind this area's structural choices is in [Decisions: Results](../../decisions/results.md).

## Known deviations

None.
