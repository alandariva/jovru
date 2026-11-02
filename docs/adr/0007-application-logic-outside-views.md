# ADR-0007: Application logic outside views

- **Status:** Proposed
- **Date:** 2026-11-02

## Context

The application's views live in `src/mock/`, a name left over from the visual
prototype, next to the demo data. Logic that needs no window lives inside
them: statement splitting and SQL preview checks in `src/mock/sql.rs`, query
orchestration and its state in `Workspace`, with a run tracked by several
booleans. Such logic can only be tested through the view, and a second
interface (a CLI, an automated check) would have to repeat it.

## Decision

- Rename `src/mock/` to `src/app/`. Demo data moves to `src/app/demo/` and
  stays behind `JYKRE_DEMO=1`.
- Move logic that needs no GPUI into the `jykre` library, in modules that
  views call:
  - `jykre::sql`: statement splitting, normalisation and preview checks. Pure
    and synchronous: the same input gives the same output.
  - `jykre::query`: a run's state machine
    (`Idle | Running | Cancelling | Done | Failed | Cancelled`), statement
    statuses and the batch of results, driven by `backend`.
- Identifier quoting for previews moves from `backend::preview_sql` to the
  drivers' dialect, removing the `match` on `DriverKind`.
- The result grid becomes its own entity (`ResultGrid`), embedded in the
  workspace with `.cached(...)`.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Keep the current layout | No churn | Logic stays untestable without a window; the `mock` name misleads | The cost grows with every feature |
| A separate `jykre-app` crate for services | Strong boundary | Another crate and more wiring for a single front end | Library modules are enough for now |

## Consequences

- Positive: SQL handling and run states get unit tests; views shrink; one
  state value drives the footer, gutter and buttons.
- Negative: a large mechanical move; it must be split into small tasks
  (rename, then `sql`, then `query`, then `ResultGrid`), each behaviour-neutral
  and checked in the application.
- Required after acceptance: new logic follows this layout.

## How to validate

Each step passes the gate and a visual check of the affected flow; the run
state machine has unit tests for every transition, including cancellation and
stale results.
