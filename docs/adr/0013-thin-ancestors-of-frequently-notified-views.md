# ADR-0013: Thin ancestors for frequently notified views

- **Status:** Proposed
- **Date:** 2026-11-02

## Context

[ADR-0012](0012-result-grid-as-a-cached-entity.md) moved the result grid to
its own entity embedded with `.cached(...)`. Its first validation expects
hovering, scrolling and selecting in the grid to run `ResultGrid::render`
only. That cannot hold in GPUI 0.3.11 (`gpui-pre`, read in the registry
source):

- `Window::mark_view_dirty` (`window.rs`) marks every ancestor of a notified
  view dirty. A cached view reuses its last frame only when it is not dirty
  and the frame is not a refresh (`prepaint_view` in `view.rs`).
- The window root is not cached, so it renders on every frame
  (`draw_roots` in `window.rs`).
- A `.hover(...)` or `group_hover(...)` style and a scroll offset change
  notify the view that painted them (`div.rs`). There is no paint-only
  redraw: `request_animation_frame` also notifies the current view.
- Showing or hiding a tooltip, and the mouse down and up of a clickable
  element, call `window.refresh()`, which turns off every cache for that
  frame, the grid's included.

So a grid frame renders the whole path from the root: gpui-kit's `Root`,
`AppView`, `GroupedTabs`, `Workspace` and `ResultGrid`. The cache still
works the other way: typing in the editor does not render the grid.

Frame probe in release on the reference machine (Windows 11, i5-13600KF,
RTX 4060 Ti), 2026-11-02, demo mode with synthetic results shaped like
`sales.event_log` (a measurement-only patch, not committed), window
1480 × 920 with the explorer and the assistant open:

| Scenario | 1,000 × 15 p50 / p95 | 100,000 × 5 p50 / p95 |
| --- | --- | --- |
| Hover, scroll, keyboard selection | 4.3–4.4 / 5.2–8.0 ms | 3.6 / 5.0–5.8 ms |
| Same, with every region but the grid replaced by empty boxes of the same size | 2.8–2.9 / 4.4–5.3 ms | 2.0–2.1 / 3.3–4.2 ms |
| Hover, scroll, keyboard with a 1,000-statement script in the editor (before the fix below) | 57 / 59 ms | Not measured |

- `Workspace::render` itself took about 0.07 ms. Rendering the workspace's
  other regions again (layout and paint included) cost about 0.8 ms per grid
  frame, and the explorer and title bar in `AppView` about 0.7 ms more:
  35–45% of the frame, all of it for regions that did not change.
- The cost that broke the budget came from data-sized work in an ancestor.
  The editor gutter split the whole script and counted lines from the start
  for every statement on each `Workspace` render, which is quadratic. With
  200 statements a grid frame took 6.9 / 9.0 ms; with 1,000 statements it
  took 57 ms, and so did each keystroke.

## Decision

1. **A notified view renders with all its ancestors.** For the grid this is
   `AppView`, `GroupedTabs` and `Workspace` on every hover, scroll and
   keyboard selection. Clicks and tooltips cost a frame without caches. This
   is how GPUI works and is accepted.
2. **Views on the path to a frequently notified view stay thin.** The
   `render`, layout and paint of `AppView`, `GroupedTabs` and `Workspace`
   do no work that grows with data or with content off screen: result rows,
   the editor's text, metadata. What they show from such data is derived when
   the data changes and kept on the entity. The editor gutter is the first
   case: statement lines and the line count are computed when the SQL changes
   (`jykre::sql::statement_lines`), and only statements on screen get a run
   button.
3. **Splitting the editor, the assistant and the explorer into cached
   entities** stays the standard of the
   [UI guidelines](../standards/ui-guidelines.md#how-gpui-renders-and-what-follows-from-it)
   and a known deviation. It is done when the measured cost of the ancestors
   justifies it (today, about 1.5 ms p50 per grid frame), not as part of
   this decision.
4. This replaces the first validation of ADR-0012 with the one below. The
   rest of ADR-0012 stands.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Split the editor, the assistant and the explorer into cached entities now | About 1.5 ms p50 less per grid frame; the frame stops depending on those regions | A refactor as large as ADR-0012, across `AppView` and `Workspace`, on the release branch; clicks and tooltips gain nothing | Today's frames are within budget once the gutter is fixed; kept as the standard for later |
| Redraw hover or selection without notifying the view | No ancestor renders | GPUI 0.3.11 has no such API; every redraw renders from the root | Not available; changing GPUI is a dependency decision |
| Fewer notifications: no hover highlight on cells | Moving the mouse over the grid stops producing frames | A UX change; scroll, selection, clicks and tooltips are unchanged, and the cost per frame too | Treats the frequency, not the cost |
| Keep ADR-0012's validation and record a known deviation only | No decision needed | A validation that GPUI can never meet; nothing stops data-sized work in ancestors | Leaves the rule that broke the budget unwritten |

## Consequences

- Positive: the validation matches how GPUI renders. Grid frames and
  keystrokes no longer grow with the script: with 1,000 statements, 4.3–4.4 /
  5.3–6.3 ms per grid frame and 3.0 ms per keystroke frame. The rule that
  caught the gutter applies to every ancestor.
- Negative: each grid frame still pays about 1.5 ms for regions that did not
  change, and that grows with features added to `AppView` and `Workspace`.
  Each edit now also maps statements to lines, once: 0.6 ms per keystroke
  with 1,000 statements, next to the 1.3 ms the run bookkeeping already took.
  The explorer is not virtualized, so a large expanded tree adds to every
  grid frame; it has not been measured with a real connection.
- Required: a change to `AppView`, `GroupedTabs` or `Workspace` that could
  grow a frame is measured with the frame probe during grid interactions.
  When this ADR is accepted, the paragraph on cached views in the
  [UI guidelines](../standards/ui-guidelines.md#how-gpui-renders-and-what-follows-from-it)
  says that a cached view also renders when a view inside it is notified, and
  that a refresh ignores every cache.

## How to validate

With the [frame probe](../standards/performance.md#how-to-measure), in release:

- Hovering, scrolling and selecting in a 1,000-row and a 100,000-row result
  stay within the [frame budget](../standards/performance.md#budgets), also with a
  1,000-statement script in the editor. `AppView` and `Workspace` render as
  often as `ResultGrid` in these scenarios; that is expected.
- Typing in the editor with a 100,000-row result open does not run
  `ResultGrid::render` (unchanged from ADR-0012).
- `a_long_script_maps_its_statements_to_lines_within_budget` keeps the
  statement-line mapping linear.
