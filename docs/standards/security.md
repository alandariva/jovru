# Security

A SQL client holds the keys to its users' databases. Security mistakes here
have a high impact and are hard to undo once a secret has reached a file or a
log. How to report a vulnerability is in [SECURITY.md](../../SECURITY.md); this
document is the engineering standard.

## Credentials

### Where they live

| Credential | Storage |
| --- | --- |
| PostgreSQL/MySQL password, with **Remember password** | OS vault through `jykre-secrets`: service `JykreStudio`, account `connection:<connection-id>:password` ([ADR-0004](../adr/0004-os-vault-for-credentials.md)) |
| PostgreSQL/MySQL password, without it | Memory only, for the session |
| Assistant API key (Anthropic, OpenAI, OpenAI-compatible endpoint) | Only in the OS vault through `jykre-secrets` (`ApiKeyStore`): service `JykreStudio`, accounts `assistant:anthropic:api-key`, `assistant:openai:api-key` and `assistant:openai-compatible:api-key`. The accounts are fixed (`ApiKeyAccount`); the UI never names one. Never in `config.db`, settings or logs |
| OpenTelemetry collector headers (usually its authentication) | Only in the OS vault through `jykre-secrets` (`ApiKeyStore`): service `JykreStudio`, account `telemetry:otlp:headers` (`ApiKeyAccount::telemetry_headers`). Never in `config.db`, settings or logs, never shown back ([ADR-0029](../adr/0029-otlp-export-of-logs-and-traces.md)) |
| Databricks token | **Not stored by Jykre Studio.** Resolved by the Databricks CLI when needed and kept in memory ([ADR-0005](../adr/0005-databricks-through-cli-and-rest.md)) |

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

## Encrypted transport

| Database | Modes | Notes |
| --- | --- | --- |
| PostgreSQL | `disable`, `prefer`, `require`, `verify-ca`, `verify-full` | The mode is always explicit in the configuration; the form starts at `prefer` |
| MySQL | `disabled`, `preferred`, `required`, `verify-ca`, `verify-identity` | The form starts at `preferred`. Authentication methods that need RSA require TLS |
| Databricks | HTTPS only | The workspace origin must be `https`; redirects are disabled and external result links are not followed |

- The whole stack uses **rustls** with the operating system's root
  certificates, so corporate CAs work and builds behave the same everywhere.
- There is no "accept any certificate" option. Adding one needs an ADR.
- The UI shows whether the active connection is **encrypted**, and warns when
  `prefer`/`preferred` ended up without TLS.

## Authentication and authorization

- The application uses the database user's own permissions. It never tries to
  elevate privileges or work around a permission error.
