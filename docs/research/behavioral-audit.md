# Behavioral audit — Jykre Studio versus reference tools

Date: November 3, 2026 · Branch `release/0.1.0` (`15446f2` plus uncommitted local changes). This replaces the earlier behavioral-audit backlog, retained in Git history.

**Reference tools.** Three widely used database managers are named **Tool A** (commercial database IDE), **Tool B** (universal open-source client) and **Tool C** (lightweight native client). Attributed behavior was checked in their official documentation.

**Method.** Read views (`src/app/*`), models (`query.rs`, `result_tabs.rs`, `sql.rs`, `grid.rs`, `workspace_state.rs`, `connection_store.rs`, `grouped_tabs/`) and `docs/`. **The application was not run.** “Confirmed” means confirmed in code.

**Reading references.** Each item identifies tools documenting its reference behavior. “Undocumented” means the consulted documentation does not cover it, not that behavior differs. Where tools disagree, the finding is justified only if Jykre is worse than every approach or internally inconsistent. General principles such as executing the requested operation, preventing silent data loss and internal consistency are marked **Principle**.

**Approval.** Only **Approved** items are active development work. **Pending** items await the maintainer and must not be implemented. A **Decision** overrides the recommendation when approval changes it. Implemented items record **Status**, dates, commits and deviations. Translation does not change these approvals.

**Previous backlog.** Before `15446f2`, this file used different Nxxx IDs. This audit restarts at N001; still-open earlier items say “Backlog: Nxxx”. Two earlier items were already fixed and excluded: old N028 (refresh reports when the tree was not reloaded, `node_status`) and old N031 (saving cancels refresh only after successful storage).

## 1. Executive summary

**Alignment: medium.** The execution core already follows the tools: selection/current-statement execution, one result per statement and detailed server errors. Some Jykre behavior is more careful than the references; see section 4.

The main divergences are at the edges:

1. **Triggers that reverse meaning:** run shortcuts also cancel; save also deletes; repetition undoes the first action.
2. **Results detached from their origin:** rerun uses the cursor rather than result SQL; `Ctrl+Enter` overwrites the newest tab, even one intended to be retained. A/B offer pinning.
3. **Name-based identity:** tabs, history and saved queries identify connections by name; missing names fall back to the first connection or demo. A/C retain connection-specific history.
4. **Connection flows without dialogue:** unsaved passwords are never requested; saving a profile destroys active work without warning.
5. **Opening tables does not display data:** references open data/object editors with paging; Jykre opens an unexecuted `SELECT … LIMIT 100` draft.

There are 47 findings: 10 high, 22 medium, 15 low, none critical; 15 reconfirm previous-backlog items. **46 are approved.** N001–N016 were approved/implemented November 3 (`f18136c`), with November 4 commits `8208602` for N008 and `66f262b` for N010. N017–N047 except N043 were approved November 4: 29 were implemented in that slice; N040/N044 already met requirements and were verified. N031 was implemented November 7 in 0.2.0 M2: grid and query-page copies first, then the value inspector's **Copy** and report-panel copy menus. N043 was **rejected**.

## 2. Divergences

### N001 — Execution shortcuts immediately cancel active execution without confirmation

- **Priority:** High
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Execution/cancellation
- **Behavior at audit time:** During a run, `Ctrl+Enter`, `Ctrl+Shift+Enter`, `Ctrl+Alt+Enter`, Pending gutter buttons and a second Run click (now Stop) call `stop_execution` immediately, without confirmation.
- **Reference behavior:** Tools differ on buttons, but none documents cancellation through run shortcuts. A has separate cancellation and warns before a second request terminates the server process. C changes Run to Cancel, which opens a process list. B: undocumented.
- **Difference:** A toggling button is acceptable (C does this); making a run shortcut cancel in one press is the problem.
- **Impact:** Repeating `Ctrl+Enter` to rerun or because the first press seemed ignored interrupts a long `UPDATE`/`DELETE` instead.
- **Recommendation:** Run shortcuts never cancel; report “already running”, as during Cancelling. Use a separate cancel button/shortcut.
- **Decision:** Repeating any execution shortcut during a run asks “Cancel execution?” with **Yes** initially focused, allowing Enter to confirm. `Ctrl+Alt+Enter` confirmation cancels the whole run.
- **Project evidence:** `src/app/workspace.rs`: `toggle_run`, `execute_current`, toolbar `execute`, `run_button` (Pending → `stop_execution`).
- **Confidence:** Confirmed in code; documented references A/C.

### N002 — “Run query again” executes cursor SQL instead of result SQL

- **Priority:** High
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Results
- **Behavior at audit time:** The result tab's ↻ calls `execute_current`, executing the current selection/cursor and writing to the newest tab, ignoring its stored `tab.sql`.
- **Reference behavior:** Principle: rerunning a result repeats its generating statement. No equivalent button was documented in the three tools; the reference is the label's promise.
- **Difference:** The label promises to repeat the visible result, but the action uses editor state.
- **Impact:** Refreshing a `SELECT` can run a `DELETE` under the cursor and write into another tab.
- **Recommendation:** Rerun the active tab's `tab.sql` into that same tab.
- **Project evidence:** `src/app/workspace.rs`: `refresh-results` → `execute_current`; `ResultTab::sql` in `src/result_tabs.rs`.
- **Confidence:** Confirmed.

### N003 — Opening a table does not show its data

- **Priority:** High
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3; `f18136c`). Updated November 5 at the maintainer's request: replaced by the **Data** table viewer. Double-click, Enter, palette and **View data** open `SELECT * FROM …;` without `LIMIT`, execute immediately in a Data result tab and page using the session cursor, with SQL visible. **New query for table** remains in the menu, opening SQL with `LIMIT` without executing.
- **Feature:** Explorer/navigation
- **Behavior at audit time:** Double-click, Enter, “Open preview” and palette Enter call `add_query` with `SELECT * FROM … LIMIT 100;`, opening the empty “Ready to explore — Run the query” state without execution.
- **Reference behavior:** A opens the table data editor on double-click/F4; B opens the object editor with configurable initial page (Data, Properties, etc.); C opens paged data on table-name click, default 300 rows/page.
- **Difference:** “Open” produces a SQL draft rather than data.
- **Impact:** Every inspection needs an extra `Ctrl+Enter`; “Open preview” promises an absent preview.
- **Recommendation:** Execute on opening, or rename to “New query for table”.
- **Decision:** Initially rename every entry point (context menu, explorer double-click/Enter, palette) to **New query for table**, still opening SQL without executing. The later status update above records its replacement.
- **Project evidence:** `src/app/connections.rs`: `open_relation`; `src/app/mod.rs`: `add_query`, palette `PressEnter`; `src/backend.rs`: `preview_sql`.
- **Confidence:** Confirmed; documented in all three references.

