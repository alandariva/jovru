# Decisions: Persistence

Why this area is built as it is. Each entry records what was decided, why, and which alternatives lost. The rules themselves are in [Persistence](../reference/persistence/README.md). Entries are numbered `D-NNNN`, and the numbers are stable.

<a id="d-0003"></a>
## D-0003: SQLite for local storage, one metadata file per connection

*Accepted 2026-11-02 (recorded for a decision already in effect) · implemented*

**Decision.** `config.db` holds profiles and preferences, and each connection's metadata cache lives in its own `metadata/<connection-id>.db`. Access goes only through the repositories in `jovru-storage`, on SQLx's SQLite driver, with the configuration in [SQLite configuration](../reference/persistence/files.md#sqlite-configuration); a refresh stages objects and publishes a scope in one transaction ([Atomic metadata refresh](../reference/persistence/metadata-db.md#atomic-metadata-refresh)). No secret and no query result is stored in these files.
**Why.** Metadata can reach hundreds of thousands of objects per connection, the explorer and search must work offline and fast, and a failed refresh must never lose the previous read. Separate files isolate rebuildable data: a corrupted or huge cache can be set aside without touching profiles. The cost is that schema changes need migrations ([Migrations](../reference/persistence/files.md#migrations)).
**Rejected.**
- JSON files: no transactions or paged reads, everything in memory; earlier builds used `connections.json` and it was migrated away.
- One database for everything: a corrupted or huge cache would endanger profiles.
- An embedded key-value store: no SQL for paging and search; SQLite is enough and already in the stack.

<a id="d-0018"></a>
## D-0018: Numbered migrations for the metadata cache

*Accepted 2026-11-07 · implemented*

**Decision.** The metadata file has numbered, forward-only steps in `jovru-storage`, run on open between its `user_version` and the application's, in one transaction that ends by setting `user_version`; a file newer than the application is refused. Step 2 adds Unicode-folded `search_name`, `last_published_at` and the FTS5 trigram index; the mechanics and the step table are in [Migrations](../reference/persistence/files.md#migrations).
**Why.** Two backlog items needed the schema to change. Search read `instr(lower(name), lower(?))`, and SQLite's `lower` folds ASCII only (`ação` did not find `AÇÃO`) while the palette decoded every object's JSON to filter tables after the limit; and a partial refresh published but never set `last_success_at`, so the explorer said "Last successful refresh: none" about data just read. The benchmark at 500,000 objects measured a scan at 235 ms (112 ms over the covering index `objects_search`), above the 50 ms budget, so step 2 also builds `objects_fts` (external content, kept by triggers): three characters or more match in about 3.5 ms, shorter ones scan `objects_search` in about 114 ms. The first open after the upgrade fills `search_name` once.
**Rejected.**
- Recreate the cache on a schema change: every upgrade rereads every chosen namespace from the server, which is slow for large catalogs.
- A SQLite function registered for Unicode folding: SQLx exposes no portable way to register one, and an index cannot use it; a stored column is simple and indexable.
- Adding the FTS5 index only if needed: it was measured first and the scan missed the budget, so it was adopted.

<a id="d-0019"></a>
## D-0019: Compact keys and one copy of each path in the metadata cache

*Accepted 2026-11-03 · implemented* · Amends D-0018 (step 3)

**Decision.** Step 3 writes keys as each segment's JSON with `%` as `%25` and `/` as `%2F`, joined by `/`, instead of hex; drops `path_json`; and stores `object_json` without the path, which reads rebuild from the key. The key rules are in [Schema: metadata](../reference/persistence/metadata-db.md#schema-metadataconnection-iddb) and the step in [Migrations](../reference/persistence/files.md#migrations).
**Why.** A synthetic cache of 500,000 tables (one database, 100 schemas) measured 775 MB at version 1, mostly the same path in five places: `path_key` 111 MB, `parent_key` 73.5 MB, `path_json` 57 MB, the path inside `object_json` about 57 MB, and the primary-key and `objects_parent` indexes repeating the keys about 300 MB. At version 2 the step 2 indexes repeat `path_key` once more: 892 MB vacuumed, 1.5 GB as a full refresh leaves it. After step 3: 461 MB (-48%) and 753 MB (-50%); two-character search 114 ms to 93 ms. The costs are a one-time rewrite and vacuum of every cache on first open (about 20 s at 500,000 objects, under a second for a few thousand, off the UI thread) and slightly slower reads that decode paths (200 children 0.44 to 0.65 ms; search of three characters or more 3.5 to 7.4 ms), both far inside their 50 ms budgets (measured 2026-11-03, release build).
**Rejected.**
- Keep the layout: caches several times larger than their data.
- `\x00` as the separator with escapes: every prefix-range query changes, and NUL in TEXT is fragile across tools.
- Integer ids with a parent id column: every read joins to rebuild paths and prefix ranges need a recursive query; a larger change than escaping.
- Compress `object_json`: CPU on every read, with keys and indexes (the larger part) unchanged.
- Rewrite rows with foreign keys on: 89 s for 500,000 objects against 18-20 s with them off and a check before committing.

<a id="d-0031"></a>
## D-0031: Saved queries as plain `.sql` files, with tags and associations kept locally

*Accepted 2026-11-07 · implemented (checked in the application on Windows the same day)*

**Decision.** A saved query is a UTF-8 `.sql` file holding only SQL, titled by its file name, in a library folder the user can change (a Git working copy included). Tags and the connection association live in the `query_library` setting, never in the file; unsaved text lives in recovery files, not in `workspace.state`; saved queries from `config.db` migrate to files once. Rules and limits are in [Schema: `config.db`](../reference/persistence/config-db.md#schema-configdb) and [User experience](../reference/ux/README.md).
**Why.** Files can be moved, backed up, edited elsewhere and versioned, `config.db` stops holding query text, and drafts and file edits recover the same way after a crash. The maintainer chose to keep every piece of metadata out of the files after a first draft put title and tags in a comment header. `query_library` is a key of its own because `workspace.state`'s writer keeps only the fields it knows, so an older version would erase it (as with reports, D-0024). Paths are absolute so two libraries never share an association with a possibly production connection. Drafts reopen without asking on close (they lose nothing), but unsaved file tabs are asked about. The costs are that tags do not travel with the files, a title is limited to what a file name holds (a rename renames the file), files change under the application (conflict, missing-file and encoding paths), a file held open on Windows can refuse the rename, an earlier version opens migrated tabs empty, and GPUI's open dialog neither starts in the library nor filters `.sql`. The 4 MiB limit failed its first measurement (opening blocked the UI thread about 1.4 ms per KiB); on 2026-11-08 the maintainer kept it, with soft wrap off above 256 KiB (wrapping returns at 192 KiB or less). Measured that day, release on Windows: 4 MiB opens in one 54 ms frame, a file just under 256 KiB (wrapped) in 185 ms, typing costs at most 2.3 ms a key, and a click in unwrapped text stalls about 25 ms per MiB (97-131 ms at 4 MiB), inside the editor component; the click stays a known deviation in Performance.
**Rejected.**
- A comment header, a tags line, hashtags or a footer in each file: the application would write metadata into the user's SQL.
- A library file in the folder: a second file to keep in step, and external renames still lose tags.
- Title kept apart from the file name: it would need a place in or beside the file.
- Keep saved queries in SQLite and export them: SQL would stay in the database, against the goal.
- Drafts stay in `workspace.state`: large SQL stays in `config.db` and drafts and file edits recover differently.
- Ask about drafts when closing: a regression, since drafts reopen today.
- Associations by connection name only: a rename loses them.
- Paths relative to the folder: libraries with equal relative names would share associations; a wrong connection is worse than lost tags.
- Top-level folder only: Git repositories organise queries in folders.
- A file watcher (`notify`): a new dependency; activation, refresh and the check before each save cover safety.
- A hash crate for the fingerprint: a new dependency for a detector that guards nothing.
