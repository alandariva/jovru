# Proposal: `rmcp` for the assistant's MCP server

**Date:** November 10, 2026
**Approval:** Approved by the maintainer on 2026-11-10 (Q-19, option A),
together with [ADR-0042](../adr/0042-mcp-server-for-the-assistant-tools.md).
**Implementation:** In progress (crate added; the server is not written yet).
**For:** not assigned to a release
([Q-1](assistant-mcp.md#12-open-questions-for-the-maintainer) of the
[MCP plan](assistant-mcp.md))

Facts below carry their source:

- **[cio]**: the crates.io API for `rmcp` and for version 3.9.0.
- **[gh]**: the GitHub API for `modelcontextprotocol/rust-sdk`, and the tag
  `rmcp-v3.9.0` (`Cargo.toml`, `LICENSE`, `README.md`).
- **[src]**: `rmcp` 3.9.0 as published, including
  `src/transport/streamable_http_server/tower.rs`.
- **[docs]**: docs.rs for `rmcp` 3.9.0.
- **[rustsec]**: `rustsec/advisory-db` `crates/rmcp/` on `main`.
- **[lock]**: this worktree's `Cargo.lock` after `cargo fetch`.
- **[deny]**: `cargo deny --locked check` in this worktree.

All were read on 2026-11-10.

## Request

Add [`rmcp`](https://crates.io/crates/rmcp) 3.9.0 (`Apache-2.0`) as a
workspace dependency of the `jykre` package. Nothing calls it until
`jykre::mcp` is written. No dummy use is added to silence a warning.

```toml
# [workspace.dependencies]
# MCP server for the assistant's tools (ADR-0042, Q-19 option A,
# approved 2026-11-10, docs/plans/rmcp-dependency.md). Server and
# streamable HTTP only. No client, elicitation, stdio or reqwest:
# the `reqwest` feature turns on reqwest's `rustls`, which compiles
# aws-lc-rs, and `auth-client-credentials-jwt` depends on
# jsonwebtoken with `aws_lc_rs`.
rmcp = { version = "3.9.0", default-features = false, features = ["server", "transport-streamable-http-server"] }
```

```toml
# [dependencies] of the jykre package
# MCP server (ADR-0042, Q-19). Nothing calls it until `jykre::mcp` is written.
rmcp.workspace = true
```

## Why

[Section 6](assistant-mcp.md#6-transport) of the MCP plan asks for streamable
HTTP on loopback, stateless: no `Mcp-Session-Id`, an `initialize` that still
returns a description and no session id, and `server/discover` when the
revision has it. The protocol layer (versions, cancellation, progress, HTTP
and SSE framing) is the kind of code the dependency policy says not to
hand-roll. The maintainer chose option A, `rmcp`, on 2026-11-10 (Q-19).
Option B, a JSON-RPC layer of our own over `hyper`, was not taken.

`rmcp` 3.9.0 is the current stable release **[cio `max_stable_version`]**.
There is no git dependency. Its README at tag `rmcp-v3.9.0` says the SDK
implements the MCP **`2026-07-28`** specification and stays compatible with
**`2025-11-25`** and earlier **[gh README]**. The same README's stateless
section calls `2026-07-28` a draft (SEP-2567). The MCP plan, the same day,
calls that revision a release candidate. The specification site's own
publication status was **not verified**.

What 3.9.0 actually does, from `tower.rs` at that tag **[src]**:

| The plan's shape | What 3.9.0 does |
| --- | --- |
| Stateless streamable HTTP | Present. `StreamableHttpService` is a Tower service. It does not listen by itself; `axum` is only a dev-dependency of `rmcp` and is not pulled **[gh Cargo.toml, lock]** |
| `2026-07-28` without a session | Requests negotiated as `2026-07-28` are served statelessly even when `legacy_session_mode` is on. The README says that path mints no `Mcp-Session-Id`, opens no standalone GET/DELETE stream and does not resume with `Last-Event-ID` **[gh README, src]** |
| `server/discover` | Present. `ClientRequest::DiscoverRequest` is served by `serve_negotiated_request_directly`, which does not insert `Mcp-Session-Id` **[src]**. `DiscoverResult` carries `instructions` and stores the server implementation, including `title`, through `set_server_info` in the result metadata **[docs]** |
| `initialize` returns a description and no session id | `InitializeResult` has `instructions` and `server_info`, and `Implementation` has `title` **[docs]**. An `initialize` request is classified as legacy whatever version its params name **[src `is_legacy_request`]**. With the default `legacy_session_mode: true`, that path creates a session and the response inserts `Mcp-Session-Id`. That insert is the only one in `tower.rs`. With `with_legacy_session_mode(false)`, `initialize` takes the stateless branch and that header is not inserted |
| Ignore a session id the client sends | On the stateless branch the header is not read **[src `handle_post`]**. On the default (session) branch a legacy request carrying one is routed to that session |
| POST only | With `legacy_session_mode` false and no event store, `handle` allows only `POST`. The default allows `GET`, `POST` and `DELETE` **[src]** |

The gap against the plan is the default, not a missing transport. A server
built on `StreamableHttpServerConfig::default()` still mints
`Mcp-Session-Id` for `initialize`. The plan's shape is
`with_legacy_session_mode(false)` (and, when the server is written,
`max_request_body_bytes` of 1 MiB against this crate's default of 4 MiB, and
Origin validation, which stays off while `allowed_origins` is empty unless
`enforce_origin_validation` is called). Host validation already defaults to
`localhost`, `127.0.0.1` and `::1`. None of that is set here: the server is
not written. The JSON-RPC layer is not hand-rolled.

## License

The published crate's `license` field is `Apache-2.0` **[cio, src
`Cargo.toml`]**. `Apache-2.0` is in `deny.toml`'s allow-list.

The repository `LICENSE` at tag `rmcp-v3.9.0` is a transition document: it
prints the Apache-2.0 text, states that contributions not yet relicensed
remain MIT, and says documentation other than specifications is CC-BY-4.0
**[gh LICENSE]**. `cargo deny` reads the crate's SPDX field, which is
`Apache-2.0` alone. CC-BY-4.0 was not added to the allow-list.

`sse-stream` 0.2.6, the one new transitive crate, is `MIT OR Apache-2.0`
**[registry `sse-stream-0.2.6/Cargo.toml`]**. Both sides are already allowed.

## Health

- 3.9.0 was published on 2026-11-05 by `alexhancock`, not yanked, crate size
  677,294 bytes, checksum
  `d24a7e92f4bcaa36b97fa93c4fa7ca8f3e669eced949345b3462dba333c7f909`,
  241,064 downloads of that version **[cio]**. The previous release on the
  crates.io version list is 3.8.0 on 2026-10-26 **[cio]**.
- The crate has 47,318,308 downloads and `recent_downloads` 27,371,704, 72
  versions, created 2025-03-16, `max_stable_version` 3.9.0 **[cio]**.
- `modelcontextprotocol/rust-sdk`: not archived, default branch `main`,
  pushed 2026-11-08, `updated_at` 2026-11-10, 4,106 stars, 706 forks,
  `open_issues_count` 68 (GitHub counts pull requests in that field)
  **[gh]**. The repo API reports the license as `NOASSERTION`; the crate
  field is `Apache-2.0`.
- Minimum Rust 1.88 **[cio]**. Ours is 1.95.
- `rustsec/advisory-db` `crates/rmcp/` lists four advisories **[rustsec]**.
  None of them still affect 3.9.0:

  | Advisory | What | Patched |
  | --- | --- | --- |
  | RUSTSEC-2026-0189 | Streamable HTTP server did not validate `Host` (DNS rebinding) | `>= 1.4.0` |
  | RUSTSEC-2026-0461 | Unbounded tool-schema cache in the Streamable HTTP server | `>= 3.6.0` |
  | RUSTSEC-2026-0457 | OAuth client SSRF filter checked host strings, not resolved addresses | `>= 3.7.0` (and `< 2.0.0` unaffected). The `auth` feature is not enabled |
  | RUSTSEC-2026-0462 | Streamable HTTP clients buffered a response body with no limit | `>= 3.9.0`. The client features are not enabled |

- `cargo deny --locked check` on 2026-11-10: advisories ok, bans ok,
  licenses ok, sources ok **[deny]**. No advisory was ignored and
  `deny.toml` was not edited. The existing duplicate-version warnings and
  the yanked `async-executor` 1.14.0 (GPUI) remain; they are not from
  `rmcp`.

`sse-stream`'s own release history was not reviewed separately. It is in
the tree `cargo deny` checked.

## Weight

`cargo fetch` locked two packages and no others: `rmcp` 3.9.0 and
`sse-stream` 0.2.6 **[lock]**. `cargo update -p rmcp` could not run before
that, because the package was not in the lock yet. No existing version was
bumped. The only other lock edit is `schemars` 1.2.3 gaining `chrono`: the
`server` feature turns on `schemars`' `chrono04` feature **[src
`Cargo.toml`]**. `chrono` stays 0.4.45, already in the lock.

| | Crates |
| --- | --- |
| New | `rmcp` 3.9.0, `sse-stream` 0.2.6 |
| Already in `Cargo.lock`, now also used by `rmcp` | `async-trait`, `base64` 0.23.1, `bytes`, `chrono` 0.4.45, `futures`, `http`, `http-body`, `http-body-util`, `indexmap` 2.14.2, `libc`, `pastey` 0.2.3, `pin-project-lite`, `rand` 0.10.3, `schemars` 1.2.3, `serde`, `serde_json`, `sha2` 0.11.1, `thiserror` 2.0.21, `tokio` 1.54.1, `tokio-stream`, `tokio-util`, `tower-service`, `tracing`, `uuid` **[lock]** |
| Not pulled by these features | `hyper`, `hyper-util`, `reqwest`, `rustls`, `axum`, `aws-lc-rs`, `rmcp-macros`, `process-wrap`, `oauth2`, `jsonwebtoken` **[lock]** |

`sha2` 0.11.1, `schemars` 1.2.3, `rand` 0.10.3, `base64` 0.23.1 and `pastey`
0.2.3 were already in the lock beside older majors (`sha2` 0.10.9 through
our own fingerprint dependency, `schemars` 0.9, `rand` 0.8 and 0.9, `base64`
0.22, `pastey` 0.1). `rmcp` does not add another copy of any of them.
`sha2` 0.11 is an unconditional dependency of `rmcp` (skill digests and
request-state sealing), not a feature we can turn off **[gh Cargo.toml]**.

Tokio stays the workspace's 1.54.1. `rmcp` asks for `sync`, `macros`, `rt`,
`time` and, through `transport-async-rw`, `io-util`, which the workspace
already enables. `tokio-util`'s `codec` feature is unified in; its lock
entry gained no dependency. No second Tokio, and no TLS stack: the server
transport is plain HTTP. `hyper` 1.12.0 stays the copy already in the tree
through `reqwest`.

`hyper` 1.12.0 and `hyper-util` 0.1.21, already in the lock, are now direct
dependencies of `jykre` so the Tower `StreamableHttpService` can bind a
loopback socket. No other HTTP stack was added.

`reqwest`'s feature on `rmcp` is `reqwest?/rustls`. In this workspace that
feature compiles `aws-lc-rs`, which `deny.toml` bans. It is not enabled.
`auth-client-credentials-jwt` depends on `jsonwebtoken` with the feature
`aws_lc_rs` **[gh Cargo.toml]**. It is not enabled. `cargo deny`'s ban
check passed, so `aws-lc-rs` is not in the resolved graph **[deny]**.

The workspace forbids `unsafe_code` in our crates. That lint is unchanged,
and no `#![allow]` was added. The published `rmcp` 3.9.0 `src/` has one
`unsafe` block, `libc::fcntl` in `transport/io.rs`, and that module is
compiled only with `transport-io`, which this set does not enable **[src]**.
`sse-stream` 0.2.6 `src/` has no `unsafe` **[registry]**.

## Minimal features

`default-features = false`. The default is `base64`, `macros` and `server`
**[gh Cargo.toml]**. Enabled:

- `server`: the server types and the tool system. It turns on
  `transport-async-rw` (a generic async read/write codec, not stdio),
  `schemars`, `pastey` and `uuid`. `schemars` cannot be left off while
  `server` is on.
- `transport-streamable-http-server`: the Streamable HTTP server. It turns
  on `server-side-http` (`http`, `http-body`, `http-body-util`, `bytes`,
  `sse-stream`, `rand`, `tower-service`, `base64`, `uuid`, `tokio-stream`)
  and the session types the service is built on.

Left off, on purpose:

- `client`, `elicitation`, `transport-io` (stdio), `transport-child-process`,
  `transport-streamable-http-client` and its reqwest and unix-socket
  variants, `auth`, `auth-client-credentials-jwt`,
  `auth-enterprise-managed`, `reqwest`, `reqwest-native-tls`,
  `reqwest-tls-no-provider`, `request-state`, `macros`, `which-command`.
- `macros` would pull `rmcp-macros` and the `#[tool]` macro. A
  `ServerHandler` does not need it. It can be a later, separate decision
  when the server is written.

`transport-async-rw` is not a second transport we opted into. It is part of
the `server` feature. Stdio is `transport-io` and stays off.

## What was checked

- crates.io API for the crate and for 3.9.0; the tag `rmcp-v3.9.0` on
  GitHub (manifest, license, README, HTTP server source); docs.rs for
  `InitializeResult`, `Implementation` and `DiscoverResult`; the four
  rustsec files under `crates/rmcp/`.
- `cargo update -p rmcp --precise 3.9.0` and `cargo update -p rmcp` both
  failed with "did not match any packages", before the lock had `rmcp`.
- `cargo fetch` then locked `rmcp` 3.9.0 and `sse-stream` 0.2.6 only.
- `cargo deny --locked check`: passed (advisories, bans, licenses,
  sources). `cargo xtask check` was not run; other work is editing source.
- The specification site's publication status for `2026-07-28` was not
  verified beyond the crate's README. Whether `schemars`' `chrono04`
  feature enables a chrono feature the rest of the tree did not already
  build was not measured; the lock did not add a second `chrono`.
