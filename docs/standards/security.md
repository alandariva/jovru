# Security

A SQL client holds the keys to its users' databases. Security mistakes here
have a high impact and are hard to undo once a secret has reached a file or a
log. How to report a vulnerability is in [SECURITY.md](../../SECURITY.md); this
document is the engineering standard.

## Credentials

### Where they live

| Credential | Storage |
| --- | --- |
| PostgreSQL/MySQL password, with **Remember password** | OS vault through `jovru-secrets`: service `Jovru`, account `connection:<connection-id>:password` (D-0004) |
| PostgreSQL/MySQL password, without it | Memory only, for the session |
| Assistant API key (Anthropic, OpenAI, OpenAI-compatible endpoint) | Only in the OS vault through `jovru-secrets` (`ApiKeyStore`): service `Jovru`, accounts `assistant:anthropic:api-key`, `assistant:openai:api-key` and `assistant:openai-compatible:api-key`. The accounts are fixed (`ApiKeyAccount`); the UI never names one. Never in `config.db`, settings or logs |
| OpenTelemetry collector headers (usually its authentication) | Only in the OS vault through `jovru-secrets` (`ApiKeyStore`): service `Jovru`, account `telemetry:otlp:headers` (`ApiKeyAccount::telemetry_headers`). Never in `config.db`, settings or logs, never shown back (D-0029) |
| MCP client token | OS vault through `jovru-secrets`: service `Jovru`, account `mcp:token:<id>`. `<id>` is 10 lowercase base32 characters and is not the secret. The secret is never in `config.db` or logs (D-0042, D-0004) |
| Databricks token | **Not stored by Jovru.** Resolved by the Databricks CLI when needed and kept in memory (D-0005) |

### Rules

1. **Never in plain text** in SQLite, JSON, configuration, error messages,
   logs, panic messages or `Debug` output. Driver configurations reject
   password and token fields.
2. **No home-made encryption.** A file encrypted with a key inside the binary
   is plain text with extra steps.
3. **Vault unavailable** (no Secret Service, a locked session): the password
   is used only for the session, and the UI says why. **Remember password**
   is saved as off, so the preference never claims a password the vault does
   not hold; a remembered connection whose entry is missing says so when it
   opens. There is never a fallback to a file.
4. In Rust, passwords exist only as `SecretString`; `expose_secret()` is called
   at the point of use. In the UI, a password exists only in the form field
   while the dialog is open. The application never shows a stored password
   back.
5. Turning off **Remember password**, or deleting a connection, removes the
   vault entry. A failure is reported to the user, without the secret.
   Editing a connection with the password field empty keeps the previous
   password only while the driver, host, port and user stay the same; any
   other change sends no password and removes a remembered one.
6. Connection URLs with an embedded password (`postgres://user:pass@host`) are
   not accepted; drivers reject URLs in the host field.
7. The vault is reached through `keyring` (Windows Credential Manager, macOS
   Keychain, the Linux Secret Service). Vault calls run on the blocking pool
   (`spawn_blocking`), never on the UI thread, and their failures are
   reported without the secret.

## Encrypted transport

| Database | Modes | Notes |
| --- | --- | --- |
| PostgreSQL | `disable`, `prefer`, `require`, `verify-ca`, `verify-full` | The mode is always explicit in the configuration; the form starts at `prefer` |
| MySQL | `disabled`, `preferred`, `required`, `verify-ca`, `verify-identity` | The form starts at `preferred`. Authentication methods that need RSA require TLS |
| Databricks | HTTPS only | The workspace origin must be `https`; redirects are disabled and external result links are not followed |

- The whole stack uses **rustls** with the operating system's root
  certificates, so corporate CAs work and builds behave the same everywhere.
- There is no "accept any certificate" option. Adding one needs the maintainer's approval and a decision entry.
- The UI shows whether the active connection is **encrypted**, and warns when
  `prefer`/`preferred` ended up without TLS.

## Authentication and authorization

