# Persistence: config.db

Part of [Persistence](README.md).

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
`jovru::connection_appearance`). A missing value, or one naming a colour or
environment this version does not know, reads as the driver's colour and no
environment.

`keymap.shortcuts` keeps the keys the user chose in **Preferences ›
Shortcuts** (`jovru::keymap::UserKeys`, 0.3.0 A12), by action name as the
keymap knows it: `{"jovru::GoToLine": ["ctrl-l"], "input::Search": []}`.
Each key is one keystroke as GPUI writes it; an action that is not there
keeps its default keys, and an empty list leaves it without one. It is read
once at start and written at each change, the last write asked for winning
(`connection_store::SHORTCUT_WRITES`). A value that is not an object, an
action this version does not offer and keys that cannot be read are left
out, said once in the window's notice, and the value is written again
without them; nothing here stops the application from starting. The demo
(`JOVRU_DEMO=1`) reads and writes none.

`export.preferences` keeps the export bar's last choices
(`jovru::export::ExportPreferences`): `{"format": "csv", "separator": ";",
"quoting": "all", "header": true, "bom": false, "directory": "…"}`, the
directory being the folder of the last exported file. It is read each time a
tab opens its export bar and written when an export starts, the last write
asked for winning across tabs. A missing or unknown value takes its default
(CSV with `;`, every value quoted, headers, no BOM, the Downloads folder).

