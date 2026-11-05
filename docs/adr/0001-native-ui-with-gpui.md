# ADR-0001: Native UI with GPUI through `gpui-kit`

- **Status:** Accepted (recorded 2026-11-02 for a decision already in effect)
- **Date:** 2026-11-02

## Context

The product promise is a database workspace that starts quickly, stays light
and answers immediately, written in Rust end to end. A WebView shell would add
a second runtime, an IPC boundary and a JavaScript heap that multiplies the
memory of large results.

## Decision

- The interface is drawn natively with GPUI, a GPU-accelerated Rust UI
  framework, consumed through `gpui-kit`, which pins a matching set of GPUI
  crates (`gpui-pre-*`) and provides components (`gpui-component`).
- There is no WebView, no IPC layer and no frontend language besides Rust.
- `gpui-kit` and the crates it pins are upgraded together, followed by a
  check of the whole application.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Tauri with a web frontend | Large ecosystem, familiar tooling | WebView memory, IPC serialization, two languages | Conflicts with the lightness goal |
| egui / iced | Pure Rust | Fewer ready components for an IDE-like layout (editor, docks, virtual lists) | GPUI fits a code-editor-like UI better |

## Consequences

- Positive: one language, one process, direct access to data without
  serialization, GPU rendering.
- Negative: GPUI is young and its API moves; upgrades need a manual check of
  the whole application. Linux and macOS builds are not validated yet.
- Required: views follow [UI guidelines](../ui-guidelines.md); in particular,
  long collections are virtualized because GPUI rebuilds a notified view on
  every frame.
