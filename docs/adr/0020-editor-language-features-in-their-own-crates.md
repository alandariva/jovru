# ADR-0020: Editor language features in their own crates, apart from query execution

- **Status:** Accepted (by the maintainer on 2026-11-07; implemented)
- **Date:** 2026-11-04
- **Authors:** AI agent, at the maintainer's request (plan approved 2026-11-04)

## Context

The SQL editor's highlighting, completion, hover, go to definition and
Format grew inside code that query execution also depends on:

- `jykre::sql` held the tokenizer and statement splitting (which every run
  uses) together with Format (sqruff-lib) and the demo's preview check.
- `jykre::completion` borrowed the name helpers of `jykre::ddl`, and
  `backend::name_index` built the editor's index inside the backend.
- The `Workspace` view held the completion caches, the provider methods, the
  highlighter set-up and the whole Format flow.

The [editor investigation](../research/sql-editor-investigation.md) plans more of
these features: dialect catalogs, a model of each statement's queries for
completion and semantic highlighting. Several may be replaced (a formatter,
that model) or dropped; tree-sitter was evaluated for the model and not
adopted (section G.4 of the investigation). With the features tangled in execution code and in the workspace,
removing one would touch what runs queries, and every new engine would be
compiled into the library.

## Decision

1. **`crates/sql` (`jykre-sql`)** holds what query execution and the editor
   share:
   - the tokenizer by dialect;
   - statement splitting, `statement_at`, `statement_lines` and
     normalisation;
   - `explain`;
   - `map_offset`;
   - the qualified-name helpers (`names`).

   It depends only on `jykre-core`. The library re-exports it as
   `jykre::sql`.
2. **`crates/sql-language` (`jykre-sql-language`)** holds the editor's
   language features: completion and hover analysis (`completion`, with
   the queries and relations of each statement in `completion::scopes`)
   and Format (`format`). It is pure: no I/O, no GPUI, no `lsp-types` and no
   translations. It depends only on `jykre-core` and `jykre-sql`. Engines are
   optional features with a fallback: `sqruff` (on by default) formats with
   sqruff-lib, and without it Format uses a built-in layout.
3. **`src/app/sql_editor/`** is the only bridge between the editor
   (gpui-kit) and the language features. `SqlEditor` is its own entity:
   - it installs the highlighter and the providers;
   - it reads columns from the cache;
   - it formats off the UI thread;
   - it reports through events (`Reveal`, `Notice`).

   The `Workspace` only tells it the dialect, the connection, the names of
   the cache and the tab's context.
4. **The `jykre` library never uses `jykre-sql-language`.** The backend
   only reads the cache (`backend::cached_relations`); the adapter builds the
   name index.
5. `cargo xtask check` enforces it:
   - `jykre-sql` depends only on `jykre-core` and never on an editor engine
     (`sqruff-lib`, `tree-sitter*`);
   - `jykre-sql-language` depends only on `jykre-core` and `jykre-sql`;
   - neither depends on GPUI or `lsp-types`;
   - no file of the library outside `src/app` and `src/main.rs` names
     `jykre_sql_language`;
   - `jykre-sql-language` is linted and tested without its default features.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Modules inside the `jykre` crate, with Cargo features | No new crates | Nothing stops the library from using the features; engines compile into the library | The boundary would rest on review alone |
| One `jykre-sql` crate with everything | One crate fewer | Query execution would depend on sqruff and on future editor engines | Execution must not carry editor engines |
| Keep the current layout | No work | Removing or replacing a feature touches execution and the workspace | Contradicts the goal of replaceable features |

## Consequences

- **Positive:**
  - A feature is removed by not installing it in `SqlEditor::new` and
    deleting its module; removing them all means dropping
    `jykre-sql-language` and `src/app/sql_editor/`, and query execution
    does not change.
  - Engines are swapped behind a feature.
  - Execution builds without sqruff.
- **Negative or costs:**
  - Two more crates, and a few calls the workspace makes to keep
    `SqlEditor` in step (dialect, connection, context, names).
  - The language crate's tests also run without default features.
- **What becomes required or forbidden:**
  - New editor language features go in `jykre-sql-language` (pure) and
    `src/app/sql_editor/` (wiring).
  - The library, the backend and execution never use them.

## How to validate

- `cargo xtask check` passes with the new rules, and its tests reject the
  forbidden edges.
- `cargo test -p jykre-sql` builds and passes on its own.
- The application behaves as before: colours, suggestions, hover, Ctrl+click
  and Format (manual check).