`assistant.preferences` keeps the AI assistant's choices
(`jovru::assistant::AssistantPreferences`, D-0028):
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
never here: it is in the OS vault ([Security](../../standards/security.md#where-they-live)).
Conversations are not saved.

`assistant.mcp` keeps the MCP server's switch, port and token records
(D-0042). The secret
is not in this setting: it is in the OS vault. The setting is read once at
start and written when the server switch, the port or a token record
changes. A missing setting, or a value that is not this object, reads as
the server off, port 47321 and no tokens. The object in `tokens` is the
shape of one record. A missing or unknown field on a token takes the
default below. `connection_id` is the connection's id as stored today (the
text of `ConnectionId`). `connection_revision` is the integer
`connections.revision`, which increments on every successful save of that
profile. `environment_label` is that connection's environment key
(`development`, `staging` or `production`), or `""` when it has none; the
key lives in `connection.<id>.appearance`. A differing revision or
environment key suspends the token until the user confirms it.
`created_unix_ms` and `expires_unix_ms` are Unix milliseconds; null means
the token does not expire. `review` is `every-call`, `first-session` or
`never` (`first-session` is the first cache answer for that token since
this process started). `suspended` null means not suspended; this version
writes `connection-changed`, and any other string still suspends the token
and is kept. An `id` that is not 10 lowercase base32 characters is left
out. Last used time, activity, cursors and pending cards are not
written.

The MCP section's server switch saves `enabled` before replacing the
listener. A failed write is shown in the section and leaves the current
listener intact. **Off** closes the socket and pending requests; quitting
closes them without changing `enabled`, so the next launch restores the
saved choice. A replacement listener waits for the old socket to close
before binding the same port.

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

`telemetry.preferences` keeps the telemetry choices
(`jovru::telemetry::TelemetryPreferences`, D-0029):
`{"logs": false, "traces": false, "protocol": "http", "endpoint": ""}`.
`protocol` is `http` (OTLP over HTTP/protobuf) or `grpc`. It is read once at
start and written from Preferences, the last write asked for winning. A
missing or unknown value takes its default (off). The collector's headers
are never here: they are in the OS vault
([Security](../../standards/security.md#where-they-live)).

`copy.preferences` keeps the choices of the copy menu
(`jovru::copy_as::CopyOptions`): `{"format": "text", "header": false,
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
(`jovru::workspace_state`): the open query tabs in order (title, connection
name, the identity of its saved query, the file it edits as `{"path",
"base": {"size", "modified", "digest"}, "line_ending", "bom"}` (what it was
loaded or last saved from, D-0031),
the name of its recovery file, the SQL only when neither the file nor a
recovery file holds it (a tab of an earlier version, or a recovery file that
could not be written), the table or view the tab was opened for, its context, the
statement timeout and its `:name` parameter fields: name, type chosen
(`auto` or a type), the text typed and whether it binds NULL,
D-0026), the tab on screen, the saved queries, each with a stable
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
tabs closed during a session (`jovru::closed_tabs`, for **Reopen closed
query tab**) are kept in memory only and are never written. Its saved
queries are only those of an earlier version not moved to files yet; the
recovery files a snapshot names are written before it, and the ones no
tab needs any more are removed after it.

Saved queries are plain UTF-8 `.sql` files holding only SQL
(D-0031, `jovru::query_files`):
a query's title is its file name. The library's folder defaults to
`<Documents>/Jovru/Queries` (`<home>/Documents/…` without a Documents
folder, the data folder's `Queries` without a home), or
`<JOVRU_CONFIG_DIR>/Queries` when that variable is set, and is created when
something is first saved into it. A file is opened only when it is UTF-8
(a byte order mark allowed) and at most 4 MiB (above 256 KiB the editor
does not soft-wrap it, see [User experience](../ux/README.md)); its line endings and byte
order mark are written back as they were. Writes go to a temporary file in
the same folder, flushed, synced and renamed over the file. Opening,
discovering, migrating or recovering files never writes to the user's
folder and never runs SQL.

The application writes nothing into a saved query's file but its SQL: no
header, title, tags or connection, so any UTF-8 `.sql` file is a saved query
as it is. Only an explicit save, *Save as*, a rename or the migration writes
into the user's folder. Text must be UTF-8; a UTF-16 or UTF-32 byte order mark
or invalid UTF-8 is reported as an unsupported encoding, and nothing is
replaced or written. A file (or recovery file) over 4 MiB is an error naming
its size and the limit: nothing is opened or replaced, and a recovery file
that large is kept and its path shown. Content is never truncated to fit.
The editor holds the text with `\n` only; the file's style (CRLF when its
first line break is CRLF, otherwise LF) is written back on save, so a file
with mixed endings is saved in the style of its first break, only on an
explicit save.

A query's title is its file name without the extension. **Renaming** renames
the file in its folder: the title is made safe as `reports::export::file_name`
does (characters Windows forbids become `_`, reserved device names get a `_`
in front); an existing file is never replaced (the name is checked right
before the rename, which is refused when taken; a change of case alone is
allowed); open tabs, the local entry, its tags and its association follow the
file. A rename that fails changes nothing and is reported; a missing file
cannot be renamed. Editing tags never touches the file.

Each file tab keeps what it was loaded or last saved from (`base`: size,
modification time and a 64-bit FNV-1a digest of the bytes, a change detector
and not a security measure). Before writing, the file is checked against it: a
file changed on disk, moved or missing is never overwritten, recreated or
replaced by another file without the user choosing it
([User experience](../ux/README.md)). A save that fails or is cancelled removes the
temporary file and leaves the destination as it was; the tab keeps its text,
unsaved. A tab without a file keeps its "Query N" title until it is saved.

**Discovery** (`jovru::query_files`, `scan.rs`) lists every `*.sql` file
(extension in any case) in the library's folder and its subfolders, skipping
hidden folders (names starting with `.`, such as `.git`) and not following
directory links. It stops at depth 8 and at 10,000 files and says so. A scan
keeps, per file, its path, title and up to 16 KiB of its SQL for the preview
and the search, and joins the tags from `query_library`; the index never holds
more than 32 MiB of SQL, and a file past those bounds is still listed and
found by title, tags and path. The index lives in memory only (no search index
is stored in SQLite), is published whole when the scan ends, and the previous
one stays on screen while a scan runs. A scan runs on the runtime
(`backend::run`, `spawn_blocking`) with a `CancellationToken` (a newer scan
cancels the older), shows its progress when it lasts over 2 s and never
writes. Scans run at start, when the library is shown, when the window
becomes active again, after a save, *Save as*, rename or migration, and from
**Refresh**; there is no file watcher and no polling timer.

`query_library` keeps what is local to this machine about those files
(`jovru::query_files::library`): `{"version": 1, "directory": null or the
folder chosen, "next_id": n, "entries": [{"id", "path", "tags",
"connection": {"id", "name"}}], "migration": null or [{"id", "path"}]}`.
Each file has a stable identity, its tags (labels without `#`, each once
ignoring case) and the connection its tab opens on (by identity, then by
name); never its SQL or title. `migration` is the plan of a migration that
has not finished, written before its files. Keys this version does not know
are kept. It lives apart from `workspace.state`, so an older version cannot
erase it; a value that cannot be read is reported and never written over.
It is written 300 ms after a change settles. A missing value is the default
folder with no tags or associations. `directory` is `null` for the default;
`path` is absolute (compared ignoring case on Windows) and not relative to the
folder, so two libraries with the same relative names never share tags or an
association, and a query does not open on another library's (possibly
production) connection. Tags follow the rules of `workspace_state::parse_tags`
and are **local**: copying or versioning the folder carries the SQL and the
titles, not the tags. The connection is resolved by its ID and, when that ID
is unknown, by its name; the name is updated when the connection is seen
again, so a rename keeps the association. A file renamed or moved outside the
application appears as a new file with no tags and its old entry as missing;
the entry keeps its tags and association until the user reassociates it
(**Locate…**) or forgets it, and the application never guesses which file it
became. Changing the folder in Preferences only points the library elsewhere:
no file is moved, copied, overwritten or deleted, and open tabs keep their
files. Files anywhere else open and save through the system dialogs and are
not listed.

A query tab holds no SQL in `workspace.state`, nor `saved_sql` or `saved_id`:
only `recovery` and, for a file tab, `file`. A recovery file is
`recovery/<uuid>.sql`, the tab's text as the editor has it (plain UTF-8, LF,
so it can be opened by hand). It exists for a tab without a file that is not
blank, and for a file tab whose text differs from its file; it is written with
the same temporary-file rename as a save, never replaces the user's last
saved script, and is removed after the text is saved, discarded or reloaded,
and after the tab is closed with the user's agreement. At start a tab is
filled from its recovery file when it has one, otherwise from its file; a file
tab whose file changed meanwhile (its `base` no longer matches) opens with the
recovered text, unsaved, and says so. A recovery file no tab names reopens as
a **Recovered** draft and is never deleted unseen; one that cannot be read is
reported with its path and kept. The recovery file is written first, then
`workspace.state`: a crash between them leaves at worst an unnamed recovery
file or an outdated `base` (a false alarm that loses nothing). A
`workspace.state` written by 0.1.0 still reads (a tab with an inline `sql`
opens with it; the next write moves it to a recovery file and drops `sql` only
after that file was written); an earlier version reading the new value opens
those tabs empty, and the files stay on disk. Recovery restores text only,
never results, sessions or transactions, and the demo writes none.

