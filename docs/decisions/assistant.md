# Decisions: AI assistant and MCP server

Why this area is built as it is. Each entry records what was decided, why, and which alternatives lost. The rules themselves are in [AI assistant and MCP server](../reference/assistant.md). Entries are numbered `D-NNNN`, and the numbers are stable.

<a id="d-0028"></a>
## D-0028: An agentic AI assistant over the metadata cache, with every request reviewable

*Accepted 2026-11-07 · implemented*

**Decision.** The application drives an agentic loop with `search_objects`,
`describe_relation` and, optionally, `run_query`, over the local metadata
cache only ([Tools](../reference/assistant.md#tools)). Every request is reviewable before it leaves,
what may be told is an allow-list in types, the key lives in the system vault,
and nothing runs without the user ([What the assistant may read](../reference/assistant.md#what-the-assistant-may-read),
[The agentic loop](../reference/assistant.md#the-agentic-loop-and-request-review)). The context is the
whole conversation, compacted only by the user.

**Why.** The assistant was a simulated panel, and the project is still
discovering how such an assistant should behave, so the user must see each
request, each tool step and each token and cost. The maintainer chose what
may leave: the SQL and errors as written, and metadata with defaults and
comments, but never result values, bound values or live reads. A window of
the oldest turns (48 KiB in the first version) was removed because cutting
from the front changed the start of every request and so defeated the
providers' prompt cache. Values may leave only through `run_query`, added
2026-11-06, with a second approval of the exact text; its own session keeps
the tab's transaction and temporary tables intact. The costs are a second
reqwest (0.13 beside the Databricks driver's 0.12) and `genai`'s
dependencies, and prompt injection through comments or results, which the
read-only tools and the approvals contain. Spike of `genai` 0.7.3
(2026-11-06): streaming, tool calls and usage including cached tokens read
against a local fake, and `aws-lc-rs` is absent from the tree.

**Rejected.**
- Our own client on reqwest 0.12: about a thousand lines to keep in step with
  each provider's format; `genai` covers four protocol families and the
  private transport keeps this as the fallback.
- `rig-core`: an agent framework (RAG, vector stores) far larger than a chat
  client; we drive the loop.
- Sending the schema ahead: large requests, mostly irrelevant tables.
- A window that leaves out the oldest turns: breaks the prompt cache and
  decides silently what is dropped.
- Summarizing on its own past a size: a paid, lossy request nobody asked for.
- Sampling rows without asking: values would leave unseen.
- `run_query` on the tab's session: could run inside the tab's open
  transaction or discard a result being paged.
- Letting the user edit the statement: the model would believe it ran what it
  asked for.
- Tools that read the server: live reads and definitions can hold literal
  data; later, with consent per tool.
- Masking literals in SQL and errors: the maintainer chose to send them as
  written.

<a id="d-0042"></a>
## D-0042: An MCP server for the assistant's tools

*Accepted 2026-11-10 · partly (the server, tokens, vault account, `assistant.mcp` setting, broker and `assistant_privacy` scan are in `src/mcp/` and `crates/secrets`; the shared card state machine in `jovru::assistant::approval` is not, see [Known deviations](../reference/assistant.md#known-deviations))* · Amends D-0028

**Decision.** An external MCP client chosen by the user may call the tools
D-0028 already defines, as a second destination for the same answers. The
server is loopback only, stateless and open only while the window is open;
one token is one connection, its secret is in the OS vault and the record is
the `assistant.mcp` setting; tools are denied by default and approval happens
only in Jovru's window ([MCP server](../reference/assistant.md#mcp-server)).

**Why.** D-0028 said new tools read the local cache only and the Security
standard spoke of "the provider the user chose". An MCP client is a different
destination Jovru cannot see, so the destination needed a decision, along
with a new listening socket beside `jjp/1`, a vault account whose name is
chosen at run time and an MCP library. The tools, their bounds and both
`run_query` approvals already existed, so the MCP layer only adapts them. A
token alone must never run SQL or release a value, so approval stays in the
window; since Jovru cannot see the client's own request, reviewing each tool
result is the closest equivalent of D-0028's request review, and is the
default. Stateless HTTP removes session state to keep, cap and drop. A
credential per token avoids the Windows vault's 2,560-byte cap on one
credential holding every token. The cost is a listening socket while the
window is open, the new account type and the `rmcp` dependency.

**Rejected.**
- A hand-rolled JSON-RPC layer over `hyper`: fewer crates, but session,
  cancellation, progress and specification revisions to keep; the maintainer
  chose `rmcp`.
- A hash of the token in `config.db`: a verifier of the secret would sit
  beside the record; declined.
- One vault credential holding every token: the Windows cap and one secret
  shared by every client.
- A new SQLite table for tokens: a migration for data that is a setting.
- stdio or a stdio bridge: a second process and a token path that is not the
  header; none in this phase.
- Stateful HTTP with `Mcp-Session-Id`: session state to keep, cap and drop.
- Binding beyond loopback, or TLS: remote access, or a certificate to manage.
- A token in the URL: URLs are logged, stored and shared; the header is the
  only place.
