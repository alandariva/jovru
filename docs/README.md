# Jykre Studio development documentation

This folder is the source of truth for how Jykre Studio is built: a native
Rust and GPUI desktop workspace for PostgreSQL, MySQL, Databricks and SQLite.
The root [README](../README.md) describes the product for users.

> If a task contradicts the project standards or a requirement in the
> reference documentation, stop and ask the maintainer. Update the relevant
> documentation together with an accepted implementation change. Structural
> decisions follow the [ADR process](adr/README.md).

## How to read

1. Before the first task, read the [golden rules](#golden-rules),
   [Architecture](reference/architecture.md) and [Workflow](standards/workflow.md).
2. Before changing an area, read its standards and reference documents below.
   The rules in [standards/](standards/) apply to all new work.
3. Before adding a dependency, read
   [Stack and dependencies](standards/stack-and-dependencies.md).
4. Consult plans for scope and approval, research for evidence, and ADRs for
   the reasons behind structural decisions. A proposal or recommendation is
   not implementation approval.

## Organization and authority

| Directory | Purpose | How to use it |
| --- | --- | --- |
| [standards/](standards/) | Development rules, quality budgets and verification policy | Follow these when writing and reviewing changes |
| [reference/](reference/) | Architecture, contracts, data models and product behavior | Read before changing the corresponding area; mandatory requirements remain binding |
| [plans/](plans/) | Proposals, accepted technical debt and planned work | Check approval and implementation separately before starting |
| [plans/releases/](plans/releases/) | Release scope, milestones and acceptance criteria | Use the release plan to decide what belongs in that version |
| [research/](research/) | Investigations, audits and architectural reviews | Use as dated evidence; recommendations do not override standards or approvals |
| [adr/](adr/README.md) | Structural decisions, alternatives and consequences | Follow each record's status; only the maintainer accepts an ADR |

Reference documents currently combine descriptions and mandatory requirements.
Moving them does not weaken those requirements. Documents that define a
standard the code does not yet fully meet record the gap in *Known deviations*.
New code follows the standard; existing code is migrated deliberately, never
as a side effect of unrelated work. Accepted debts requiring their own work
are tracked in [Technical debt](plans/technical-debt.md).

## Index

### Standards

| Document | Content |
| --- | --- |
| [Stack and dependencies](standards/stack-and-dependencies.md) | Approved stack and dependency policy |
| [Rust guidelines](standards/rust-guidelines.md) | Errors, async, types, generated SQL, secrets and lints |
| [UI guidelines](standards/ui-guidelines.md) | GPUI views, state, rendering, actions, text and accessibility |
| [Performance](standards/performance.md) | Budgets, principles, rejected patterns and measurement |
| [Security](standards/security.md) | Credentials, TLS, logs, external processes and review |
| [Reliability](standards/reliability.md) | Isolation, timeouts, cancellation, errors and logs |
| [Workflow](standards/workflow.md) | Agent rules, gate, review and Git |
| [Testing](standards/testing.md) | Fixtures, local gate and testing policy |

### Reference

| Document | Content |
| --- | --- |
| [Architecture](reference/architecture.md) | Layers, crates, dependency rules, repository layout, flows and concurrency |
| [Drivers](reference/drivers.md) | Driver contracts, configuration and adding a database |
| [Persistence](reference/persistence.md) | SQLite files, schemas, refresh, migrations and recovery |
| [Results](reference/results.md) | Result model, limits, grid, copying, export and reports |
| [User experience](reference/ux.md) | Layout, visible states, feedback, shortcuts and text |
| [SQL catalogs](reference/sql-catalogs.md) | Implemented catalog storage, loading, generation and validation |

### Plans

| Document | Content |
| --- | --- |
| [Technical debt](plans/technical-debt.md) | Accepted debts, severity, status and completion criteria |
| [Version 0.2.0](plans/releases/0.2.0.md) | Daily-use hardening, application icon, Linux validation and release acceptance |
| [Version 0.3.0](plans/releases/0.3.0.md) | Approved milestones for the first automation: SQL → script |
| [Automation engine specification](plans/automation-engine-specification.md) | Version 1.2, approved for 0.3.0 on 2026-11-08: SQL → script first; ADR-0033 and ADR-0034 accepted |
| [Process-tree supervision dependency](plans/process-supervision-dependency.md) | `process-wrap` for automation's external programs, approved 2026-11-08 (Q-11) |
| [Automation SQL Job proof of concept](plans/automation-sql-job-poc.md) | Headless experiments that de-risk phase 1 without schema, contract or dependency changes; complete, with measurements on every relational driver |
| [Large `.sql` files in the editor](plans/editor-large-files.md) | Proposal for ADR-0031's D7 file limit: the editor component's soft wrap on opening, measured options and a recommendation |
| [SQL catalog proposal](plans/sql-catalog-proposal.md) | Original format proposal and future standalone catalog repository; see the reference for current behavior |

### Research and decisions

| Document | Content |
| --- | --- |
| [Behavioral audit](research/behavioral-audit.md) | Findings N001–N047, approvals, decisions and implementation evidence |
| [SQL editor investigation](research/sql-editor-investigation.md) | Alternatives, proof of concept and editor findings |
| [Automation engine architecture review](research/automation-engine-architecture-review.md) | Technical findings, proposed changes, trade-offs and required maintainer decisions |
| [Architecture decision records](adr/README.md) | Numbered records and their status |

## Maintaining documentation

- Write documentation in English. Use descriptive English filenames in
  `kebab-case`; release plans use the version number, and ADRs keep their IDs.
- Give each subject one authoritative description. Plans and research link
  to the current reference instead of maintaining a second implementation guide.
- For new plans, record **Approval** (`Proposed`, `Approved` or `Rejected`)
  and **Implementation** (`Not started`, `In progress`, `Partial` or `Complete`)
  separately. Existing records retain their item-level decisions and evidence;
  reviewer consensus does not imply maintainer approval.
- When work is implemented, update the relevant standards/reference, mark
  the plan's progress and retain the plan or research as history. Do not
  automatically accept an ADR or approve the next phase.
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
2. **Backend crates never depend on the UI.** `jykre-core`, the drivers,
   `jykre-storage` and `jykre-secrets` know nothing about GPUI. `cargo xtask
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
   changes, never per frame. See [Performance](standards/performance.md).
8. **Results are bounded.** A row limit (and a memory ceiling, see
   [Results](reference/results.md)) always applies. Never load everything "because it is
   easier".
9. **Database values are not reinterpreted.** Exact numbers never pass through
   floating point, NULL is distinct from the text `NULL`, and temporal values
   are not converted between time zones.
10. **Metadata is published atomically.** A failed or cancelled refresh keeps
    the last good read.
11. **Local schema changes go through a new migration.** Never edit a schema
    that has shipped. See [Persistence](reference/persistence.md).
12. **Critical behaviour gets an automated test with the change**, and the
    gate passes before it is committed. See [Testing](standards/testing.md).
