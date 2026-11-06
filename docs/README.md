# Jykre Studio development documentation

This folder is the source of truth for how Jykre Studio is built: a native
Rust and GPUI desktop workspace for PostgreSQL, MySQL, Databricks and SQLite.
The root [README](../README.md) describes the product for users; these
documents describe the rules for changing it.

> If what you need to do contradicts these documents, do not improvise: stop
> and ask the maintainer. When a change is accepted, update the document (or
> write an ADR) **together with** the code. This applies to people and to
> AI agents alike.

Some documents define a **standard that the code does not fully meet yet**.
Each of them ends with a *Known deviations* section listing where the current
code differs. New code follows the standard; existing code is migrated on
purpose, never as a side effect of an unrelated change.

## How to read

1. Everyone: read the [golden rules](#golden-rules), [Architecture](architecture.md)
   and [Workflow](workflow.md) before the first task.
2. Before changing an area, read its document in the index below.
3. Before adding a dependency, read [Stack and dependencies](stack-and-dependencies.md).
4. Structural decisions live in [`adr/`](adr/README.md) and change only
   through a new ADR.

## Index

| Document | Content |
| --- | --- |
| [Architecture](architecture.md) | Layers, crates, dependency rules, repository layout, data flow and concurrency |
| [Stack and dependencies](stack-and-dependencies.md) | Approved stack and the policy for new dependencies |
| [Rust guidelines](rust-guidelines.md) | Errors, async, types, generated SQL, secrets and lints |
| [UI guidelines](ui-guidelines.md) | GPUI views and entities, state, rendering, actions, text and accessibility |
| [Performance](performance.md) | Budgets, principles, rejected patterns and how to measure |
| [Persistence](persistence.md) | Local SQLite files, schemas, atomic metadata refresh, migrations and recovery |
| [Security](security.md) | Credentials, TLS, logs, external processes and the review checklist |
| [Reliability](reliability.md) | Failure isolation, timeouts, cancellation, error classification and logs |
| [Results](results.md) | Result model, limits, grid behaviour and copying |
| [User experience](ux.md) | Layout, visible states, feedback, shortcuts, error messages and text |
| [Workflow](workflow.md) | How work flows, rules for agents, gate, review, Git |
| [Testing](testing.md) | Local gate, fixtures and testing policy |
| [Drivers](drivers.md) | Driver contracts, configuration and adding a database |
| [Technical debt](technical-debt.md) | Accepted debts across areas, their severity and the way out |
| [ADRs](adr/README.md) | Architecture decision records |

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
   changes, never per frame. See [Performance](performance.md).
8. **Results are bounded.** A row limit (and a memory ceiling, see
   [Results](results.md)) always applies. Never load everything "because it is
   easier".
9. **Database values are not reinterpreted.** Exact numbers never pass through
   floating point, NULL is distinct from the text `NULL`, and temporal values
   are not converted between time zones.
10. **Metadata is published atomically.** A failed or cancelled refresh keeps
    the last good read.
11. **Local schema changes go through a new migration.** Never edit a schema
    that has shipped. See [Persistence](persistence.md).
12. **Critical behaviour gets an automated test with the change**, and the
    gate passes before it is committed. See [Testing](testing.md).
