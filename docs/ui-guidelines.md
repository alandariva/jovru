# UI guidelines (GPUI)

Views are **presentation**. They do not know which database is on the other
side, do not parse SQL and do not decide anything that a second interface (a
CLI, a test) would have to repeat.

## What views may and may not do

| May | May not |
| --- | --- |
| Hold UI state: open tabs, active tab, expanded nodes, selection, panel sizes, focus | Be the source of truth for connections, metadata or results; that lives in the library and storage |
| Format values for display through `cell_format` (alignment, NULL style, `↵`) | Convert database values to floats or dates |
| Start operations through `backend` and show their progress and outcome | Do network, disk or vault I/O directly |
| Validate a form for immediate feedback | Be the only validation: drivers always validate again |
| Adapt the layout to a driver's descriptor and capabilities | Branch on `DriverKind` (see [golden rule 3](README.md#golden-rules)) |

## How GPUI renders, and what follows from it

GPUI keeps no retained element tree. When an entity is notified
(`cx.notify()`), or a hover or focus change happens inside it, GPUI runs that
entity's `render()` again and lays out and paints the elements it returns. An
idle window costs nothing, but every frame rebuilds the notified view
completely. A view embedded with `.cached(style)` reuses its previous frame
while it has not been notified. Therefore:

1. **Virtualize long collections.** Anything that grows with data (result
   rows, metadata nodes, history, search results) renders through
   `uniform_list` (equal heights) or `list` (variable heights). Never
   `.children(iter)` over a collection without a small fixed bound (roughly
   100 items).
2. **`render()` only reads.** Filtering, sorting, measuring column widths and
   bulk formatting happen when their inputs change and are cached on the
   entity, keyed by what they depend on. The result grid's `GridCache` is the
   reference pattern.
3. **Split entities by how often they change, not by code size.** A region
   that changes often and independently (the result grid with hover and
   selection, the editor while typing, an animated spinner, the chat) and sits
   next to expensive regions that do not change with it becomes its own
   entity, embedded with `.cached(...)`. Small, rarely changing pieces (a
   footer, a header) stay in their parent; every entity costs events, weak
   handles and coordination.
4. **Notify only who changed.** `cx.notify()` goes to the entity whose state
   changed. Observing a child just to call `cx.notify()` on the parent
   redraws the parent on every child change; do it only when the parent shows
   that state.
5. **Keep per-element work small.** Use scoped element IDs (`("cell", col)`
   inside a row with its own ID) instead of formatting a unique string per
   element; build tooltips in their closure, not up front.

See [Performance](performance.md) for budgets and how to measure.

## Structure

- One module per area of the window (explorer and connection dialog,
  workspace, result grid, assistant). A component with no application
  knowledge (like `grouped_tabs`) lives in the library and has an example
  under `examples/`.
- Shared layout helpers (`row`, `column`, `button`, `label`, `badge`) and
  colour tokens live in one UI module (`ui.rs`); views use them instead of
  repeating styles.

## Async work in views

- Start work with `cx.spawn`, run it through `backend::run`, and report back
  with `WeakEntity::update`. The task stops when the entity is gone.
- Guard against stale results with a generation counter, as `Workspace` does
  with `execution_id`.
- Show progress for anything that may exceed 300 ms and offer cancellation for
  anything that may exceed 2 s ([User experience](ux.md#always-visible-states)).
- Never hold a `RefCell` borrow of shared UI data across `.await`.

## Actions and shortcuts

- Every application shortcut is a GPUI **action** (`actions!`), bound in one
  keymap at startup with `cx.bind_keys` and handled with `.on_action`.
  Component-scoped shortcuts use a `key_context`, so the same keys can mean
  different things in the editor, the grid and the explorer.
- Bindings are listed in [User experience](ux.md#keyboard-shortcuts). A new
  shortcut updates that table with the change.
- Raw `on_key_down` handlers are only for text-like navigation inside a single
  focused component when an action would not fit.

## Text

- Every user-facing string goes through `tr!` with positional placeholders
  (`tr!("Query {0}: {1}", n, title)`). Do not concatenate translated fragments;
  translators need whole sentences.
- After adding or changing strings, run `cargo xtask i18n pt-BR` and fill in
  the new entries (empty entries fall back to English).
- Terminology is fixed; see [User experience](ux.md#text-and-terminology).

## Colours and theme

- The application uses a dark theme (`ThemeMode::Dark`). Colours come from
  the named tokens in `ui.rs` (`BG`, `PANEL`, `TEXT`, `MINT`, `RED`, ...);
  views do not use raw hex values. A new colour is a new named token.
- Value colours by type family are defined once in
  [Results](results.md#value-presentation).

## Accessibility and keyboard

- Every essential flow works without a mouse.
- Interactive elements have an ID, a `role` and an accessible label
  (`aria_label`); stateful ones expose their state (`aria_selected`).
- Focus is always visible (`focus_visible` styles) and contrasts with the
  control's own fill (the primary button gets a light border, not a mint one
  on mint); a focusable area tracks a `FocusHandle` and restores focus
  sensibly after dialogs close.
- A focusable control acts through `on_click`: GPUI turns Enter and Space on
  the focused element into the same click event, so a button never needs its
  own key handler and never reacts to the mouse only.
- Status regions use `Role::Status` (or `Role::Alert` for errors) so changes
  are announced.

## Tests

- Logic extracted from views (selection blocks, grid views, statement
  statuses, value formatting) gets unit tests without a window.
- A visual change requires opening the application and checking the affected
  flow, keyboard and focus; the check is recorded separately in the task
  report ([Testing](testing.md#rules-for-changes-and-review)).
- GPUI's headless test support is not enabled; enabling it is a dependency
  decision.

## Known deviations

| Standard | Current code |
| --- | --- |
| Shortcuts are actions in one keymap | Only the run shortcuts, closing a tab and moving between tabs are actions (`ExecuteCurrentQuery`, `ExecuteCurrentQueryInNewResult`, `ExecuteAllQueries`, `CloseActiveTab`, `NextTab`, `PreviousTab`). The others are matched by hand in `on_key_down`/`capture_key_down` handlers in `src/app/mod.rs`, `workspace.rs`, `connections.rs`, `result_grid.rs` (the grid's navigation keys) and `grouped_tabs` |
| Frequently changing regions are their own cached entities | The result grid is its own cached entity (`src/app/result_grid.rs`, [ADR-0012](adr/0012-result-grid-as-a-cached-entity.md)); the editor and the assistant still render inside `Workspace` |
| Colours come from named tokens | A few views still use inline hex values (the gutter's running and error icons, the assistant panel) |
| Long collections are virtualized | The result grid, the explorer tree, Messages and the saved queries and history are. The command palette builds all its matches, which the search limits to 200 per connection |
