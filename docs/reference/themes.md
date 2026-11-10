# Themes

Colour themes are JSON files. The application resolves one of them into
typed tokens, keeps that result in memory, and paints from it. A new file
in a themes directory is available without recompiling. Font size, density,
panel sizes and layout are not part of a theme
([UI guidelines](../standards/ui-guidelines.md#colours-and-theme),
[Persistence](persistence.md)).

## Architecture

There is one theme system. `jykre::theme` loads files, checks them, resolves
inheritance and overrides, and stores the result. Views read
`ui::colors()`, `ui::editor_colors()` or `ui::results_colors()`. Those
return a handle to a palette already resolved; drawing does not parse JSON.

`src/app/theme.rs` copies the resolved tokens onto the component kit's
`Theme` (surfaces, tabs, trees, scrollbars, caret, selection, and the
editor gutter). The kit keeps drawing through `Theme` and `ThemeColor`.
The kit's own theme registry is not a second system: its files mix fonts
and radius with colour and have no aliases, inheritance or result tokens.

Resolution runs when the catalog is loaded (startup, and again when
Preferences opens) and when the choice or the system appearance changes.
`jykre::theme::install` publishes the result. Later frames clone an `Arc`.
The grid converts its paint colours to GPUI's HSLA representation once per
visible row batch and shares them across the cells. Neither the palette nor
that projection is retained as view state.

The resolved value has three palettes that start equal:

| Palette | Who reads it | What can make it differ |
| --- | --- | --- |
| `application` | Shell, tabs, panels, trees, buttons, menus, dialogs | `tokens`, then user `tokens` |
| `editor` | SQL highlighter and the editor chrome copied onto the kit | the chain's `editor` sections, then user `editor` |
| `results` | The result grid | the chain's `results` sections, then user `results` |

A later picker can choose a different file for the editor or the grid by
filling those two palettes. This delivery has one active application theme.
Preferences lists it; it does not add a second picker.

## Where a file is read

In order, a later file with the same `id` replaces an earlier one:

1. The two themes compiled into the binary (`themes/jykre-dark.json` and
   `themes/jykre-light.json`, also the fallback).
2. `themes/` next to the executable.
3. `themes/` in the data folder (`JYKRE_CONFIG_DIR` when set, otherwise the
   folder [Persistence](persistence.md) already uses).

To add a theme, save `my-theme.json` in either `themes/` directory and open
Preferences. The file's `name` appears next to System, Light and Dark. The
choice is kept across restarts. A missing directory is skipped.
Files are read in filename order within each directory and limited to
1 MiB each. An unreadable directory or an oversized file is logged and skipped.
Preferences resolves its previews on the blocking pool for both system
appearances; rendering only reads those cached previews. Reopening Preferences
cancels publication of any older reload still in flight.

## JSON format

Schema version is `1`. A file is an object:

| Field | Required | Meaning |
| --- | --- | --- |
| `schema` | yes | Must be `1`. Any other number is skipped. |
| `id` | yes | 1–64 characters: an ASCII letter or digit, then letters, digits, `-` or `_`. Stored as `appearance.theme` when chosen. |
| `name` | yes | The label in Preferences. |
| `appearance` | no | `"dark"` or `"light"`. Chooses the kit mode and the built-in palette a missing token falls back to. Omitted means the parent's appearance, or the system appearance when nothing in the chain sets it. |
| `extends` | no | Another theme's `id`, not a path. |
| `tokens` | no | Colour tokens for every surface. |
| `editor` | no | Tokens that apply only to the SQL editor palette. |
| `results` | no | Tokens that apply only to the result-grid palette. |

Unknown object keys are ignored, so a newer file can add a key an older
build does not read. A value that is not an object, or JSON that does not
parse, is skipped and a warning is logged. The application does not crash.
The ids `system`, `light` and `dark` are reserved for the persisted built-in
choices and are rejected as file ids.

A colour is `#RGB`, `#RRGGBB` or `#RRGGBBAA`. An alias is `{other.token}`
and names a token, not a hex value. These tokens store alpha in the low
byte and are painted with `rgba`: `color.scrim`, `search.match.background`,
`scrollbar.background`, `scrollbar.thumb`, `editor.search.background`.
Every other token is opaque and is painted with `rgb`.
RGB literals on an alpha token gain full opacity (`ff`). An eight-digit
literal on an opaque token uses its RGB bytes and drops alpha. Aliases
convert between these representations too, preserving RGB and preserving
alpha only when both tokens carry it.

## Resolution

Precedence, from lowest to highest:

1. The built-in palette of the chain's appearance (dark or light).
2. `tokens` of each theme in the chain, parent first.
3. The user's `tokens` overrides.
4. `editor` or `results` of each theme in the chain, parent first.
5. The user's `editor` or `results` overrides.

A missing token keeps the built-in value. An unknown token name, a colour
that does not parse, and an alias to an unknown token are skipped; the
token keeps its previous value and a warning is logged. Aliases are
resolved after the literals of each layer and retained for later layers,
so inherited aliases follow a target changed by a child or user override.
A later literal replaces an inherited alias. A cycle (`A` → `B` → `A`,
or `A` → `A`) keeps the previous value and warns. The same rules apply to
user overrides. Editor and results sections have independent alias graphs.

`extends` is one parent. The chain is parent first, then the child, at most
8 themes. A cycle, a missing parent, a chain longer than that, or a chosen
id that is not loaded falls back to the built-in palette. Light and Dark
fall back to that built-in appearance. System and a named id fall back to
the system appearance. User overrides are still applied on that fallback.
The same inputs always produce the same palette.

## Preference

`appearance.theme` in `config.db`'s `settings` table is `"system"` (the
default), `"light"`, `"dark"`, or a theme id. `"light"` and `"dark"` are
the bundled files. A missing value, a non-string, an empty string, or an id
that could not be a file name reads as `"system"`.

