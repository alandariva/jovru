# JSON theme implementation review

Reviewed on 2026-11-10 against the uncommitted JSON theme implementation.
This is review evidence; the implemented behaviour is described in
[Themes](../reference/themes.md).

## Findings and changes

| Priority | Finding | Resolution |
| --- | --- | --- |
| P1 | Inherited aliases were converted to colours before child and user layers. For example, Ocean changed `color.accent` but its inherited primary button, chart and success colour stayed green. | Retain alias definitions across layers and resolve them again after changes. Editor and results have independent graphs. A literal replaces an alias. Regression tests cover child, user and surface overrides. |
| P1 | Colour parsing lost the distinction between RGB and RGBA storage. `#123456` on a scrim was painted as `#00123456`; eight-digit colours and aliases could also shift opaque tokens' channels. | Normalize literals and aliases to the destination token's representation. Tests cover short RGB, RGB, RGBA, transparent black and aliases in both directions. |
| P2 | Preferences resolved every catalog entry inside `appearance_page`, on every render. | Prepare previews for both system appearances on the blocking pool during reload. Rendering reads those previews. Keep the reload task on the view so a newer reload cancels publication by an older one. |
| P2 | File ids `system`, `light` and `dark` could appear in the picker but deserialize as built-in choices after a restart. | Reject those reserved file ids and test their absence from the catalog and picker ids. |
| P2 | Theme files were read into an unbounded string; unreadable directories disappeared silently. | Read at most 1 MiB plus a sentinel byte, skip oversized files and warn on directory errors other than absence. Tests also cover filename and directory precedence. |
| P2 | Concurrent `install` calls could publish a generation after releasing the palette lock and cache a superseded palette as current. | Publish the generation and the installing thread's cache while holding the same lock as the palette replacement. |
| P2 | The component kit's dedicated primary-button fields retained the kit defaults. | Copy the resolved primary-button tokens to those fields as well as the general primary role. |
| P2 | Alternate grid colours used the source row index, so sorting could leave adjacent displayed rows with the same stripe. | Use the displayed row position for alternation. |
| P2 | Missing named themes were labelled as the fallback's bundled name and as always dark/light. | Keep the requested id in the picker and describe its current system fallback. |
| P1 | Release measurements exceeded the 8 ms grid frame budget. Repeated palette-handle clones and RGB-to-HSLA conversions occurred throughout cell construction. | Share one palette snapshot and HSLA projection across a visible batch, pass it to row and value painting, and snapshot header colours once per header. Deferred drag callbacks continue reading the current palette. Performance results below distinguish the prior implementation from the reviewed code. |

The SQL highlighter now takes one palette snapshot per highlighting request,
instead of cloning the active handles for each classified token. No
dependency, SQLite migration, core contract or ADR changed.

## Validation

`cargo xtask check`: **passed**, including architecture, SQL catalogs,
formatting, Clippy, workspace tests and doctests, and the SQL-language checks
without default features. All **19 theme tests passed** (seven added in this
review). `git diff --check` passed. No test was removed or weakened.
The binary also has a regression test for the grid's paint projection:
value families, NULL, match alpha and optional alternate-row colours must
keep the resolved palette's values.

Full gate, audit and live integration: **not run**; this batch changes no
driver, execution contract or dependency and is not release preparation.
The gate keeps its existing ignored live/vault tests and the Node example
(`needs node on PATH`); they were not counted as executed checks.

The first gate attempt stopped at Clippy's
`cloned_ref_to_slice_refs` lint in a new fixture test; the test now uses
`std::slice::from_ref` without suppressing the lint.

Native visual verification could not start: both the `computer-use` skill's
Sky API and the unified computer-use inventory reported that the native
pipe was unavailable (Windows error 2). Theme switching, keyboard focus and
font-size preservation in the actual window therefore remain unverified.
Automated resolver and persistence checks do not replace that check.

`cargo build --release --locked`: **passed** for the final code. A stale
release library from the isolated baseline build was cleared with
`cargo clean -p jykre --release` before rebuilding the final executable.

## Performance measurements

The existing executor was run as
`JYKRE_PROBE=1 jykre.exe --test-performance=grid`, with synthetic rows, a
1480 × 920 window at scale 1, Windows x86_64 and 20 logical CPUs. No build
ran during either measured run. The baseline is an unmodified archive of
commit `808dd375892349c5d389b9d8468e46f5f617019e`, before JSON themes; the
reviewed run includes the final HSLA projection. This is not a comparison
against the initially submitted, uncommitted JSON implementation.

Worst frame p95 across selection, selection near the last row, scrolling
and horizontal panning, for each size:

| Synthetic grid | Before JSON themes | Reviewed code | Budget | Reviewed status |
| --- | --- | --- | --- | --- |
| 1,000 × 5 | 4.70 ms | 4.86 ms | ≤ 8 ms | Passed |
| 100,000 × 5 | 5.59 ms | 4.85 ms | ≤ 8 ms | Passed |
| 1,000 × 400, three frozen columns | 8.82 ms | 8.41 ms | ≤ 8 ms | Failed |

The reviewed code passed **11 of 12** frame checks. Scrolling the 400-column
grid remained at **8.4144 ms p95**. That path already failed before JSON
themes (**8.6468 ms**); the baseline failed three wide-grid phases. The
budget is unchanged and overall grid-budget compliance is **not passed**.
The remaining wide-grid failure requires further work; these measurements
do not justify calling all performance checks successful.

Raw evidence: [baseline](json-theme-review-data/baseline.json) and
[reviewed code](json-theme-review-data/reviewed.json). These are single
sequential measurements on a shared desktop, not statistically isolated
benchmarks. Earlier runs before the final HSLA projection reached
9.54–22.87 ms on the wide grid under varying desktop conditions; they do
not establish a reliable before/after speedup for that optimization.

The executor does not load theme files or user settings, so this measures
the new palette access in the grid with its default dark palette, rather
than an interactive external-theme switch. External-file loading and
resolution are covered by unit tests; a native performance run with an
external theme remains unverified.

The first performance attempt used a hidden window. It completed all phases
in 134 s and measured stalls of 11.84–12.96 ms, but recorded no frames.
Those missing frame measurements are not passes. Although the report's
generic missing-sample message says the probe was off, its metadata confirms
`frame_probe: true`; hiding the window prevented frame samples. The run was
repeated in the foreground for the frame measurements.

## Known deviations

| Standard | Current code |
| --- | --- |
| Every declared component token controls its surface | `dialog.*` and `tooltip.*` are accepted without independent consumers. App dialogs use global panel/border colours and kit tooltips use the menu popover colours. This is now explicit in the theme reference. |
| Long collections are virtualized | Preferences still builds every theme card. The previews are cached, but a large catalog still increases the number of elements rendered. |
| Result-grid frames stay at or below 8 ms p95 | The final 400-column scrolling phase measured 8.41 ms p95. The pre-JSON baseline also exceeded the budget on that phase (8.65 ms). |
| Visual changes are checked in the application | The native computer-use service was unavailable during this review. |
