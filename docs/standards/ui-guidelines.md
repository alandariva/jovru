# UI guidelines (GPUI)

These guidelines cover how GPUI code is written; what the user sees and does is specified in the [user experience reference](../reference/ux/README.md).

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
| Adapt the layout to a driver's descriptor and capabilities | Branch on `DriverKind` (see [golden rule 3](../README.md#golden-rules)) |

## How GPUI renders, and what follows from it

GPUI keeps no retained element tree. When an entity is notified
(`cx.notify()`), or a hover or focus change happens inside it, GPUI runs that
entity's `render()` again and lays out and paints the elements it returns. An
idle window costs nothing, but every frame rebuilds the notified view
completely, **with all its ancestors**: GPUI marks every ancestor of a
notified view dirty, and the window root is not cached, so it renders on
every frame. A view embedded with `.cached(style)` reuses its previous frame
only while neither it nor a view inside it has been notified, and only when
the frame is not a refresh: showing or hiding a tooltip, and the mouse down
and up of a clickable element, call `window.refresh()`, which turns off every
cache for that frame. There is no paint-only redraw (a `.hover(...)` style, a
scroll offset change and `request_animation_frame` all notify the view that
painted them). So a hover, scroll or selection in the result grid also
renders `AppView`, `GroupedTabs` and `Workspace`, while typing in the editor
does not render the grid (D-0013). Therefore:

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
6. **Views on the path to a frequently notified view stay thin.** The
   `render`, layout and paint of `AppView`, `GroupedTabs` and `Workspace` do
   no work that grows with data or with content off screen: result rows, the
   editor's text, metadata. What they show from such data is derived when the
   data changes and kept on the entity. The editor gutter is the reference:
   statement lines and the line count are computed when the SQL changes
   (`jovru_sql::statement_lines`), and only statements on screen get a run
   button. A change to `AppView`, `GroupedTabs` or `Workspace` that could
   grow a frame is measured with the [frame
   probe](performance.md#how-to-measure) during grid interactions, and
   `a_long_script_maps_its_statements_to_lines_within_budget` keeps the
   statement-line mapping linear.

See [Performance](performance.md) for budgets and how to measure.

## Structure

- Logic that needs no GPUI lives in the library, in modules the views call,
  so it is tested without a window and a second interface does not repeat
  it (D-0007). SQL text handling (splitting, normalisation, preview checks)
  is `sql` (from `jovru-sql`): pure and synchronous. A run's state is
  `jovru::query::QueryRun`, with `RunState` (`Idle | Running | Cancelling |
  Done | Failed | Cancelled`), the statement statuses and the batch of
  results, driven by `backend`; one state value drives the footer, the gutter
  and the buttons. `Cancelling` waits for the driver's outcome and no new run
  starts until it arrives, because the stopping run may still change data on
  the server. Identifier quoting for previews belongs to the driver
  (`Driver::quote_identifier`, `Driver::relation_name`), not to a `match` on
  `DriverKind`.
- The views are `src/app/`; demo data is `src/app/demo/`, behind
  `JOVRU_DEMO=1`.
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
  anything that may exceed 2 s ([User experience](../reference/ux/states.md#always-visible-states)).
- Never hold a `RefCell` borrow of shared UI data across `.await`.
- Work that must finish before the application exits is not registered with
  a view's `cx.on_app_quit`: closing the last window releases the window's
  views before GPUI runs the quit handlers, so that handler never runs. Take
  what to write when the view is released (`cx.on_release`) and hand the
  write to `exit_writes::queue` (`src/app/exit_writes.rs`), which one handler
  of the whole application awaits.

## Actions and shortcuts

- Every application shortcut is a GPUI **action** (`actions!`), bound in one
  keymap at startup with `cx.bind_keys` and handled with `.on_action`.
  Component-scoped shortcuts use a `key_context`, so the same keys can mean
  different things in the editor, the grid and the explorer.
- The keymap the user can change is built in `src/app/shortcuts.rs`: the
  bindings in place at start (the application's `application_bindings`
  and `shortcuts::default_bindings`, and the editor component's) are its
  defaults, and each change rebuilds it from them. A new shortcut is an
  action with a default binding there and an entry in `shortcuts::catalog`,
  so **Preferences › Shortcuts** offers it; a test checks that every listed
  action of the application has a key and that the defaults do not collide.
  The rules (keys, scopes, collisions, the stored choices) are in
  `jovru::keymap`, outside the views.
- A view that names a key reads it from the keymap
  (`shortcuts::label::<Action>(cx)`), never a fixed string, so it shows the
  key in use.
- Bindings are listed in [User experience](../reference/ux/shortcuts.md#keyboard-shortcuts). A new
  shortcut updates that table with the change.
- Raw `on_key_down` handlers are only for text-like navigation inside a single
  focused control when an action would not fit: typing and moving in a text
  field or a list it drives (the explorer's search field and typing on its tree,
  the search's list, the column search), the tab bar and its picker
  (`grouped_tabs`), a splitter's arrows, and `Esc` closing a layer of the
  grid or a report panel after the window's `Dismiss` lets it through.
  These keys are fixed; the user cannot change them.

## Text

- Every user-facing string goes through `tr!` with positional placeholders
  (`tr!("Query {0}: {1}", n, title)`). Do not concatenate translated fragments;
  translators need whole sentences.
- After adding or changing strings, run `cargo xtask i18n pt-BR` and fill in
  the new entries (empty entries fall back to English). Keep positional
  placeholders (`{0}`, `{1}`), which may be reordered.
- To ship another language, run `cargo xtask i18n <tag>`, fill
  `locales/<tag>.json`, and add
  `("<tag>", include_str!("../locales/<tag>.json"))` to `CATALOGS` in
  `src/i18n.rs`. A unit test checks that each catalog parses and keeps its
  placeholders. Error text from `jovru-storage` and `jovru-secrets` is not
  passed through `tr!`.
- Terminology is fixed; see [User experience](../reference/ux/feedback.md#text-and-terminology).
- Fonts come from roles, `ui::fonts()`: `ui` for the interface, `code` for
  SQL (ligatures allowed), `mono` for values and messages, shown as typed.
  Views never name a family. `fonts::resolve` picks each role once at
  startup, the first of its platform's candidates that is installed (Segoe
  UI and Cascadia on Windows; the desktop's sans family and Cascadia, Noto
  Sans Mono, DejaVu Sans Mono, Liberation Mono or Ubuntu Mono on Linux),
  else GPUI's `.SystemUIFont`. A family that is not installed would let GPUI
  draw code in a proportional font.

## Colours and theme

- Colour comes from one JSON theme system
  ([Themes](../reference/themes.md)). The user chooses **System** (the
  default, following the operating system), **Light**, **Dark**, or a theme
  file's id in Preferences (`jovru::theme::ThemeChoice`, kept as
  `appearance.theme`). **Light** and **Dark** are the bundled
  `jovru-light` and `jovru-dark` files. A file dropped in a themes directory
  is listed with them after Preferences is opened again.
- Views read the named tokens of the theme on screen, `ui::colors()`
  (`bg`, `panel`, `text`, `mint`, `red`, ...), fields of the resolved
  `Palette`. The SQL editor reads `ui::editor_colors()` and the result grid
  reads `ui::results_colors()`. Those two match `colors()` until an editor
  or results override is set. Views do not use raw hex values. A new colour
  is a new token, with a value in both built-in palettes and in both bundled
  files; hues in the light palette keep a 4.5:1 contrast on `bg`.
- Read the token while drawing; never keep a resolved colour in state, and
  never parse JSON while drawing. Loading happens when the catalog is read
  and when the theme changes. What is chosen by the user (a connection's
  colour) is kept as its hue (`ConnectionColor`) and resolved with
  `connection_color` when drawn; tab groups are given their shades again
  when the theme changes.
- `theme::apply` (`src/app/theme.rs`) installs the resolved palette and
  copies its tokens onto the component kit's `Theme` (including the editor's
  caret, selection, active line and gutter). GPUI's refresh redraws cached
  views as well. Font size, density and layout are not theme tokens.
- Exceptions that stay hardcoded are listed in
  [Themes](../reference/themes.md#known-exceptions).
- Value colours by type family are defined once in
  [Results](../reference/results/model.md#value-presentation).

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
- A modal dialog drawn by the application sits on `ui::modal_backdrop`,
  which occludes the window behind it: in GPUI an element that only paints
  over others does not stop their listeners from seeing the pointer. The
  component kit's dialogs (`open_alert_dialog`) occlude on their own.

## Tests

- Logic extracted from views (selection blocks, grid views, statement
  statuses, value formatting) gets unit tests without a window.
- A visual change requires opening the application and checking the affected
  flow, keyboard and focus; the check is recorded separately in the task
  report ([Testing](testing.md#rules-for-changes-and-review)).
- GPUI's headless test support is not enabled; enabling it is a dependency
  decision.

## Decisions

The reasoning behind this area's structural choices is in [Decisions: UI guidelines (GPUI)](../decisions/ui-guidelines.md).

## Known deviations

| Standard | Current code |
| --- | --- |
| Frequently changing regions are their own cached entities | The editor and the explorer still render inside `Workspace` and `AppView`; they are split when the measured cost of the ancestors (about 1.5 ms p50 per grid frame on 2026-11-02) justifies it (D-0013) |
| Long collections are virtualized | The command palette builds its rows without a virtual list, capped at 10 without text and 50 for a search (`jovru::palette::limit`). Preferences builds every theme card, so its element count grows with the catalog |
