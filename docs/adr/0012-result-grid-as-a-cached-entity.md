# ADR-0012: Result grid as its own cached entity

- **Status:** Accepted (by the maintainer on 2026-11-02; implemented on 2026-11-02)
- **Date:** 2026-11-02

## Context

The result grid is the most used part of the application: users scroll,
hover, select and copy there all day. It renders inside `Workspace`, together
with the toolbar, the editor, the result tabs, the footer and the assistant,
all from one `render()`.

GPUI keeps no retained element tree. When an entity is notified, or a hover
or focus change happens inside it, GPUI runs that entity's `render()` again
and lays out and paints everything it returns. Because the grid belongs to
`Workspace`:

- hovering a cell, scrolling or changing the selection re-renders the
  toolbar, the editor and everything else in the workspace;
- typing in the editor (observed by `Workspace`) re-renders the grid on every
  keystroke.

After virtualization (commit `0742c67`) a workspace frame costs about 2.5 ms
in release, within budget. But each frame still pays for regions that did not
change: in a debug build the workspace alone costs about 11.5 ms of a 19 ms
grid frame, and the waste grows with every feature added to either side.

## Decision

The grid becomes a separate entity, `ResultGrid`, embedded in the workspace
with `.cached(...)`:

```text
Workspace
├── toolbar, editor, result tabs, filter bar, footer, notice bar
└── ResultGrid (its own entity, embedded with .cached())
    └── header + visible rows (uniform_list)
```

### What each side owns

| `ResultGrid` | `Workspace` |
| --- | --- |
| The current result, shared as `Rc<ResultData>` | The run: SQL, statement statuses, execution state |
| `GridCache` (row order, widths, families) | The filter input, the number-format toggle, the result tabs |
| Selection, focus handle, scroll handle | The footer and the notice bar |
| Row rendering, mouse and keyboard handling, context menu, copy | Chart and Messages views, CSV export (reading the same `Rc<ResultData>`) |

### How they talk

- **Workspace → grid**, through methods that update the grid and notify only
  it: `set_result(Rc<ResultData>)`, `set_filter(String)`,
  `set_number_format(bool)`, `set_sort(...)`.
- **Grid → workspace**, through events (`EventEmitter<ResultGridEvent>`):
  - `SelectionChanged { cells }`, so the footer shows the selected count;
  - `Notice(Feedback)`, for copy results and similar feedback in the notice
    bar.

  The workspace keeps only the small state those events carry; it never
  reaches into the grid's fields.

### Rules

- Hover, scroll, selection and copy notify **only** the grid. They must not
  call `cx.notify()` on `Workspace`; a `SelectionChanged` event updates the
  footer through the workspace's own small state.
- Changes in the workspace (typing, toolbar, footer) reuse the grid's last
  frame through `.cached(...)`. The cached style gives the grid its size
  (`size_full`), since a cached view is laid out with that style.
- The result is shared, never copied: one `Rc<ResultData>` serves the grid,
  the chart, the messages and the CSV export.
- Everything that does not need GPUI (the grid view, selection blocks, copy
  formatting) stays in plain functions with unit tests.
- Grid behaviour does not change: selection, copy, tooltips, `↵`,
  `name : type` headers and the footer work as described in
  [Results](../results.md).

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Keep the grid inside `Workspace` | No refactor | Every grid interaction re-renders the workspace and every keystroke re-renders the grid | Wasted work on the most used surface, growing with features |
| A separate entity without `.cached()` | Scoped notifications for grid interactions | Still re-rendered whenever the workspace renders | Loses half the benefit |
| One entity per row or cell | Finest invalidation | Thousands of entities, events and handles | Overhead beyond any gain; `uniform_list` already limits rows to the visible ones |

## Consequences

- Positive: a grid interaction costs only the grid; typing costs only the
  workspace. The grid can be reused (for example, for a table preview or a
  second result) and gains a clear API.
- Negative: state moves across an entity boundary; selection count and
  notices travel by events. The refactor touches most of the grid code.
- Required: new grid features live in `ResultGrid`, and the rules above hold.
  This ADR covers the grid part of [ADR-0007](0007-application-logic-outside-views.md).

## How to validate

With the [frame probe](../performance.md#how-to-measure), in release and with
the dev databases:

- Hovering, scrolling and selecting in a 1,000-row and a 100,000-row result
  run `ResultGrid::render` only, not `Workspace::render`, and stay within the
  [frame budget](../performance.md#budgets).
- Typing in the editor with a 100,000-row result open does not run
  `ResultGrid::render`.
- A visual check of the whole grid flow: selection by click, Shift+click,
  Shift+arrows, row number and `Ctrl A`; copy with and without headers;
  tooltips; keyboard focus; footer count; notice bar.

## Implementation notes

Implemented on 2026-11-02 (`src/app/result_grid.rs`, `jykre::grid`).

- There is no `set_sort`: the only sort is the demo's header toggle, which
  lives inside the grid.
- The frame probe confirmed that typing in the editor does not re-render the
  grid (24 `Workspace` renders, 1 `ResultGrid`). Hover, scroll and selection
  in the grid still re-render `Workspace`, because GPUI marks every ancestor of
  a notified view dirty, so the first validation above does not hold as
  written ([Performance](../performance.md#known-deviations)).
