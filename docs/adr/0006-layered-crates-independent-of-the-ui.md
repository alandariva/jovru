# ADR-0006: Layered crates independent of the UI, with a static driver registry

- **Status:** Accepted (recorded 2026-11-02 for a decision already in effect)
- **Date:** 2026-11-02

## Context

Drivers, storage and credentials must be testable without a window, reusable
by other front ends (examples, a future CLI), and isolated from each other so
that adding a database does not touch the others.

## Decision

- `jykre-core` holds contracts and types, with no I/O dependencies.
- Each driver is its own crate depending only on `jykre-core`; so are
  `jykre-storage` and `jykre-secrets`. None of them depends on GPUI.
- Only the `jykre` crate composes the drivers, statically, in
  `drivers::builtin_registry()`. There are no dynamic plugins.
- `cargo xtask check` enforces these edges from `cargo metadata`, including
  dev and build dependencies.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| One crate | Simple builds | Everything compiles GPUI; boundaries erode | Tests and reuse need the split |
| Dynamic driver plugins | Third-party drivers | ABI instability, security surface | No need yet |

## Consequences

- Positive: backend tests run without GPUI; a new database is one crate and
  one registration line.
- Negative: more crates and `Cargo.toml` files to keep in sync.
- Required: the [dependency rules](../reference/architecture.md#dependency-rules).
