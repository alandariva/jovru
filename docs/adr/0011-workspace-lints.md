# ADR-0011: Workspace-wide lints for panics, output and async locks

- **Status:** Proposed
- **Date:** 2026-11-02

## Context

The rules against panics on production paths, stray output and locks held
across `.await` ([Rust guidelines](../rust-guidelines.md)) are checked only in
review. Review misses things, and agents tend to add a "temporary" `unwrap`.

## Decision

Declare lints once in the root `Cargo.toml` and opt every crate in with
`[lints] workspace = true`:

```toml
[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
todo = "deny"
dbg_macro = "deny"
print_stdout = "deny"
print_stderr = "deny"
await_holding_lock = "deny"
await_holding_refcell_ref = "deny"
```

- Tests opt out of `unwrap_used` and `expect_used` with
  `#[cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]`.
- `xtask` may print (it is a command-line tool) and allows `print_stdout` and
  `print_stderr` explicitly.
- The few remaining invariant `unwrap`/`expect` calls are rewritten or get a
  local `#[allow]` with a comment proving the invariant, approved in review.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Review only | No churn | Inconsistent | The rules are mechanical; a tool should check them |
| `clippy::pedantic` | Many checks | Noisy, opinionated | Targeted lints carry the value |

## Consequences

- Positive: the gate enforces the rules; review can focus on design.
- Negative: an initial task to fix existing violations.
- Required after acceptance: a new `#[allow]` for these lints needs the
  maintainer's approval.

## How to validate

`cargo xtask check` passes with the lints enabled and no blanket `allow`.
