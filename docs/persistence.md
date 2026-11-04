# Persistence

Local data lives in SQLite files owned by `jykre-storage`
([ADR-0003](adr/0003-sqlite-for-local-storage.md)). Passwords are never stored
here ([Security](security.md)). The application never writes query results
to disk on its own: only an export the user asks for writes them, to the
file the user chose ([Results](results.md#export)).

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
| `logs/jykre.<date>.log` | JSON log lines, one file per day; the last 7 are kept ([Reliability](reliability.md#logs)) | Rotated automatically; safe to delete |
| `connections.json` (legacy) | Profiles from earlier builds | Imported once into `config.db` in a single transaction, deleted only after the import is verified |

## SQLite configuration

Every file is opened by `open_pool`:

- `journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout=5 s`, at most 4 pooled
  connections.
- `synchronous=FULL` for `config.db` (user data) and `NORMAL` for metadata
  caches (rebuildable).
- `PRAGMA quick_check` on open; a file that fails is not used.

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

`export.preferences` keeps the export bar's last choices
(`jykre::export::ExportPreferences`): `{"format": "csv", "separator": ";",
"quoting": "all", "header": true, "bom": false, "directory": "…"}`, the
directory being the folder of the last exported file. It is read each time a
tab opens its export bar and written when an export starts, the last write
asked for winning across tabs. A missing or unknown value takes its default
(CSV with `;`, every value quoted, headers, no BOM, the Downloads folder).

`workspace.state` keeps the workspace between sessions
(`jykre::workspace_state`): the open query tabs in order (title, connection
name, SQL, the SQL last saved to the library and the identity of that saved
query, the table or view the tab was opened for, its context and the
statement timeout), the tab on screen, the saved queries, each with a stable
numeric `id` (its title is only a label; `next_id` is the next one), and the
last 200 history entries, each with when it ended, how (`succeeded`,
`failed`, `cancelled`), its outcome and how long it took. Values written by
an earlier version still read: saved queries without an `id` get one, a tab
is linked to the saved query with its title, connection and SQL, and old
history entries have no time or status. It is
written half a second after changes settle and when the application quits,
and read once the connections are loaded. A tab whose connection no longer
exists reopens in a group of its own, without a connection. A value that
cannot be read starts an empty workspace. Results are never stored.

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
| `objects` | Published objects. `path_key` is the only copy of the structural path: each segment's JSON with `%` and `/` escaped, joined by `/`, so dots, slashes, wildcards and case cannot collide and a node's descendants are one key range ([ADR-0019](adr/0019-compact-metadata-cache-keys.md)). `object_json` holds the object without its path, which reads rebuild from the key. `parent_key` plus the `objects_parent` index serve paged child reads |
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
(`Driver::cache_identity`, [ADR-0017](adr/0017-driver-metadata-contract-batch.md)),
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

## Migrations

Each file's schema version is `PRAGMA user_version`. `config.db` is at
version 1. The metadata files are at version 3
([ADR-0018](adr/0018-metadata-cache-migrations.md)): a new file is created at
version 1 and then migrated, and an older one gets only the steps it lacks.
A file with a higher version is refused ("created by a newer version of the
application").

Metadata steps (`jykre-storage`, `metadata.rs`):

| Step | Change |
| --- | --- |
| 2 | `objects.search_name` and `staging_objects.search_name`: the name in Unicode lower case, written with every object and filled for existing rows by the step; the index `objects_search` and the FTS5 trigram table `objects_fts`, kept in step with `objects` by triggers; `scope_status.last_published_at`: when a refresh last published the scope, complete or partial |
| 3 | Compact keys ([ADR-0019](adr/0019-compact-metadata-cache-keys.md)): every `path_key`, `parent_key` and `scope_key` rewritten from hex to escaped JSON, `object_json` stored without the path, `path_json` dropped from `objects` and `staging_objects`; refreshes left running are dropped and their scopes marked interrupted. The step runs with foreign keys off, checks them before committing and vacuums the file afterwards. A cache of 500,000 objects goes from 892 MB to 461 MB (1.5 GB to 753 MB as a refresh leaves it) in about 20 s, once |

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
  (`ConfigRepository`, `MetadataRepository`). Other crates do not write SQLite
  SQL.
- Repositories take and return `jykre-core` types, never raw rows.
- Reads for the UI are paged (children of a node, search with a limit).
- Nothing secret is stored, and no query result is written to disk.
- A schema change follows [Migrations](#migrations) and needs an ADR when it
  changes what is stored.

## Known deviations

| Standard | Current code |
| --- | --- |
| Numbered forward-only migration steps | `config.db` has no step yet; the metadata files have their mechanism and step 2 |
| `config.db` backup before migrating, restore offered on failure | The restore is offered, from the copies taken at each start (`backups/config-v<version>-<millis>.db`). The only migration so far (creating version 1) still copies to `backups/config-before-v1-<id>.db`, a name the restore does not list |
