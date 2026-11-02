# Architecture decision records

An ADR records a structural decision: its context, the decision, the
alternatives and the consequences. It exists so nobody has to guess **why**
something is the way it is, and so a decision is not undone by accident.

## Rules

- A new ADR is a new file `NNNN-short-title.md` based on the
  [template](0000-template.md), written on its own or with the code that motivates
  it.
- Statuses: **Proposed** → **Accepted** (approved by the maintainer and
  committed) → **Superseded by ADR-NNNN** or **Deprecated**.
- Agents may write ADRs as *Proposed*; only the maintainer changes the status
  to *Accepted*.
- An accepted ADR is not rewritten. To change a decision, write a new ADR and
  mark the old one as superseded.
- ADRs 0001 to 0006 were recorded on 2026-11-02 for decisions already in
  effect in the code. Their *Alternatives considered* were reconstructed when
  recording them; the maintainer may correct them.

## Index

| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-native-ui-with-gpui.md) | Native UI with GPUI through `gpui-kit` | Accepted |
| [0002](0002-sqlx-for-postgres-and-mysql.md) | SQLx for PostgreSQL and MySQL, with rustls and the text protocol | Accepted |
| [0003](0003-sqlite-for-local-storage.md) | SQLite for local storage, with one metadata file per connection | Accepted |
| [0004](0004-os-vault-for-credentials.md) | Operating system vault for credentials, with no file fallback | Accepted |
| [0005](0005-databricks-through-cli-and-rest.md) | Databricks through CLI profiles and REST APIs | Accepted |
| [0006](0006-layered-crates-independent-of-the-ui.md) | Layered crates independent of the UI, with a static driver registry | Accepted |
| [0007](0007-application-logic-outside-views.md) | Application logic outside views | Proposed |
| [0008](0008-compact-result-storage.md) | Compact result storage with a configurable row limit and a memory ceiling | Proposed |
| [0009](0009-structured-logging-and-timings.md) | Structured logging and timings with `tracing` | Proposed |
| [0010](0010-benchmarks-with-criterion.md) | Benchmarks with `criterion` for hot paths | Proposed |
| [0011](0011-workspace-lints.md) | Workspace-wide lints for panics, output and async locks | Proposed |
| [0012](0012-result-grid-as-a-cached-entity.md) | Result grid as its own cached entity | Accepted (implementation pending) |
