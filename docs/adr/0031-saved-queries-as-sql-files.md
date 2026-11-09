# ADR-0031: Saved queries as plain `.sql` files named by their title, with tags and associations kept locally

- **Status:** Accepted (by the maintainer on 2026-11-07; implemented and checked in the application on Windows the same day; the 4 MiB limit failed its measurement; on 2026-11-08 the maintainer kept it, with soft wrap off above 256 KiB, and confirmed it after the measurement on the reference machine the same day, see D7)
- **Date:** 2026-11-07
- **Authors:** AI agent, at the maintainer's request (milestone M7 of
  [Version 0.2.0](../plans/releases/0.2.0.md); decisions D1–D7 below approved
  by the maintainer on 2026-11-07, D1 revised the same day to keep every
  piece of metadata out of the files)

## Context

Saved queries live today inside the `workspace.state` setting of `config.db`
([Persistence](../reference/persistence.md#schema-configdb),
`src/workspace_state.rs`): each one with a numeric `id`, a title, tags, its
SQL and the **name** of its connection. A query tab points at the saved query
it edits by `saved_id`, and every tab, saved or not, keeps its SQL in the same
setting.

M7 makes UTF-8 `.sql` files the source of truth for the SQL of saved queries,
in a default directory the user can change (including to a Git working
directory). The library becomes a view of those files, existing saved
queries migrate to files without loss, and drafts survive a crash through
recovery files. M7 requires this record before implementation for the
recovery storage and the migration.

A first draft of this record put the title and tags in a comment header
inside each file. The maintainer chose instead to keep the files as plain
SQL: the file name is the title, and the tags stay with the other local data.

Constraints found in the code and the standards:

- No SQLite schema change and no new dependency without approval
  ([0.2.0 boundaries](../plans/releases/0.2.0.md#boundaries-applying-to-every-milestone)).
  `settings` already holds JSON values by key, and `dirs` (already a
  dependency) gives the Documents folder.
- `WorkspaceState::to_value` writes back only the fields it knows, so a field
  added there is erased by an older version. [ADR-0024](0024-reports.md) put
  reports under a key of their own for this reason.
- The editor (`gpui-kit`) keeps `\r` in its text, but pressing Enter inserts
  `\n`, so a CRLF file edited as is would end up with mixed line endings.
- GPUI's open dialog (`App::prompt_for_paths`) takes neither a starting folder
  nor a file-type filter. Its save dialog (`App::prompt_for_new_path`) starts
  in a folder with a suggested name, but on Windows lists "All files" and adds
  no default extension.
- `std::fs::rename` replaces an existing destination, on Windows as on Linux.
- `JYKRE_CONFIG_DIR` isolates the data folder for tests and manual checks; a
  default under the real Documents folder would escape that isolation.
- Network, disk and data-sized work stays off the UI thread, is cancellable
  and shows feedback ([golden rule 6](../README.md#golden-rules)); polling
  timers are a rejected pattern ([Performance](../standards/performance.md#rejected-patterns)).

## Decision

### Plain files, named by their title (D1)

- A saved query is a `.sql` file holding only its SQL. The application never
  writes anything else into it: no header, no title, no tags, no connection.
  Any UTF-8 `.sql` file is a saved query as it is.
- Its **title is its file name** without the extension. A tab with a file
  shows that title; the library also shows the folder of a file in a
  subdirectory, so two files with the same name stay apart.
- **Renaming** a saved query (the library's pencil, or the tab's details)
  renames its file in the same folder:
  - the title is made safe for a file name as `reports::export::file_name`
    does (characters Windows forbids become `_`, reserved device names get a
    `_` in front), and the field shows the resulting name before the rename;
  - an existing file is never replaced: the name is checked right before the
    rename, which is refused when taken (a change of case alone is allowed);
  - open tabs, the local entry and its tags and association follow the file.
    A rename that fails changes nothing and is reported. A missing file
    cannot be renamed.
- **Tags** are edited in the same details and kept in the local state below.
  Changing them never touches the file and never leaves the tab unsaved.
- A tab without a file keeps its "Query N" title until it is saved.

### The library directory

- The default directory is `<Documents>/Jykre Studio/Queries`, from
  `dirs::document_dir()`; without one, `<home>/Documents/Jykre Studio/Queries`;
  without a home directory, `<data folder>/Queries`. When `JYKRE_CONFIG_DIR`
  is set, the default is `<JYKRE_CONFIG_DIR>/Queries`, so tests and manual
  checks never touch the user's documents.
- It is created only when something is first written into it (a save that
  starts there, the migration). A default directory that does not exist yet
  is an empty library, not an error.
- The user changes it in Preferences with the system folder picker. Changing
  it only points the library elsewhere: no file is moved, copied, overwritten
  or deleted, and open tabs keep their files.
- Files anywhere else can be opened and saved through the system dialogs;
  they work like library files but are not listed in the library.

### Discovery (D5, D6)

- The library lists every `*.sql` file (extension in any case) in the
  directory and its subdirectories, skipping hidden directories (a name that
  starts with `.`, such as `.git`) and not following directory links. It
  stops at a depth of 8 and at 10,000 files and says so when it stopped.
- A scan keeps, per file, its path, title and up to 16 KiB of its SQL for the
  preview and the search, and joins the tags from the local state; the index
  never holds more than 32 MiB of SQL. A file past those bounds is still
  listed and found by title, tags and path. The index lives in memory only
  and is published whole when the scan ends; a scan still running keeps the
  previous index on screen.
- A scan runs on the runtime (`backend::run`, `spawn_blocking`), takes a
  `CancellationToken` (a newer scan cancels the older one) and shows its
  progress in the library when it runs longer than 2 s. It never writes.
- Scans run at start, when the library is shown, when the window becomes
  active again, after a save, a *Save as*, a rename or the migration, and from
  a **Refresh** button. There is no file watcher and no polling timer.

### Opening, saving and closing

- **Open…** (`Ctrl O`) uses the system dialog (several files at once). A file
  already open shows its tab instead of a copy. **Save** (`Ctrl S`) and
  **Save as…** (`Ctrl Shift S`) are the only ways to write a query; the
  editor toolbar's save button and saving to the library are **Save**.
- A tab opens in the connection associated with its file (below). A file with
  no association opens in the connection of the tab group on screen, as **New
  query** does, and the association is recorded; a missing connection opens
  the tab without one and says so, as a saved query does today.
- **Save** on a tab without a file is **Save as…**: the save dialog starts in
  the library directory with the tab's title, made safe, as the name. When
  the name chosen has no extension, `.sql` is added; if that changes the name
  and a file already has it, the application asks before replacing it, since
  the system dialog asked about another name. *Save as* on a file tab writes
  the new file, moves the tab to it, copies the old file's tags and
  association and leaves the old file untouched.
- **Save** on a file tab without changes writes nothing.
- **Writing** follows `jykre::export::ExportFile`: a temporary file created
  with `create_new` next to the destination, written, flushed, synced and
  renamed over it. A failure or cancellation removes the temporary file and
  leaves the destination as it was; the tab keeps its text and stays unsaved.
- **The text as written:** the editor receives the text with `\n` only. The
  file's style (CRLF when its first line break is CRLF, otherwise LF) and its
  byte order mark are remembered and written back on save. A file with mixed
  line endings is saved in the style of its first line break; this happens
  only on an explicit save.
- **External changes:** each file tab keeps what it was loaded or last saved
  from: the size, the modification time and a 64-bit FNV-1a digest of the
  bytes (a change detector, not a security measure). Before writing, the file
  is checked against it:
  - changed on disk: **Overwrite**, **Reload from disk** (after confirming
    that the tab's changes are discarded), **Save as…** or **Cancel**;
  - moved or missing: **Locate…** (choose the file to associate the tab and
    its tags and connection with; its text is loaded only after the user
    confirms it replaces the tab's), **Save as…** or **Cancel**. A missing
    file is never recreated at its old path or replaced by another file
    without the user choosing it.
- **Reload:** reloading a file tab (asked from the tab, or chosen above)
  reads the file again; external edits appear then, and in the library after
  its next scan.
- **Unsaved state:** a file tab whose text differs from what it was loaded or
  saved from is marked unsaved in the tab strip, and its Save button says so.
- **Closing a file tab** with unsaved changes asks **Save**, **Don't save** or
  **Cancel**, combined with the existing questions about a running query, an
  open transaction or an export. A failed or cancelled save keeps the tab open
  and unsaved.
- **Closing the application (D3)** asks once, listing the file tabs with
  unsaved changes: **Save all**, **Don't save** or **Cancel**, together with
  the existing transaction question. Tabs without a file are not asked about:
  they reopen at the next start from their recovery files, as drafts do today.

### Local state apart from the files

- A new `query_library` key in `settings` holds what is local to this
  machine, never SQL or titles:

  ```json
  {"version": 1, "directory": null, "next_id": 12,
   "entries": [{"id": 4, "path": "C:\\Users\\…\\Queries\\Monthly sales.sql",
                "tags": ["finance", "monthly"],
                "connection": {"id": "…", "name": "PostgreSQL · Local"}}],
   "migration": null}
  ```

  `directory` is `null` for the default. Each entry gives a file, by absolute
  path (compared ignoring case on Windows), a stable `id` used by tabs and
  the palette, its tags (cleaned by the `workspace_state::parse_tags` rules:
  no `#`, no blanks, no repeats ignoring case) and its connection
  association. Keys this version does not know are kept, in the value and in
  each entry.
- It does not live in `workspace.state`, whose writer would let an older
  version erase it ([ADR-0024](0024-reports.md)). A missing value is the
  default directory with no tags or associations; a value that cannot be read
  is reported and never written over. It is written half a second after
  changes settle and when the application quits; the demo writes nothing.
- **Tags are local.** They do not travel with the files: copying the folder,
  backing it up, keeping it in Git or opening it on another machine carries
  the SQL and the titles, not the tags.
- **Connection association (D4):** `{"id", "name"}`, resolved by the
  connection's ID and, when that ID is unknown, by its name; the name is
  updated when the connection is seen again, so a rename keeps the
  association.
- Paths are absolute, not relative to the directory: two libraries with the
  same relative names never share tags or a connection association, so a
  query does not open on another library's (possibly production) connection.
- A file renamed or moved outside the application appears as a new file with
  no tags, and the old entry as missing. The entry keeps its tags and
  association until the user reassociates it (**Locate…**) or forgets it from
  the library; the application never guesses which file it became.
- No search index is stored in SQLite; it is rebuilt by each scan.

### Tabs and recovery files (D2)

- In `workspace.state`, a tab no longer holds its SQL, nor `saved_sql` and
  `saved_id`. It holds `recovery` (the name of its recovery file, when it has
  one) and, for a file tab, `file`: `{"id", "path", "base": {"size",
  "modified", "digest"}, "line_ending", "bom"}`. Its title, connection,
  context, timeout, parameters and the other fields stay as they are.
- A recovery file is `<data folder>/recovery/<uuid>.sql`, holding the tab's
  text as the editor has it (plain UTF-8, LF), so it can also be opened by
  hand. It exists for a tab without a file that is not blank, and for a file
  tab whose text differs from its file. It is written half a second after
  edits settle and when the application quits, with the same temporary-file
  rename as a save, and it is never the user's file: it never replaces the
  last explicitly saved script.
- It is removed after the tab's text is saved, discarded or reloaded, and
  after the tab is closed with the user's agreement.
- **At start**, each tab is filled from its recovery file when it has one,
  otherwise from its file. A file tab whose file changed meanwhile (its `base`
  no longer matches) opens with its recovered text, unsaved, and says the file
  changed on disk; a missing file is reported as above. A recovery file no
  tab names (a crash between the two writes) reopens as a draft tab called
  "Recovered", never deleted unseen. A recovery file that cannot be read is
  reported with its path and kept.
- Writing order: the recovery file first, then `workspace.state`. A crash in
  between leaves at worst an unnamed recovery file (reopened as above) or an
  outdated `base` (reported as a change on disk, a false alarm that loses
  nothing).
- Reading a `workspace.state` written by 0.1.0 keeps working: a tab with an
  inline `sql` opens with it, and the next write moves that text to a recovery
  file and drops `sql` only after the recovery file was written. An earlier
  version reading the new value opens those tabs empty; the files and the
  recovery files stay on disk.
- Recovery restores text only: never results, sessions or transactions. The
  demo writes no recovery files.

### Migration of the saved queries in `config.db`

1. **When:** at start, once connections and state are read, if
   `workspace.state` still holds saved queries. It runs on the runtime, shows
   its progress in the library, can be retried and never executes SQL.
2. **Plan:** each query gets `<library directory>/<title>.sql`, the title made
   safe as for a rename (an empty title becomes "Query"). A name taken by an
   existing file or by another planned file (ignoring case) becomes
   `<title> (2).sql`, `(3)`, and so on. Saved queries could share a title, so
   this is expected; the query's title becomes its new file name.
3. **Journal:** the plan (`id` → path) is written to `query_library.migration`
   before any file is created.
4. **Write:** each file is created with `create_new`, so an existing file is
   never overwritten, and holds the SQL exactly as stored, with LF line
   endings.
5. **Verify:** each file is read back and its SQL compared with the stored
   query.
6. **Backup:** the original `saved_queries` array and `next_id` are written to
   `<data folder>/backups/saved-queries-<millis>.json`, outside the rotation
   of the `config.db` copies, and kept until the user deletes it.
7. **Commit:** `query_library` gets one entry per verified file, keeping the
   query's `id`, its tags and its connection (by name, plus the ID of the
   connection of that name when there is one); then `workspace.state` is
   written without those queries. A tab linked to a migrated query becomes
   that file's tab: unchanged when its SQL equals the saved SQL, otherwise
   unsaved with its text in a recovery file.
- **Retry:** a planned file that exists and verifies as its query is adopted
  instead of written again; one that exists but differs keeps its content,
  and the query takes the next free name. A query already in `query_library`
  with a verified file is only removed from `workspace.state`. So a crash at
  any step neither duplicates nor loses a query.
- **Partial failure:** verified queries are committed; the others stay in
  `workspace.state`, still listed and openable in the library, marked as not
  migrated with the error, and **Retry** runs the migration again (it also
  runs at each start).
- When it ends, a notice says how many queries became files, where, and
  which ones got a title different from the one they had (a name made safe
  or numbered).
- The history stays in `workspace.state`; it is not part of the library.

### Limits (D7)

| What | Limit | Beyond it |
| --- | --- | --- |
| A file or recovery file opened into a tab | 4 MiB | An error with the size and the limit; nothing is opened or replaced. A recovery file over it is kept and its path shown |
| Discovery | Depth 8, 10,000 files | Listing stops and says so |
| Search text per file / in total | 16 KiB / 32 MiB | Found by title, tags and path only |

- Text must be UTF-8, with or without a byte order mark. A UTF-16 or UTF-32
  byte order mark, or invalid UTF-8, is reported as an unsupported encoding;
  the tab's SQL is not replaced and nothing is written.
- The 4 MiB limit is confirmed by measuring the editor with a file of that
  size against the frame budget before this record is accepted; a lower
  number found then replaces it here. Measured on 2026-11-07: no size measured meets the
  budget, since opening a file blocks the UI thread about 1.4 ms per KiB
  ([Performance](../standards/performance.md#the-editor-with-a-large-sql-file));
  the maintainer decides the number. Decided on 2026-11-08: the limit stays
  4 MiB, and a tab whose text exceeds 256 KiB does not soft-wrap (wrapping
  comes back at 192 KiB or less), since the editor component's wrapping
  measures every line when the file opens; typing no longer reads the
  whole text on the UI thread ([Large `.sql` files in the editor](../plans/editor-large-files.md)).
  Measured on the reference machine on 2026-11-08 (release, Windows): a
  4 MiB file opens in one frame of 54 ms and one just under 256 KiB, still
  wrapped, in 185 ms; typing costs our code at most 2.3 ms a key at 4 MiB;
  a click into a text that is not wrapped stalls the UI thread about 25 ms
  per MiB, 97–131 ms at 4 MiB, inside the editor component
  ([Performance](../standards/performance.md#on-the-reference-machine-2026-11-08)).
  The maintainer confirmed the 4 MiB limit on 2026-11-08 with these
  numbers; the click stays a known deviation in Performance.
- The limits are documented in [Persistence](../reference/persistence.md) and
  [User experience](../reference/ux.md); content is never truncated to fit.

### Where the code lives

- A new library module, `jykre::query_files` (`src/query_files/`): file
  names, renaming, the fingerprint, reading and writing within the limits,
  discovery, recovery, the `query_library` value and the migration. It knows
  nothing of GPUI and names no session, driver or `backend::query`;
  `cargo xtask check` enforces that, so opening, discovering, migrating or
  recovering files cannot run SQL.
- Its file work runs through `backend::run` and `spawn_blocking`, with a
  `CancellationToken`, like exports.
- The views (`src/app/`) add the commands, the dialogs (a three-button variant
  of `confirm`), the unsaved marker, the library view of the index (still a
  `uniform_list`), the rename and tags details, and the directory in
  Preferences.
- `jykre-storage` and the SQLite schema do not change: both settings are
  written through `connection_store::write_setting`.
- `src/workspace_state.rs` keeps the tabs and the history; its saved-query
  functions are kept only for reading 0.1.0 values and for the migration.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| A comment header with title and tags in each file (the first draft) | Metadata travels with the SQL | The application writes into the user's SQL; a grammar, versions, malformed and unknown fields to handle | The maintainer prefers plain SQL files (D1) |
| A tags line, hashtags or a footer in each file | Tags travel with the SQL | Still metadata inside the user's SQL | Same reason |
| A library file in the folder (`.jykre-library.json`) | Tags travel with the folder | A second file to keep in step; external renames still lose the tags | Tags stay with the other local data instead |
| Title kept apart from the file name | Any characters in a title | Needs a place for it in or beside the file | The file name already names the query |
| Keep saved queries in SQLite and export them | No migration | SQLite stays the source of truth for SQL | Contrary to M7 |
| Drafts stay in `workspace.state` (D2) | No change for drafts | Large SQL stays in `config.db`; drafts and file edits would recover differently | M7 asks for separate recovery files for drafts and pending edits |
| Ask about drafts when the application closes (D3) | Uniform | A regression: drafts reopen today without asking | Drafts lose nothing on close |
| Associations by connection name only (D4) | As today | A rename loses them | The ID survives a rename, as reports and favorites need |
| Paths relative to the directory | Moving the library keeps tags and associations | Two libraries with the same relative names share them, possibly with a production connection | A wrong connection is worse than lost tags |
| Top-level directory only (D5) | Simpler and faster | Git repositories organize queries in folders | Subdirectories within limits |
| A file watcher (`notify`, D6) | Changes appear at once | A new dependency and its platform behaviour | Activation, refresh and the check before each save cover safety without it |
| A hash crate (`sha2`, `blake3`) for the fingerprint | Stronger digest | A new dependency | FNV-1a only detects change; it guards nothing |

## Consequences

- Positive:
  - Queries can be moved, backed up, edited elsewhere and kept in Git; the
    application never writes anything but the user's SQL into them.
  - `config.db` no longer holds query text, so its copies stay small.
  - Drafts and file edits recover the same way after a crash.
- Negative or costs:
  - Tags are local: another machine, a copy of the folder or a rename
    outside the application does not carry them.
  - A title is limited to what a file name can hold; renaming a query renames
    its file, which Git records as a rename.
  - Files can change under the application: the conflict, missing-file and
    encoding paths need code, tests and dialogs.
  - On Windows a file held open by another program may refuse the rename;
    the save fails, is reported and the tab stays unsaved.
  - An earlier version opens migrated tabs empty and shows an empty library;
    files, recovery files and the backup stay on disk.
  - The open dialog cannot start in the library or filter `.sql` files, and
    the save dialog lists every file (GPUI); the library list is the main
    way in.
- What becomes required or forbidden:
  - Only an explicit save, *Save as*, a rename or the migration writes into a
    user's directory; opening, running, discovering and recovering never do.
  - The application writes nothing into a saved query's file but its SQL.
  - After the migration, SQL and titles are not stored in `config.db` (the
    backup file aside, and queries not migrated yet); tags are, as local data.
  - [Persistence](../reference/persistence.md), [User experience](../reference/ux.md)
    and [Architecture](../reference/architecture.md) are updated with the
    implementation.

## How to validate

- File tests in temporary directories: safe and colliding names, a rename
  refused over an existing file, a change of case allowed, `create_new` never
  overwriting, a failed write leaving the destination intact, line endings
  and byte order mark kept, a change on disk and a missing file detected
  before writing, unsupported encodings replacing nothing, the size limit.
- Local state tests: tags and associations kept by path and identity,
  following a rename and a *Save as*, resolved by ID and then by name, kept
  for a missing file until reassociated, unknown keys kept.
- Migration tests: success; collision with a user's file and between equal
  titles; a crash after the journal (planned file present, matching or not);
  a verification mismatch; partial failure and retry; identities, tags,
  associations and tab links kept; the backup's content.
- Recovery tests: drafts and file tabs restored; a file changed while closed;
  an unnamed recovery file reopened; a 0.1.0 state moved to recovery files.
- Discovery: hidden directories skipped, depth and count limits, cancellation,
  and timing of a scan of 10,000 small files in a release build.
- Performance: the editor with a 4 MiB file in the frame probe, and the
  heartbeat while a scan runs (≤ 16 ms), recorded in
  [Performance](../standards/performance.md).
- In the application on Windows, and on Linux for M5: open, save, *Save as*,
  rename, tags, conflict, missing file, closing a tab and the application,
  the library and the directory setting, with keyboard and focus.
