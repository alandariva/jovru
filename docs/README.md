# Jovru development documentation

This folder is the source of truth for how Jovru is built: a native
Rust and GPUI desktop workspace for PostgreSQL, MySQL, Databricks and SQLite.
The root `README.md` describes the product for users.

> If a task contradicts the project standards or a requirement in the
> reference documentation, stop and ask the maintainer. Update the relevant
> documentation together with an accepted implementation change. Structural
> decisions are recorded in `decisions/`, one file per area.

## How to read

1. Before the first task, read the [golden rules](#golden-rules),
   `reference/architecture.md` and `standards/workflow.md`.
2. Before changing an area, read the documents for it in `standards/` and
   `reference/`. The rules in `standards/` apply to all new work.
3. Before adding a dependency, read `standards/stack-and-dependencies.md`.
4. Consult plans for scope and approval, research for evidence, and
   [decisions/](decisions/) for the reasons behind structural choices. A
   proposal or recommendation is not implementation approval.

This page describes how the folders are organized, not what each document
contains. Open a folder to see its documents; their titles and opening
paragraphs say what they cover.

## How the folders are organized

| Folder | What it holds | How to use it |
| --- | --- | --- |
| [standards/](standards/) | The rules that apply to any change, whatever the area: the development guide, the approved stack and dependency policy, the Rust, UI, performance, security, reliability, workflow and testing standards, and the checks done by hand | Follow these when writing and reviewing changes |
| [reference/](reference/) | How each area of the product works and what it guarantees: the architecture and its layers, the drivers, persistence, results, user experience, themes, SQL catalogs, updates, automation and the assistant. A large area is a folder with its own `README.md` and one file per topic | Read the one for the area you change; mandatory requirements remain binding |
| [decisions/](decisions/) | The structural choices behind the standards and the reference, one file per area: what was decided, why, and which alternatives lost. Entries are numbered `D-NNNN`, and the folder's `README.md` finds one by its number | Read it to learn why something is the way it is before changing it |
| [plans/](plans/) | What is still to do: proposals, accepted technical debt and approved work in progress, each recording its approval and implementation status. Release scope lives in `plans/releases/`, one file named for the version, holding the minimum the next release must contain | Check approval and implementation separately before starting. Once a plan is implemented, what lasts moves into standards, reference or decisions and the plan is deleted |
| [research/](research/) | Investigations and architectural reviews, kept as dated evidence and trimmed to what still informs a decision | Recommendations here do not override standards or approvals |
| [adr/](adr/) | Open proposals for structural decisions, and the template to write one | Only the maintainer accepts a proposal; once accepted it moves into the rules and the decisions file of its area |
| [images/](images/) | Screenshots used by the root README | |

Reference documents combine descriptions and mandatory requirements. A
document that defines a standard the code does not yet fully meet records the
gap in a *Known deviations* table at its end. New code follows the standard;
existing code is migrated deliberately, never as a side effect of unrelated
work. Accepted debts that need their own work are tracked in the technical debt
plan in `plans/`.

## Maintaining documentation

- Write documentation in English. Use descriptive English filenames in
  `kebab-case`; release plans use the version number, and decisions keep their
  numbers.
- Give each subject one authoritative description. Plans and research link
  to the current reference instead of maintaining a second implementation guide.
- Keep this page about folders. Do not list documents here; a list drifts
  when a document is added, renamed or removed. The only per-document list is
  the decision index in `decisions/`.
- For new plans, record **Approval** (`Proposed`, `Approved` or `Rejected`)
  and **Implementation** (`Not started`, `In progress`, `Partial` or `Complete`)
  separately. Existing records retain their item-level decisions and evidence;
  reviewer consensus does not imply maintainer approval.
- A plan holds only what is still to do. When its work is implemented,
  move what lasts into the relevant standards or reference (a decision goes
  in the `decisions/` file of its area), then delete the plan; git keeps the
  history. A release plan is the minimum scope of the next release only, and
  is deleted when the release ships. Research stays as dated evidence. Do not
  automatically accept an ADR or approve the next phase.
- The project was called Jykre Studio until 2026-11-11. Research keeps the
  old name and its identifiers (`jykre`, `JYKRE_*`) as the history it records.
- Keep finding IDs, decision IDs, commits and historical evidence stable.
  Historical line references describe the snapshot the investigation examined.
- When moving or renaming a document, update relative links, repository paths,
  source documentation and `AGENTS.md`, including links to translated headings.

## Golden rules

These rules prevent the mistakes that cost the most to undo. A change that
breaks one of them is not accepted.

1. **Views present; they do not decide.** GPUI views render state and capture
   intent. Logic that does not need GPUI (SQL splitting, query orchestration,
   formatting rules, persistence) lives outside the views, where it can be
   tested without a window.
2. **Backend crates never depend on the UI.** `jovru-core`, the drivers,
   `jovru-storage` and `jovru-secrets` know nothing about GPUI. `cargo xtask
   check` enforces the dependency graph.
3. **Database-specific behaviour stays in the driver.** Outside the driver
   crates and the registry, do not `match` on `DriverKind` to decide SQL
   behaviour; use the driver's descriptor, capabilities or dialect.
4. **Credentials exist only as `SecretString`** and are persisted only in the
   operating system vault. Never in SQLite, JSON, logs, error messages or
   `Debug` output.
5. **No panics on production paths.** Errors travel as `DriverResult` and reach
   the UI as a classified `DriverError`.
6. **Never block the UI thread.** Network and database work runs on the Tokio
   runtime through `backend::run`; CPU work that grows with the data runs off
   the UI thread. Every long operation can be cancelled and has a timeout.
7. **A frame costs what is on screen, not what is in memory.** Long
   collections are virtualized; data derived from them is computed when it
   changes, never per frame. See the performance standard.
8. **Results are bounded.** A row limit (and a memory ceiling, see
   the results reference) always applies. Never load everything "because it is
   easier".
9. **Database values are not reinterpreted.** Exact numbers never pass through
   floating point, NULL is distinct from the text `NULL`, and temporal values
   are not converted between time zones.
10. **Metadata is published atomically.** A failed or cancelled refresh keeps
    the last good read.
11. **Local schema changes go through a new migration.** Never edit a schema
    that has shipped. See the persistence reference.
12. **Critical behaviour gets an automated test with the change**, and the
    gate passes before it is committed. See the testing standard.