`appearance.theme_overrides` is only colour:

```json
{
  "tokens": { "color.accent": "#2266aa" },
  "editor": { "sql.keyword": "{color.accent}" },
  "results": { "grid.header": "#1b2028" }
}
```

Anything that is not that object is no override. The application does not
write this key from Preferences in this delivery; a file or a later setting
can. Font size stays in `editor.font_size`.

System follows the operating system, including a change while the
application is running. Light, Dark and a named theme stay as chosen.

## Tokens

The bundled files set every token. An alias is preferred where the colour
is the same as another token.

### Global

`color.background`, `color.panel`, `color.surface`, `color.hover`,
`color.border`, `color.border.strong`, `color.chrome`,
`color.input.border`, `color.text`, `color.muted`, `color.faint`,
`color.accent`, `color.on-accent`, `color.accent.hover`,
`color.accent.background`, `color.selection`, `color.scrim`.

Hue roles used by charts, connection marks and a few states:
`color.blue`, `color.purple`, `color.red`, `color.amber`, `color.teal`,
`color.pink`, `color.slate`, `color.neutral`.

`chart.fill`, `chart.track`, `chart.null`, `chart.empty`.
`search.match.text`, `search.match.background`.

### Components

`button.foreground`, `button.hover.background`, `button.hover.foreground`,
`button.primary.background`, `button.primary.foreground`,
`button.primary.hover`.

`menu.background`, `menu.foreground`, `menu.hover`.
`dialog.background`, `dialog.border`.
`tooltip.background`, `tooltip.foreground`.
`tab.bar`, `tab.active`, `tab.foreground`, `tab.active.foreground`.
`tree.background`, `tree.hover`, `tree.selection`.
`scrollbar.background`, `scrollbar.thumb`, `scrollbar.thumb.hover`.

Kit menus and tooltips share `popover` and `popover_foreground`, which are
filled from `menu.background` and `menu.foreground`. Independent
`tooltip.*` and `dialog.*` tokens are parsed but do not yet have consumers;
see the deviations below.

### SQL editor

`editor.background`, `editor.foreground`, `editor.caret`,
`editor.selection`, `editor.active-line`, `editor.line-number`,
`editor.active-line-number`, `editor.gutter`, `editor.search.background`,
`editor.search.text`.

The bundled editor chrome matches the kit's editor, which is darker than
`color.background` in the dark theme. The panel around the editor stays
`color.background`.

Syntax, from the highlighter the editor actually uses:
`sql.keyword`, `sql.function`, `sql.type`, `sql.string`, `sql.number`,
`sql.comment`, `sql.parameter`, `sql.operator`, `sql.identifier`.

### SQL results

`grid.background`, `grid.header`, `grid.line`, `grid.hover`,
`grid.selected`, `grid.focus`, `grid.header.text`, `grid.type.text`,
`grid.row.alt`, `grid.change`, `grid.written`, `grid.error`.

`value.text`, `value.number`, `value.temporal`, `value.binary`,
`value.null`.

`grid.row.alt` is painted only when it differs from `grid.background`.
The bundled themes set them equal, so rows are not striped.

### States

