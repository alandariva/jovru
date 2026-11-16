# Decisions: Rust guidelines

Why this area is built as it is. Each entry records what was decided, why, and which alternatives lost. The rules themselves are in [Rust guidelines](../standards/rust-guidelines.md). Entries are numbered `D-NNNN`, and the numbers are stable.

<a id="d-0011"></a>
## D-0011: Workspace-wide lints for panics, output and async locks

*Accepted 2026-11-02 · implemented 2026-11-02*

**Decision.** The lints in [Lints](../standards/rust-guidelines.md#lints) are declared once in the root `Cargo.toml` (`[workspace.lints]`) and every crate opts in with `[lints] workspace = true`. A new `#[allow]` for them needs the maintainer's approval and a comment; the few invariant `unwrap`/`expect` calls are rewritten or get a local `#[allow]` whose comment proves the invariant.
**Why.** The rules against panics on production paths, stray output and locks held across `.await` were checked only in review, which misses things, and agents tend to add a "temporary" `unwrap`. The rules are mechanical, so the gate checks them and review can focus on design. Approved exceptions of 2026-11-02, beyond `xtask`: the command-line examples (`connect`, `drivers`) allow `print_stdout`, the benchmarks allow `expect_used` for their setup, and a test sink in the Databricks driver that must never be called uses `unreachable!` instead of `panic!`. Validation: `cargo xtask check` passes with the lints enabled and no blanket `allow`.
**Rejected.** Review only: no churn, but inconsistent for mechanical rules. `clippy::pedantic`: many checks, but noisy and opinionated; the targeted lints carry the value.
