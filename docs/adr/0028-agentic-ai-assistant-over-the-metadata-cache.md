# ADR-0028: An agentic AI assistant over the metadata cache, with every request reviewable

- **Status:** Proposed
- **Date:** 2026-11-06
- **Authors:** Alan de Oliveira (decisions; plan approved 2026-11-06), drafted with an AI agent

## Context

The assistant panel was simulated: it matched keywords and showed canned
answers. The maintainer wants it to work with a real model for three tasks:

1. **Fix with AI**: a statement failed; the model gets the statement and the
   server's error and returns a corrected statement.
2. **Write SQL**: the model writes a query from a description.
3. A **conversation** about the database's structure.

What may leave the computer was decided by the maintainer: the SQL as the
user wrote it, the server's error message as it came (codes, message,
position), the metadata in the local cache *including column defaults and
comments*, and how many rows a statement returned (and whether there were
more, and how many it changed). Result values, the values bound to `:name`
parameters, live reads of the server (Inspect, definitions) and the API key
(beyond the provider's authentication header) never leave.

The project is discovering how such an assistant should behave, so every
request must be visible before it is sent, the tools the model can call must
be visible, and each call's tokens and cost must be shown. Several providers
must work, including a model running on the user's computer.

## Decision

1. **The application drives an agentic loop, round by round.** A request
   starts with one message (the fix, the description or the question). The
   model has two tools, `search_objects` (name patterns with `*` and `?`;
   `*` in a schema lists it) and `describe_relation`, which read only this
   computer's metadata cache, with bounded answers (100 matches per page,
   300 columns and 16 KiB per description). A separate listing tool was
   dropped: it duplicated `search_objects` with `*` and invited the model to
   read whole schemas before searching. When the model asks for tools,
   the application answers them from the cache and prepares the next round;
   a turn stops after 8 rounds until the user continues it. No metadata is
   sent ahead: the model asks for what it needs.
2. **Every request can be reviewed before it leaves.** Each round's
   canonical request (system prompt, messages, tool results, tool
   definitions and limits) is shown as a card with its estimated tokens and
   cost, as messages, tools or JSON. The review mode (Preferences) pauses
   before every request (default during discovery), before the first only,
   or never. Each tool step shows its arguments and the full answer the next
   request carries. The provider's real token counts (input, output, cached)
   and the cost at the prices the user entered are shown per round and per
   conversation; no price is built in. **Copy trace** exports the whole
   conversation as JSON.
3. **What may be told is an allow-list in types.** `src/assistant/facts.rs`
   (`ErrorFacts`, `RunFacts`, `SchemaScope`) is the only way a tab's state
   reaches a request; none of them holds a value, a bound parameter, a
   session or a credential. The xtask check `assistant_privacy` forbids the
   assistant's code from naming result data, parameters, sessions,
   credentials or server reads, and a test runs the three tasks against a
   fake provider and proves that a result value and a bound value never reach
   any request body, while the comment and default do.
4. **Providers through `genai`, behind a private transport.** The crate
   `jykre-assistant` (no UI, core contracts only) holds the canonical
   request, one streamed round, error classification, pricing and SQL-block
   detection; `genai` 0.7 is used only in its `transport` module. Providers:
   Anthropic, OpenAI, any OpenAI-compatible server (LM Studio, Databricks
   serving, gateways) and Ollama. Addresses must be HTTPS except loopback;
   redirects are not followed. TLS uses rustls with `ring` (installed as the
   process's crypto provider) and the system's trust store, so `aws-lc-rs`
   is not built.
5. **The key lives in the system vault.** `ApiKeyStore` in `jykre-secrets`
   (accounts `assistant:{anthropic|openai|openai-compatible}:api-key`) keeps
   it, read lazily on the first request, within the vault's 10-second
   limit; it is never shown back. The provider, model, address, review mode
   and prices are the `assistant.preferences` setting.
6. **Nothing runs on its own.** Suggested SQL is shown in blocks with Copy,
   Insert at the cursor and, for a fix, Replace the failed statement, which
   replaces it only where it still is unchanged, as one undo step; a block
   that changes or deletes every row is marked.
7. **The panel is its own cached entity** (as ADR-0012), with a virtualized
   list; a streamed answer redraws the panel at most about 30 times a
   second and never the workspace per piece of text.
8. **Limits and failures.** Connection 15 s, first byte 120 s, silence
   between pieces 60 s, a round 300 s, **Test** 30 s, a tool 10 s. A
   failure is classified by status only (the provider's body is never
   shown, logged or kept), and a round is never retried automatically.
   Logs record per round the provider, time, token counts and code; never
   a prompt, answer, schema text or key, and the provider library's own log
   is off.
9. The demo (`JYKRE_DEMO`) keeps canned answers and sends nothing.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Our own client on the existing reqwest 0.12 (SSE for OpenAI and Anthropic) | No new dependency; the exact wire body is available before sending | About a thousand lines to keep in step with each provider's streaming and tool format | genai covers four protocol families today; the transport stays private so this remains the fallback |
| `rig-core` | Many providers, agents | An agent framework (RAG, vector stores) far larger than a chat client | We drive the loop ourselves |
| Send the schema ahead with each request | One round | Large requests, mostly irrelevant tables | Tools fetch only what the model asks for |
| Sample rows to ground answers | Better answers about data | Values leave the computer | Rejected by the maintainer |
| Tools that read the server (Inspect, definitions) | Indexes, view bodies | Live reads; definitions can hold literal data | Later, with consent per tool |
| Mask literals in SQL and error text | Fewer values leave | The maintainer chose to send SQL and errors as written | Not wanted |

## Consequences

- Positive: the three tasks work with cloud and local models; the user sees
  and approves every request, tool step, token and cost; privacy is enforced
  by types, a source check and a test.
- Negative or costs: a second reqwest (0.13, beside the Databricks driver's
  0.12) and genai's dependencies; the review card shows the canonical
  request, not the provider's wire JSON, because genai does not expose the
  body before sending (the test checks that every reviewed text is what the
  provider received); comments, defaults and error messages can hold
  instructions aimed at the model (prompt injection), so tools are read-only
  and nothing is executed; the provider's own retention of requests is the
  user's choice of provider.
- What becomes required or forbidden: the assistant's code reads tab state
  only through `facts`; new tools read the local cache only unless an ADR
  says otherwise; a new provider goes through the transport; `jykre-core`
  never depends on `genai`.

## How to validate

- Spike of genai 0.7.3 (2026-11-06): streaming, tool calls and usage
  (including cached tokens) read against a local fake; `cargo tree -i
  aws-lc-rs` finds nothing.
- `cargo xtask check`: the transport tests (streaming, tools, statuses
  without bodies, cancel closing the connection, silence and round
  timeouts), the privacy test, the log test, the loop tests and the
  architecture and `assistant_privacy` checks.
- Visual check: Fix with AI against a SQLite fixture and a fake
  OpenAI-compatible server: review, two rounds with tool steps, streamed
  answer, costs, Replace.
