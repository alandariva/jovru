# ADR-0042: An MCP server for the assistant's tools

- **Status:** Accepted by the maintainer on 2026-11-10
- **Date:** 2026-11-10
- **Authors:** drafted from [docs/plans/assistant-mcp.md](../plans/assistant-mcp.md); accepted by the maintainer on 2026-11-10

## Context

[ADR-0028](0028-agentic-ai-assistant-over-the-metadata-cache.md) lets the
in-app assistant call three tools over one connection's metadata cache:
`search_objects`, `describe_relation` and `run_query`. It says new tools
read the local cache only unless an ADR says otherwise, and the
[Security](../standards/security.md#ai-assistant) standard sends what those
tools return to the provider the user chose. The maintainer wants the same
three tools to answer an external MCP client as well. That client is chosen
by the user and is not seen by Jykre. Its model, prompt and later use of
the answer are outside this application.

The tools, their bounds and the two approvals for `run_query` already
exist. What does not exist is a second destination for their answers, a
listening socket beside `jjp/1`, a vault account whose name is chosen at
run time, or an MCP library. Each of those crosses a crate, the stack or
the credential rule, so each needs this record
([Workflow](../standards/workflow.md#architecture-decisions)). The
[plan](../plans/assistant-mcp.md) is the specification for states, texts,
deadlines and limits. This record decides only the structure below. The
maintainer accepted it on 2026-11-10, with the plan and with Q-19.

## Decision

1. **A second destination for the same answers.** An external MCP client,
   chosen by the user and not seen by Jykre, may call the tools ADR-0028
   already defines. This extends ADR-0028. The in-app assistant is
   unchanged: it still sends requests to the provider the user chose, under
   that ADR. There is no new tool. The tools stay `search_objects`,
   `describe_relation` and `run_query`, implemented by `CacheTools`,
   `run::query_sql`, `guard::risk` and `released`. The MCP layer only
   adapts calls and answers.
2. **Loopback, stateless, only while the window is open.** The server binds
   `127.0.0.1` only, on port 47321 unless the user has set another. The
   transport is streamable HTTP. It is stateless: the server does not mint
   `Mcp-Session-Id` and ignores the header. `rmcp` 3.9.0 does that only
   with `legacy_session_mode` off; its default still mints the header on
   `initialize` and allows `GET` and `DELETE`. The body limit is 1 MiB,
   not the crate's 4 MiB default, and origin validation is turned on.
   The listener exists only while
   the window is open. Nothing listens when the server is off, when
   `JYKRE_DEMO` is set, or before the settings have been read. There is no
   TLS and no CORS. `Origin` and `Host` are checked against DNS rebinding.
   The token is accepted only as `Authorization: Bearer`. A token in the
   URL is refused.
3. **One token is one connection.** The token form is
   `jykmcp_<id>_<secret>`. `<id>` is 10 lowercase base32 characters from
   the alphabet `abcdefghijklmnopqrstuvwxyz234567`. It is not secret: it
   names the record and the vault account, and it is the prefix the user
   sees later. `<secret>` is 256 bits written as 64 lowercase hex digits,
   drawn from the secure random source of the `ring` provider rustls
   already installs. In Rust the secret is redacted in `Debug` and the type
   is not `Serialize`. The secret is stored only in the OS vault, service
   `JykreStudio`, account `mcp:token:<id>`, through a new `jykre-secrets`
   account type built only from a validated `<id>`
   ([ADR-0004](0004-os-vault-for-credentials.md)). It is never written to
   `config.db`, JSON, logs, telemetry, error text, `Debug` output, a URL
   or argv. The record without the secret is the existing `settings` row
   `assistant.mcp`. There is no new SQLite table and no schema migration.
   The setting is this JSON. A missing setting, or a value that is not
   this object, reads as the server off, port 47321 and no tokens. A
   missing or unknown field takes the default shown. The object in
   `tokens` is the shape of one record, not a token that exists by
   default. The secret is absent.

   ```json
   {
     "enabled": false,
     "port": 47321,
     "tokens": [
       {
         "id": "<10 lowercase base32>",
         "name": "<label, at most 60 characters>",
         "connection_id": "<ConnectionId as stored today>",
         "connection_revision": 0,
         "environment_label": "",
         "created_unix_ms": 0,
         "expires_unix_ms": null,
         "tools": {
           "search_objects": true,
           "describe_relation": true,
           "run_query": false
         },
         "review": "every-call",
         "suspended": null
       }
     ]
   }
   ```

   - `connection_id` is the connection's id as stored today: the text of
     `ConnectionId` (a UUID). `connection_revision` is the integer
     `connections.revision`. That integer increments on every successful
     save of the profile, including the name, the remembered-password
     flag, colour, row limit, scope and revalidation, not only host, user,
     driver and environment. `environment_label` is the environment key
     stored for that connection (`development`, `staging` or
     `production`), or `""` when it has none. It is not a translated word,
     and it is not the grant code `none`. The key lives in
     `connection.<id>.appearance`, not on the `connections` row. A token
     is suspended when the current revision differs from
     `connection_revision`, or when the current environment key differs
     from `environment_label`, until the user confirms it.
   - `created_unix_ms` and `expires_unix_ms` are Unix milliseconds.
     `expires_unix_ms` null means the token does not expire. The creation
     form's default of 30 days (Q-6) is a timestamp written at creation.
     A missing `expires_unix_ms` reads as null, which means never.
   - `review` is `every-call`, `first-session` or `never`. An unknown
     value reads as `every-call`. `first-session` is the first cache
     answer for that token since this process started. The wire has no
     session, and the setting stores no session id.
   - `suspended` null, or a missing field, means the token is not
     suspended. This version writes the string `connection-changed`. Any
     other string still suspends the token and is kept as stored, so a
     reason this version does not know does not clear the suspension.
   - An `id` that is not 10 lowercase base32 characters is left out. It
     cannot name a vault account.
   - Last used time, activity, cursors and pending cards are memory only.
     They are not fields of this setting. Hidden-column names and result
     previews are memory only as well.
4. **Default deny, and approval only in the window.** A new token has
   `search_objects` and `describe_relation` enabled and `run_query`
   disabled. The review default is every call. `run_query` always needs
   the two in-app approvals, first the statement and then the result, in
   the MCP section. Those two approvals do not depend on the token's
   review mode or on `AssistantPreferences.queries`. Cache tools follow
   the token's review: every call (the default), the first cache answer
   since this process started, or never. Enabling a tool never approves a
   call. Approval happens only in Jykre's window.
5. **Dependency.** The MCP library is `rmcp`, the official Rust MCP SDK
   ([Q-19](../plans/assistant-mcp.md#12-open-questions-for-the-maintainer),
   option A, accepted by the maintainer on 2026-11-10). Only the server and
   streamable-HTTP-server features are used, with default features off. The JSON-RPC layer is not hand-rolled. Licence, health, weight and
   the features' contents are recorded in
   [docs/plans/rmcp-dependency.md](../plans/rmcp-dependency.md).
6. **Where the controls sit is the maintainer's instruction, not a further
   decision of this record.** MCP is a section of the rail, after
   Automation and before History. It is not a page of Preferences and not
   a panel on the edge of the window (maintainer, 2026-11-10). States and
   texts are specified in the [plan](../plans/assistant-mcp.md).
7. **Unchanged.** `Driver`, `Session`, `RowSink`, `IntrospectionSink`,
   `SecretStore`, the SQLite schema and `jykre-assistant` stay as they
   are. This phase has no stdio bridge. The server does not offer MCP
   resources, prompts, sampling or elicitation. It does not accept a
   remote client. It does not run without the window.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Hand-rolled JSON-RPC over `hyper` (Q-19, option B) | Fewer crates; we would own the subset we call | Session, cancellation, progress and specification revisions to keep | The maintainer chose `rmcp` (Q-19, option A) |
| A hash of the token in `config.db` (Q-2) | No new vault account | A verifier of the secret would sit beside the non-secret record | One vault credential per token. A hash was declined |
| One vault credential holding every token | One account name | The Windows credential cap, and one secret shared by every client | Account `mcp:token:<id>` (Q-2) |
| A new SQLite table for tokens | Columns and checks of its own | A migration for data that is a setting | The existing `settings` table, key `assistant.mcp` |
| stdio, or a stdio bridge (Q-13) | Clients that only speak stdio | A second process, and a path for the token that is not the header | Streamable HTTP on loopback. No bridge in this phase |
| Stateful HTTP with `Mcp-Session-Id` (Q-12) | Older clients that expect a session | Session state to keep, cap and drop | Each request carries its own bearer token. The header is ignored and never minted |
| Bind beyond loopback, or TLS (Q-11) | Reachable off this computer | Remote access, or a certificate to manage | `127.0.0.1` only, plain HTTP, no TLS |
| Token in the URL | Easy to paste into a client | URLs are logged, stored and shared | Refused. The header is the only place |

## Consequences

- Positive: an external client can use the three tools without a second
  implementation, a second model client or a schema migration. The secret
  stays in the OS vault. A token alone does not run a statement or release
  a value: approval stays in the window.
- Negative or costs: a listening socket while the window is open; a new
  account type in `jykre-secrets`; the `rmcp` dependency described in the
  proposal; a GPUI-free state machine and a section that renders it.
- What becomes required:
  - Views present and do not decide. Token records, review, pending
    requests and their deadlines live in `jykre::mcp`, which does not use
    GPUI.
  - The card state machine moves to `jykre::assistant::approval`, shared by
    the in-app panel and the MCP section.
  - Running an approved statement moves to `jykre::assistant_query`,
    outside `src/assistant/`, because `assistant_privacy` forbids
    `backend::query` inside `src/assistant/`. It returns only a
    `QueryPreview`.
  - `src/mcp/` and `src/app/mcp_section.rs` join the `assistant_privacy`
    scan.
  - `jykre::assistant_query` is the only new module that may name
    `backend::query_in_context`. `released.rs` stays the only code that
    turns a result into text.
  - [Persistence](../reference/persistence.md) documents `assistant.mcp`.
    [Security](../standards/security.md) documents the vault account and
    the second destination.
- What becomes forbidden:
  - A new tool, a prompt sent to a provider from this server, or a change
    to the in-app assistant's transport.
  - The secret in `config.db`, JSON, logs, telemetry, errors, `Debug`, a
    URL or argv.
  - A listener on anything other than `127.0.0.1`, a listener while the
    server is off, in the demo, or before settings are read, and a token
    read from the URL.
  - Treating an enabled tool as an approved call, or skipping either
    `run_query` approval because of the review mode or
    `AssistantPreferences.queries`.
  - A new SQLite table or a migration for this setting. A stdio bridge.
    MCP resources, prompts, sampling or elicitation. Remote access.
    Running without the window.

## How to validate

The checks are listed in [section 13](../plans/assistant-mcp.md#13-verification-after-a-future-implementation)
of the plan: the privacy test, approval required, tokens, the listener,
`assistant_privacy`, `cargo xtask check`, `cargo xtask check --full` and
`cargo deny`. They were not run for this record. This record changes
documentation only.