- Introspection with partial permissions does not fail: unreadable subtrees
  are kept from the last read and marked with a warning
  ([Persistence](../reference/persistence.md#atomic-metadata-refresh)).
- Databricks requests run with the token of the CLI profile; Unity Catalog
  permissions are enforced by the server.
- A report panel in another PostgreSQL database of the server opens its
  session there with the profile's own user, credentials and TLS, and only
  when the profile offers the server's other databases (`allDatabases`);
  otherwise the panel fails with a validation error
  ([ADR-0025](../adr/0025-report-panels.md)). The server decides what that user
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
  ([ADR-0023](../adr/0023-named-parameters-bound-by-drivers.md)), and on a
  classic Databricks cluster, which cannot bind, each marker becomes a typed
  literal: text only as the hexadecimal of its bytes
  (`decode(X'…', 'UTF-8')`), the other types only after checking that they
  hold nothing but their own characters
  ([ADR-0026](../adr/0026-named-parameters-in-sql-tabs.md)). SQL warehouses bind
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

## AI assistant

The assistant ([ADR-0028](../adr/0028-agentic-ai-assistant-over-the-metadata-cache.md))
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
- The rule is enforced by types (`jykre::assistant::facts`, and
  `jykre::assistant::released` for query results), by a source check in
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

## Telemetry

Telemetry is off by default; the user turns logs and traces on separately in
**Preferences › Diagnostics › Telemetry**
([ADR-0029](../adr/0029-otlp-export-of-logs-and-traces.md)). What may leave:

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
  (`jykre::telemetry::attributes::ALLOWED`), per-layer filters on the
  bridges, and tests that run a statement with a synthetic password, SQL,
  values and host and check none is exported.
- The collector's address is HTTPS, or plain HTTP on this computer; it
  never carries a user, a password, a query or a fragment. Redirects are
  not followed. The trace context is not sent to databases, Databricks or
  AI providers.

## Logs and diagnostics

These rules apply to any output, including the logs ([Reliability](reliability.md#logs)):

- Never log passwords, tokens, `Authorization` headers, connection URLs,
  cell values or query parameters.
- SQL text only at `trace`, which is off by default; at higher levels, log its
  length.
- Automation never logs an input value, a result, a server or program
  message, a program's path, arguments or environment, or a `jjp/1` token;
  the program's own output goes to its run's log files, not to the
  application's log ([above](#automation-external-programs-and-jjp1)).
- No header logging in `reqwest`, and statement logging in SQLx turned off
  (`disable_statement_logging()`) in every driver and in `jykre-storage`,
  because SQLx logs SQL by default.

## External processes (Databricks CLI)

The authentication cache is scoped by CLI profile, canonical configuration
file, executable and HTTPS workspace origin. It never mixes workspace tokens.
Idle entries are pruned on cache access; token refreshes are serialized and
expired/rejected tokens are discarded. The CLI receives the same explicit
configuration file used to resolve the workspace. HTTP pools are shared within
that identity. Compute names/IDs are non-sensitive profile options; tokens
never reach connection options or the select's state.

- The CLI runs with **arguments as a vector** (`Command::new(path).args(...)`),
  never through a shell, so a profile name cannot inject a command.
- The executable comes from the configuration (`cliPath`) or the `PATH`.
- Its output contains the token: never log stdout. On failure, report the exit
  status and a filtered message, not the raw output.
- The process has a timeout (60 s) and `kill_on_drop(true)`.

## Automation: external programs and `jjp/1`

An automation job can start an external program
([ADR-0038](../adr/0038-external-program-runs.md)). **The program is trusted
code**: it runs with the user's privileges and is supervised, never
sandboxed. Studio ends its process tree on Stop and on a deadline; it cannot
stop the program from reading the user's files or using the network. The
interface says so on the first run of each version
([specification](../plans/automation-engine-specification.md), Q-10), and
there is no import or export of jobs that could bring code in unseen.

**Starting a program** (`automation::process`):

- The program is started **without a shell**: `Command::new(program)` with
  the arguments as a vector, so no argument is parsed by `sh` or `cmd.exe`.
  A `.bat` or `.cmd` file, which Windows would run through `cmd.exe`, is
  refused before the run, whether it is named or found on `PATH`.
- A program is an absolute path or a bare name looked up on `PATH` (and
  `PATHEXT` on Windows, so `tool` finds `tool.exe`). A relative path such as
  `bin/tool` is refused, as is a relative working directory: nothing depends
  on the directory Studio was started in.
- **The environment is inherited** from Studio, minus a denylist: `JYKRE_*`,
  `OTEL_*`, `NODE_OPTIONS`, `PYTHONPATH`, `PYTHONSTARTUP` and `PYTHONHOME`
  (matched without regard to case on Windows). Everything else, **secrets
  included**, reaches the program, and the interface lists what is removed. The job's
  own `unset` and `set` apply after the denylist.
- A published version of a program **pins its entrypoint file by SHA-256**.
  Nothing is copied: the program runs from its path, and a file that
  changed or disappeared blocks the run until the version is published again.
  The hash identifies what ran; it does not make the file safe.
- The program's stdin carries only the `jjp/1` token line (or end of file);
  its stdout and stderr are captured to log files and never inherit Studio's
  console. On Windows it starts without a console window.
- No program path, argument, environment value, log content, token, input or
  output reaches `tracing`, an error message or a `Debug` output. The
  telemetry allowlist gains no attribute for it: a program run's span carries
  only the exit code and the error type.

**`jjp/1`** is how a program receives inputs and returns outputs
([specification](../plans/automation-engine-specification.md), section 9.2):

- A loopback listener (`127.0.0.1`, a port the system picks) exists **only
  while a `jjp/1` run is registered**: the first run opens it and the last to
  end closes it. Nothing listens at startup, and nothing listens for other
  hosts.
- Each run gets a **256-bit random token**. It reaches the program **through
  its stdin only**, never through the environment, an argument, a log or
  telemetry. The program presents it in `hello`, which **consumes** it at
  once: a wrong token, a reused one, a second connection and any message
  before `hello` are refused and close the connection. No `hello` within 30 s
  ends the run.
- Messages are one JSON-RPC object per line, at most 1 MiB, enforced before
  the line is buffered; at most 16 connections are served at once; a program
  that leaves more than 4 MiB of responses unread, or sends more than ten
  times the log quota, fails its run. Only `hello`, `progress`, `log`,
  `complete` and `fail` are accepted.
- Same-user debugging (ptrace, `/proc/<pid>/mem`) can read a process's
  memory, including its token; the token protects against other local users
  and other programs guessing the port, not against the same user.
- The values of a run never go to `tracing`, spans, history or errors
  (CA-13). `log` messages and the `fail` message go to the run's `jjp.log`
  only.

**Program logs** are the exception to "no cell or message is stored":
stdout, stderr and `jjp.log` are written as received under
`automation/logs/<run-id>/`, at most 1 MiB each and kept 7 days
([Persistence](../reference/persistence.md#automation-automationdb),
[ADR-0037](../adr/0037-program-logs-on-disk.md)). A program must not print
secrets, and the interface warns the user of it. They are read back only when
the user opens a run's log. Declared outputs the user chose to persist are
the other exception ([ADR-0035](../adr/0035-persisted-declared-outputs.md)).

## Untrusted content

Values from databases, error messages from servers and metadata names are
rendered as **text**. They are never interpreted as markup, file paths or
commands.

### Exported files

- An export writes only to the file the user chose in the system's save
  dialog. A table or tab name only suggests the file's name: characters a
  file name cannot hold become `_`, names Windows reserves (`CON`, `NUL`,
  `COM1`) get a `_` in front, and no folder comes from it
  (`jykre::export::suggested_file_name`).
- Values are written as the database returned them. Nothing is changed to
  stop a spreadsheet from reading a value that starts with `=`, `+`, `-` or
  `@` as a formula, since changing values would break
  [golden rule 9](../README.md#golden-rules) (approved 2026-11-04). Quotes in a
  CSV do not stop a spreadsheet from evaluating such a value: a file exported
  from data others wrote is as untrusted as that data when opened in one. In
  XLSX every value is a string, a number or a boolean cell, never a formula.
- Messages about the file never name its folder, which may hold the user's
  name; logs record the format, rows, bytes and duration, never the path.
- Temporary files (the export being written and Excel's constant-memory
  rows) live next to the chosen file and are removed when the export ends,
  fails or is cancelled.

## Updates

The application checks GitHub's release list for a newer version and, after
the user's click, installs it
([ADR-0033](../adr/0033-update-check-and-signed-self-install.md)).

- **Sent:** once a day by default (off in **Preferences › About ›
  Updates**), or on **Check now**, one `GET` of
  `https://api.github.com/repos/alandariva/jykre-studio/releases` with
  `User-Agent: JykreStudio/<version>`. GitHub sees the computer's address,
  the version and the time. **Never sent:** a token, a cookie, a
  `Referer`, an identifier of the user or the computer, the operating
  system. Downloads happen only after **Download and install**.
- **Signature, required.** An archive is installed only when it verifies
  against a public key compiled in (`assets/update/release-key.pub`,
  `jykre::update::keys`), signed with zipsign (ed25519ph). The context of
  the signature is the archive's file name, which the application builds
  from the version, never from GitHub's answer, so an older signed archive
  cannot pass for a newer one. A checksum next to the archive is not
  trusted: whoever can replace a release can replace it.
- **Key custody.** The private key exists only offline with the maintainer
  and as the `ZIPSIGN_PRIVATE_KEY` secret of the release workflow's `sign`
  job, which verifies what it signed with the committed public key before a
  release is drafted. To rotate it, ship the new public key beside the old,
  sign a release with both, then drop the old one. A leaked key is rotated
  at once; a lost key means users update by hand once.
- **Redirects, the one exception.** The release list follows none. A
  download starts at the repository's API asset address
  (`…/releases/assets/<number>`, checked) and follows at most 3 redirects,
  only over HTTPS on port 443 to `objects.githubusercontent.com` or
  `release-assets.githubusercontent.com`, without a user or a password.
  Those addresses carry a signed token in their query, so no address is
  logged or put in an error message.
- **What is written.** The archive (at most 256 MiB) goes to
  `.jykre-update/` next to the executable; it is unpacked only after its
  signature verifies; the unpacked folder must hold one folder with the
  executable and `catalog/`, and no symbolic link. The swap replaces only
  the entries the archive holds, in the executable's folder, and undoes
  every step on failure. The data folder and the user's own files are
  never touched.
- **Only where it ships.** A development build, another platform, a copy
  not in the release's folder or a folder the user cannot write to is
  offered the release page instead; the application never asks for
  elevated rights.

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
      environment is Studio's minus the denylist, and the `jjp/1` token
      reaches it only through stdin and is consumed by `hello`.
- [ ] The `jjp/1` listener exists only while a `jjp/1` run is registered and
      binds the loopback address only.
- [ ] Anything that can end a program ends its whole tree, and nothing
      replays a run.
- [ ] New network calls use HTTPS/TLS as configured, with an explicit timeout,
      and follow no redirect (the update download's allowlist is the one
      exception, [Updates](#updates)).
- [ ] A new external call has a `client` span; a new span attribute that
      may leave is in the telemetry allowlist and holds no free text.
- [ ] Database content is rendered as text.
- [ ] A new way to open a session (another database, another context) keeps
      the profile's user, credentials and TLS, and the production check
      covers every statement it runs.

## Known deviations

| Standard | Current code |
| --- | --- |
| A program's environment holds no secret Studio did not mean to give | Automation inherits Studio's environment minus a short denylist ([Q-27](../plans/automation-engine-specification.md#12-decisions-required-from-the-maintainer)); variables that hold secrets reach the program. Accepted by the maintainer, shown in the interface |
| An external program is sandboxed | It is not: it runs with the user's privileges and may read files and reach the network (Q-10). Supervised, not contained |

The last closed deviation, showing whether the session is encrypted
and warning on plain text, was closed in 0.2.0 M3: the server's answer is
shown beside **Connected**
([ADR-0032](../adr/0032-session-encryption-reported-by-drivers.md)).
