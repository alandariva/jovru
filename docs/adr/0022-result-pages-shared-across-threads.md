# ADR-0022: Result pages shared across threads with `Arc`

- **Status:** Accepted (by the maintainer on 2026-11-07; implemented)
- **Date:** 2026-11-04
- **Authors:** AI agent, at the maintainer's request (decision approved by the maintainer on 2026-11-04)
- **Supersedes in part:** [ADR-0012](0012-result-grid-as-a-cached-entity.md), the point that a result page is shared as `Rc<ResultData>`

## Context

[ADR-0012](0012-result-grid-as-a-cached-entity.md) shares the page on screen
between the workspace, the grid, the chart and the CSV copy as one
`Rc<ResultData>`, never copied. `Rc` cannot leave the UI thread, so anything
that reads a whole page has to do it there:

- **Copy** of a large selection builds its text on the UI thread (about
  85 ms for 500,000 cells), the known deviation that keeps N031 partial
  ([Results](../reference/results.md#known-deviations)).
- **Export** of a result to a file ([Results](../reference/results.md#export)) has to
  write the pages the tab already holds (up to 256 MB) before the rows the
  session still keeps open. On the UI thread that would stall the window for
  seconds; copying the pages there first costs tens of milliseconds and twice
  the memory.

`ResultData` is plain owned data (`String`, `Vec<u32>`, `Vec<u64>`), already
`Send` and `Sync`.

## Decision

Result pages are shared as **`Arc<ResultData>`** instead of `Rc<ResultData>`:
`result_tabs::Page::data`, the pages a tab lets go of
(`ResultTabs::take_dropped`) and the grid's current result (`ResultGrid`).
Everything else in ADR-0012 stands: one shared value, never copied, rendered
only where visible.

- Work that reads a whole page takes a clone of the `Arc` to the database
  runtime or GPUI's background executor, never the UI thread.
- A page nobody shows any more is released with `backend::release`, which
  now takes the `Arc` itself: the memory is freed wherever its last holder
  drops it, off the UI thread when that holder is a background task.
- Tasks that must not keep a released page alive (the grid's statistics)
  hold a `Weak` and stop when it is gone, as they did with `Rc`.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Keep `Rc` and copy pages in slices, one per frame, to the runtime | No change to how pages are shared | More code in the views; an export of 256 MB of pages takes hundreds of frames and holds a second copy of each slice | Slower and more complex for the same result |
| Keep `Rc` and copy a whole page before exporting | Simple | A memcpy of up to 256 MB on the UI thread and twice the memory while it lasts | Breaks the frame budget the ADR exists for |
| Convert pages to `Arc` only when an export starts | No change elsewhere | The conversion is the same copy | Same cost as above |

## Consequences

- Positive: a page can be read off the UI thread without copying it. Export
  writes held pages on the runtime's blocking pool, and copying a large
  selection can move off the UI thread too (N031).
- Negative or costs: cloning or dropping a handle is an atomic operation
  instead of a plain one; it happens a few times per page shown, never per
  row or per frame.
- Required: no `Rc<ResultData>` comes back. A background task holding a page
  is bounded by the work it does (it ends with the export, the copy or the
  statistic) and is cancellable.

## How to validate

- The gate passes with the grid, result tab and export tests unchanged in
  what they assert.
- The frame probe shows no change for scrolling, hovering and selecting in a
  100,000-row result ([Performance](../standards/performance.md#budgets)).
- Exporting a tab that holds several pages keeps the UI thread under the
  stall budget (heartbeat, `stall_max_ms`).