- The application uses the database user's own permissions. It never tries to
  elevate privileges or work around a permission error.
- Introspection with partial permissions does not fail: unreadable subtrees
  are kept from the last read and marked with a warning
  ([Persistence](../reference/persistence/metadata-db.md#atomic-metadata-refresh)).
- Databricks requests run with the token of the CLI profile; Unity Catalog
  permissions are enforced by the server.
- A report panel in another PostgreSQL database of the server opens its
  session there with the profile's own user, credentials and TLS, and only
  when the profile offers the server's other databases (`allDatabases`);
  otherwise the panel fails with a validation error
  (D-0025). The server decides what that user
  may read in it.
- A report's panels may run on several connections: each connection that
  needs a password asks for it before the run, one after another, and the
  vault is read for each as for a query tab. On production connections the
  risky statements of each are checked and one confirmation names those
  connections before anything runs.

## SQL sent to servers

- The application runs the SQL the user wrote, as written. It never rewrites
  it or appends to it, with one exception: in a report or a query tab, the
  PostgreSQL and MySQL drivers replace each `:name` parameter marker with a
  fixed reference to a value they bind, never with the value itself
  (D-0023), and on a
  classic Databricks cluster, which cannot bind, each marker becomes a typed
  literal: text only as the hexadecimal of its bytes
  (`decode(X'…', 'UTF-8')`), the other types only after checking that they
  hold nothing but their own characters
  (D-0026). SQL warehouses bind
  the values themselves and get the SQL unchanged.
- A query tab keeps the values typed in its parameter fields in `config.db`
  with the tab, as it keeps the SQL (a report keeps only its defaults). They
  are never logged nor written to the history. **Explain**
  is an explicit action that sends `EXPLAIN` followed by the chosen query or
  data change, never `EXPLAIN ANALYZE`, so the statement does not run
  ([Architecture](../reference/architecture.md#run-a-query)). Opening a table runs
  `SELECT * FROM` it, quoted by the driver, which is written in the editor
  before it runs and only reads; the row limit bounds each page.
- SQL generated by the application binds values as parameters and quotes
  identifiers with the driver's rules ([Rust guidelines](rust-guidelines.md#sql-generated-by-the-application)).
- Editing a result
  (D-0039) writes
  only after one confirmation that lists every change and the statements
  with their placeholders; on a production connection the same dialog warns
  and its button names it. The driver generates one statement per change,
  naming one row by its key, with every value bound (never written into the
  SQL), checks the table again before the first statement and commits only
  when each change affected exactly one row. The catalog queries that read
  the table bind their names. Pending edits are held in memory only: never
  in `config.db`, logs, telemetry or the history; a write is logged with its
  counts and outcome, never its values. Databricks edits become statements
  with `:name` markers in a new tab, whose values sit in its parameter
  fields (kept with the tab like any typed value) and which run, like any
  run, through `jovru::guard`.

## AI assistant

The assistant (D-0028)
sends requests to the provider the user chose. What a request may carry:

- **Sent:** the SQL as the user wrote it (the failed statement, the editor's
  SQL for **Write SQL**), the server's error message as it came, with its
  codes and position, how many rows a statement returned and changed, and
  what the metadata tools read from this computer's cache: names, types,
  nullability, keys, column defaults and comments.
- **Sent only after two approvals:** the result of a query the model asked
  for with `run_query`. The user approves the statement, which runs in a
  session of the assistant's own; then the user sees the rows and the exact
  text to be sent, may hide any column (its name and type stay, each value
  becomes `[hidden]`) and sends it or not. At most 50 rows, cells cut at
  200 characters, 16 KiB. Preferences can turn the tool off.
- **Never sent:** the values of result tabs, the values bound to `:name`
  parameters, a hidden column's values, a result the user did not send,
  anything read live from the server (Inspect, definitions), credentials,
  and the API key outside the provider's authentication header.
- Error messages and SQL may hold values the user typed or the server
  quoted; the maintainer chose to send them as they are.
- Every request can be reviewed before it leaves (Preferences: every
  request by default, the first only, or never), with its tool steps and
  token counts. The review shows the request the application builds; the
  provider library turns it into its wire format without adding content.
- Every request repeats the whole conversation, so a result the user sent
  goes again with each later request, until the user compacts the context:
  **Prune long tool results** stops sending it; **Summarize with the
  model** replaces the conversation with the model's summary (asked to
  leave values out), which then goes in the system prompt of later
  requests.
- The rule is enforced by types (`jovru::assistant::facts`, and
  `jovru::assistant::released` for query results), by a source check in
  `cargo xtask check` (`assistant_privacy`, which lets only `released.rs`
  name a result) and by tests that prove a result tab's value, a bound
  value, a hidden column's value, a withheld result and a declined query
  never reach a request.
- Addresses are HTTPS, except a server on this computer (loopback); TLS uses
  rustls with the system's trust store; redirects are not followed.
- The model's answer is untrusted text: it is shown as plain text, never
  as markup, and its SQL is never run by the application without the
  user's approval. Comments, defaults, error messages and query results can
  carry instructions aimed at the model; the cache tools only read
  metadata, and a statement the model asks for runs only after the user
  has read and approved it.
- Logs record each round's provider, time, token counts and error code;
  never a prompt, an answer, schema text or a key. The provider library's
  own log is off (`genai=off`), as its warnings can quote answers.

### MCP client

An MCP client is a second destination for the same tools
(D-0042). The server
binds the loopback address only and accepts the token only as a bearer.
An answer leaves only through the MCP section's approval. `run_query`
still needs both approvals. Cache text follows the token's review mode.
Nothing here sends a prompt to a provider. The in-app assistant still
sends its requests to the provider the user chose, under the rules above.

## Telemetry

Telemetry is off by default; the user turns logs and traces on separately in
**Preferences › Diagnostics › Telemetry**
(D-0029), which are the only configuration: the application reads no
`OTEL_*` variable of its own (the OTLP library still merges
`OTEL_EXPORTER_OTLP_*HEADERS` into the headers when they are set). Only logs
and traces are exported, no metrics, over OTLP with HTTP/protobuf or gRPC.
What may leave:

- **Sent:** span names, kinds and times; the database system and namespace
  (database, catalog, schema), the statement's first keyword from a fixed
  list (otherwise `OTHER`), row counts, sizes, SQLSTATE and error codes; the
  HTTP method, status and URL template with every identifier replaced by
  `{id}`; the Databricks CLI subcommand and exit code; the AI provider,
  model and token counts; the connection's identifier; the vault item kind
  and call; and the log's events as the local log keeps them, without the
  ones below.
- **Never sent:** SQL text, even sanitized; values and bound parameters;
  credentials and headers; hosts, ports, users and connection options;
  paths; request and response bodies; server error messages; the computer's
  or the user's name; `trace` events; events with a free-text field (`sql`,
  `file`, `path`, `url`, `error`, `account`), which stay in the local log.
- Enforced by an allowlist of span attributes applied before export
  (`jovru::telemetry::attributes::ALLOWED`), per-layer filters on the
  bridges, and tests that run a statement with a synthetic password, SQL,
  values and host and check none is exported
  (`backend::tests::telemetry_of_a_run_carries_metadata_only`,
  `telemetry::tests`). Before export every span attribute outside the
  allowlist is removed, with span events, links and status descriptions. A
  fake collector on `127.0.0.1:0` also checks that protobuf arrives with the
  configured headers on `/v1/traces` and that a 401 is an authentication
  failure.
- A new allowlisted name is reviewed like a log field, and free text never
  goes into a span field. A span around an external call uses
  `jovru::client_span!` ([Reliability](reliability.md#structured-fields)).
- The resource names the service, its version, an identifier of the run, the
  operating system and the architecture; never the computer's or the user's
  name.
- The collector's address is HTTPS, or plain HTTP on this computer; it
  never carries a user, a password, a query or a fragment. Redirects are
  not followed. Its authentication headers are kept in the vault as a secret
  ([Credentials](#where-they-live)). The trace context is not sent to
  databases, Databricks or AI providers.

## Logs and diagnostics

These rules apply to any output, including the logs ([Reliability](reliability.md#logs)):

- Never log passwords, tokens, `Authorization` headers, connection URLs,
  cell values or query parameters.
- SQL text only at `trace`, which is off by default; at higher levels, log its
  length.
- Automation never logs an input value, a result, a server or program
  message, a program's path, arguments or environment, or a `jjp/1` token;
  the program's own output goes to its run's log files, not to the
  application's log ([Automation](../reference/automation.md#automation-external-programs-and-jjp1)).
- No header logging in `reqwest`, and statement logging in SQLx turned off
  (`disable_statement_logging()`) in every driver and in `jovru-storage`,
  because SQLx logs SQL by default.

## External processes (Databricks CLI)

The Databricks CLI's authentication cache and the rules for running it are in
[Drivers: configuration](../reference/drivers/configuration.md#external-processes-databricks-cli). Any external
process runs without a shell and with a timeout ([checklist](#security-review-checklist)).

## Automation: external programs and `jjp/1`

An automation job can start an external program, which is trusted code, and
`jjp/1` carries its inputs and outputs. The rules for starting a program, its
environment, the `jjp/1` listener and token, and its logs are in
[Automation](../reference/automation.md#automation-external-programs-and-jjp1).

## Automation: schedules and their consent

A schedule's consent is a durable, explicit and revocable grant; the rules are
in [Automation](../reference/automation.md#automation-schedules-and-their-consent).

## Untrusted content

Values from databases, error messages from servers and metadata names are
rendered as **text**. They are never interpreted as markup, file paths or
commands.

### Exported files

What an export writes, where and how it names the file are in
[Results: copying, export and import](../reference/results/transfer.md#exported-files).

## Updates

The daily update check and the signed self-install (D-0033) are specified in
[Updates](../reference/updates.md#updates): what is sent, the signature
requirement, the redirect allowlist and what is written.


## Dependencies

`cargo deny --locked check` (in `cargo xtask check --full`, in CI and weekly)
blocks advisories, unapproved licenses and unknown sources. A vulnerability is
fixed, not ignored; only `unmaintained` notices may be accepted, with a reason
and a review date in `deny.toml`.

## Security review checklist

- [ ] No new field holding a secret reached SQLite, JSON, an error message,
      output or `Debug`.
- [ ] No generated SQL builds values by concatenation; identifiers are quoted
      by the driver.
- [ ] External processes run without a shell and with a timeout.
- [ ] An automation change keeps values (inputs, cells, scalars, outputs,
      messages, program output) out of `automation.db`, the application log,
      spans and errors, except declared outputs and program logs within
      their limits; the CA-13 tests still pass.
- [ ] An external program is never started through `cmd.exe` or `sh`, its
      environment is Jovru's minus the denylist, and the `jjp/1` token
      reaches it only through stdin and is consumed by `hello`.
- [ ] The `jjp/1` listener exists only while a `jjp/1` run is registered and
      binds the loopback address only.
- [ ] Anything that can end a program ends its whole tree, and nothing
      replays a run.
- [ ] New network calls use HTTPS/TLS as configured, with an explicit timeout,
      and follow no redirect (the update download's allowlist is the one
      exception, [Updates](../reference/updates.md#updates)).
- [ ] A new external call has a `client` span; a new span attribute that
      may leave is in the telemetry allowlist and holds no free text.
- [ ] Database content is rendered as text.
- [ ] A new way to open a session (another database, another context) keeps
      the profile's user, credentials and TLS, and the production check
      covers every statement it runs.

## Decisions

The reasoning behind this area's structural choices is in [Decisions: Security](../decisions/security.md).

## Known deviations

The deviations of external programs (the inherited environment and the missing
sandbox) are in [Automation](../reference/automation.md#known-deviations).