`state.success`, `state.warning`, `state.error`, `state.info`,
`state.hint`, `state.loading`, `state.execution`, `state.disabled`,
and the soft fills `state.success.background`, `state.warning.background`,
`state.error.background`, `state.info.background`.

`state.execution` is the colour of a statement that is running.
`state.loading` is the spinner. `state.disabled` is a control that cannot
be used. The ready action keeps `color.accent`.

## A new theme

`ocean.json`, placed in a themes directory. It inherits Jykre Dark and
changes the accent. Every other token stays the bundled dark value.

```json
{
  "schema": 1,
  "id": "ocean",
  "name": "Ocean",
  "appearance": "dark",
  "extends": "jykre-dark",
  "tokens": {
    "color.accent": "#3d8bfd",
    "color.on-accent": "#04101f",
    "color.accent.hover": "#6aa6ff",
    "color.accent.background": "#1b2a40",
    "color.selection": "#1e3a5f"
  },
  "editor": {
    "sql.keyword": "{color.accent}"
  },
  "results": {
    "grid.selected": "#1e3a5f"
  }
}
```

Open Preferences and choose Ocean. The window updates without a restart.
Buttons, charts and the success state that alias `color.accent` follow it.
The editor keyword follows the `editor` section only; the rest of the
window keeps the purple keyword.

To change a token everywhere, set it in `tokens`. To change it on one
surface, set it in `editor` or `results`.

## Overrides

User overrides use the same token names and the same colour or alias
syntax. They win over the selected file. They do not change which theme is
selected. They are stored in `appearance.theme_overrides`, not in the theme
file, so replacing a file does not wipe them.

## Adding a token

1. Add the field and its token name in `palette_fields!` in
   `src/theme/palette.rs`, and a value in both `DARK` and `LIGHT`.
2. Set it in `themes/jykre-dark.json` and `themes/jykre-light.json`.
   Prefer an alias when the colour already exists.
3. If the low byte is alpha, add the name to `carries_alpha`.
4. Read it from `colors()`, `editor_colors()` or `results_colors()` at the
   call site. Do not add a token that only renames one that already exists.
5. If the kit paints that role, map the token in `src/app/theme.rs` inside
   the `Theme::update` that follows `Theme::change`.

A token that is not in `palette_fields!` is unknown and is skipped.

## Validation and fallback

| Problem | Result |
| --- | --- |
| Directory missing | Skipped. Bundled themes remain. |
| File is not JSON, or `schema` is not 1 | File skipped, warning logged, other files kept. |
| `id` or `extends` is not a valid id | File skipped. |
| Chosen id missing, parent missing, cycle, or chain longer than 8 | Built-in palette, `fell_back`, user overrides still applied. |
| Token missing | Built-in value of the chain's appearance. |
| Colour invalid, token unknown, alias unknown or cyclic | That token skipped, warning logged. |
| Settings value unreadable | System theme. The problem is reported with the other settings. |
| Overrides value is not the object above | No overrides. |

Warnings go to the log. They are not a dialog.

## Known exceptions

These stay hardcoded. They are not a second theme.

| Place | Why |
| --- | --- |
| `src/reports/export.rs` print HTML and chart SVG | A printed page has its own stylesheet. It is not the window theme. |
| `assets/icon/jykre.svg` | The product icon, not a window surface. |
| `src/grouped_tabs` test and picker fixtures | Fixture colours for tests, not the product. |
| Connection hues | The user picks a hue (`ConnectionColor`). `connection_color` resolves it from the theme when drawn. |
| Diagnostic underline backgrounds inside the kit | `StatusColors` fields that carry a diagnostic fill are private. Foreground fallbacks follow `theme.red`, `theme.yellow`, `theme.blue`, `theme.green` and `theme.cyan`, which this system sets from `state.error`, `state.warning`, `color.blue`, `state.success` and `color.teal`. |
| The kit's find bar | It has no search-specific colour. `editor.search.*` is on the editor palette for our highlighter and for a later hook. Result matches use `search.match.*`. |

## What a theme does not change

Font family, SQL font size (`editor.font_size`, 12–24), density, corner
radius (kept at 5), panel widths and layout. `Theme::change` reloads the
kit theme and would reset the SQL font; `theme::apply` writes the previous
size back.

## Known deviations

| Standard | Current code |
| --- | --- |
| Each declared component token affects its component | `dialog.background`, `dialog.border`, `tooltip.background` and `tooltip.foreground` are parsed but not wired to their surfaces. App dialogs still use global panel and border tokens; kit tooltips share the menu's popover colours. |
| Long collections are virtualized | Preferences caches resolved previews but builds all theme cards. Large catalogs still increase the element count of that page. |