### N004 — Unsaved passwords are never requested when connecting

- **Priority:** High
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Authentication
- **Behavior at audit time:** After restart without Remember password, the first run/refresh fails “Could not sign in”. Users must configure the connection, enter a password and save; the save notice says “Enter the password before querying the server”.
- **Reference behavior:** A requests unsaved passwords per connection or retains them until restart according to the option. B requests credentials when saving is disabled. C: undocumented; uses the system keychain.
- **Difference:** Not storing a password causes an error instead of a prompt.
- **Impact:** Good credential practice requires failure/profile editing each session, including N005 side effects.
- **Recommendation:** Prompt in a dedicated dialog when opening a credential-less session; retain in memory until restart and offer remembering.
- **Project evidence:** `src/connection_store.rs`: `save_to`; `src/app/connections.rs`: `form_input`; `src/error_report.rs` authentication action; no password prompt in `src/app` at the audited snapshot.
- **Confidence:** Confirmed; references A/B documented.

### N005 — Saving a connection cancels runs and clears every tab's results without warning

- **Priority:** High
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Connections/tabs
- **Behavior at audit time:** Changing host, port, default database, sslMode or any profile option makes `update_connection` call `set_connection` in every associated tab: `abandon_run` cancels, closes the session and rolls back transactions; results clear and the assistant closes. No prior confirmation or later tab notice.
- **Reference behavior:** Principle: configuration changes must not destroy ongoing work without asking. Jykre already asks before moving a tab between connections. Reference tools do not document editing a connected data source.
- **Difference:** Configuration edits immediately destroy active work.
- **Impact:** Fixing profile details cancels a long `UPDATE` and removes all open results.
- **Recommendation:** Ask before saving if affected tabs have active runs/transactions. Otherwise preserve results/session until the next run and show a configuration-change notice.
- **Project evidence:** `src/app/connections.rs`: `save_connection`, `panel.update_connection` loop; `src/app/workspace.rs`: `update_connection`, `set_connection`, `abandon_run`.
- **Confidence:** Confirmed; principle/internal consistency.

### N006 — Saved queries and history open on another connection when the original is missing

- **Priority:** High
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Library/history
- **Behavior at audit time:** Entry clicks resolve the connection by name, then silently fall back through `.or(this.connections.first())`.
- **Reference behavior:** A history belongs to its connection console; C keeps history per connection; B undocumented.
- **Difference:** A missing source connection becomes an implicit connection switch.
- **Impact:** SQL, potentially `DELETE`, is ready on the wrong database when a profile fails startup or was renamed externally.
- **Recommendation:** Open disconnected, as orphan tabs do, or ask the user to select a connection.
- **Project evidence:** `src/app/mod.rs`: `sidebar_content`, entry `on_click` with `.or(this.connections.first())`.
- **Confidence:** Confirmed; documented A/C.

### N007 — A disconnected tab executes SQL against demo data

- **Priority:** High
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Execution/connection state
- **Behavior at audit time:** `input == None` sends `execute_queries` to demo: `sample_result` fabricates rows, `preview_error` rejects writes, footer says “Demo data · MOCK”. Outside `JYKRE_DEMO`, this affects restored orphan groups or tabs moved to Without connection. Header database comes from demo name heuristics (`commerce`, `finance`, `analytics`).
- **Reference behavior:** Principle: no connection means no execution; request a connection.
- **Difference:** Demonstration code handles a production path.
- **Impact:** Plausible fabricated rows or “The demo accepts only read queries” appear during real use.
- **Recommendation:** Outside demo, Run shows “Choose a connection for this tab” and opens the selector.
- **Project evidence:** `src/app/workspace.rs`: `execute_queries` after `if let Some(input)`, `Workspace::new` database/color heuristics; `src/app/state.rs`: `group_for`; `src/app/demo/mod.rs`: `sample_result`.
- **Confidence:** Confirmed.

### N008 — Explicit transactions are invisible and roll back when the tab closes (Backlog: N005)

