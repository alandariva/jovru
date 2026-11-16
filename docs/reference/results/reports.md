# Results: reports

Part of [Results](README.md).

## Reports

A report (D-0024, D-0025) shows, in each panel, the last result
of the panel's SQL up to its connection's row limit, without paging, in the
same grid. A panel's result may also take the memory ceiling divided by the
report's number of panels (`jovru::reports::run::memory_ceiling`), so a
report holds no more than one query tab. How a report runs (sessions per
connection and context, at most four panels at once and four idle sessions,
the password and production checks) is in
[Architecture](../architecture.md#run-a-report); how its definition is stored,
in [Persistence](../persistence/README.md); its tab, in [User experience](../ux/README.md). The
decisions of which panels run and how they group live in
`jovru::reports::run`, a module without GPUI; the view only runs them.

- A report holds up to 12 **panels** (`MAX_PANELS`), each with its
  connection, SQL, context (the database, schema or catalog it runs in; empty
  is the session default), how it shows its result (a table, bars, lines, a
  pie or **numbers**), whether the table shows under its chart and its width
  (half or the whole row). The `:name` parameters are the report's, shared by
  the panels, one form field per name, each panel's markers read by its
  connection's dialect; a run checks only the parameters of the panels it
  runs. A parameter may offer a **list of choices** (at most 50): the form
  shows a menu, and a value must be one of them and valid for the
  parameter's kind. The choices are part of the definition; the values a run
  binds never are. A panel is its own entity, embedded with `.cached`, so a
  panel's grid or chart redraws without laying the others out.
- A panel's chart (`jovru::reports::chart`) finds its columns by name and
  draws at most 50 bars, 1,000 line points or 10 pie slices plus an "Other"
  slice that sums the rest, and says how many it shows of how many. It is
  built from one label column and one or more value columns that are
  numbers, with `gpui_kit::component::chart`: bars (`BarChart`) as small
  multiples, one per value column; lines (`AreaChart`, transparent fill), one
  series per value column; pies (`PieChart`). Every chart is built with
  `.appear(false)`, so a new result paints once, in place, instead of drawing
  in over several frames. Numbers become floating point only for the
  drawing (bar lengths, point positions, slice angles): the tooltips, the
  table and the print document keep the database's text, and the "Other"
  slice is summed exactly (`jovru::decimal::Sum`).
- A **number** panel shows the first row's value of up to eight columns
  (`MAX_NUMBERS`) large, side by side, exactly as the database wrote them,
  NULL in the NULL style. Each number may turn **green** or **red** when it
  meets a condition the user sets: a comparison (`>`, `>=`, `<`, `<=`, `=`,
  `<>`) with a target number, one condition per colour, both optional. The
  comparison is exact; NULL, text or a target that is not a number never
  colour; when both hold, red wins so an alert is never hidden. A coloured
  number also shows a mark, so colour is not its only sign.
- What a panel draws is computed off the UI thread, with its result, and
  again when its chart changes.
- **Copy as CSV** and **Copy for a spreadsheet**, in a panel's menu, copy its
  whole result with the column names, as **Copy this page as CSV** and as
  tab-separated text, whatever the copy menu says, with the columns the
  panel's grid shows, in display order. They run off the UI thread like
  every copy ([Copying](transfer.md#copying)), asking first above 50 MiB.
- **Export for printing or PDF…** (`jovru::reports::export`) writes one HTML
  file where the user chooses: the title, description, the connections, the
  values the panels ran with, and each panel with its connection and
  database, its chart (as SVG) or number and, when it shows one, its table, in two columns like the report, with every text
  escaped, nothing loaded from elsewhere and print styles (A4, the header
  repeated on each page). Its tables keep every column in the result's
  order: the grid's column layout is not part of a report. Like an export,
  it reaches the disk only at the user's request, at the path they chose
  ([Persistence](../persistence/README.md)). The save dialog is the system's
  (`cx.prompt_for_new_path`) and `.html` is appended when the name lacks it,
  because the Windows dialog does not; the file is then opened with the
  system's browser (`cx.open_with_system`), where the user prints it or
  saves it as a PDF. The file is self-contained: a Content-Security-Policy
  meta allows no external resource, the chart is inline SVG, rows are kept
  whole across pages, it says when it was generated (UTC) and its print
  button is hidden from the printout. It is written through `tokio::fs` on
  the runtime (`backend::run`), never on the UI thread; no temporary file is
  written and no PDF library is added. The application neither tracks nor
  deletes the file once written.
- Report tabs are not reopened after a restart (the definitions are kept, the
  open tabs are not), and a report never writes results or typed values to
  disk. Tests cover the escaping of a `<script>` in a cell, that no URL
  points outside the file, the chart caps, NULL and non-numeric columns, the
  exact "Other" slice and the suggested file name.
