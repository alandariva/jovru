# Persistence

Local data lives in SQLite files owned by `jykre-storage`
([ADR-0003](adr/0003-sqlite-for-local-storage.md)). Passwords are never stored
here ([Security](security.md)); query results are never written to disk.

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
missing or out-of-range value reads as the default, 1,000).

`workspace.state` keeps the workspace between sessions
(`jykre::workspace_state`): the open query tabs in order (title, connection
name, SQL, the SQL last saved to the library and the statement timeout), the
tab on screen, the saved queries and the last 200 history entries. It is
written half a second after changes settle and when the application quits,
and read once the connections are loaded. A tab whose connection no longer
exists reopens in a group of its own, without a connection. A value that
cannot be read starts an empty workspace. Results are never stored.

`config_json` holds the driver's non-sensitive options; drivers reject
password and token fields there. `revision` provides optimistic concurrency:
a save names the revision it edited, so a change made in another window is
rejected instead of silently overwritten.

## Schema: `metadata/<connection-id>.db`

| Table | Purpose |
| --- | --- |
| `metadata_info` | Single row: identity of the connection configuration the cache belongs to, and the cache version |
| `objects` | Published objects. `path_key` encodes the structural path (hex segments, so dots, slashes, wildcards and case cannot collide); `parent_key` plus the `objects_parent` index serve paged child reads |
| `refresh_runs` | Refresh attempts; a partial unique index allows only one `running` run |
| `staging_objects` | Objects of a run in progress |
| `protected_scopes` | Subtrees the run could not read (permission warnings) and must keep |
| `scope_status` | Per-scope state, last attempt, last success, version and message |

The cache records the identity of the configuration it was read from.
`MetadataRepository::prepare` reconciles it after the user saves a changed
profile; background readers use `open`, which never replaces another
configuration's identity.

## Atomic metadata refresh

1. `begin(scope)` takes the writer lock and inserts a `running` run.
2. The driver streams batches; `batch` writes them to `staging_objects`.
3. On success, `activate` replaces the objects of that scope with the staged
   ones **in one transaction**, keeps protected subtrees, bumps the cache
   version and records the scope status.
4. On failure or cancellation, `fail` records the error and discards the
   staged objects. The published objects are untouched.
5. When a cache is opened, `recover_interrupted` marks runs left `running` by a
   crash as interrupted and removes them; the previous read stays.

A scope's ancestors (its database or catalog) are outside the scope, so the
driver does not emit them; `activate` adds the missing ones when the scope
published anything. Reading a choice publishes each last-level namespace on
its own. After a complete read, `retain(subtrees, nodes)` keeps only the
objects inside the chosen subtrees, the named namespaces and their ancestors,
removes everything else with its scope status in one transaction and bumps
the version. It waits up to five seconds for a refresh that is finishing to
release the writer lock.

Readers that page through the cache compare the version before and after; if
a refresh published in between, they stop and ask the user to expand the node
again instead of mixing two reads.

## Migrations

Each file's schema version is `PRAGMA user_version`. Today both files are at
version 1, created in one transaction, and a file with a higher version is
refused ("created by a newer version of the application").

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
- **`config.db`:** never recreated automatically. The application reports the
  error; with backups in place it offers to restore the latest one.

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
| Numbered forward-only migration steps | Only version 1 exists; there is no step mechanism yet. The first schema change must introduce it |
| `config.db` backup before migrating, restore offered on failure | Not implemented; a broken `config.db` produces an error only |