- **Priority:** High
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3–4; `f18136c`, `8208602`). Deviation: `Session::transaction` reports PostgreSQL server state without taking a snapshot; MySQL reports only `autocommit = 0`. Unknown states such as MySQL `BEGIN` are inferred from statements. Protocol reading remains critical debt [TD-001](../plans/technical-debt.md#td-001--transaction-state-is-not-read-from-the-protocol).
- **Feature:** Transactions
- **Behavior at audit time:** Autocommit is the only supported UI mode. `BEGIN` stays open invisibly without Commit/Rollback. Closing/moving tabs, saving profiles (N005) or losing sessions silently rolls back. “Previous statements were not rolled back automatically” is false inside an aborted PostgreSQL transaction.
- **Reference behavior:** B exposes auto/manual commit, pending changes, Commit/Rollback and pending transactions. A exposes transaction mode and Commit/Rollback (Likely; not checked here). C undocumented.
- **Difference:** Session transaction state is hidden.
- **Impact:** Work may disappear or appear persisted when it is not.
- **Recommendation:** Detect/show open transactions; confirm close/move/reconfigure; make no unobserved rollback claims.
- **Decision:** Approved as recommended; show transaction-state icons such as autocommit, open transaction and pending changes, as reference tools do.
- **Project evidence:** `src/query.rs` rollback message; `src/app/workspace.rs`: `abandon_run`, `Drop`; `docs/reference/architecture.md` deviations.
- **Confidence:** Confirmed; documented B.

### N009 — Statement splitting ignores the dialect (Backlog: N010)

- **Priority:** High
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3; `f18136c`). Deviation: splitter uses driver `SqlDialect` but remains in application `jykre::sql`, recognizing blocks by keywords. See [Architecture deviations](../reference/architecture.md#known-deviations).
- **Feature:** Submitted SQL
- **Behavior at audit time:** Every semicolon outside quotes/comments/dollar quotes ends a statement; MySQL procedure/trigger `BEGIN … END` and PostgreSQL `BEGIN ATOMIC` split. No `DELIMITER`; leading `/*! … */` is discarded.
- **Reference behavior:** B has configurable delimiter and blank-line splitting (always/never/smart). A dialect-aware (Likely). C documents only semicolon splitting, like Jykre.
- **Difference:** Matches C but falls below A/B despite supporting three dialects.
- **Impact:** Routines reach the server fragmented; `Ctrl+Enter` selects fragments.
- **Recommendation:** Driver-provided dialect splitter with routines, `DELIMITER`, executable comments.
- **Project evidence:** `src/sql.rs`: `statements`; architecture deviations.
- **Confidence:** Confirmed; documented B.

### N010 — Format can alter MySQL strings and reset the caret (Backlog: N011)

- **Priority:** High
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3–4; `f18136c`, `66f262b`). Maintainer-selected `sqruff-lib` 0.44 formats off the UI thread using only layout and keyword-case rules.
- **Feature:** Formatting
- **Behavior at audit time:** Format's `tokens()` ignores string backslash escapes while `statements()` handles them, tokenizing `'a\'b'` differently. `replace_all` resets caret and scroll.
- **Reference behavior:** Principle: formatting never changes literals or loses the editing position.
- **Difference:** Disagreeing tokenizers plus whole-buffer replacement.
- **Impact:** Literal meaning may change; the editing position always moves.
- **Recommendation:** One dialect-aware tokenizer; restore caret, selection and scroll.
- **Decision:** Approved; evaluate a dedicated formatter dependency under [dependency policy](../standards/stack-and-dependencies.md), presenting the choice to the maintainer before adding it.
- **Project evidence:** `src/sql.rs`: `tokens` versus `statements`; workspace Format button.
- **Confidence:** Confirmed.

### N011 — Fixed `LIMIT 100` ignores the connection row limit

- **Priority:** Medium
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Explorer/pagination
- **Behavior at audit time:** `preview_sql` always produces `SELECT * FROM x LIMIT 100;`; configured limits (1–100,000, default 1,000) and paging have no effect beyond 100 rows.
- **Reference behavior:** A has configurable table page size/navigation. C defaults to 300 rows with adjustable limit/offset. B not checked.
- **Difference:** Embedded SQL limit defeats product pagination.
- **Impact:** Row 101 requires editing SQL; Row limit seems broken.
- **Recommendation:** Generate `SELECT * FROM x` and let row limits/paging bound volume.
- **Decision:** Initially retain `LIMIT` using the connection's row limit. November 5 update: applies only to **New query for table**; Data writes no `LIMIT` and pages through the session (N003).
- **Project evidence:** `src/backend.rs`: `preview_sql`; `docs/reference/results.md` limits.
- **Confidence:** Confirmed; documented A/C.

### N012 — `Ctrl+Enter` overwrites the result tab opened with `Ctrl+Shift+Enter`

- **Priority:** Medium
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Result tabs
- **Behavior at audit time:** `ReplaceLast` targets highest-numbered `last_created`, visible or not. A tab opened to retain a result is replaced first; with Result 1 visible, output may go to Result 3.
- **Reference behavior:** A reuses by default with explicit pinning; B offers new-tab shortcut and protects pinned tabs. C undocumented.
- **Difference:** Opening a new tab does not protect it; replacement ignores the visible tab.
- **Impact:** Retained results disappear while visible results remain stale.
- **Recommendation:** Replace active/last unpinned tab; explicit pinning prevents `Ctrl+Enter` replacement.
- **Project evidence:** `src/result_tabs.rs`: `begin`, `last_created`, test with `["c"], Placement::New` then `["d","e"], ReplaceLast`.
- **Confidence:** Confirmed; documented A/B.

### N013 — A cursor outside any statement executes a neighboring statement

- **Priority:** Medium
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Editor/execution
- **Behavior at audit time:** `statement_at` chooses the next statement between statements, or the nearest before the first/after the last, without asking.
- **Reference behavior:** A explicitly offers no execution, whole script or everything below the cursor; none is nearest-neighbor. B offers configurable blank-line delimiters. C undocumented.
- **Difference:** Implicit heuristic instead of a predictable configurable rule.
- **Impact:** `Ctrl+Enter` on blank lines can execute a destructive statement below.
- **Recommendation:** Do nothing and notify, or highlight/confirm a candidate; optionally configurable.
- **Decision:** Do not execute outside a statement; notify, with no confirmation/configuration.
- **Project evidence:** `src/sql.rs`: `statement_at`; `src/query.rs`: `Scope::Cursor`.
- **Confidence:** Confirmed; documented A/B, A's default undocumented.

### N014 — Grid filtering silently searches only the loaded page

- **Priority:** Medium
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Grid
- **Behavior at audit time:** “Filter results…” checks only current-page rows up to the limit. A match on another page still produces “No results for this selection. The query or the current filter returned no rows.”
- **Reference behavior:** B SQL criteria implicitly form `WHERE`, applying to the query. A query-level filter (Likely). C not checked.
- **Difference:** Page-local search appears global.
- **Impact:** “Not found” is misleading when other pages exist or limits were reached.
- **Recommendation:** Show scope, e.g. “this page: 1,000 rows”, and explain excluded pages.
- **Project evidence:** `src/app/result_grid.rs`: `set_filter`; `src/grid.rs`: `GridView::new`.
- **Confidence:** Confirmed in code; reference Likely.

### N015 — Switching result tabs loses selection and inherits another tab's scroll

- **Priority:** Medium
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Grid/result tabs
- **Behavior at audit time:** One `ResultGrid` serves all tabs. `set_page` clears selection; a shared `UniformListScrollHandle` transfers scroll offsets.
- **Reference behavior:** B gives each result tab its own data editor; A/C undocumented.
- **Difference:** Grid state belongs to the component rather than the tab.
- **Impact:** Comparing results by switching tabs destroys selections and jumps position.
- **Recommendation:** Save/restore selection and scroll by `ResultId`.
- **Project evidence:** `src/app/result_grid.rs`: `set_page`, `scroll`; workspace `show_active`.
- **Confidence:** Selection confirmed; scroll Likely.

### N016 — Messages loses a DML outcome when its result tab is replaced

- **Priority:** Medium
- **Approval:** Approved (November 3, 2026)
- **Status:** Implemented (November 3, 2026; `f18136c`).
- **Feature:** Output/messages
- **Behavior at audit time:** Messages lists existing tabs' status/SQL without timestamps/duration. Replacement by `Ctrl+Enter`/`Ctrl+Alt+Enter` removes the previous UPDATE's “5 rows affected”; history also omits affected rows.
- **Reference behavior:** B logs time, duration, affected rows/errors independently of tabs. A keeps console execution output (Likely); C undocumented.
- **Difference:** Execution evidence follows result-tab lifetime.
- **Impact:** Proof of modifications disappears precisely for statements returning no rows.
- **Recommendation:** A per-query-tab session log with time, statement summary, rows, duration/error, independent of result tabs.
- **Project evidence:** workspace `result_messages`; `src/result_tabs.rs`: `begin`.
- **Confidence:** Confirmed; documented B.

### N017 — The editor path does not show the actual session context

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Session-reported schema/database/catalog selection after each run already came from `ed9173a`. This removes invented segments: disconnected headers show only the connection-tab name, without “— / SQL”; the editor footer shows SQL without an unknown dialect. Demo retains its example database.
- **Feature:** Connection context
- **Behavior at audit time:** Header “connection / database / SQL” uses profile `options.database`/`defaultCatalog` or “—”, initially demo name heuristics such as commerce. SQL is fixed text. `USE`, `SET search_path`, `USE CATALOG` never update it although tab sessions retain context.
- **Reference behavior:** A/B expose active database/schema selectors; B has a shortcut. C undocumented. Whether typed `USE` updates them is undocumented in all three.
- **Difference:** Context indicator is static and sometimes invented.
- **Impact:** Users rely on an incorrect label for DML destinations.
- **Recommendation:** Display driver/session context after runs, or remove unverified segments.
- **Project evidence:** workspace `editor_panel`, `Workspace::new` database, `set_connection`.
- **Confidence:** Confirmed; reference tracking of `USE` needs validation.

### N018 — Format ignores the selection and reformats the whole script

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). `sql::format_range` formats only selections, otherwise the whole script; the tooltip explains this.
- **Feature:** Formatting
- **Behavior at audit time:** Format always uses all `editor.text()`, ignoring `selected_range`.
- **Reference behavior:** A offers selection formatting; B instructs selecting SQL before formatting; C undocumented.
- **Difference:** No formatting scope.
- **Impact:** Formatting one query changes a long script, worsened by N019/N010.
- **Recommendation:** Format only the selection when present.
- **Project evidence:** workspace Format button.
- **Confidence:** Confirmed; documented A/B.

### N019 — Format removes blank lines and joins DML statements onto one line

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). `sql::format` formats statements independently using `sql::statements`, preserving intervening blank lines, comments (including post-semicolon line comments) and `DELIMITER` lines. Every statement starts separately; multiline sqruff output may push later statements down. Unformattable statements remain unchanged with a partial-format warning.
- **Feature:** Formatting
- **Behavior at audit time:** Token reconstruction breaks only before SELECT/FROM/WHERE/etc.; `INSERT …;\n\nDELETE …;` becomes one line and end-of-line comments move down.
- **Reference behavior:** A offers preserving original line breaks; B/C undocumented.
- **Difference:** Destructive inter-statement layout changes.
- **Impact:** Unreadable scripts, moved gutter buttons and changed `Ctrl+Enter` positions.
- **Recommendation:** Format each statement independently; preserve intervening blank lines/comments.
- **Project evidence:** `src/sql.rs`: `format`.
- **Confidence:** Confirmed; documented A.

### N020 — Reopened saved queries lose their library association

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Tabs open with `saved_id`/`saved_sql`; existing editing tabs are focused instead of duplicated. Association persists in `workspace.state` across restart.
- **Feature:** Library
- **Behavior at audit time:** Library clicks call `add_query` with `saved_sql = None`; the icon says Save query and closing edited SQL warns it is not saved.
- **Reference behavior:** Principle: a tab identifies its origin and saved state. A scripts are files and editing changes the script (Likely).
- **Difference:** The tab loses its origin.
- **Impact:** False saved-state display and uncertainty over editing originals versus copies.
- **Recommendation:** Populate `saved_sql` and associate by query identity.
- **Project evidence:** `src/app/mod.rs`: `sidebar_content` → `add_query`; workspace `saved_sql`.
- **Confidence:** Confirmed.

### N021 — Clicking Save query again removes the saved query

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Save only saves/updates, saying “This query is already saved.” when unchanged. Library-row deletion asks confirmation; associated tabs become drafts and prompt on closing. Message now says “Query saved in the library.”
- **Feature:** Library
- **Behavior at audit time:** Bookmark clicks remove unchanged saved SQL without confirmation/undo; first save says “Query saved in this session.” despite durable `workspace.state` storage.
- **Reference behavior:** Principle: saving is idempotent; deletion is separate.
- **Difference:** One control saves and deletes.
- **Impact:** Double-save deletes work; persistence wording misleads.
- **Recommendation:** Save/update only; separate list deletion with confirmation or undo; correct the message.
- **Project evidence:** workspace toolbar `save-query`; `src/app/mod.rs`: `WorkspaceEvent::Saved`.
- **Confidence:** Confirmed.

### N022 — Saved queries collide by title (Backlog: N018)

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Stable numeric IDs in `workspace.state`, with `next_id`; titles are labels. Older entries receive IDs and tabs associate through title/connection/SQL as before. This changes JSON in settings, not SQLite schema.
- **Feature:** Library
- **Behavior at audit time:** `(title, connection)` identity plus `retain` removes same-name entries. Query 3/table-name titles mean two orders tabs on one connection overwrite each other.
- **Reference behavior:** Principle: stable identity independent of display name.
- **Difference:** Display name is the key.
- **Impact:** Silent durable loss of saved queries.
- **Recommendation:** Stable IDs; names are labels only.
- **Project evidence:** `WorkspaceEvent::Saved` and `retain` in `src/app/mod.rs`; `QueryEntry` in `src/workspace_state.rs`.
- **Confidence:** Confirmed.

### N023 — History shows tab titles instead of SQL, without time or status

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Rows show SQL prefix, status icon, connection, relative time, duration/outcome; search covers SQL/title/connection. Library/history lists are virtualized. Deviation: relative time (“5 min ago”), as in Messages; local-clock formatting needs an approved date-library dependency.
- **Feature:** History
- **Behavior at audit time:** Entries show only source title/connection; every Query 2 execution looks identical, without time, duration, status or search.
- **Reference behavior:** All show SQL. A offers search/delete and editor insertion; B records time/duration/rows/error; C keeps per-connection SQL and inserts into a new editor tab.
- **Difference:** Identifies the tab, not the execution.
- **Impact:** Finding SQL requires opening entries individually.
- **Recommendation:** SQL prefix, timestamp/status and search.
- **Project evidence:** `sidebar_content`; `src/workspace_state.rs`: `record`.
- **Confidence:** Confirmed; documented A/B/C.

### N024 — History omits failed executions (Backlog: N035)

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Every finished statement records success/failure/cancellation, outcome/duration through the same path as Messages. Toolbar COMMIT/ROLLBACK remains Messages-only.
- **Feature:** History
- **Behavior at audit time:** Only completed-statement `WorkspaceEvent::Executed` reaches history; failed statements never do.
- **Reference behavior:** A stores all console executions; B includes errors; C does not specify failures.
- **Difference:** History retains only successes.
- **Impact:** SQL needing correction disappears.
- **Recommendation:** Record every attempt and its status.
- **Project evidence:** workspace `execute_queries`, event emission within the `completed` loop.
- **Confidence:** Confirmed; documented A/B.

### N025 — Palette items and tab titles omit the schema

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Palette shows a muted object path after the name. Table tabs use `schema.table`, or MySQL `database.table`, through `explorer::qualified_title`.
- **Feature:** Navigation
- **Behavior at audit time:** Palette shows object/connection names; tabs show only table names. Same-name orders tables in different schemas look identical.
- **Reference behavior:** Principle: distinguish homonymous objects at selection points.
- **Difference:** `ObjectPath` exists but is hidden.
- **Impact:** Wrong tables open and tabs are indistinguishable.
- **Recommendation:** Show qualified names or muted full paths in palette and tooltip/title.
- **Project evidence:** `command_palette`; `open_relation` uses final segment for title; `search_cache`.
- **Confidence:** Confirmed.

### N026 — Explain does not execute EXPLAIN (Backlog: N024)

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Actual `EXPLAIN <statement>` runs on selection/cursor in a new result tab through connection `SqlDialect`/`sql::explain`, without driver/contract changes. Only SELECT/WITH/VALUES/TABLE/INSERT/UPDATE/DELETE/MERGE/REPLACE are supported, never ANALYZE, to avoid execution. Disconnected clicks explain a server is required. The mock assistant is removed from this action. `docs/standards/security.md` now names this explicit generated SQL action; maintainer review still applies.
- **Feature:** Explain
- **Behavior at audit time:** Calls mock `ask("Explain this query")`, returning fixed sales-example text even for real connections.
- **Reference behavior:** Principle: display the real database plan. A/B actual plans are Likely, not checked here.
- **Difference:** Real toolbar action has a simulated outcome.
- **Impact:** Promises a nonexistent plan.
- **Recommendation:** Run dialect EXPLAIN, or remove/rename while simulated.
- **Project evidence:** workspace `explain`, `ask`.
- **Confidence:** Confirmed.

### N027 — “Insert into editor” replaces all SQL (Backlog: N025)

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). “Insert at the cursor” inserts at caret/replaces selection, supports Ctrl+Z and preserves other SQL.
- **Feature:** Assistant
- **Behavior at audit time:** Unconfirmed `replace_all`.
- **Reference behavior:** Principle: insertion adds at caret/selection, not whole-buffer replacement.
- **Difference:** Label promises insertion; action replaces.
- **Impact:** Overwrites unsaved SQL.
- **Recommendation:** Insert at caret, or label Replace editor and confirm.
- **Project evidence:** workspace `apply-ai`.
- **Confidence:** Confirmed.

### N028 — The connection test cannot be cancelled through its button (Backlog: N029)

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Test button becomes **Cancel test**, cancels and shows elapsed time after 2 s.
- **Feature:** Connection dialog
- **Behavior at audit time:** Inert “Testing…” without elapsed time; cancellation requires field editing/dialog closure. Databricks can take 90 s.
- **Reference behavior:** A can cancel connection opening. Jykre UX requires time/cancellation above 2 s (`docs/reference/ux.md`).
- **Difference:** Run becomes Cancel; Test does not.
- **Impact:** Users remain waiting.
- **Recommendation:** Cancel test plus elapsed time after 2 s.
- **Project evidence:** `test_connection`, footer `test-connection` in `src/app/connections.rs`.
- **Confidence:** Confirmed; documented A.

### N029 — Connection saves write profile and preferences separately (Backlog: N030)

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Profile, row limit, metadata selection and revalidation hours use one `ConfigRepository::save_with_settings` transaction, a new storage API without schema change. Vault writes still precede it and cannot participate; failed transactions remove a new connection's password as before.
- **Feature:** Persistence
- **Behavior at audit time:** `repository.save` increments revision before `set_row_limit` and `set_metadata_scope`. Later failure reports an error despite modified profile; retry has stale revision.
- **Reference behavior:** Principle: atomic configuration save.
- **Difference:** Three writes, one feedback result.
- **Impact:** Partial state, misleading failure and likely retry conflict.
- **Recommendation:** One transaction.
- **Project evidence:** `src/connection_store.rs`: `save_to`.
- **Confidence:** Confirmed.

### N030 — Gutter status survives whitespace changes inside literals (Backlog: N032)

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). `normalized_statements` collapses only inter-token whitespace; literals, quoted identifiers and comments remain unchanged.
- **Feature:** Gutter
- **Behavior at audit time:** Whole-text `split_whitespace` treats `SELECT 'a b'` and `SELECT 'a  b'` identically.
- **Reference behavior:** Principle: invalidate indicators when executable text changes.
- **Difference:** Layout normalization changes literals.
- **Impact:** Checkmarks appear on never-executed statements.
- **Recommendation:** Collapse only inter-token whitespace.
- **Project evidence:** `src/sql.rs`: `normalized_statements`; `src/query.rs`: `sync`.
- **Confidence:** Confirmed.

### N031 — Large grid copies run on the UI thread (Backlog: N033)

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 7; 0.2.0 M2: grid/query-page scope in `fa97819`, then the value inspector's **Copy** and report-panel **Copy as CSV** / **Copy for a spreadsheet**). `copy_as::CopyRequest` shares page/display order through `Arc`, estimates size and generates all 11 formats, a single whole value (`CopySource::Value`) and report-panel copies off-thread; `src/app/copying.rs` runs every copy for any view implementing `CopyHost`. Confirmation above 50 MiB uses the original snapshot. After 100 ms show progress/cancellation via button or Esc (report panels in their notice line); each phase has a 60 s limit. Cancelled, page-switched, result-replaced or superseded requests never publish. The inspector copies the whole stored value, never its 1 MB display. Only handing complete text to the OS clipboard stays on the UI thread.
- **Feature:** Clipboard
- **Current recorded behavior:** Copy this page as CSV, Ctrl+C/Ctrl+Shift+C, grid menu formats, the value inspector's Copy and report-panel copy menus estimate/generate on the blocking pool; only complete-text delivery to the OS clipboard stays on UI.
- **Reference behavior:** Principle/Jykre rule: confirm above 50 MiB and copy off-thread.
- **Difference:** Resolved for every result copy.
- **Impact:** None remaining for result copies.
- **Recommendation:** Estimate, confirm above 50 MiB, generate off-thread.
- **Project evidence:** `src/copy_as/job.rs`; `src/app/copying.rs` (`start`, `encode`, `CopyHost`); grid `copy`, `copy_value`, `copy_page_csv`; report panel `copy`.
- **Confidence:** Confirmed.

### N032 — Long execution does not show elapsed time (Backlog: N036)

- **Priority:** Medium
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Footer shows “Running… 12.4 s” after 2 s, tooltip names statement timeout. After cancellation exceeds 5 s, “Still cancelling…” reports wait/next action. `QueryRun::running_for`/`cancelling_for` drive one-second footer updates only during runs; accessibility announces state, not clock ticks.
- **Feature:** Progress
- **Behavior at audit time:** Running/Cancelling text omits `RunState::Running { started_at }`. Per-tab timeout may be up to 1 h.
- **Reference behavior:** A reports progress/stalled cancellation; Jykre requires elapsed time beyond 2 s.
- **Difference:** State contains a hidden clock.
- **Impact:** Remaining timeout/cancellation stalls are unclear.
- **Recommendation:** Time after 2 s and stalled-cancellation warning.
- **Project evidence:** workspace `result_footer`; `src/query.rs`.
- **Confidence:** Confirmed; documented A.

### N033 — The palette shows old results while a new search runs (Backlog: N034)

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Pending searches show a header spinner/Searching… and dim old items until new results arrive.
- **Feature:** Search
- **Behavior at audit time:** Searching… appears only for empty lists; existing matches hide pending state.
- **Reference behavior:** Principle: pending state must be visible.
- **Difference:** Old results appear to match new input.
- **Impact:** Enter can open a stale match.
- **Recommendation:** Pending indicator even with existing results.
- **Project evidence:** connections `search_cache`; `src/app/mod.rs`: `command_palette`.
- **Confidence:** Confirmed.

### N034 — Opening the same object again creates a duplicate tab

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Tabs retain their object identity, including in `workspace.state`; explorer/palette reopen focuses the same table/connection tab. Node menus offer **New query for table in another tab** explicitly.
- **Feature:** Tabs/navigation
- **Behavior at audit time:** Each table double-click/palette entry creates another same-title tab.
- **Reference behavior:** C has explicit open-in-new-tab, implying normal opening reuses; A reuse is Likely; B undocumented.
- **Difference:** No reuse.
- **Impact:** Identical tabs clutter the bar and contribute to N022.
- **Recommendation:** Focus existing object tabs, with an explicit open-another action.
- **Project evidence:** `add_query`; `open_relation` in app connections.
- **Confidence:** Confirmed; reference Likely.

### N035 — Footer row counts ignore the filter

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). With filtering, show “3 of 1,000 rows” or “3 of rows 1,001–2,000” for multi-page results using `ResultGridEvent::RowsShown`/`result_tabs::rows_label`.
- **Feature:** Grid/footer
- **Behavior at audit time:** Footer says 1,000 rows while only 3 appear.
- **Reference behavior:** Principle: footer/grid counts agree.
- **Difference:** Conflicting counts.
- **Impact:** Match count is unclear.
- **Recommendation:** “3 of 1,000 rows” while filtered.
- **Project evidence:** workspace `page_summary`, `page.data.row_count()`.
- **Confidence:** Confirmed.

### N036 — Grid navigation lacks Home, End, PageUp, PageDown and Ctrl+Home/End

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Home/End navigate the row, Ctrl+Home/End the page, PageUp/PageDown a screen of rows; Shift extends via `CellSelection::jumped`. Deviation: keys stay in grid `on_key_down`, like arrows, instead of keymap actions; documented in `docs/standards/ui-guidelines.md`.
- **Feature:** Grid/keyboard
- **Behavior at audit time:** `key_down` handles only arrows, Shift+arrows, Ctrl+A/C and Esc.
- **Reference behavior:** Grid/spreadsheet conventions for page/extreme navigation; not checked in tool documentation.
- **Difference:** Incomplete keyboard navigation.
- **Impact:** Traversing 1,000 rows requires holding an arrow.
- **Recommendation:** Row Home/End, grid Ctrl+Home/End, PageUp/PageDown, Shift extension.
- **Project evidence:** grid `key_down`.
- **Confidence:** Confirmed; reference Likely.

### N037 — Keyboard tab switching requires focus on the tab bar

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Keymap `NextTab`/`PreviousTab` use Ctrl+Tab/Shift+Tab and Ctrl+PageDown/PageUp anywhere, including the editor; focus moves to the chosen editor.
- **Feature:** Tabs/keyboard
- **Behavior at audit time:** Arrows/Home/End switch only from the focused tab bar; no global next/previous shortcut.
- **Reference behavior:** Tabbed-editor convention, not checked against the three tools.
- **Difference:** Switching requires multiple focus-navigation steps.
- **Impact:** Slow keyboard-only use.
- **Recommendation:** Global next/previous actions in the keymap.
- **Project evidence:** grouped-tabs `navigate`; app `capture_key_down`.
- **Confidence:** Confirmed; reference Likely.

### N038 — Middle-click does not close tabs

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Middle-click follows X's `CloseRequested` path and confirmations.
- **Feature:** Tabs
- **Behavior at audit time:** Only X or Ctrl+W/Ctrl+F4 closes.
- **Reference behavior:** IDE/browser tab convention, not checked against tool documentation.
- **Difference:** Missing standard closing gesture.
- **Impact:** Users must aim at X, only visible on hover for inactive tabs.
- **Recommendation:** Middle-click invokes `CloseRequested` with identical confirmations.
- **Project evidence:** `src/grouped_tabs/mod.rs`, no `MouseButton::Middle` handler at audited snapshot.
- **Confidence:** Confirmed; reference Likely.

### N039 — New queries are prefilled with `SELECT 1;`

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). New queries start empty with “Write SQL here. Ctrl+Enter runs the statement at the cursor.”, connected or disconnected.
- **Feature:** Tabs
- **Behavior at audit time:** `new_query` fills `SELECT 1;`, or `SELECT * FROM orders LIMIT 100;` when disconnected.
- **Reference behavior:** New editors start empty, Likely and not documentation-verified.
- **Difference:** Content must be deleted before typing.
- **Impact:** Every new query begins with selection/deletion.
- **Recommendation:** Empty editor plus placeholder.
- **Project evidence:** app `new_query`.
- **Confidence:** Confirmed; reference Likely.

### N040 — “Load more objects” is far from its node, at the connection's end

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Already implemented; verified November 4 without changes. Since `aca08bc`, `Tree::push_children` places Load more objects as its node's final child row at the same indentation.
- **Feature:** Explorer
- **Behavior at audit time:** All nodes' continuation buttons collect at the connection tree's end, identified only by parent name.
- **Reference behavior:** Principle: continuation belongs next to its list.
- **Difference:** Control detached from the extended list.
- **Impact:** Long schemas hide incompleteness and the button is hard to find.
- **Recommendation:** Render as the corresponding node's last child.
- **Project evidence:** connections `live_connection_tree`, `metadata_more`.
- **Confidence:** Confirmed.

### N041 — Single-click behavior differs by node type

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Click selects; double-click/Enter opens tables or toggles other nodes. All parents, including databases/schemas/connections, have chevrons; left/right arrows also toggle connections.
- **Feature:** Explorer
- **Behavior at audit time:** Database/schema click selects and toggles; table click selects only, columns use a chevron; connection click toggles without selecting.
- **Reference behavior:** Internal consistency. B configures double-click opening/expansion; none documents three simultaneous single-click rules.
- **Difference:** Three rules in one tree.
- **Impact:** Selecting a schema for its context menu collapses it.
- **Recommendation:** Click selects; chevron/double-click/arrows expand.
- **Project evidence:** connections node `on_click`, `expandable`, connection handlers.
- **Confidence:** Confirmed.

### N042 — Execution duration is always displayed in milliseconds

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). `cell_format::duration` uses ms below 1 s, locale-decimal seconds with one digit, min/s, h/min; used in footer, Messages, history and connection tests.
- **Feature:** Footer
- **Behavior at audit time:** `format!("{} ms", elapsed_ms)` displays a three-minute run as “183421 ms”.
- **Reference behavior:** Readability principle: use the appropriate duration scale.
- **Difference:** No unit scaling.
- **Impact:** Long durations are hard to read.
- **Recommendation:** Seconds/minutes/hours above 1 s.
- **Project evidence:** workspace `result_footer`.
- **Confidence:** Confirmed.

### N043 — Empty tab groups disappear and return at the bar's end (Backlog: N037)

- **Priority:** Low
- **Approval:** Rejected (November 4, 2026)
- **Status:** Discarded. Removing groups after the last tab and recreating them at the end is expected behavior; no code change.
- **Feature:** Tab groups
- **Behavior at audit time:** `model.close` removes empty groups; `ensure_group` recreates them at the end.
- **Reference behavior:** Principle: stable UI positions.
- **Difference:** Order follows tab history.
- **Impact:** Group positions become unpredictable.
- **Recommendation:** Keep group positions or consistently hide empty groups. This recommendation was rejected as recorded above.
- **Project evidence:** `src/grouped_tabs/model.rs`: `close`; app `ensure_group`.
- **Confidence:** Confirmed.

### N044 — Explorer rebuilds every visible node each render (Backlog: N038)

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Already implemented; verified November 4 without changes. Since `aca08bc`, a flattened virtual list (`ListState`, `explorer_entries`) rebuilds only on tree/expansion/search changes. Performance no longer lists this deviation. Not measured in this change.
- **Feature:** Explorer/performance
- **Behavior at audit time:** `.children(objects…)` over all visible items, with per-node `format!` IDs; formerly a Performance deviation.
- **Reference behavior:** Jykre requires virtualized long collections.
- **Difference:** Grid virtualized, tree not.
- **Impact:** Focus/search/refresh degrade in large catalogs.
- **Recommendation:** Flattened virtualized list.
- **Project evidence:** connections `live_connection_tree`.
- **Confidence:** Confirmed, not measured.

### N045 — Font size can be persisted out of order (Backlog: N039)

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). 300 ms debounce plus generation: each write gets a `connection_store::FONT_SIZE_WRITES` ticket, writes serialize and newer requests supersede older ones.
- **Feature:** Preferences
- **Behavior at audit time:** Each Ctrl+/− sends a generation-less write; errors appear in the Explorer notice.
- **Reference behavior:** Principle: last change wins.
- **Difference:** Persistence is not monotonic.
- **Impact:** UI shows 18 px but restarts at 16.
- **Recommendation:** Debounce or generations.
- **Project evidence:** connections `persist_font_size`.
- **Confidence:** Confirmed.

### N046 — Demo sorting only reverses row order (Backlog: N040)

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). Real sorting for all results: header cycles ascending/descending/server order through `grid::ColumnSort`. Compare numbers exactly as decimal text, including exponents (`1.5e-05` from float8), never floating point; NULLs last, equal values preserve server order. Deviation: current-page sorting runs on UI during `GridView` reconstruction. Release benchmark about 5 ms for 100,000 already sorted rows, within 50 ms budget; not measured in-app.
- **Feature:** Grid/demo
- **Behavior at audit time:** Third demo column toggles `descending`/`reverse`; real headers never sort.
- **Reference behavior:** Principle: a header sorting arrow orders column values.
- **Difference:** Sorting affordance merely reverses.
- **Impact:** Demo teaches a gesture absent in real use.
- **Recommendation:** Remove the arrow or implement actual sorting.
- **Project evidence:** grid header `index == 2 && self.sales`.
- **Confidence:** Confirmed.

### N047 — Vault writes have no timeout and can trap the dialog (Backlog: N041)

- **Priority:** Low
- **Approval:** Approved (November 4, 2026)
- **Status:** Implemented (November 4; uncommitted local changes). All `connection_store` vault reads/writes/deletes have 10 s `VAULT_TIMEOUT`. On expiry treat the vault as unavailable: password remains session-only, Remember password is saved off with a warning and dialog exits Saving. Recorded in Reliability timeouts.
- **Feature:** Vault/dialog
- **Behavior at audit time:** Dialog cannot close during `connection_saving`; `secrets.set` has no timeout.
- **Reference behavior:** Principle/Jykre rule: external operations have timeouts.
- **Difference:** Unlimited waiting.
- **Impact:** A stuck keyring traps the modal.
- **Recommendation:** Timeout plus warning.
- **Project evidence:** connections `close_connection_dialog`; `crates/secrets/src/lib.rs`.
- **Confidence:** Likely; OS backend-dependent.

## 3. Priority table

Reference letters identify documentation supporting the behavior. P means a principle/project rule; ~ means a likely, unverified reference. This table records the audit's findings; implementation updates above explain current resolution.

| ID | Feature | Divergence | Impact | Reference | Priority | Approval |
| --- | --- | --- | --- | --- | --- | --- |
| N001 | Execution | Run shortcut cancels | Accidental long-DML cancellation | A, C | High | Approved · Implemented |
| N002 | Results | Rerun uses cursor | May execute a different statement | P | High | Approved · Implemented |
| N003 | Explorer | Opening table shows no data | Extra inspection step | A, B, C | High | Approved · Implemented |
| N004 | Authentication | Unsaved password not requested | Failure/profile edit each session | A, B | High | Approved · Implemented |
| N005 | Connections | Save destroys runs/results | Loss of ongoing work | P | High | Approved · Implemented |
| N006 | Library/history | Falls back to first connection | SQL on wrong database | A, C | High | Approved · Implemented |
| N007 | Execution | Disconnected tab runs demo | Fabricated data during real use | P | High | Approved · Implemented |
| N008 | Transactions | Invisible transaction | Loss or illusory persistence | B, A~ | High | Approved · Implemented |
| N009 | SQL | Dialect-agnostic splitter | Fragmented routines | B, A~ | High | Approved · Implemented |
| N010 | Formatting | MySQL literals/caret | Meaning/position changes | P | High | Approved · Implemented |
| N011 | Explorer | Fixed LIMIT 100 | Ineffective pagination | A, C | Medium | Approved · Implemented |
| N012 | Result tabs | Overwrites retained tab | Lost result | A, B | Medium | Approved · Implemented |
| N013 | Editor | Outside cursor executes neighbor | Unexpected execution | A, B | Medium | Approved · Implemented |
| N014 | Grid | Page-only filter | False not-found | B~, A~ | Medium | Approved · Implemented |
| N015 | Grid | Selection/scroll not per tab | Difficult comparison | B | Medium | Approved · Implemented |
| N016 | Messages | No execution log | DML outcome disappears | B, A~ | Medium | Approved · Implemented |
| N017 | Context | Static/invented path | Incorrect label | A, B | Medium | Approved · Implemented |
| N018 | Formatting | Ignores selection | Whole script changed | A, B | Medium | Approved · Implemented |
| N019 | Formatting | Joins statements | Unreadable script | A | Medium | Approved · Implemented |
| N020 | Library | Reopened query detached | False saved state | P | Medium | Approved · Implemented |
| N021 | Library | Save deletes on second click | Accidental deletion | P | Medium | Approved · Implemented |
| N022 | Library | Title collision | Silent overwrite | P | Medium | Approved · Implemented |
| N023 | History | Title instead of SQL | Hard to find queries | A, B, C | Medium | Approved · Implemented |
| N024 | History | Failures absent | Correction SQL lost | A, B | Medium | Approved · Implemented |
| N025 | Navigation | Missing schema | Wrong table | P | Medium | Approved · Implemented |
| N026 | Explain | Simulated | False promise | P, A~, B~ | Medium | Approved · Implemented |
| N027 | Assistant | Insertion replaces | SQL overwritten | P | Medium | Approved · Implemented |
| N028 | Connection | No test cancellation | User trapped waiting | A, P | Medium | Approved · Implemented |
| N029 | Persistence | Non-atomic save | Partial state | P | Medium | Approved · Implemented |
| N030 | Gutter | Whitespace in literals | False checkmark | P | Medium | Approved · Implemented |
| N031 | Clipboard | Large copy on UI | Freeze | P | Medium | Approved · Implemented |
| N032 | Progress | No elapsed time | Invisible timeout | A, P | Medium | Approved · Implemented |
| N033 | Search | Hidden pending state | Stale match opens | P | Low | Approved · Implemented |
| N034 | Tabs | Duplicates | Cluttered bar | C, A~ | Low | Approved · Implemented |
| N035 | Footer | Filter ignored in count | Disagreeing numbers | P | Low | Approved · Implemented |
| N036 | Grid | Missing navigation keys | Slow navigation | ~ | Low | Approved · Implemented |
| N037 | Tabs | Switch requires tab-bar focus | Slow keyboard workflow | ~ | Low | Approved · Implemented |
| N038 | Tabs | No middle-click | Precise close-button aiming | ~ | Low | Approved · Implemented |
| N039 | Tabs | Prefilled queries | Friction | ~ | Low | Approved · Implemented |
| N040 | Explorer | Load more far from node | Incomplete list unnoticed | P | Low | Approved · Implemented |
| N041 | Explorer | Inconsistent clicks | Unwanted collapse | P | Low | Approved · Implemented |
| N042 | Footer | Milliseconds only | Poor readability | P | Low | Approved · Implemented |
| N043 | Groups | Reappears at end | Unstable order | P | Low | Rejected |
| N044 | Explorer | Nonvirtual tree | Poor scalability | P | Low | Approved · Implemented |
| N045 | Preferences | Out-of-order font writes | Wrong size on restart | P | Low | Approved · Implemented |
| N046 | Demo | Reverse instead of sort | Misleading gesture | P | Low | Approved · Implemented |
| N047 | Vault | No timeout | Trapped modal | P | Low | Approved · Implemented |

## 4. Stronger project behaviors to preserve

- **Cancellation blocks new runs:** Cancelling prevents another run in the same tab; Stopping… avoids competing sessions. B's documentation warns parallel execution may deadlock.
- **Open-cursor pagination without rerunning:** later pages never repeat expensive/nondeterministic SQL; closed cursors are disclosed.
- **Per-tab sessions with honest notices:** reopening/closing explains reset schema/temporary-object context.
- **Complete server errors:** classified title, original message, SQLSTATE, position, Copy details and Go to the position without selecting SQL that the next keystroke could erase.
- **Moving tabs between connections asks first and preserves SQL.**
- **Closing SQL outside the library asks first;** workspace restores across sessions.
- **Passwords never migrate between servers:** `keeps_password` retains an empty-entered password only for matching driver/host/port/user.
- **Faithful values:** NULL distinct from empty/spaces (`(empty)`, `␠ N spaces`), exact decimals, no timezone conversion, line breaks as ↵.
- **User-chosen introspection:** atomic publication and refresh queues absorb covered nodes.
- **Keyboard explorer/palette:** click selects, double-click/Enter opens, arrows navigate.
- **Right-click outside selection first selects the cell:** context actions target the pointer location.
- **Per-tab timeout visible in the toolbar.**
- **Per-statement gutter follows the server:** Pending → Running → Success/Error.

## 5. Main cross-cutting inconsistencies

1. **Controls reverse their action:** run cancels (N001), save removes (N021); repetition undoes intent.
2. **Name-based identity:** connection names/query titles cause N006/N007/N020/N022/N034. Stable `ConnectionId`/query IDs address the family.
3. **Results/context detached from origin:** stored `tab.sql` ignored (N002), newest rather than visible replacement (N012), outcome lost with tabs (N016), context not tracking sessions (N017).
4. **Demo code in real paths:** disconnected demo (N007), simulated Explain/assistant (N026/N027), heuristic header database (N017), demo-only Chart.
5. **State changes without dialogue:** saving cancels (N005), closure rolls back (N008), absent password errors (N004), replacement/failures lose history (N016/N024).
6. **Shortcuts outside the keymap:** manual `capture_key_down`/`on_key_down` handling, documented in UI guidelines, explains incomplete navigation (N036/N037) and missing cancel shortcut (N001).

## 6. Recommended correction order

This orders all findings; only **Approved** items enter development. Pending items retain their place until approved.

1. **Execution safety: N001, N002, N013, N007, N006.** Wrong operations/destinations; small fixes with immediate effect.
2. **Connection/session: N004, N005, N008.** Password prompts avoid forced saves, saves preserve work and visible transactions address silent loss.
3. **Results as independent objects:** N012 pinning, N016 logs, N015 per-tab state, N014/N035 filter scope.
4. **Actual table opening:** N003/N011 with N025/N034 navigation.
5. **Stable library/history identity:** N022/N020/N021/N023/N024, alongside inconsistency 2.
6. **Submitted/formatted SQL:** N009/N010/N018/N019/N030. Dialect splitting comes before formatting and shares the tokenizer.
7. **Feedback/polish:** N017, N026–N029, N031–N033, N036–N047. Rejected N043 remains excluded from development.

## Limitations

- The application was not run. Project-side Likely findings (N015 scroll, N047) depend on GPUI/OS runtime behavior.
- Tool documentation coverage differs; C is shortest and appears less often. Undocumented does not mean different behavior.
- Reference ~ or Likely statements were not verified in the consulted documentation and need validation before guiding product decisions.
