# Decisions: Performance

Why this area is built as it is. Each entry records what was decided, why, and which alternatives lost. The rules themselves are in [Performance](../standards/performance.md). Entries are numbered `D-NNNN`, and the numbers are stable.

<a id="d-0010"></a>
## D-0010: Benchmarks with `criterion` for hot paths

*Accepted 2026-11-02 · implemented 2026-11-02*

**Decision.** `criterion` (0.8, without default features) is a dev-dependency, and `benches/hot_paths.rs` benchmarks the hot paths listed under [Benchmarks](../standards/performance.md#how-to-measure): they keep their benchmarks. `cargo xtask check --full` runs them in quick mode only to prove they work; a task that touches one compares against a saved baseline and reports the result. The frame probe and heartbeat live in the repository, behind `JOVRU_PROBE=1` (`src/app/probe.rs`), and log their summary every 5 s, so UI measurements are reproducible.
**Why.** Budgets had been measured by hand with a temporary harness, and unit tests catch only gross complexity regressions, so a gradual slowdown in a hot path would go unnoticed until users felt it. Baselines (`--save-baseline main` before, `--baseline main` after) give comparable numbers. The cost is one dev-dependency and longer full-gate runs.
**Rejected.** Only time budgets in unit tests: no dependency, but coarse and catching only big regressions; kept as a complement. `divan`: lighter, but a smaller ecosystem and fewer comparison tools, while `criterion` has baselines built in.
