# MCP server for the assistant's tools

- **Approval:** Approved (maintainer, 2026-11-10)
- **Implementation:** In progress
- **Date:** 2026-11-10

[ADR-0042](../adr/0042-mcp-server-for-the-assistant-tools.md) records the
decision, and the `rmcp` proposal is [rmcp-dependency.md](rmcp-dependency.md).

The maintainer approved this plan,
[ADR-0042](../adr/0042-mcp-server-for-the-assistant-tools.md) and the `rmcp`
dependency on 2026-11-10. The body remains the specification. Statements
marked **(verified)** were checked in the code on branch `release/0.3.1` at
`06377d1`; statements marked **(hypothesis)** were not, and must be checked
before implementation.

> **Maintainer, 2026-11-10.** MCP is a section of the rail, as Automation
> is. The server, tokens, which tools each token may call, and the approval
> of each call are controlled there. It is not a page of Preferences and not
> a panel on the edge of the window.
>
> **Maintainer, 2026-11-10.** One token is one connection ([Q-3](#12-open-questions-for-the-maintainer)).
> When a client connects, the server's description names that connection
> ([8.1](#81-what-the-client-is-told-when-it-connects)).

## 1. Summary

Jykre Studio would host a [Model Context Protocol](https://modelcontextprotocol.io)
(MCP) server while it runs, so an external MCP client (an IDE agent, a
desktop chat client, a CLI agent) can call the three tools the in-app
assistant already offers its model: `search_objects`, `describe_relation`
and `run_query`. The server calls the existing tool code, keeps the same
bounds and the same privacy allow-list, and adds three things the in-app
assistant does not need: **tokens** that say which client may call, **a
per-token switch for each tool**, and **an approval surface that is not tied
to a query tab**. It is not a second model client: it never sends a prompt
to a provider and never touches `jykre-assistant`'s transport. The user
controls it from an **MCP** section of the rail
([5.2](#52-create-a-new-token)).

## 2. Goal and non-goals

**Goal.** An MCP client the user chose can read the structure of one
connection's metadata cache and, only with the user's two approvals, run one
statement and receive its bounded result, exactly as the in-app model can
([ADR-0028](../adr/0028-agentic-ai-assistant-over-the-metadata-cache.md)).

**Non-goals.**

- No new tool. No tool that reads the server (Inspect, definitions), a
  session's state, a result tab, a bound parameter, a credential or a result
  the user did not approve. ADR-0028 deferred live reads; this plan keeps
  them out ([section 14](#14-known-deviations-and-out-of-scope)).
- No second implementation of the tools, the result cut or the statement
  checks. The MCP layer adapts calls and answers; the work is done by
  `CacheTools`, `run::query_sql`, `guard::risk` and `released`.
- No model, prompt, provider, price or token count. The MCP server does not
  use `jykre-assistant`'s `transport`, `ModelRequest` or `genai`.
- No remote access. The server binds the loopback address only.
- No MCP resources, prompts, sampling or elicitation. Approval happens in
  Jykre Studio's window, never in the client.
- No headless mode. The server exists only while the application's window
  is open.

## 3. Relationship to ADR-0028

The in-app assistant stays as it is. The MCP server is **another client of
the same tools**:

| | In-app assistant (ADR-0028) | MCP server (this plan) |
| --- | --- | --- |
| Who decides which tool to call | A model Jykre calls through `jykre-assistant` | An external client's model, unknown to Jykre |
| Where answers go | Into a `ModelRequest` the user can review before it leaves | Into an MCP tool result the client receives; what the client does next is outside Jykre's sight |
| Which tools | `CacheTools::specs(queries)` | The same specs, filtered by the token's enabled tools |
| Which cache | The tab's connection (`SchemaScope` from the workspace) | The one connection the token is bound to |
| Where `run_query` runs | The assistant's own session of that tab | A session of the MCP server's own, never a tab's |
| Approval of `run_query` | Statement card, then result card, in the panel | The same two decisions, in the MCP section's **Requests** tab |
| Review of cache tools | The next request is reviewable per **Review mode** | Each answer is reviewable before it is returned, per token ([5.5](#55-verify-and-approve-requests)) |

ADR-0028 says "new tools read the local cache only unless an ADR says
otherwise". This plan adds no tool. It does add a **new destination** for
what the tools return: ADR-0028 and the [Security](../standards/security.md#ai-assistant)
standard speak of "the provider the user chose". An MCP client is a different
destination, chosen by the user but not seen by Jykre. That change needs an
ADR before implementation ([section 11](#11-what-needs-an-adr-and-what-can-stay-here)).

## 4. What the code does today

### 4.1 Tool contracts (verified)

The MCP tools must match these. Sources: `src/assistant/tools.rs`,
`src/assistant/run.rs`, `src/assistant/released.rs`,
`src/app/assistant_panel.rs`, `src/app/workspace.rs`.

| Tool | Contract | Bounds and limits |
| --- | --- | --- |
| `search_objects` | `{pattern, kinds?, namespace?, after?}`; `pattern` required. Case-insensitive name match, `*` any run, `?` one character, no wildcard means "contains". `kinds` items are `table`, `view` (which also matches materialized views) or `column`. `namespace` is a schema or `catalog.schema` resolved against the cache (ambiguous names are refused with up to 20 candidates). `after` is a `page-N` value **issued by the same `CacheTools` instance**; any other value is refused. Answer: dotted paths with kind and, when known, data type, then `next: page-N` when more exist | `SEARCH_PAGE` = 100 per page; `TOOL_TIMEOUT` = 10 s around the whole call |
| `describe_relation` | `{path}`; dotted or unique partial path (`parse_path` reads `"…"` quoting). Exact tail match, then case-insensitive; an ambiguity is settled by the tab's context (`SchemaScope::context`) when that leaves one. Answer: columns in order with type, `NULL`/`NOT NULL`, default and comment (newlines flattened), then `primary key: (…)` and `foreign keys:` | `MAX_COLUMNS` = 300, `MAX_DEFAULT` = 100 characters, `MAX_COMMENT` = 200 characters, `MAX_DESCRIPTION` = 16 KiB (cut on a character boundary with a note); 10 s |
| `run_query` | `{sql, purpose}`, both required in the schema. Offered only when `queries` is on in `AssistantPreferences` (default on) and never in the demo (`self.queries = preferences.queries && !self.demo` in the panel). `run::query_sql` refuses an empty `sql`, more than one statement (`sql::statements` by the dialect) or any `:name` marker (`sql::named_markers`) before the user is asked; nothing is bound | See below |

`run_query`, in order (verified):

1. **Refusal before asking.** A refused call becomes `released::rejected`
   with the reason text (`QueryDecision::Rejected`); no card is shown.
2. **Statement card** (`CardState::Asked`): purpose, the statement, a
   **Changes or deletes data** badge when `guard::risk` finds one
   (`UPDATE`/`DELETE` without `WHERE`, `DROP`, `TRUNCATE`, an `ALTER` that
   drops), and on a production connection the line "This is a production
   connection." **Correction to the task's list:** the button reads **Run on
   production** only when the connection is production **and** the statement
   is risky; otherwise it reads **Run** even on production. **Decline**
   answers `released::declined` ("The user declined to run this query.").
3. **Run** (`Workspace::run_assistant_query`): asks for the password first
   when the profile needs one; runs with `backend::query_in_context` in the
   assistant's own session of the tab (`assistant_session`), the tab's
   context and options with `max_rows = released::READ_ROWS` (51) and a
   memory ceiling of 4 MiB (`ASSISTANT_MEMORY_CEILING`). The rows become a
   `QueryPreview` on the runtime and are dropped there. A run that was
   stopped, or for which no password was given, is answered as **declined**.
4. **Result card** (`CardState::Review`): the rows, a toggle per column, and
   the exact text `released::release` would send. At most `MAX_ROWS` = 50
   rows, cells cut at `MAX_CELL` = 200 characters with `…`, `MAX_BYTES` =
   16 KiB in all (512 bytes kept for the notes). A hidden column keeps its
   name and type and every value becomes `[hidden]`; columns hidden once
   start hidden in the conversation's next results (`hidden_by_name`).
   **Send result** answers `released::sent`; **Don't send** answers
   `released::withheld` (that it ran, failed or not, and the row count).
   An error is sent only the same way.

### 4.2 Where the pieces live today (verified)

- `CacheTools` holds one `SchemaScope` and the search cursors of **one
  conversation**; `CacheTools::call` never fails and returns text. Error
  answers start with `Error:`.
- `SchemaScope` (`src/assistant/facts.rs`) is built by the workspace view
  (`Workspace::push_assistant_target`) from the tab's input, cache ID and
  context. It holds no credential.
- The statement run lives in a **view** (`Workspace::run_assistant_query`,
  `src/app/workspace.rs`), which owns the assistant session and the password
  prompt. It names `backend::query_in_context`, which the `assistant_privacy`
  check forbids inside `src/assistant/` (`backend::query` is a forbidden
  substring), so it cannot simply move there.
- The card state machine (`QueryCard`, `CardState`) and its decisions
  (`run_card`, `decline_card`, `toggle_column`, `settle_card`) live in
  `src/app/assistant_panel.rs`.
- `assistant_privacy` (`xtask/src/main.rs`) scans `src/assistant/**`
  (except `tests.rs`), `src/app/assistant_panel.rs` and
  `src/app/assistant_settings.rs` for `ResultData`, `QueryBatch`,
  `QueryResult`, `.parameters`, `inspect_object`, `object_definition`,
  `ResolvedConnection`, `Credentials`, `backend::query`, `backend::fetch` and
  `held_pages`; only `src/assistant/released.rs` may name the first three.
- `jykre-secrets` has `ApiKeyStore` over a fixed `ApiKeyAccount(&'static
  str)` (`assistant:{anthropic|openai|openai-compatible}:api-key`,
  `telemetry:otlp:headers`). It cannot name an account built at run time.
- `jjp/1` (`src/automation/jjp.rs`) already has a loopback listener and a
  256-bit token generated through `rustls::crypto::ring`'s secure random
  source, compared without stopping at the first difference, with a
  redacted `Debug`. The MCP token reuses that pattern, not that listener.

## 5. Client access: tokens, approval and enabled tools

This section is the whole contract for how an external client may call a
tool. Its three flows are one design, and they share one place in the
window: a **token** names a client and binds it to one connection;
**enabled tools** say what that token may ask for; **approval** says whether
a particular call's answer leaves. Enabling a tool never approves a call,
and a token never enables a tool by itself. Preferences keeps the in-app
assistant; it does not configure the server, a token or a pending call.

### 5.1 Principles

1. **The token is a credential.** It lives only in the OS vault through
   `jykre-secrets` ([ADR-0004](../adr/0004-os-vault-for-credentials.md)),
   is shown once, and is never written to `config.db`, JSON, settings, logs,
   telemetry, error messages, `Debug` output, a URL or a command line.
2. **One token, one connection** ([5.2.4](#524-binding-to-one-connection)).
3. **Default deny.** A new token has `search_objects` and
   `describe_relation` enabled and `run_query` disabled. Every call is
   reviewed until the user relaxes the review for that token.
4. **Approval is in Jykre, never in the client.** The client is the party
   being authorised; it cannot answer its own request. MCP elicitation is not
   used.
5. **Same answers as in-app.** The text a client receives is the text the
   in-app model would receive for the same call, produced by the same
   functions. The MCP layer adds only its own refusals (token, disabled tool,
   timeout, busy).
6. **Views present; they do not decide.** Tokens, enabled tools, review
   policy, pending requests and their deadlines are a GPUI-free state machine
   in the library ([5.8](#58-where-the-logic-lives)); views render snapshots
   and send decisions.

### 5.2 Create a new token

#### 5.2.1 Where

**MCP** is a section of the rail, in the same place Automation is. The rail
today is Explorer, Saved queries, Reports, Automation, History, and
Preferences as the gear at the bottom (`AppView::rail` in `src/app/mod.rs`,
verified). MCP is inserted after Automation and before History. Preferences
is unchanged: no new page, and nothing under **Assistant** configures a
token. The section does not depend on a provider being configured, because
the server does not call a model.

The section follows Automation (`src/app/automation/mod.rs`, verified): the
sidebar lists the things the section owns, and opening one opens a tab in a
grey group of its own, which offers no new tab and takes no other tab. A
token is not a query of its connection, so its tab is not filed under that
connection's group.

```text
Rail: Explorer · Saved queries · Reports · Automation · MCP · History
                                                      Preferences (bottom)

Sidebar, while MCP is selected
┌ MCP                                              [ New token… ] ┐
│ Server  [ Off | On ]   [ Pause ]                                │
│ http://127.0.0.1:47321/mcp                              [Copy]  │
│ Port [47321]   Listening · 1 client                             │
│                                                                 │
│ Requests                                              2 waiting │
│                                                                 │
│ Cursor on laptop   jykmcp_k3f9q2…   PG · Sales (Production)     │
│ search ✓  describe ✓  run_query ✗   every call                  │
│ CLI agent          jykmcp_8hw1zt…   SQLite · fixtures  Expired  │
│                                                                 │
│ Tokens are kept in the system vault. A token is shown once.     │
└─────────────────────────────────────────────────────────────────┘

Main area: the open MCP tabs (each its own grey group)
┌ Requests ─────────────┐  ┌ Cursor on laptop ──────┐
│ waiting cards         │  │ name, connection,      │
│ activity              │  │ tools, review, expiry  │
└───────────────────────┘  │ [Rotate] [Revoke]      │
                           └────────────────────────┘
```

- **Server**, **Pause**, the address, the port and the listener's state sit
  in the sidebar, above the list, and only while this section is selected.
  They are the server's controls; a token's tab does not repeat them.
- **Requests** is one entry, with the number waiting, as Automation's
  **Runs** entry carries the number active. Choosing it opens the **Requests**
  tab ([5.5.1](#551-requests-in-the-mcp-section)). One such tab; choosing the
  entry again shows the one already open.
- The rows under it are the tokens. Choosing a row opens that token's tab.
  **New token…** opens a token tab on the creation form. **Rotate**,
  **Revoke** and the tool switches live on the token's tab, not on the row;
  the row shows the name, the prefix, the connection, the tools, the review
  policy and the state (expired, suspended, vault unavailable).
- The list is virtualized, as Automation's job list is. Empty: "No tokens
  yet. Create one for each MCP client." and **New token…**.

#### 5.2.2 The flow

1. **New token…** opens a token tab in the MCP section with: **Name**
   (required, at most 60 characters, unique among tokens; free text the user
   types, used only as a label), **Connection** (one saved connection; the
   list shows each connection's colour and environment label), **Expires**
   (7, 30 or 90 days, 1 year, or **Never**; default 30 days;
   [Q-6](#12-open-questions-for-the-maintainer)), **Tools** (the three
   switches of [5.4](#54-enable-tools), `run_query` off) and **Review**
   ([5.5.2](#552-review-of-search_objects-and-describe_relation)).
2. **Create** generates the token on the runtime and writes it to the vault
   within the vault's 10-second limit. Only when the vault write succeeded
   is the token's record saved to `config.db`. If the record save fails, the
   vault entry is removed and the tab says so; no half-made token remains.
3. The same tab switches to **Copy your token**: the token in a read-only,
   selectable, monospaced field, a **Copy** button, and the text "This is the
   only time Jykre Studio shows this token. Store it in your MCP client's
   configuration." Below it, a ready-to-paste client snippet (the address
   and an `Authorization: Bearer` header placeholder) **without** the token
   in it, so a snippet copied to a chat or a ticket does not carry it.
4. **Done** drops the token from memory (the field's text and the clipboard
   aside, which the user controls) and leaves the tab on the token's
   controls, with the secret gone. Closing the tab, or leaving it any other
   way, drops the secret too. The token can never be shown again; a lost
   token is replaced with **Rotate**.

#### 5.2.3 What the token is

`jykmcp_<id>_<secret>`:

- `<id>`: 10 random lowercase base32 characters (50 bits), **not secret**.
  It identifies the record, names the vault account and is what the user
  sees later as the prefix `jykmcp_<id>…`.
- `<secret>`: 256 random bits as 64 lowercase hex digits, from the
  operating system's secure random source through the `ring` provider rustls
  already installs (as `jjp/1`'s `Token::generate`). No new dependency.
- In Rust the token exists only in a type whose `Debug` is redacted and
  which is not `Serialize` (a `SecretString` for the secret part).
  `expose_secret()` is called only to write the vault entry, to show it once
  and to compare.
- Comparison is constant time over the secret (as `jjp/1`'s
  `Token::matches`), after looking the record up by `<id>`.

#### 5.2.4 Binding to one connection

A token is bound to **exactly one** saved connection. Reasons, from the code:

- `CacheTools` reads one `SchemaScope`, which is one connection's cache;
  `describe_relation` settles ambiguity with one context; search cursors are
  per instance. A set of connections would need a `connection` argument in
  every tool, which `CacheTools::specs` does not have, so the MCP tools would
  diverge from the in-app ones.
- `run_query`'s production mark, risk badge, password prompt and session are
  per connection. One connection per token keeps each card about one target.
- Revoking or suspending a token then has one obvious effect.

A user who wants two connections creates two tokens (each a separate server
entry in the client). The maintainer confirmed this on 2026-11-10
([Q-3](#12-open-questions-for-the-maintainer)). The description the client
receives when it connects names this connection
([8.1](#81-what-the-client-is-told-when-it-connects)).

The record stores the connection's ID and its profile **revision** and
environment label at creation. A later change to that connection's profile
(host, user, driver, environment label) **suspends** the token until the
user confirms it again on the token's tab, as schedule grants do
([ADR-0041](../adr/0041-schedules-and-grants.md)). Deleting the connection
deletes its tokens and their vault entries. (Hypothesis: the `revision`
column of `connections` changes on exactly those edits; to check in
`jykre-storage`.)

#### 5.2.5 Where the secret is stored, and what is shown later

| Item | Where | Shown later |
| --- | --- | --- |
| The token's secret part | OS vault only, service `JykreStudio`, account `mcp:token:<id>` | Never |
| `<id>` | `config.db` record and the vault account name | As the prefix `jykmcp_<id>…` |
| Name, connection ID, its revision and label, created and expiry times, enabled tools, review policy, suspended reason | `config.db`, setting `assistant.mcp` (JSON, beside `assistant.preferences`; no new table) | Yes |
| Last used time, connected clients, activity | Memory only, for this run of the application | Yes, until quit |

The vault account is new: `ApiKeyAccount` only has fixed `&'static str`
names. `jykre-secrets` would gain an account type built only from a
validated `<id>` (10 base32 characters), so no free text reaches the vault
([Q-2](#12-open-questions-for-the-maintainer)). Its accounts must never
collide with `connection:` or `assistant:` names, tested as
`api_key_accounts_never_collide_with_connection_passwords` does today.

On first use after start, the server reads the secret from the vault (10 s
limit) and keeps it in memory, redacted, until the token is revoked,
rotated, suspended, expires or the application quits. No vault call happens
per request after that.

#### 5.2.6 Vault unavailable

- **At creation:** the token is **not created** and nothing is saved; the
  tab says the system vault is unavailable and why (locked, no Secret
  Service, past the time limit). There is no session-only token and no
  fallback to a file ([Q-5](#12-open-questions-for-the-maintainer)).
- **At first use:** the request fails with HTTP 503 and a generic body
  ("Jykre Studio cannot read this token from the system vault."); the token's
  row shows **Vault unavailable**, and its tab offers **Try again**. The
  client is never told whether the token was right.
- **A record whose vault entry is missing** (removed outside Jykre): the row
  shows **Token missing from the vault**; requests with it fail as
  unauthorised; the token's tab offers only **Rotate** and **Delete**.
- **At revoke or delete:** the record is removed and the in-memory secret
  dropped at once, so the token stops working even if the vault removal
  fails; a failed removal is reported without the secret, and the row stays
  as **Revoked, vault entry not removed**, with **Try again** on the tab.

#### 5.2.7 Expiry, revocation and rotation

- **Expiry** is checked on every request against the wall clock. An expired
  token is refused like an unknown one (HTTP 401, same body, same time
  cost); its row shows **Expired**, and its tab offers **Rotate** and
  **Delete**. Expiry does not delete the vault entry, so the user sees what
  expired.
- **Revoke** asks once ("Clients using this token stop working at once."),
  then removes the record and the vault entry, declines its pending cards,
  cancels its running query and closes its database session.
- **Rotate** keeps the name, connection, tools and review policy, generates a
  new `<id>` and secret, writes the new vault entry, swaps the record, then
  removes the old vault entry. The old token stops at once (no grace period,
  [Q-7](#12-open-questions-for-the-maintainer)). The new token is shown once,
  as at creation.
- The token's tab changes name, tools, review policy and expiry (only
  shorter, or re-confirmed), never the connection: binding a token to
  another connection is a new token.

### 5.3 What a request must carry

Every MCP HTTP request carries `Authorization: Bearer <token>`. Order of
checks, all before any tool code runs:

1. Server on, not in the demo, request from loopback, `Origin` absent or a
   loopback origin, `Host` a loopback name with the configured port;
   otherwise HTTP 403 with no detail.
2. Body at most 1 MiB, enforced while reading.
3. Token: well formed, `<id>` known, not revoked, not expired, not
   suspended, secret matches (constant time). Any failure: HTTP 401 with the
   same generic body. A token in the URL query is never read; a request that
   has one is refused (403) so a client that leaks tokens into URLs fails
   loudly.
4. No protocol session. A request is accepted or refused on its own
   bearer token ([section 6](#6-transport)). An `Mcp-Session-Id` header is
   ignored and never minted.

### 5.4 Enable tools

- Each token has three switches: **Search objects**, **Describe tables and
  views**, **Run queries (asks you each time)**. They appear on the token's
  tab, including the new-token tab, and as compact marks on the token's row.
- `run_query` starts **off** on every new token. The assistant's `queries`
  preference does not change it, and this switch does not change the
  assistant ([Q-4](#12-open-questions-for-the-maintainer)). Turning it on
  asks once: "Clients with this token may ask to run statements on *Sales*.
  Each statement and each result still waits for you." On a connection
  labelled Production the tab says so and the button reads **Allow on
  production**.
- In the demo it is never offered.
- **Enabling is not approving.** An enabled tool is listed to the client;
  each call still goes through [5.5](#55-verify-and-approve-requests).
- **What the client sees:** `tools/list` returns only the enabled tools, with
  the in-app specs. When the user changes the switches while a client is
  connected, the server sends `notifications/tools/list_changed`
  (hypothesis: the client keeps a stream open to receive it; otherwise it
  sees the change on its next `tools/list`).
- **A disabled or unknown tool** called anyway gets the JSON-RPC error
  `-32602` with the message `Unknown tool: <name>` (the name echoed only if
  it is a plain tool name, as `run::run_tools` records it, else `other`).
  No card is shown, nothing runs, and only a count is logged. (Hypothesis:
  `-32602` with that message is the MCP specification's example for an
  unknown tool; to check against the version implemented.)
- **Seeing what a token may call:** the token's row and its tab list the
  enabled tools and the review policy; each card in **Requests** shows the
  same for its token.

### 5.5 Verify and approve requests

#### 5.5.1 Requests, in the MCP section

Calls are not tied to a query tab, so they cannot use a tab's assistant
panel. They are answered in the MCP section, on its **Requests** tab, the
way Automation's **Runs** entry opens one tab for every run. There is no
panel on the edge of the window.

```text
┌ Requests ───────────────────────────────────────────────────────────┐
│ Cursor on laptop · PG · Sales (Production)                          │
│ Tools: search, describe, run_query · Review: every call             │
│ ┌ search_objects ─────────────────────────────────────── 00:01:43 ┐ │
│ │ pattern *order*  kinds table, view                              │ │
│ │ Answer to send (1.2 KiB):                                        │ │
│ │   sales.orders (table) …                                         │ │
│ │                          [Don't send]  [Send answer]             │ │
│ └──────────────────────────────────────────────────────────────────┘ │
│ ┌ run_query ──────────────────────────────── waiting in line ─────┐ │
│ └──────────────────────────────────────────────────────────────────┘ │
│ Activity (this session)                                             │
│ 14:02 describe_relation sales.orders · sent · 3.1 KiB               │
│ 14:01 run_query · declined                                          │
└─────────────────────────────────────────────────────────────────────┘
```

- **Requests** is the only place a call is approved, declined or sent. The
  server switch and **Pause** stay in the section's sidebar
  ([5.2.1](#521-where)).
- While a card is waiting, the rail's MCP icon shows the count, in amber,
  the way Automation's icon shows scheduled runs that ended badly
  ([User experience](../reference/ux.md)). The status bar names it
  (**MCP · n waiting**, or **MCP · Listening**, or **MCP · Off because …**)
  and opens this tab. Neither draws a card. A new card updates both, and
  when the window is not focused it asks the window system for attention
  (hypothesis: GPUI exposes a request for attention; otherwise the rail and
  the status bar change). It never steals keyboard focus. The status bar
  line exists only while the server is on.
- Cards are grouped by token and answered **one at a time per token, in
  arrival order**; others show **waiting in line** with their own deadline.
- The card UI is the in-app query card, extracted into a shared component
  so the assistant panel and this tab render the same states; neither keeps
  a copy ([5.8](#58-where-the-logic-lives)).
- The client's self-reported name (`clientInfo` from `initialize`) is shown
  as "says it is *X*", in muted text; the token's name is the trusted label.
- Keyboard: the section and this tab are reachable with a shortcut
  (registered with the keymap actions, rebindable) and by Tab; buttons are
  focusable and labelled for screen readers; **Enter never approves by
  default**; Escape leaves the tab without deciding. Each card's state is a
  live status region, as the in-app card's is.

#### 5.5.2 Review of `search_objects` and `describe_relation`

The answer is computed first, within the 10-second tool timeout, then held.
How it is released depends on the token's **Review** setting, which mirrors
the in-app `ReviewMode`:

| Review | Behaviour | Default |
| --- | --- | --- |
| **Every call** | Each answer waits on a card showing the arguments and the **exact text** to be returned; **Send answer** or **Don't send** | Yes, for every new token |
| **First call of each client session** | The first cache answer of each MCP session waits on a card; later ones are returned at once and listed in Activity | No |
| **Never** | Answers are returned at once and listed in Activity | No |

Why this matches ADR-0028:

- ADR-0028 requires every request to be **reviewable** before it leaves and
  starts in discovery mode (pause before every request). The MCP server
  cannot see the client's request to its own model, so the place where Jykre
  data leaves Jykre is the tool result. Reviewing each tool result is the
  closest equivalent, and is the default.
- ADR-0028 lets the user relax review (**first only**, **never**); the same
  choices exist here, per token, and every answer still appears in
  Activity with its arguments and size.
- Enabling the tool alone never covers a call under **Every call**. **Never**
  is offered ([Q-8](#12-open-questions-for-the-maintainer)): cache text then
  leaves without a card and is listed in Activity. With no protocol session,
  **First call of each client session** means the first cache answer for
  that token since this process started.

**Don't send** returns a normal result: "The user chose not to share this
answer." (a new text, in `released`'s style). Comments and defaults are sent
as in-app (ADR-0028 allows them); the card shows them before they leave.

#### 5.5.3 Approval of `run_query`

Both in-app approvals stay, in every review mode:

1. **Refusal before asking.** `run::query_sql` with the connection's
   dialect. A refused call returns the same reason text, flagged as an error
   result; no card.
2. **Statement card.** Purpose (in a quoted box labelled "The client
   says:"), the statement in full, the risk badge from `guard::risk`, the
   production line, the connection's name and colour, and "It runs in a
   session of the MCP server's own on *Sales*. Nothing of the result is sent
   until you approve it." Buttons **Decline** and **Run**, or **Run on
   production** under the in-app rule. Whether every statement on a
   production connection should read **Run on production** for MCP is
   [Q-9](#12-open-questions-for-the-maintainer).
3. **Password.** If the profile needs a password that is not in memory,
   **Run** opens the existing password dialog; cancelling it answers as
   declined (as `query_done(None)` does today).
4. **Running.** A **Stop** button; the elapsed time; the statement time
   limit of [section 9](#9-errors-cancellation-and-timeouts).
5. **Result card.** Rows, per-column hide toggles, the exact text from
   `released::release`, **Don't send** and **Send result**. Columns hidden
   earlier for this token, since the process started, start hidden.
6. The answer is `released::declined`, `released::withheld` or
   `released::sent`, unchanged.

At most **one `run_query` card per token** is pending or running at a time;
a second call while one waits returns at once: "Error: another query from
this client is waiting for the user. Ask again after it is answered." (an
error result; no card).

#### 5.5.4 Deadlines while the user has not answered

- Each decision has a deadline: **120 seconds** for a cache answer, a
  statement and a result ([Q-10](#12-open-questions-for-the-maintainer)). A
  card waiting in line starts its deadline when it reaches the front.
- When a deadline passes, the card closes as **Not answered**, nothing more
  happens, and the client gets an **error result**: "The user did not answer
  in time; nothing was sent." (for a statement: "…; the query did not run."
  For a result: "…; the query ran and its result was not sent." with no row
  count). A hung approval is never a successful result.
- While a card waits, the server sends MCP progress notifications when the
  client gave a `progressToken`, so clients that reset their own timeout on
  progress keep waiting (hypothesis: client behaviour varies).
- If the client cancels (`notifications/cancelled`) or disconnects, the card
  closes as **Cancelled by the client**; a running query is stopped through
  its `CancellationToken`, and no response is sent, as MCP specifies.

#### 5.5.5 What the client receives (decline texts)

| Situation | Result | Text |
| --- | --- | --- |
| Cache answer sent | normal | The tool's text |
| Cache answer not sent | normal | "The user chose not to share this answer." |
| Statement refused before asking | error | `run::query_sql`'s reason |
| Statement declined, password not given, or stopped | normal | "The user declined to run this query." |
| Result sent | normal | `released::release` text |
| Result not sent | normal | `released::withheld` text |
| No answer in time | error | [5.5.4](#554-deadlines-while-the-user-has-not-answered) |
| Another query pending | error | [5.5.3](#553-approval-of-run_query) |
| Server paused | error | "Jykre Studio is not accepting requests now." |

#### 5.5.6 What is logged

Per call, at `info`, in an `mcp_tool` span: the tool name
(plain names only, else `other`), the outcome (`sent`, `not_sent`,
`declined`, `withheld`, `rejected`, `timed_out`, `cancelled`, `disabled`,
`busy`), elapsed milliseconds, bytes returned, and for `run_query` the row
and hidden-column counts `released::sent` already logs. The connection ID
may be logged (it is a structured field today). Authentication failures log
one `warn` per kind, rate-limited.

**Never logged**, at any level: the token or any part of it including
`<id>`, the token's name, the `Authorization` header, SQL text, `purpose`,
patterns, paths and namespaces (they are metadata names), tool answers, row
values, the client's self-reported name. New span attributes go through the
telemetry allowlist; none holds free text.

### 5.6 States

| State | Where it shows | What the client gets |
| --- | --- | --- |
| **Server off** (default) | Section: switch off; no status bar line | Connection refused (nothing listens) |
| **Port in use** | Section: "Port 47321 is in use; choose another"; status bar **MCP · Off because the port is in use** | Connection refused |
| **Demo** (`JYKRE_DEMO`) | Section: "Not available in the demo"; switch disabled | Nothing listens |
| **No tokens** (empty) | Tokens list: "No tokens yet. Create one for each MCP client." and **New token…** | 401 |
| **Listening, idle** | Section **Listening**; status bar **MCP · Listening** | — |
| **Pending** | **Requests** tab; rail count; status bar **MCP · n waiting** | Waiting; progress notifications |
| **Declined / not sent** | Card closed with the decision; Activity row | Normal result ([5.5.5](#555-what-the-client-receives-decline-texts)) |
| **Not answered in time** | Card closed **Not answered** | Error result |
| **Tool disabled** | Nothing new; Activity counts it | JSON-RPC `-32602` |
| **Vault unavailable** | Token row **Vault unavailable**; **Try again** on its tab; creation refused | 503 |
| **Expired token** | Token row **Expired** | 401 |
| **Revoked / unknown token** | Row gone | 401 |
| **Suspended** (connection changed) | Token row **Suspended: the connection changed**; **Confirm** on its tab | 401 |
| **Connection deleted** | Tokens deleted with it | 401 |
| **Paused** | Section **Paused**; status bar **MCP · Paused** | Error result per call |
| **Cache empty or stale** | Card shows the cache's last refresh time | The tool's own "may need to refresh" text |

### 5.7 Stored versus never stored

| Stored (`config.db`, setting `assistant.mcp`) | Stored (OS vault) | Memory only | Never stored anywhere |
| --- | --- | --- | --- |
| Server on/off, port; per token: `<id>`, name, connection ID, its revision and environment label, created and expiry times, enabled tools, review policy, suspended reason | The token's secret, account `mcp:token:<id>` | The secret after first use (redacted); MCP sessions; search cursors; pending cards; previews; hidden-column names per session; last used time; Activity (bounded, last 200 entries) | The token in any file, setting, log, span, error or URL; SQL; purpose; tool answers; row values; the client's self-reported name |

Activity rows keep a call's arguments and answer only while the
application runs, as the in-app conversation does; they are never written.

### 5.8 Where the logic lives

| Piece | Location (proposed) | GPUI | Notes |
| --- | --- | --- | --- |
| Token format, generation, comparison, expiry | `jykre::mcp::token` | No | Pattern of `jjp/1`'s `Token` |
| Token records, enabled tools, review policy, suspension | `jykre::mcp::registry` | No | Reads and writes the `assistant.mcp` setting and the vault |
| Pending requests, ordering, deadlines, decisions | `jykre::mcp::broker` | No | One state machine; publishes a counter like `Engine::changes` |
| Protocol adapter (`initialize`, `tools/list`, `tools/call`, cancellation, progress) | `jykre::mcp::server` | No | Calls `CacheTools`, `run::query_sql`, `guard::risk`, `released` |
| Card state machine (asked, running, review, done) | `jykre::assistant::approval` | No | Moved out of `assistant_panel.rs`, shared by that panel and the **Requests** tab |
| Running an approved statement | `jykre::assistant_query` (outside `src/assistant/`) | No | The runtime half of `Workspace::run_assistant_query`, shared; returns only a `QueryPreview` |
| `SchemaScope` for a connection without a tab | Beside `facts` | No | Context: the connection's default (hypothesis: empty `SessionContext` resolves as a new session would) |
| MCP section: sidebar, token tabs, **Requests** tab | `src/app/mcp_section.rs` | Yes | Presents; sends intents. Same shape as `src/app/automation/` |

`src/mcp/` and `src/app/mcp_section.rs` join the `assistant_privacy` scan.
`jykre::assistant_query` is the one new module that names
`backend::query_in_context`, as the workspace does today; it hands back only
a `QueryPreview`, so `released.rs` stays the only code that turns a result
into text.

## 6. Transport

After [Stack and dependencies](../standards/stack-and-dependencies.md): MCP
defines two transports, stdio (the client starts a process and speaks
newline-delimited JSON-RPC on its stdin and stdout) and streamable HTTP (one
HTTP endpoint, `POST` for requests, optional server-sent events).

**Recommendation: streamable HTTP on loopback**, because the server lives
inside the running application and stdio would need a second process
anyway:

- Bind `127.0.0.1` only, on the port the user set (default 47321;
  [Q-11](#12-open-questions-for-the-maintainer)). Nothing listens while the
  server is off, in the demo, or before its settings are read.
- **Stateless HTTP** ([Q-12](#12-open-questions-for-the-maintainer)). Each
  request is one `POST` carrying its own bearer token. The server never
  issues `Mcp-Session-Id` and ignores the header if a client sends it.
  Revision 2025-11-25 already allows this: a session id is optional, and
  omitting it is the stateless server. Revision 2026-07-28, still a release
  candidate on 2026-11-10, removes protocol sessions and the `initialize`
  handshake; each call stands alone, and `server/discover` is how a client
  reads the server. This plan targets that shape. `rmcp` has to be checked,
  in the dependency proposal, for which revision it implements. A client
  that still sends `initialize` gets the same description and no session id.
- Plain HTTP on loopback, as the assistant and telemetry already allow for
  local addresses. No TLS, so no certificate question arises; there is never
  an "accept any certificate" option.
- The token travels **only** in the `Authorization` header. Never in the
  URL, never as a command-line argument, never in a log line.
- `Origin` and `Host` are validated against DNS rebinding, as the MCP
  transport specification requires; no CORS headers are sent, so a web page
  cannot read answers.
- At most 4 concurrent HTTP connections and one pending `run_query` per
  token; a body over 1 MiB is refused before it is buffered
  ([Q-12](#12-open-questions-for-the-maintainer)). There is no session cap:
  the wire has no sessions.

**stdio, later and optional.** A client that only speaks stdio would start a
small bridge (the same executable in a bridge mode, no window) that relays
lines to the loopback endpoint. The bridge would need the token, and the
only channel that is neither argv nor a file it writes is the environment
the client sets from its own configuration. That is outside this plan's
first phase ([Q-13](#12-open-questions-for-the-maintainer)).

**Dependency proposal (not added).** The protocol layer (version
negotiation, session IDs, cancellation, progress, list changes, the HTTP and
SSE framing) is the kind of code the dependency policy says not to
hand-roll when a maintained crate exists. Two options for the maintainer:

| Option | What | Notes |
| --- | --- | --- |
| A (chosen 2026-11-10) | [`rmcp`](https://crates.io/crates/rmcp), the official Rust MCP SDK, with only its server and streamable-HTTP-server features | Not added yet. The formal proposal still has to clear licence, health, weight and features, including whether this revision's stateless HTTP is what the crate serves ([Q-19](#12-open-questions-for-the-maintainer)) |
| B | Our own JSON-RPC layer over `hyper` 1 (already in the tree through `reqwest`) as a direct dependency with its `server` and `http1` features | Fewer crates; we own about the MCP subset we need, and its changes across specification versions |

Either way no new crate is needed for random numbers or constant-time
comparison. The formal proposal (licence, health, weight, features) is part
of the ADR in [section 11](#11-what-needs-an-adr-and-what-can-stay-here).

## 7. Lifecycle

- **Start.** After `AppView` reads its settings, if the server is on and
  this is not the demo, the listener binds. A bind failure leaves it off with
  the reason; the rest of the application is unaffected. The automation
  engine's lock does not apply: two instances would compete for the port, and
  the second says the port is in use ([Q-14](#12-open-questions-for-the-maintainer)).
- **Per token, in this process.** The wire has no session. One `CacheTools`
  per token lives until the token is revoked, rotated, suspended or the
  application quits, so an `after` cursor from an earlier `POST` still
  works. The list is capped at the last 100 cursors; an older `after` gets
  the existing "not a value this search returned" text. Hidden columns are
  remembered the same way.
- **The database session.** One session per token, of the server's own,
  opened on the first approved query in the connection's default context,
  kept between queries and closed when the token is revoked, suspended or
  rotated, after 10 minutes idle,
  when the connection's profile changes, or at quit. After each run the
  session's transaction state is read; a session left inside a transaction
  is closed (which rolls it back) and the result card and the answer say so,
  as report panels do (`SessionRun::transaction_closed`). (Hypothesis: the
  in-app assistant session does not check this today.)
- **The connection.** Deleted: tokens deleted. Profile changed: tokens
  suspended. Password not remembered: asked on **Run**. Vault locked: the
  password dialog says why, as for a tab. Cache never refreshed or stale:
  tools answer from what is there, with the existing texts; the card shows
  the last refresh time.
- **Quit.** The listener closes first; pending cards close as **Cancelled:
  Jykre Studio is quitting** and their clients get an error result if the
  connection is still open; running queries are cancelled and their sessions
  closed within the shutdown deadline. Nothing is resumed at the next start.

## 8. Tools as MCP tools

### 8.1 What the client is told when it connects

The description is answered only after the bearer token is accepted, so the
text is that token's connection, not one description shared by every client.
On the 2026-07-28 shape the client reads it from `server/discover`. A client
that still sends `initialize` gets the same fields and no session id.

- `serverInfo.name` stays `jykre-studio`, a stable identifier.
- `serverInfo.title` is `Jykre Studio — ` plus the connection's current name
  as the sidebar shows it, and the environment label when the connection has
  one: `Jykre Studio — Sales (Production)`.
- `instructions` is a short text for the model: this server reads the
  metadata cache of that connection; if `run_query` is enabled, statements
  run only there, and only after the user approves them in Jykre Studio. It
  names the connection the same way. It does not include the host, the user,
  a credential or SQL.

The name is read on each discover or initialize, so a rename appears on the
next one. A suspended, expired or revoked token gets 401 and no description.
The name is not written to the log.

(Hypothesis: `title` and `instructions` are the fields both results carry.
Check them against the revision `rmcp` implements.)

The tool descriptions below stay the in-app ones. They do not repeat the
connection; `instructions` is where the client learns it.

`tools/list` returns, for each enabled tool, the `ToolSpec` from
`CacheTools::specs(true)` filtered by the token: `name`, `description` and
`input_schema` (as MCP `inputSchema`) unchanged, so the bounds a client reads
are the bounds in the code. Added MCP fields:

| Tool | `title` | Annotations (hints only; clients may ignore them) |
| --- | --- | --- |
| `search_objects` | Search objects | `readOnlyHint: true`, `openWorldHint: false` |
| `describe_relation` | Describe a table or view | `readOnlyHint: true`, `openWorldHint: false` |
| `run_query` | Run a query (asks the user) | `readOnlyHint: false`, `destructiveHint: true`, `openWorldHint: false` |

`run_query`'s description says "the user approves…" already; it stays.

`describe_relation` gains one optional argument, `include_comments` (default
true), shared with the in-app assistant rather than a switch that exists
only on a token ([Q-18](#12-open-questions-for-the-maintainer)). False omits
column comments and defaults from the text. The in-app model can pass it
too. This changes `CacheTools::specs`, so it is a later change to the tool
the assistant already has, not a second tool.

`tools/call` arguments go to the same functions. Answers are one `text`
content item. Error results (`isError: true`) are the MCP layer's own
refusals and the tool answers that are errors. Today a tool error is a
string starting with `Error:`; rather than test that prefix, `CacheTools::call`
would return a small typed answer (text plus whether it failed) whose text is
unchanged for the in-app path. No `outputSchema` is declared: the answers are
text by design (ADR-0028).

The MCP server does not send the in-app system prompt. What the client is
told about the connection is [8.1](#81-what-the-client-is-told-when-it-connects);
the tool descriptions are the rest.

## 9. Errors, cancellation and timeouts

| Operation | Limit | On expiry |
| --- | --- | --- |
| One cache tool | 10 s (`TOOL_TIMEOUT`, unchanged) | The existing text, as an error result |
| Waiting for the user's decision | 120 s per decision (proposal) | Error result, nothing sent |
| Approved statement | 300 s, `ExecutionOptions`' default, per token ([Q-15](#12-open-questions-for-the-maintainer)); **Stop** cancels | Error preview, sent only if the user sends it |
| Reading the token from the vault | 10 s, as passwords | 503; row shows **Vault unavailable** |
| Writing or removing the token in the vault | 10 s | Creation refused / removal retried |
| Reading a request's headers and body | 10 s | Connection closed |
| Idle HTTP connection | 60 s | Connection closed |
| Server shutdown at quit | Within the application's shutdown deadline | Work dropped; sessions closed in the background |

Each row joins the [Reliability](../standards/reliability.md#timeouts)
table when implemented. Rules carried over: a statement is never retried; a
cancelled or timed-out statement leaves its session recovered or closed by
the driver; a panic in a request task becomes an error result, not a crash;
every wait takes a `CancellationToken`. Errors from the drivers keep their
`ErrorCode`; a client never receives a server message unless the user sent
it through the result card.

## 10. Threats

| Threat | Mitigation |
| --- | --- |
| **Stolen token** | Loopback only; expiry; revoke and rotate; one connection per token; `run_query` off by default and each statement and result approved in Jykre's window, so a token alone never runs SQL or moves a value; cache answers reviewed per call by default; Activity shows every call. The client's configuration file holds the token and is outside Jykre's control; the section says so |
| **A process of the same user** | Same as `jjp/1`: the token protects against other users and port guessing, not against the same user, who can read the client's configuration anyway. Approval in the window remains the barrier for values |
| **Browser on this computer (DNS rebinding, CSRF)** | `Origin`/`Host` validation, no CORS, bearer header required |
| **A client that retries a declined query** | One pending query per token; each retry is a new card the user sees, marked with how many times that statement was declined since the process started; **Pause** stays in the section and **Revoke** on the token's tab. The server does not stop asking ([Q-16](#12-open-questions-for-the-maintainer)) |
| **A client that asks for tools the token lacks** | Not listed; `-32602`; no card; counted only |
| **Prompt injection in SQL or `purpose`** | Both rendered as plain text, never markup; `purpose` in a quoted "The client says:" box, never in Jykre's own wording; the statement shown in full with no cut; `query_sql` refuses several statements and `:name`; `guard::risk` badges; control and bidirectional characters shown as visible escapes in the card (new; [Q-17](#12-open-questions-for-the-maintainer)); nothing runs without **Run** |
| **Metadata comments or defaults that hold secrets** | Sent as in-app (ADR-0028 allows them); under **Every call** the card shows the answer before it leaves; a per-token "leave out comments and defaults" option would change `describe`'s output and is [Q-18](#12-open-questions-for-the-maintainer) |
| **Result values that carry instructions** | Text only; only after **Send result** |
| **Demo mode** | The server never starts with `JYKRE_DEMO`; the section says so |
| **Token in logs, URLs or argv** | Header only; URL tokens refused; log and telemetry tests ([section 13](#13-verification-after-a-future-implementation)) |
| **Resource exhaustion** | Body limit, 4 concurrent HTTP connections, one pending query per token, bounded Activity and cursors |

## 11. What needs an ADR and what can stay here

Per [Workflow](../standards/workflow.md#architecture-decisions), a change
that affects more than one crate, a core contract, the persistence schema or
the stack needs an ADR. This one needs **one new ADR**, proposed by an agent
and accepted only by the maintainer, before any code:

- A new destination for the tools' answers (an external client), extending
  ADR-0028's privacy statement and review rule, and the Security standard's
  "AI assistant" section.
- A new listening socket in the application (beside `jjp/1`'s).
- A new vault account family in `jykre-secrets` (crate API change and a new
  row in Security's credential table).
- The dependency in [section 6](#6-transport), with the policy's proposal
  (why, licence, health, weight, features).

What can stay in this plan: the UI placement, the states and texts, the
default values (port, deadlines, expiry choices, limits), the refactors that
move the card state machine and the query run out of views, and the
verification list. The new `assistant.mcp` setting uses the existing
`settings` table, so it is not a schema change; it is documented in
[Persistence](../reference/persistence.md) when implemented.

Not changed: `Driver`, `Session`, `RowSink`, `IntrospectionSink`,
`SecretStore`, the SQLite schema, `jykre-assistant`.

## 12. Open questions for the maintainer

Answered on 2026-11-10. Q-2, Q-5 and Q-8 were answered after the vault
limit and the cache answer were spelled out.

| ID | Decision |
| --- | --- |
| Q-1 | **Answered.** Wanted. Not assigned to a release. |
| Q-2 | **Answered.** One credential per token in the OS vault, account `mcp:token:<id>`. The Windows cap is 2,560 bytes per credential; a token is a few dozen bytes, so the cap does not apply. A hash in `config.db` was declined. |
| Q-3 | **Answered.** One connection per token. The server's description names that connection ([8.1](#81-what-the-client-is-told-when-it-connects)). |
| Q-4 | **Answered.** The assistant's `queries` preference and a token's `run_query` switch are independent. |
| Q-5 | **Answered.** The token is created only when the vault write succeeds. No session-only token and no file. |
| Q-6 | **Answered.** Choices are 7, 30 or 90 days, 1 year, or never. Default 30 days. |
| Q-7 | **Answered.** The old token stops at once. No grace period. |
| Q-8 | **Answered.** **Never** is offered. Cache text (`search_objects` and `describe_relation`) can leave without a card. The default for a new token stays **Every call**. |
| Q-9 | **Answered.** **Run on production** only when the connection is production and the statement is risky, as the in-app assistant does today. |
| Q-10 | **Answered.** 120 seconds per decision. |
| Q-11 | **Answered.** `127.0.0.1` only. Default port 47321. |
| Q-12 | **Answered.** 1 MiB body, 4 concurrent HTTP connections, one pending `run_query` per token. The wire is stateless: no `Mcp-Session-Id`, so there is no session cap. Target the 2026-07-28 shape ([section 6](#6-transport)). |
| Q-13 | **Answered.** No stdio bridge in this phase. |
| Q-14 | **Answered.** Any instance may listen. A second one reports that the port is in use. The automation lock does not decide. |
| Q-15 | **Answered.** An approved statement runs for at most 300 seconds. |
| Q-16 | **Answered.** Repeated declines are marked on the card. The server keeps asking. |
| Q-17 | **Answered.** Control and bidirectional characters are shown as escapes on MCP cards and in the in-app assistant panel. |
| Q-18 | **Answered.** Not a per-token switch. `describe_relation` gains an optional `include_comments` argument (default true), and the in-app assistant offers the same argument. |
| Q-19 | **Answered.** Option A, `rmcp`. The crate is not added until the formal dependency proposal is accepted. |
| Q-20 | **Answered.** The card shows the purpose and the client's self-reported name, both as quoted text from the client. |

**Q-2 and Q-5, the Windows vault.** Credential Manager limits one generic
credential's secret to 2,560 bytes (`CRED_MAX_CREDENTIAL_BLOB_SIZE`). Jykre
already stores passwords and API keys through the `keyring` crate into that
vault (`crates/secrets`). The maintainer chose one credential per token.
A token is a few dozen bytes, so the cap does not apply. Putting every
token into one credential would hit it; that shape is not used. A hash in
`config.db` was the alternative and was declined.

## 13. Verification after a future implementation

Nothing here was run: this task changes documentation only, and
`cargo xtask check` was not run. After an implementation the gate would need:

- **Gate.** `cargo xtask check`; `cargo xtask check --full` because the query
  run touches execution paths; `cargo deny --locked check` for the
  dependency.
- **Privacy test.** A fake MCP client over loopback against a synthetic
  SQLite file proves that a result value, a hidden column's value, a
  withheld result, a declined query and a bound parameter value never appear
  in any MCP response, while a comment and a default do (the in-app privacy
  test's cases, through MCP).
- **Approval required.** With no decision, a `run_query` call ends as an
  error result after the deadline and the fake driver counted zero
  executions; a cache call under **Every call** returns nothing before
  **Send answer**.
- **Enabled tools.** A new token lists no `run_query` even with `queries`
  on; a disabled tool gets `-32602`, no card, no execution.
- **Tokens.** Format and parsing; constant-time comparison; redacted
  `Debug`; not `Serialize`; expired, revoked, suspended and unknown tokens
  get the same 401; a token in the URL is refused; vault failure at creation
  leaves no record; the vault account never collides with `connection:` or
  `assistant:` accounts.
- **No secret in logs.** A test runs calls with a synthetic token, SQL,
  purpose and row values and checks none reaches the log above `trace` or
  the telemetry export.
- **Listener.** Binds loopback only; nothing listens when off, in the demo
  or before its settings are read; non-loopback `Origin` refused.
- **Source checks.** `assistant_privacy` scans `src/mcp/**` and the new
  views; the architecture check still passes.
- **Visual check**, recorded separately: the MCP section (empty, vault
  unavailable, expired), the token shown once in its tab and gone after
  **Done**, the **Requests** tab with a cache card, a statement card on a
  production connection, a result card with hidden columns, the rail count,
  the deadline, keyboard and focus. Preferences has no MCP page.

## 14. Known deviations and out of scope

| Item | Status |
| --- | --- |
| Live reads (Inspect, object definitions) | Out of scope, as ADR-0028 deferred them ("later, with consent per tool"); no MCP tool reads the server |
| The in-app assistant's conversation, prompts, compaction and costs | Not exposed through MCP |
| MCP resources, prompts, sampling, elicitation | Not offered |
| Remote or non-loopback access, TLS for the endpoint | Out of scope |
| Running without the window | Out of scope; the server lives with the application |
| `Workspace::run_assistant_query` holds the statement run in a view | Existing code; moved to the library by this plan's work only if approved, not as a side effect of other work |
