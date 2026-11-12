# Persistence: files, migrations and recovery

Part of [Persistence](README.md).

## Files

All files live in the data folder: `%LOCALAPPDATA%\Jovru` on Windows,
`~/Library/Application Support/Jovru` on macOS and
`$XDG_DATA_HOME/jovru` on Linux. `JOVRU_CONFIG_DIR` overrides it (use an
isolated folder for tests and manual checks).

| File | Content | Lifetime |
| --- | --- | --- |
| `config.db` | Connection profiles (non-sensitive configuration) and preferences | Permanent; losing it loses the user's connections |
| `metadata/<connection-id>.db` | Metadata cache of one connection, refresh runs and per-scope status | Rebuildable from the server |
| `metadata/<connection-id>.lock` | Writer lock: one refresh per cache at a time, across processes | Recreated as needed |
| `metadata/<connection-id>.db.corrupt-<stamp>` | A cache set aside after confirmed corruption | Kept for diagnosis; safe to delete |
| `backups/config-v<version>-<millis>.db` | Copies of `config.db`: one each start whose configuration opened and was read, unless it equals the newest copy; the five most recent are kept | Rotated automatically; restored from when `config.db` cannot be opened |
| `config.db.broken-<millis>` | A configuration set aside when a backup was restored in its place | Kept for diagnosis; safe to delete |
| `logs/jovru.<date>.log` | JSON log lines, one file per day; the last 7 are kept ([Reliability](../../standards/reliability.md#logs)) | Rotated automatically; safe to delete |
| `recovery/<name>.sql` | The text of a tab a crash must not lose: a draft, or a file tab's unsaved edits (plain UTF-8, `\n` line endings) | Written half a second after edits settle and on quit; removed once saved, discarded or closed; one no tab names reopens as a **Recovered** tab |
| `backups/saved-queries-<millis>.json` | The saved queries `config.db` held, as they were before they moved to files | Kept until the user deletes it; outside the rotation of the `config.db` copies |
| `<executable's folder>/.jovru-update/` | Not in the data folder: an update being installed (`staging-<id>/`: the downloaded archive and what it unpacks to), what it replaced (`trash-<id>/`, `done-<id>/`) (D-0033) | Removed when the update ends; at the next start, `done-*` and anything older than an hour are removed (on Windows the replaced executable can only go then); safe to delete while no update runs |
| `automation.db` | Automation: job definitions, published versions, run and chain-run history, opt-in declared outputs (D-0034) | Permanent; losing it loses the user's jobs. History is pruned ([below](automation-db.md#automation-automationdb)) |
| `automation.db.corrupt-<millis>` | An automation file set aside after confirmed corruption (with its `-wal`/`-shm` files) | Kept for diagnosis; safe to delete |
| `automation.lock` | Empty file under an operating-system lock: the instance that holds it runs automation | Recreated as needed; its existence means nothing |
| `automation/logs/<run-id>/` | `stdout.log`, `stderr.log` and, under `jjp/1`, `jjp.log` of one external-program run, at most 1 MiB each (D-0037) | Deleted 7 days after the run ended, with **Delete logs**, or when the run's row is gone |
| `connections.json` (legacy) | Profiles from earlier builds | Imported once into `config.db` in a single transaction, deleted only after the import is verified |

## SQLite configuration

Every file is opened by `open_pool`:

- `journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout=5 s`, at most 4 pooled
  connections.
- `synchronous=FULL` for `config.db` (user data) and `NORMAL` for metadata
  caches (rebuildable).
- `PRAGMA quick_check` on open; a file that fails is not used.
- `automation.db` is the one file opened with `synchronous=FULL` and
  `secure_delete=ON` ([below](automation-db.md#automation-automationdb)).

## Migrations

Each file's schema version is `PRAGMA user_version`. `config.db` is at
version 1; `automation.db` is at version 5 ([above](automation-db.md#automation-automationdb)). The metadata files are at version 3
(D-0018): a new file is created at
version 1 and then migrated, and an older one gets only the steps it lacks.
A file with a higher version is refused ("created by a newer version of the
application").

Metadata steps (`jovru-storage`, `metadata.rs`):

| Step | Change |
| --- | --- |
| 2 | `objects.search_name` and `staging_objects.search_name`: the name in Unicode lower case, written with every object and filled for existing rows by the step; the index `objects_search` and the FTS5 trigram table `objects_fts`, kept in step with `objects` by triggers; `scope_status.last_published_at`: when a refresh last published the scope, complete or partial (`last_success_at` keeps meaning a complete read) |
| 3 | Compact keys (D-0019): every `path_key`, `parent_key` and `scope_key` rewritten from hex to escaped JSON, `object_json` stored without the path, `path_json` dropped from `objects` and `staging_objects`; refreshes left running are dropped and their scopes marked interrupted. The step runs with foreign keys off, checks them before committing and vacuums the file afterwards. A cache of 500,000 objects goes from 892 MB to 461 MB (1.5 GB to 753 MB as a refresh leaves it) in about 20 s, once |

Metadata steps run on a connection of their own with foreign keys off, as
SQLite advises for schema changes, and fail when `PRAGMA foreign_key_check`
finds a broken reference; a failed step leaves the file at its previous
version. With foreign keys on, step 3 took 89 s at 500,000 objects (every key
update checks the self-reference); off, with a check before committing, 18-20
s. After step 3, a first page of 200 children costs about 0.65 ms (0.44 ms
before) and a search of three characters or more 7.4 ms (3.5 ms before).
`config.db` keeps its own version.

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
  previous version. For the metadata steps these include a version-1 file with
  objects and statuses, the Unicode search (`ação` finds `AÇÃO`; a table is
  found behind 200 columns of the same name),
  `keys_round_trip_and_a_node_s_range_holds_its_descendants_only`,
  `a_version_2_cache_compacts_its_keys_and_reads_back_the_same` and
  `a_step_that_breaks_a_reference_leaves_the_file_as_it_was`; the metadata
  benchmark reports the search time at 500,000 objects.

## Corruption and recovery

- **Metadata cache:** when opening fails and `quick_check` (or SQLite codes 11
  or 26) confirms corruption, the file and its `-wal`/`-shm` companions (those
  first) are renamed to `*.corrupt-<stamp>`, an empty cache is created and the UI tells
  the user to refresh. Permission and I/O errors never trigger this, so a
  valid cache is not set aside by mistake.
- **`automation.db`:** set aside as `automation.db.corrupt-<millis>` and
  recreated when corruption is confirmed, as described
  [above](automation-db.md#automation-automationdb); a file from a newer version is left
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
  that could not be opened is kept as `config.db.broken-<millis>` (a second
  name for it, or a copy), with its `-wal`/`-shm` files moved next to it.
  The copy is made beside it as `config.db.restoring-<millis>`, opened there,
  and then replaces `config.db` in one rename, so `config.db` is never
  missing: a failure on the way puts the journal files back and leaves
  `config.db` as it was. The connections are then loaded again.

## Rules

- SQLite is accessed **only** through the repositories in `jovru-storage`
  (`ConfigRepository`, `MetadataRepository`, `AutomationRepository`). Other crates do not write SQLite
  SQL.
- Repositories take and return `jovru-core` types, never raw rows.
- Reads for the UI are paged (children of a node, search with a limit).
- Nothing secret is stored, and no query result is written to disk, apart
  from a report's print HTML at the path the user picks
  (D-0024) and automation's two bounded
  exceptions ([declared outputs and program logs](automation-db.md#automation-automationdb)).
- The application writes nothing into a saved query's file but its SQL; only
  an explicit save, *Save as*, a rename or the migration writes into a user's
  folder.
- A schema change follows [Migrations](#migrations) and needs an entry under
  [Decisions](../../decisions/persistence.md) when it changes what is stored.
