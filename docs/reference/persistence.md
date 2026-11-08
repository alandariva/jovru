# Persistence

Local data lives in SQLite files owned by `jykre-storage`
([ADR-0003](../adr/0003-sqlite-for-local-storage.md)). Passwords are never stored
here ([Security](../standards/security.md)). The application never writes query results
to disk on its own: only an export the user asks for writes them, to the
file the user chose ([Results](results.md#export)), and so does a report's
print HTML ([ADR-0024](../adr/0024-reports.md)). Report definitions live in the
`reports` setting ([below](#schema-configdb)).

## Files

All files live in the data folder: `%LOCALAPPDATA%\JykreStudio` on Windows,
`~/Library/Application Support/JykreStudio` on macOS and
`$XDG_DATA_HOME/jykre-studio` on Linux. `JYKRE_CONFIG_DIR` overrides it (use an
isolated folder for tests and manual checks).

| File | Content | Lifetime |
| --- | --- | --- |
| `config.db` | Connection profiles (non-sensitive configuration) and preferences | Permanent; losing it loses the user's connections |
| `metadata/<connection-id>.db` | Metadata cache of one connection, refresh runs and per-scope status | Rebuildable from the server |
| `metadata/<connection-id>.lock` | Writer lock: one refresh per cache at a time, across processes | Recreated as needed |
| `metadata/<connection-id>.db.corrupt-<stamp>` | A cache set aside after confirmed corruption | Kept for diagnosis; safe to delete |
| `backups/config-v<version>-<millis>.db` | Copies of `config.db`: one each start whose configuration opened and was read, unless it equals the newest copy; the five most recent are kept | Rotated automatically; restored from when `config.db` cannot be opened |
| `config.db.broken-<millis>` | A configuration set aside when a backup was restored in its place | Kept for diagnosis; safe to delete |
| `logs/jykre.<date>.log` | JSON log lines, one file per day; the last 7 are kept ([Reliability](../standards/reliability.md#logs)) | Rotated automatically; safe to delete |
| `recovery/<name>.sql` | The text of a tab a crash must not lose: a draft, or a file tab's unsaved edits (plain UTF-8, `\n` line endings) | Written half a second after edits settle and on quit; removed once saved, discarded or closed; one no tab names reopens as a **Recovered** tab |
| `backups/saved-queries-<millis>.json` | The saved queries `config.db` held, as they were before they moved to files | Kept until the user deletes it; outside the rotation of the `config.db` copies |
| `<executable's folder>/.jykre-update/` | Not in the data folder: an update being installed (`staging-<id>/`: the downloaded archive and what it unpacks to), what it replaced (`trash-<id>/`, `done-<id>/`) ([ADR-0033](../adr/0033-update-check-and-signed-self-install.md)) | Removed when the update ends; at the next start, `done-*` and anything older than an hour are removed (on Windows the replaced executable can only go then); safe to delete while no update runs |
| `automation.db` | Automation: job definitions, published versions, run and chain-run history, opt-in declared outputs ([ADR-0034](../adr/0034-automation-database-service-and-lock.md)) | Permanent; losing it loses the user's jobs. History is pruned ([below](#automation-automationdb)) |
| `automation.db.corrupt-<millis>` | An automation file set aside after confirmed corruption (with its `-wal`/`-shm` files) | Kept for diagnosis; safe to delete |
| `automation.lock` | Empty file under an operating-system lock: the instance that holds it runs automation | Recreated as needed; its existence means nothing |
| `automation/logs/<run-id>/` | `stdout.log`, `stderr.log` and, under `jjp/1`, `jjp.log` of one external-program run, at most 1 MiB each ([ADR-0037](../adr/0037-program-logs-on-disk.md)) | Deleted 7 days after the run ended, with **Delete logs**, or when the run's row is gone |
| `connections.json` (legacy) | Profiles from earlier builds | Imported once into `config.db` in a single transaction, deleted only after the import is verified |

## SQLite configuration

Every file is opened by `open_pool`:

- `journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout=5 s`, at most 4 pooled
  connections.
- `synchronous=FULL` for `config.db` (user data) and `NORMAL` for metadata
  caches (rebuildable).
- `PRAGMA quick_check` on open; a file that fails is not used.
- `automation.db` is the one file opened with `synchronous=FULL` and
  `secure_delete=ON` ([below](#automation-automationdb)).

## Schema: `config.db`

| Table | Columns |
| --- | --- |
| `connections` | `id` (primary key), `name` (unique), `config_json`, `remember_password`, `revision`, `updated_at` |
| `settings` | `key` (primary key), `value_json` |

Preferences live in `settings` as JSON values: `editor.font_size`, and
`connection.<connection-id>.metadata_scope` for what to introspect
(`{"all": bool, "include": [paths]}`; missing means nothing was chosen; a
value that cannot be read is reported and treated as nothing chosen; a cache
written before the choice existed becomes the choice of its last-level
namespaces, once), and
`connection.<connection-id>.row_limit` for each connection's row limit (a
missing or out-of-range value reads as the default, 1,000), and
`connection.<connection-id>.appearance` for its colour and environment
(`{"color": "red", "environment": "production"}`, either key optional;
`jykre::connection_appearance`). A missing value, or one naming a colour or
environment this version does not know, reads as the driver's colour and no
environment.

`keymap.shortcuts` keeps the keys the user chose in **Preferences ›
Shortcuts** (`jykre::keymap::UserKeys`, 0.3.0 A12), by action name as the
keymap knows it: `{"jykre::GoToLine": ["ctrl-l"], "input::Search": []}`.
Each key is one keystroke as GPUI writes it; an action that is not there
keeps its default keys, and an empty list leaves it without one. It is read
once at start and written at each change, the last write asked for winning
(`connection_store::SHORTCUT_WRITES`). A value that is not an object, an
action this version does not offer and keys that cannot be read are left
out, said once in the window's notice, and the value is written again
without them; nothing here stops the application from starting. The demo
(`JYKRE_DEMO=1`) reads and writes none.

`export.preferences` keeps the export bar's last choices
(`jykre::export::ExportPreferences`): `{"format": "csv", "separator": ";",
"quoting": "all", "header": true, "bom": false, "directory": "…"}`, the
directory being the folder of the last exported file. It is read each time a
tab opens its export bar and written when an export starts, the last write
asked for winning across tabs. A missing or unknown value takes its default
(CSV with `;`, every value quoted, headers, no BOM, the Downloads folder).

`assistant.preferences` keeps the AI assistant's choices
(`jykre::assistant::AssistantPreferences`, [ADR-0028](../adr/0028-agentic-ai-assistant-over-the-metadata-cache.md)):
`{"provider": "anthropic", "model": "…", "endpoint": null, "review":
"every-request", "queries": true, "prices": {"<model>": {"input_per_mtok":
3.0, "output_per_mtok": 15.0, "cache_read_per_mtok": 0.3,
"cache_write_per_mtok": null}}}`. `provider` is `anthropic`, `openai`,
`openai-compatible`, `ollama` or `null` (off); `review` is `every-request`,
`first-only` or `never`; `queries` offers the model `run_query`, each query
and result still approved by the user; prices are per million tokens, as the
user entered them. It is read once at start and written from Preferences,
the last write asked for winning. A missing or unknown value takes its
default (off, review every request, queries offered). Columns hidden in a
conversation are not saved. The API key is
never here: it is in the OS vault ([Security](../standards/security.md#where-they-live)).
Conversations are not saved.

`telemetry.preferences` keeps the telemetry choices
(`jykre::telemetry::TelemetryPreferences`, [ADR-0029](../adr/0029-otlp-export-of-logs-and-traces.md)):
`{"logs": false, "traces": false, "protocol": "http", "endpoint": ""}`.
`protocol` is `http` (OTLP over HTTP/protobuf) or `grpc`. It is read once at
start and written from Preferences, the last write asked for winning. A
missing or unknown value takes its default (off). The collector's headers
are never here: they are in the OS vault
([Security](../standards/security.md#where-they-live)).

`copy.preferences` keeps the choices of the copy menu
(`jykre::copy_as::CopyOptions`): `{"format": "text", "header": false,
"separator": ";", "quoting": "all", "null": "empty"}`, the format being the
one `Ctrl C` copies in (`text`, `csv`, `json`, `jsonl`, `markdown`, `html`,
`insert`, `insert_values`, `in_list`, `value_list`, `column_names`) and
`null` either `empty` or `null`. It is read once at start, with the
connections, and written each time a choice changes, the last write asked
for winning across tabs. A missing or unknown value takes its default
(tab-separated text without the column names, `;`, every CSV value quoted,
NULL as an empty field); a value that cannot be read is reported and the
defaults are used.

`workspace.state` keeps the workspace between sessions
(`jykre::workspace_state`): the open query tabs in order (title, connection
name, the identity of its saved query, the file it edits as `{"path",
"base": {"size", "modified", "digest"}, "line_ending", "bom"}` (what it was
loaded or last saved from, [ADR-0031](../adr/0031-saved-queries-as-sql-files.md)),
the name of its recovery file, the SQL only when neither the file nor a
recovery file holds it (a tab of an earlier version, or a recovery file that
could not be written), the table or view the tab was opened for, its context, the
statement timeout and its `:name` parameter fields: name, type chosen
(`auto` or a type), the text typed and whether it binds NULL,
[ADR-0026](../adr/0026-named-parameters-in-sql-tabs.md)), the tab on screen, the saved queries, each with a stable
numeric `id` (its title is only a label; `next_id` is the next one) and its
`tags` (labels without `#`, each once ignoring case), and the
last 200 history entries, each with when it ended, how (`succeeded`,
`failed`, `cancelled`), its outcome and how long it took. Values written by
an earlier version still read: saved queries without an `id` get one, those
without `tags` have none, a tab
is linked to the saved query with its title, connection and SQL, and old
history entries have no time or status. It is
written half a second after changes settle and when the application quits
(closing the window included, see `src/app/exit_writes.rs`), and read once
the connections are loaded. A tab whose connection no longer
exists reopens in a group of its own, without a connection. A value that
cannot be read starts an empty workspace. Results are never stored. Query
tabs closed during a session (`jykre::closed_tabs`, for **Reopen closed
query tab**) are kept in memory only and are never written. Its saved
queries are only those of an earlier version not moved to files yet; the
recovery files a snapshot names are written before it, and the ones no
tab needs any more are removed after it.

Saved queries are plain UTF-8 `.sql` files holding only SQL
([ADR-0031](../adr/0031-saved-queries-as-sql-files.md), `jykre::query_files`):
a query's title is its file name. The library's folder defaults to
`<Documents>/Jykre Studio/Queries` (`<home>/Documents/…` without a Documents
folder, the data folder's `Queries` without a home), or
`<JYKRE_CONFIG_DIR>/Queries` when that variable is set, and is created when
something is first saved into it. A file is opened only when it is UTF-8
(a byte order mark allowed) and at most 4 MiB (above 256 KiB the editor
does not soft-wrap it, see [User experience](ux.md)); its line endings and byte
order mark are written back as they were. Writes go to a temporary file in
the same folder, flushed, synced and renamed over the file. Opening,
discovering, migrating or recovering files never writes to the user's
folder and never runs SQL.

`query_library` keeps what is local to this machine about those files
(`jykre::query_files::library`): `{"version": 1, "directory": null or the
folder chosen, "next_id": n, "entries": [{"id", "path", "tags",
"connection": {"id", "name"}}], "migration": null or [{"id", "path"}]}`.
Each file has a stable identity, its tags (labels without `#`, each once
ignoring case) and the connection its tab opens on (by identity, then by
name); never its SQL or title. `migration` is the plan of a migration that
has not finished, written before its files. Keys this version does not know
are kept. It lives apart from `workspace.state`, so an older version cannot
erase it; a value that cannot be read is reported and never written over.
It is written 300 ms after a change settles.

At start, saved queries `workspace.state` still holds move to files in the
library's folder: named by their title (made safe for a file name, numbered
`(2)`, `(3)` on a collision, never over an existing file), written and read
back, the originals backed up to `backups/saved-queries-<millis>.json`, and
only then recorded in `query_library` with their identity, tags and
connection and dropped from `workspace.state`. Tabs that edited them now
edit their files. A query that could not move stays listed and is tried
again at the next start.

`reports` keeps the report definitions ([ADR-0024](../adr/0024-reports.md),
[ADR-0025](../adr/0025-report-panels.md), `jykre::reports`):
`{"version": 3, "next_id": n, "reports": [...]}`, each report with a stable
numeric `id`, its title, description, parameters (name, label, kind,
default, required, choices), panels (`id`, title, connection as
`{"id": …, "name": …}`, SQL, context as `{"catalog": …, "schema": …}`,
optional chart, with the green and red conditions of a number panel's
numbers as `colors`, `table`, `width`) and when it was created and last changed.
A version 1 report (one top-level SQL and chart) is read as one panel, and
the report-level connection of versions 1 and 2 is given to each of its
panels; both are written back as version 3. Keys this version does not know
are kept in the library, each
report, parameter and panel. It stores definitions only: never results, and
never the values typed for a run, only each parameter's default and the
choices of a list. It lives apart from `workspace.state`, whose writer keeps
only the fields it knows, so an older version cannot erase reports. A missing
value is an empty library; a value that cannot be read is reported and never
written over. It is written half a second after changes settle and when the
application quits, closing the window included; the demo writes nothing.

`update.preferences` holds whether the application checks for updates by
itself (`{"check": true}`; missing or unreadable means it checks), and
`update.state` when the last check that reached GitHub ended
(`{"last_checked_ms": 1762560000000}`, milliseconds since the Unix epoch;
missing means never), so the daily check survives restarts
(`jykre::update`, [ADR-0033](../adr/0033-update-check-and-signed-self-install.md)).
Both are read once at start; the preference is written when the user
changes it, the state after each check that got an answer.

`explorer.favorites` keeps the explorer's favorites (`jykre::favorites`):
`{"version": 1, "favorites": [{"connection": "<connection-id>", "path":
[segments] or null, "stored_at": [segments] or null}]}`, a `null` path being
the connection itself. Connections are named by identity, so a rename keeps
them, and objects by their structural path, never by a label. Keys this
version does not know are kept, in the value and in each entry, and entries
it cannot read are written back as they were. It is read once at start, with
the connections, and written 300 ms after a change settles, the last list
winning; a value that cannot be read is reported and never written over.
The demo writes nothing.

`connection.<connection-id>.explorer` keeps what the explorer shows expanded
for that connection (`{"expanded": [paths]}`), written half a second after a
burst of expanding and collapsing. Keyed by the connection's ID, it survives
a rename. At start the expanded nodes are read again from the cache in one
snapshot; a value that cannot be read starts collapsed.

`config_json` holds the driver's non-sensitive options; drivers reject
password and token fields there. `revision` provides optimistic concurrency:
a save names the revision it edited, so a change made in another window is
rejected instead of silently overwritten. Saving a connection writes its
profile, row limit, metadata choice, revalidation hours and appearance in one
transaction (`ConfigRepository::save_with_settings`): a failure writes none
of them and the profile keeps its revision, so a retry is not refused.

`editor.font_size` is written once a burst of size changes settles, and a
write superseded by a newer size is skipped, so the size saved is the last
one shown.

## Schema: `metadata/<connection-id>.db`

| Table | Purpose |
| --- | --- |
| `metadata_info` | Single row: identity of the connection configuration the cache belongs to, and the cache version |
| `objects` | Published objects. `path_key` is the only copy of the structural path: each segment's JSON with `%` and `/` escaped, joined by `/`, so dots, slashes, wildcards and case cannot collide and a node's descendants are one key range ([ADR-0019](../adr/0019-compact-metadata-cache-keys.md)). `object_json` holds the object without its path, which reads rebuild from the key. `parent_key` plus the `objects_parent` index serve paged child reads |
| `refresh_runs` | Refresh attempts; a partial unique index allows only one `running` run |
| `staging_objects` | Objects of a run in progress |
| `protected_scopes` | Subtrees the run could not read (permission warnings) and must keep |
| `scope_status` | Per-scope state, last attempt, last success, version and message |

The cache records the identity of the configuration it was read from.
`MetadataRepository::prepare` reconciles it after the user saves a changed
profile; background readers use `open`, which never replaces another
configuration's identity.

Each connection's cache stays open while the application runs
(`backend` keeps one `MetadataRepository` per connection), so expanding,
searching and refreshing do not reopen the file. It is opened, and checked
with `quick_check`, at start, after the profile's identity changed and after
a failed read.

A cache is keyed by the driver's identity of the catalog
(`Driver::cache_identity`, [ADR-0017](../adr/0017-driver-metadata-contract-batch.md)),
not by the whole profile: changing compute, defaults or TLS keeps it. A
cache an earlier version keyed by the whole profile is adopted under the
narrower identity without being emptied (`MetadataRepository::adopt`).

The search compares `search_name` with the query in Unicode lower case
(`instr`), filters by kind in SQL before the limit and reads the ancestors
of the matches in one query.

Children are read a page at a time with a cursor (`PageCursor`), in a fixed
order of kinds (namespaces, tables, views, materialized views, columns,
foreign keys, partitions) and then by ordinal, name and key. Every query
walks the `objects_parent` index from the cursor, so a page costs the same
at any depth. `pages` reads several pages and the cache version from one
snapshot.

## Atomic metadata refresh

Two OS lock files guard each cache. The **run lock** (`<id>.run.lock`) is
held for a whole refresh, so owning it proves that no live refresh owns a
`running` run. The **writer lock** (`<id>.lock`) is held only while
publishing, retaining or resetting the cache, so a refresh that is still
reading the server does not block those; each waits up to five seconds for
the other holder.

1. `begin(scope)` takes the run lock (waiting up to five seconds for a
   refresh that is finishing) and inserts a `running` run.
2. The driver streams batches; `batch` writes them to `staging_objects`.
3. On success, `activate` takes the writer lock and replaces the objects of
   that scope with the staged ones **in one transaction**, keeps protected
   subtrees, bumps the cache version and records the scope status. A read
   that started before the configuration was reset, or before another
   publication, is refused as stale.
4. On failure or cancellation, `fail` records the error and discards the
   staged objects. The published objects are untouched.
5. When a cache is opened, `recover_interrupted` takes the run lock, marks
   runs left `running` by a crash as interrupted and removes them; the
   previous read stays.

The scope status is read as `jykre_storage::RefreshState`; the stored text
(`running`, `success`, `partial`, `warning`, `failed`, `cancelled`,
`interrupted`) is unchanged, and a value this version does not know reads as
`Unknown`.

A scope's ancestors (its database or catalog) are outside the scope, so the
driver does not emit them; `activate` adds the missing ones when the scope
published anything. Reading a choice publishes each last-level namespace on
its own. After a complete read, `retain(subtrees, nodes)` keeps only the
objects inside the chosen subtrees, the named namespaces and their ancestors,
removes everything else with its scope status in one transaction and bumps
the version. It waits up to five seconds for a publication that is finishing
to release the writer lock.

Readers that page through the cache compare the version before and after; if
a refresh published in between, they stop and ask the user to expand the node
again instead of mixing two reads.

## Automation (`automation.db`)

`jykre-storage` owns the file through `AutomationRepository`
(`crates/storage/src/automation.rs`); no other crate opens it. It is separate
from `config.db` because history grows and is pruned while `config.db` holds
the only copy of the connection profiles
([ADR-0034](../adr/0034-automation-database-service-and-lock.md)).

| Table | Content |
| --- | --- |
| `jobs` | A job's canonical-JSON definition (kind `sql`, `process` or `chain`) and its `revision`, which grows with every save |
| `job_versions` | A published version of a job: its frozen definition and fingerprint ([ADR-0036](../adr/0036-published-versions-and-chain-runs.md)). Deleting a job is refused while a chain names any of its versions |
| `runs` | One row per admitted run: the snapshot (no values), state, cause, `effect_unknown`, counters (rows delivered and discarded, rows affected, `truncated`), the SQLSTATE or vendor code, exit code or signal, a program's `fail` code, log sizes, the program's process group and start time, and the SHA-256 of the program's entrypoint ([ADR-0038](../adr/0038-external-program-runs.md)) |
| `chain_runs` | One row per chain run, with the step that stopped it |
| `run_outputs` | The declared outputs of a run whose job opted in: name, kind, size and, until it expires, the value ([ADR-0035](../adr/0035-persisted-declared-outputs.md)) |

- **Configuration.** WAL, `foreign_keys=ON`, `synchronous=FULL` (it holds job
  definitions) and `secure_delete=ON`, so SQLite overwrites deleted content
  and an expired output value does not linger in the file's free pages
  (`open_pool_with`). The file opens only through the repository, after
  `PRAGMA quick_check`.
- **Migrations.** Forward-only, by `PRAGMA user_version`, each step in one
  transaction on a connection with foreign keys off, checked with
  `foreign_key_check` before the commit: 1 (jobs and runs,
  [ADR-0034](../adr/0034-automation-database-service-and-lock.md)),
  2 (`run_outputs` and `runs.outputs_stored`,
  [ADR-0035](../adr/0035-persisted-declared-outputs.md)), 3 (kind `process`
  and the program's columns, [ADR-0038](../adr/0038-external-program-runs.md);
  `jobs` and `runs` are rebuilt) and 4 (kind `chain`, `job_versions`,
  `chain_runs` and the chain columns of `runs`,
  [ADR-0036](../adr/0036-published-versions-and-chain-runs.md)). A file whose
  version is above 4 is refused untouched: automation is off with a message
  and the rest of the application works. There is no backup copy before a
  migration ([ADR-0034](../adr/0034-automation-database-service-and-lock.md)).
- **Corruption.** A file that cannot be opened and is confirmed corrupt (not
  a database, or `quick_check` fails) is set aside with its `-wal` and `-shm`
  files as `automation.db.corrupt-<millis>` and an empty one is created; the
  job definitions in it are lost and the application says so
  (`Recovery::recreated`). Permission and I/O errors never trigger this.
- **The lock.** `automation.lock` holds an exclusive operating-system lock
  (`std::fs::File::try_lock`: `flock` on Unix, `LockFileEx` on Windows) for as
  long as the engine lives. The system releases it when the process ends,
  however it ends, so a lock is **never stranded**: the empty file left on
  disk carries no meaning, and there is no PID file, timestamp or lease. A
  second instance on the same folder cannot run or save anything: its
  automation is read-only until the first one ends and **Try again** takes the
  lock over. It opens `automation.db` read-only
  (`AutomationRepository::open_read_only`) and never creates, migrates or
  sets the file aside; a file it finds missing or not yet migrated shows no
  history until the lock holder has prepared it. A process that is alive but hung keeps the lock, which is
  correct. **Network file systems may not honour the lock**, so keep the
  data folder on a local disk; when it is not, two instances could both
  believe they hold it.
- **Recovery.** The lock holder, before admitting anything, marks every run
  and chain run still `running` as `interrupted` and never replays one.
  `effect_unknown` is set on every interrupted program and on every SQL run
  that was not `reads_only`; an interrupted chain has it when any of its
  steps has. On Linux, a program run whose recorded process group still has
  a leader with the recorded start time is listed as an **orphan** for the
  user to end; a PID with another start time is never touched. Other systems
  record no start time, so they list none; on Windows the Job Object ends the
  tree with Studio.
- **Retention.** Pruned in the background about once an hour, in transactions
  of at most 500 rows that never block admission: output values are emptied
  7 days after their run ended (name, kind and size stay, and
  `secure_delete` overwrites the value); runs and chain runs (with their
  steps and outputs) are deleted 90 days after they ended; a program's log
  folder is deleted 7 days after its run ended, or when its run's row is gone
  (a log folder is never deleted while its run is active). **Delete logs**
  removes one run's folder at once and forgets its sizes.

**What is never stored.** Input values, result cells, scalar results and
server or program messages are written to no file of automation: not to
`automation.db`, not to the application's log, not to telemetry (CA-13); the
history says what ran, where, when and how it ended. The snapshot holds the
job's definition (SQL text, parameter *specifications*, options) and the
connection's ID and revision, never a password or a value. There are two
exceptions, each bounded and visible to the user:

1. **Declared outputs** ([ADR-0035](../adr/0035-persisted-declared-outputs.md)):
   only for a job that opts in, at most 4 KiB of values per run and stored
   all or none, readable for 7 days.
2. **Program logs** ([ADR-0037](../adr/0037-program-logs-on-disk.md)): what a
   program prints, and its `jjp/1` `log` and `fail` messages, are written
   as received under `automation/logs/<run-id>/` because they are the only
   way to diagnose it. At most 1 MiB per file, kept 7 days, never read except
   when the user opens a run's log. The user is warned that a program should
   print no secrets.

## Migrations

Each file's schema version is `PRAGMA user_version`. `config.db` is at
version 1; `automation.db` is at version 4 ([above](#automation-automationdb)). The metadata files are at version 3
([ADR-0018](../adr/0018-metadata-cache-migrations.md)): a new file is created at
version 1 and then migrated, and an older one gets only the steps it lacks.
A file with a higher version is refused ("created by a newer version of the
application").

Metadata steps (`jykre-storage`, `metadata.rs`):

| Step | Change |
| --- | --- |
| 2 | `objects.search_name` and `staging_objects.search_name`: the name in Unicode lower case, written with every object and filled for existing rows by the step; the index `objects_search` and the FTS5 trigram table `objects_fts`, kept in step with `objects` by triggers; `scope_status.last_published_at`: when a refresh last published the scope, complete or partial |
| 3 | Compact keys ([ADR-0019](../adr/0019-compact-metadata-cache-keys.md)): every `path_key`, `parent_key` and `scope_key` rewritten from hex to escaped JSON, `object_json` stored without the path, `path_json` dropped from `objects` and `staging_objects`; refreshes left running are dropped and their scopes marked interrupted. The step runs with foreign keys off, checks them before committing and vacuums the file afterwards. A cache of 500,000 objects goes from 892 MB to 461 MB (1.5 GB to 753 MB as a refresh leaves it) in about 20 s, once |

The standard for schema changes:

- A change is a **new, numbered, forward-only migration step** (version
  *n* → *n*+1), applied in order inside one transaction that also sets
  `user_version`. A released step is **never edited**; a fix is another step.
- Before migrating `config.db`, the application copies it to
  `backups/config-v<old>-<timestamp>.db` in the data folder and keeps the five
  most recent copies. Metadata caches are not backed up; they can be rebuilt.
- If a migration fails, the application does not start with a partially
  migrated file; it reports the error and the backup path.
- Every step has a test that migrates an empty file and a fixture of the
  previous version.

## Corruption and recovery

- **Metadata cache:** when opening fails and `quick_check` (or SQLite codes 11
  or 26) confirms corruption, the file and its `-wal`/`-shm` companions are
  renamed to `*.corrupt-<stamp>`, an empty cache is created and the UI tells
  the user to refresh. Permission and I/O errors never trigger this, so a
  valid cache is not set aside by mistake.
- **`automation.db`:** set aside as `automation.db.corrupt-<millis>` and
  recreated when corruption is confirmed, as described
  [above](#automation-automationdb); a file from a newer version is left
  untouched and automation stays off.
- **Deleting a connection** removes its profile and every
  `connection.<connection-id>.*` setting in one transaction
  (`ConfigRepository::delete`, refused when the profile changed since it
  was read), then its vault entry and its metadata files
  (`MetadataRepository::remove`). What could not be cleaned up after the
  profile is gone is reported; the connection is never half deleted.
- **`config.db`:** never recreated automatically. The application reports the
  error; with backups in place it offers to restore the latest one
  (**Restore the backup** in the explorer, after a confirmation). The file
  that could not be opened is renamed to `config.db.broken-<millis>` (with
  its `-wal`/`-shm` files), the copy takes its place once it opens, and the
  connections are loaded again.

## Rules

- SQLite is accessed **only** through the repositories in `jykre-storage`
  (`ConfigRepository`, `MetadataRepository`, `AutomationRepository`). Other crates do not write SQLite
  SQL.
- Repositories take and return `jykre-core` types, never raw rows.
- Reads for the UI are paged (children of a node, search with a limit).
- Nothing secret is stored, and no query result is written to disk, apart
  from a report's print HTML at the path the user picks
  ([ADR-0024](../adr/0024-reports.md)) and automation's two bounded
  exceptions ([declared outputs and program logs](#automation-automationdb)).
- A schema change follows [Migrations](#migrations) and needs an ADR when it
  changes what is stored.

## Known deviations

| Standard | Current code |
| --- | --- |
| Numbered forward-only migration steps | `config.db` has no step yet; the metadata files have their mechanism and step 2 |
| `config.db` backup before migrating, restore offered on failure | The restore is offered, from the copies taken at each start (`backups/config-v<version>-<millis>.db`). The only migration so far (creating version 1) still copies to `backups/config-before-v1-<id>.db`, a name the restore does not list |
