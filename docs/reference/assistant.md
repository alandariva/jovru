# AI assistant and MCP server

Jovru has an agentic AI assistant that works over the metadata cache of the
connection a tab uses, and an MCP server that lets an external client call
the same tools. This document is the reference for what either may read and
send, the three tools, how a request is built and reviewed, how credentials
are kept and what the MCP server accepts. The panel's and the MCP section's
screens are in [User experience](ux/README.md); the rules for what leaves the
computer are in [Security](../standards/security.md#ai-assistant); the
`assistant.preferences` and `assistant.mcp` settings are in
[Persistence](persistence/README.md). The MCP server's states, texts and open
questions are specified in the [plan](../plans/assistant-mcp.md). The
reasoning behind each choice is in [Decisions](../decisions/assistant.md).

## What the assistant may read

The assistant has three tasks: **Fix with AI** (the statement and the
server's error go in, a corrected statement comes back), **Write SQL** (a
description goes in, a query comes back) and a conversation about the
database's structure.

| May leave the computer | Never leaves |
| --- | --- |
| The SQL as the user wrote it | Values of result tabs |
| The server's error as it came (codes, message, position) | Values bound to `:name` parameters |
| Metadata from the local cache, including column defaults and comments | Live reads of the server (Inspect, object definitions) |
| How many rows a statement returned, whether there were more, how many it changed | Credentials, and the API key outside the provider's authentication header |
| A query result, only after two approvals ([`run_query`](#run_query)) | A hidden column's values, a result the user did not send |

The rule is an allow-list in types, not a convention:

- `src/assistant/facts.rs` (`ErrorFacts`, `RunFacts`, `SchemaScope`) is the
  only way a tab's state reaches a request. None of them holds a value, a
  bound parameter, a session or a credential.
- `src/assistant/released.rs` (`QueryPreview`, `release`) is the only code
  that may name a query's result.
- `cargo xtask check` runs `assistant_privacy`. It scans `src/assistant/**`,
  `src/mcp/**`, `src/app/assistant_panel.rs` and
  `src/app/assistant_settings.rs` (not `tests.rs`) and fails when a file
  contains `ResultData`, `QueryBatch`, `QueryResult`, `.parameters`,
  `inspect_object`, `object_definition`, `ResolvedConnection`,
  `Credentials`, `backend::query`, `backend::fetch` or `held_pages`. Only
  `src/assistant/released.rs` may name the first three; `src/mcp/` may name
  none of them.
- A test runs the three tasks against a fake provider and proves that a
  result value and a bound value never reach a request body while a comment
  and a default do. The query test proves that a hidden column's value, a
  withheld result and a declined query never reach a request.
- New tools read the local cache only. A tool that reads the server needs a
  new decision and consent per tool.
- Comments, defaults, error messages and query results can hold instructions
  aimed at the model. The tools are read-only, the answer is shown as plain
  text, and nothing the model writes runs without the user's approval.

## Tools

The model has three tools; the names are fixed (`SEARCH_OBJECTS`,
`DESCRIBE_RELATION`, `RUN_QUERY` in `src/assistant/tools.rs`). No metadata is
sent ahead: the model asks for what it needs. Each tool call is bounded by
`TOOL_TIMEOUT` (10 s). `CacheTools` never fails: an error answer is text that
starts with `Error:`.

### `search_objects`

`{pattern, kinds?, namespace?, after?}`; `pattern` is required.

- Case-insensitive name match: `*` is any run, `?` one character, no
  wildcard means "contains". `*` in a schema lists it. There is no separate
  listing tool.
- `kinds` items are `table`, `view` (which also matches materialized views)
  or `column`.
- `namespace` is a schema or `catalog.schema` resolved against the cache; an
  ambiguous name is refused with up to 20 candidates.
- `after` is a `page-N` value issued by the same `CacheTools` instance; any
  other value is refused.
- Answer: dotted paths with kind and, when known, data type, then
  `next: page-N` when more exist. 100 matches per page.

### `describe_relation`

`{path, include_comments?}`; `path` is dotted or a unique partial path
(`"…"` quoting is read). It matches the exact tail, then case-insensitively;
an ambiguity is settled by the tab's context when that leaves one.
`include_comments` defaults to true; false omits column comments and defaults.

Answer: columns in order with type, `NULL`/`NOT NULL`, default and comment
(newlines flattened), then `primary key:` and `foreign keys:`.

| Limit | Value |
| --- | --- |
| Columns | 300 |
| Default | 100 characters |
| Comment | 200 characters |
| Description | 16 KiB, cut on a character boundary with a note |

### `run_query`

`{sql, purpose}`, both required. It asks the user to run one statement. It is
offered only when `queries` is on in the assistant's preferences (default
on) and never in the demo (`JOVRU_DEMO`).

1. **Refused before asking.** An empty `sql`, more than one statement (split
   by the connection's dialect) or any `:name` marker is refused with the
   reason as the answer; nothing is bound and no card is shown.
2. **Statement approval.** A card shows the purpose, the statement, a mark
   when `guard::risk` finds it changes or deletes data and a mark on a
   production connection. **Run on production** is the button only when the
   connection is production and the statement is risky; otherwise **Run**.
   **Decline** tells the model so. The user cannot edit the statement:
   the model would believe it ran what it asked for.
3. **Run.** The statement runs in a session of the assistant's own, never
   the tab's, so the tab's transaction, temporary tables and `USE` are
   untouched. It uses the tab's context and the default statement timeout
   (300 s), reads at most `READ_ROWS` (51) rows and has a 4 MiB memory
   ceiling, and **Stop** cancels it. It is not a result tab and not history.
   If the profile needs a password that is not in memory, the password
   dialog opens; cancelling it answers as declined.
4. **Result approval.** The card shows the rows, a toggle per column and the
   exact text the next request would carry. **Send result** or **Don't
   send** (the model learns only that it ran and how many rows it returned).
   An error is sent only the same way.

The result that reaches the model is bounded in `released.rs`:

| Limit | Value |
| --- | --- |
| Rows | 50 (`MAX_ROWS`), with a note when more existed |
| Cell | 200 characters, cut with `…` |
| Total | 16 KiB, 512 bytes of it kept for notes |
| Hidden column | name and type stay; each value becomes `[hidden]` |

Columns hidden once start hidden in the conversation's next results. Both
approvals are required in every review mode.

## The agentic loop and request review

The application drives the loop, round by round. A request starts with one
message. When the model asks for tools, the application answers them from the
cache and prepares the next round. A turn stops after 20 rounds
(`MAX_ROUNDS`) until the user continues it. The output is capped at 8192
tokens per round (`MAX_OUTPUT_TOKENS`), sent to the provider as its
`max_tokens` (`max_completion_tokens` for OpenAI's reasoning models, whose
thinking counts in it). Only the tokens written are charged. An answer cut at
the limit is marked in the panel.

**Every request can be reviewed before it leaves.** Each round's canonical
request (system prompt, messages, tool results, tool definitions and limits)
is shown as a card with estimated tokens and cost, as messages, tools or
JSON; its messages show what is new since the last request sent. The review
mode (Preferences) pauses before every request (`every-request`, the
default), before the first only (`first-only`) or never (`never`). Each tool
step shows its arguments and the full answer the next request carries. The
provider's real token counts (input, output, cached) and the cost at the
prices the user entered are shown per round and per conversation; no price is
built in. **Copy trace** exports the conversation as JSON. The card shows the
canonical request, not the provider's wire JSON, because `genai` does not
expose the body; a test checks that every reviewed text is what the provider
received.

**Nothing runs on its own.** Suggested SQL appears in blocks with Copy,
Insert at the cursor and, for a fix, Replace the failed statement, which
replaces it only where it is still unchanged and as one undo step; a block
that changes or deletes every row is marked.

**The context is the whole conversation, compacted by the user.** Provider
APIs keep no state, so every request carries the system prompt, the tools and
every message. Nothing is cut on its own, because cutting from the front
changes the start of every request and defeats the providers' prompt cache. A
summary at the top of the panel shows the session's cost, the context's
tokens and the share of input served from the provider's cache. Two
compactions, each shown with its sizes before and after:

- **Prune long tool results:** every tool answer of 256 bytes or more
  (`PRUNE_FROM_BYTES`) becomes one line naming the call, which the model may
  make again. It costs nothing.
- **Summarize with the model:** a request of its own, reviewed like any
  other. The summary joins the system prompt (a newer one replaces it) and
  every message goes. An answer that calls tools, says nothing or stopped
  at the output limit changes nothing.

A request the provider rejects, such as one past the model's context,
suggests compacting.

The panel is its own cached entity with a virtualized list; a streamed answer
redraws the panel at most about 30 times a second and never the workspace per
piece of text. The demo keeps canned answers and sends nothing.

## Providers

The crate `jovru-assistant` (no UI, core contracts only) holds the canonical
request, one streamed round, error classification, pricing and SQL-block
detection. `genai` 0.7 is used only in its private `transport` module.
`jovru-core` never depends on `genai`, and a new provider goes through the
transport.

- **Providers:** Anthropic, OpenAI, any OpenAI-compatible server (LM Studio,
  Databricks serving, gateways) and Ollama.
- **Addresses** must be HTTPS except loopback; redirects are not followed.
- **TLS** uses rustls with `ring` (installed as the process's crypto
  provider) and the system's trust store, so `aws-lc-rs` is not built.
- **Limits:** connection 15 s, first byte 120 s, silence between pieces 60 s,
  a round 900 s, **Test** 30 s, a tool 10 s.
- **Failures** are classified by status only. The provider's body is never
  shown, logged or kept, and a round is never retried automatically.
- **Logs** record per round the provider, time, token counts and code; never
  a prompt, answer, schema text or key. The provider library's own log is off
  (`genai=off`).

## Credentials and settings

- The API key lives only in the system vault: `ApiKeyStore` in
  `jovru-secrets`, service `Jovru`, accounts
  `assistant:{anthropic|openai|openai-compatible}:api-key`. It is read
  lazily on the first request, within the vault's 10-second limit, and is
  never shown back.
- Provider, model, address, review mode, `queries` and prices are the
  `assistant.preferences` setting ([Persistence](persistence/README.md)).
- The MCP token's secret is a second family of vault accounts, described
  [below](#tokens).

## MCP server

The MCP server is another client of the same three tools, for a client the
user chose and Jovru cannot see: an IDE agent, a chat client, a CLI agent.
What that client's model does with an answer is outside the application. The
server adds no tool and sends no prompt to a provider; it never uses
`jovru-assistant`'s transport. The in-app assistant is unchanged.

The MCP layer only adapts calls and answers. The work is done by
`CacheTools`, `run::query_sql`, `guard::risk` and `released`, so the text a
client receives is the text the in-app model would receive for the same
call. The MCP layer adds only its own refusals (token, disabled tool,
timeout, busy). `tools/list` returns the in-app specs of the enabled tools
with a title and annotations (`readOnlyHint`, `destructiveHint`,
`openWorldHint: false`); answers are one `text` item and no `outputSchema`
is declared.

### Transport

- Streamable HTTP on `127.0.0.1` only, port 47321 unless the user set
  another. No TLS and no CORS.
- Stateless: no `Mcp-Session-Id` is minted and the header is ignored. Each
  request is accepted or refused on its own bearer token.
- The listener exists only while the window is open, and only when the server
  is on. Nothing listens when it is off, in the demo, or before settings are
  read. A port in use leaves the server off with the reason.
- Order of checks before any tool code runs: the server is on, `Origin`
  absent or a loopback origin and `Host` a loopback name (DNS rebinding),
  otherwise 403 with no detail; the body is at most 1 MiB; then the token.
- A token in the URL is never read; a request that has one is refused.
- At most 4 concurrent HTTP connections and one pending or running
  `run_query` per token. Reading a request's headers and body 10 s; an idle
  connection 60 s.
- The library is `rmcp` 3.9.0, with default features off and only the server
  and streamable-HTTP-server features; the listener is hand-served with
  `hyper` because `rmcp`'s HTTP service does not bind a socket. Stateless
  operation needs `legacy_session_mode` off (its default mints the header
  on `initialize` and allows `GET` and `DELETE`), the body limit set to
  1 MiB (the crate's default is 4 MiB) and origin validation enabled. See
  [rmcp-dependency.md](../plans/rmcp-dependency.md).
- Not offered: stdio or a stdio bridge, MCP resources, prompts, sampling or
  elicitation, remote access, running without the window.

### Tokens

One token is one saved connection. A user who wants two connections creates
two tokens.

- **Form:** `jovmcp_<id>_<secret>`. `<id>` is 10 lowercase base32 characters
  (`abcdefghijklmnopqrstuvwxyz234567`), not secret: it names the record and
  the vault account and is the prefix shown later. `<secret>` is 256 bits as
  64 lowercase hex digits from the secure random source of the `ring`
  provider rustls installs. In Rust the secret is redacted in `Debug` and the
  type is not `Serialize`. The secret is compared in constant time after
  looking the record up by `<id>`.
- **Vault:** the secret is stored only in the OS vault, service `Jovru`,
  account `mcp:token:<id>`, through a `jovru-secrets` account type built
  only from a validated `<id>`, so no free text reaches the vault. One
  credential per token; the accounts never collide with `connection:` or
  `assistant:` names. It is never written to `config.db`, JSON, logs,
  telemetry, error text, `Debug`, a URL or argv. It is shown once, at
  creation or rotation.
- **Creation** writes the vault first, within the 10-second limit; the record
  is saved only when the vault write succeeded, and a failed record save
  removes the vault entry. With no vault there is no token: no session-only
  token and no file. At first use after start the secret is read once and
  kept in memory, redacted; a vault failure answers 503 with a generic body.
- **Record:** the `assistant.mcp` row of the existing `settings` table; there
  is no new table and no migration. A missing setting, or a value that is not
  the object, reads as the server off, port 47321 and no tokens.

  ```json
  {
    "enabled": false,
    "port": 47321,
    "tokens": [
      {
        "id": "<10 lowercase base32>",
        "name": "<label, at most 60 characters>",
        "connection_id": "<ConnectionId as stored>",
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

  A missing or unknown field takes the default shown. An `id` that is not 10
  lowercase base32 characters is left out. `expires_unix_ms` null (or
  missing) means never; the creation form's default is 30 days, written as a
  timestamp. `review` is `every-call`, `first-session` or `never`; an unknown
  value reads as `every-call`.
- **Suspension:** a token is suspended (`suspended` is `connection-changed`)
  when the connection's `connections.revision` (it increments on every
  successful save of the profile) or its environment key (`development`,
  `staging`, `production` or `""`, stored in `connection.<id>.appearance`)
  differs from the record, until the user confirms it. Any other stored
  string also suspends the token and is kept, so a reason this version does
  not know does not clear it. Deleting the connection deletes its tokens and
  their vault entries.
- **Expiry, revocation, rotation:** an expired, revoked, suspended or unknown
  token gets the same 401 and body. Revoke removes the record and the vault
  entry, declines the token's cards, cancels its query and closes its
  session; the token stops even if the vault removal fails. Rotate keeps
  name, connection, tools and review, makes a new `<id>` and secret, and the
  old token stops at once. A token is never moved to another connection.
- **Memory only:** last used time, activity (last 200 entries), search
  cursors (last 100), pending cards, previews and hidden-column names.

### Tools, review and approval

Default deny, and approval only in Jovru's window:

- A new token has `search_objects` and `describe_relation` enabled and
  `run_query` disabled, with review **every call**. Only enabled tools are
  listed; a disabled or unknown tool gets JSON-RPC `-32602`
  (`Unknown tool: <name>`), no card, and is only counted.
- **Enabling a tool never approves a call.** The token's `run_query` switch
  and the assistant's `queries` preference are independent.
- **Cache tools** follow the token's review: every call (each answer waits
  on a card showing the exact text, with **Send answer** or **Don't send**),
  the first cache answer for the token since this process started, or
  never (the text leaves without a card and is listed in Activity).
- **`run_query`** always needs both in-app approvals, whatever the review
  mode, in the MCP section's **Requests** tab. It runs in a session of the
  MCP server's own, never a tab's: one per token, opened on the first
  approved query in the connection's default context, closed on revoke,
  rotate, suspend, after 10 minutes idle, when the profile changes and at
  quit. The purpose is shown as quoted text from the client, never in
  Jovru's wording.
- Cards are answered one at a time per token in arrival order. Each decision
  has a 120-second deadline (`DECISION_TIMEOUT`); a deadline that passes
  closes the card and gives the client an **error result**, never a
  success. If the client cancels or disconnects, the card closes and a
  running query is stopped.
- What the client receives:

  | Situation | Result | Text |
  | --- | --- | --- |
  | Cache answer sent | normal | the tool's text |
  | Cache answer not sent | normal | "The user chose not to share this answer." |
  | Statement refused before asking | error | the reason from `run::query_sql` |
  | Statement declined, password not given or stopped | normal | "The user declined to run this query." |
  | Result sent / not sent | normal | the `released` text |
  | No answer in time | error | "The user did not answer in time; …" |
  | Another query pending for the token | error | "…another query from this client is waiting for the user…" |
  | Server paused | error | "Jovru is not accepting requests now." |

- When a client connects, the description is answered only after the token
  is accepted: `serverInfo.name` is `jovru`, `serverInfo.title` is
  `Jovru — ` plus the connection's current name and environment label, and
  `instructions` names the connection and, if `run_query` is enabled, says
  statements run only there after the user approves them in Jovru. It holds
  no host, user, credential or SQL.

### What the MCP server never exposes

A new tool; live reads of the server (Inspect, definitions); a session's
state; a result tab; a bound parameter; a credential; a result the user did
not send; the in-app conversation, prompts, compaction and costs. The
server never sends a prompt to a provider.

**Logging.** Per call, at `info`, in an `mcp_tool` span: the tool name (plain
names only, else `other`), the outcome (`sent`, `not_sent`, `declined`,
`withheld`, `rejected`, `timed_out`, `cancelled`, `disabled`, `busy`),
milliseconds, bytes returned and, for `run_query`, the row and hidden-column
counts. Authentication failures log one rate-limited `warn` per kind. Never
logged at any level: the token or any part of it (including `<id>`) or its
name, the `Authorization` header, SQL, purpose, patterns, paths, namespaces,
answers, row values and the client's self-reported name.

### Module boundaries

- Token records, review, pending requests and their deadlines live in
  `jovru::mcp` (`token`, `registry`, `broker`, `server`), which does not use
  GPUI. Views present and send decisions; they do not decide.
- Running an approved statement lives in `jovru::assistant_query`, outside
  `src/assistant/` because `assistant_privacy` forbids `backend::query`
  there. It is the only module besides the workspace that names
  `backend::query_in_context`, and it hands back only a `QueryPreview`.
- `src/mcp/` and the MCP section's views are in the `assistant_privacy`
  scan. The architecture check also fails when `jovru-assistant` depends on
  the UI, on the application library or on any project crate other than
  `jovru-core`, and when `jovru-core` depends on `genai`
  ([Architecture](architecture.md)).
- `Driver`, `Session`, `RowSink`, `IntrospectionSink`, `SecretStore`, the
  SQLite schema and `jovru-assistant` are unchanged by the MCP server.

### Known deviations

| Standard | Current code |
| --- | --- |
| The card state machine is shared by the assistant panel and the MCP section as `jovru::assistant::approval` (D-0042) | `QueryCard` and `CardState` still live in `src/app/assistant_panel.rs`; the module does not exist yet ([plan](../plans/assistant-mcp.md)) |

## Tests

- The AI assistant is tested against a local fake of an OpenAI-compatible
  server (port 0, synthetic text, a synthetic key): streaming, tool calls,
  token counts, statuses classified without their bodies, cancellation
  closing the connection and the silence and round limits
  (`crates/assistant`). `src/assistant/tests.rs` runs **Fix**, **Write** and
  **Ask** through the agentic loop against the fake and a temporary
  metadata cache, and proves that a result value and a bound `:name` value
  never reach a request, that every reviewed text is in what the provider
  received, and that rounds are never logged with their text or the key.
  The same file runs `run_query` on a synthetic SQLite file of 60 rows,
  answered as the user would (send with a column hidden, withhold,
  decline): a hidden column's value, a withheld result and a declined
  query never reach a request, a declined query never runs, at most 50
  rows are sent, and a call with two statements is refused before the user
  is asked. The loop's tests (`src/assistant/run.rs`) prove that nothing is
  left out of a request on its own, that each request knows what the last
  sent one carried, that pruning keeps the calls and stops a sent value
  from being sent again, and that a summary replaces the conversation only
  when the model answered with one.
  Real providers are checked by hand: record the provider, model and
  outcome, never the key.

## Decisions

The reasoning behind this area's structural choices is in [Decisions: AI assistant and MCP server](../decisions/assistant.md).
