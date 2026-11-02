# ADR-0010: Benchmarks with `criterion` for hot paths

- **Status:** Proposed
- **Date:** 2026-11-02

## Context

Performance budgets are measured by hand with a temporary harness, and unit
tests only catch gross complexity regressions. A gradual slowdown in a hot
path would go unnoticed until users feel it.

## Decision

- Add `criterion` as a dev-dependency, with benchmarks in `benches/` for:
  building the grid view (filter, sort, widths) at 100,000 rows, converting
  driver rows to result data, statement splitting on large scripts, and
  paging metadata children from a cache with 500,000 objects.
- `cargo xtask check --full` runs them in quick mode, only to prove they work.
- A task that touches one of these paths compares against a saved baseline
  (`cargo bench -- --save-baseline main` before, `--baseline main` after) and
  reports the result.
- The frame probe and heartbeat used for UI budgets move into the repository
  behind an environment variable, so UI measurements are reproducible.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Only time budgets in unit tests | No dependency | Coarse; catches only big regressions | Kept as a complement |
| `divan` | Lighter | Smaller ecosystem, fewer comparison tools | `criterion` has baselines built in |

## Consequences

- Positive: measurable, comparable numbers for the paths that matter.
- Negative: one dev-dependency and longer full-gate runs.
- Required after acceptance: the listed paths keep their benchmarks.