At start, saved queries `workspace.state` still holds move to files in the
library's folder: named by their title (made safe for a file name, numbered
`(2)`, `(3)` on a collision, never over an existing file), written and read
back, the originals backed up to `backups/saved-queries-<millis>.json`, and
only then recorded in `query_library` with their identity, tags and
connection and dropped from `workspace.state`. Tabs that edited them now
edit their files. A query that could not move stays listed and is tried
again at the next start. The migration runs on the runtime once connections
and state are read, shows its progress in the library and never executes SQL.
Its order is: plan the files (an empty title becomes "Query"), write the plan
(`id` to path) to `query_library.migration`, create each file with
`create_new` with the SQL exactly as stored and LF endings, read each back
and compare it with the stored query, write the backup, and only then commit
`query_library` entries (keeping the query's `id`, tags and connection by
name, plus the ID of the connection of that name when there is one) and write
`workspace.state` without those queries. A tab linked to a migrated query
becomes that file's tab, unchanged when its SQL equals the saved SQL,
otherwise unsaved with its text in a recovery file. On retry after a crash, a
planned file that exists and verifies as its query is adopted instead of
rewritten, one that exists but differs keeps its content and the query takes
the next free name, and a query already in `query_library` with a verified
file is only removed from `workspace.state`: nothing is duplicated or lost.
Verified queries are committed even when others fail; the rest stay in
`workspace.state`, listed and openable, marked as not migrated with the error,
and **Retry** (also each start) runs it again. A notice says how many queries
became files, where, and which got a title different from their old one. The
history stays in `workspace.state`. After the migration, SQL and titles are not
in `config.db` (apart from the backup and queries not yet migrated); tags are,
as local data.

The code is the library module `jovru::query_files` (`src/query_files/`). It
knows nothing of GPUI and names no session, driver or `backend::query`
(`cargo xtask check` enforces it), so opening, discovering, migrating or
recovering files cannot run SQL; its file work runs through `backend::run`
and `spawn_blocking` with a `CancellationToken`. `jovru-storage` and the
SQLite schema are untouched: both settings are written through
`connection_store::write_setting`.

`reports` keeps the report definitions (D-0024,
D-0025, `jovru::reports`):
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
(`jovru::update`, D-0033).
Both are read once at start; the preference is written when the user
changes it, the state after each check that got an answer.

`explorer.favorites` keeps the explorer's favorites (`jovru::favorites`):
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

`appearance.theme` keeps the colour theme (`jovru::theme::ThemeChoice`):
`"system"` (the default), `"light"`, `"dark"`, or the id of a theme file.
A missing value, a non-string, or an id that could not be a file name reads
as `"system"`. An id whose file is missing or invalid also draws the
built-in theme for the system appearance. `appearance.theme_overrides`
keeps only colour overrides (`{"tokens": {}, "editor": {}, "results": {}}`),
not font size, density or layout. A value that is not that object is no
override. Both are read at start with the other settings. Only the choice
is written when it changes; Preferences does not edit overrides yet.
Opening Preferences reads the theme files again. The
files themselves are not in this database ([Themes](../themes.md)).
