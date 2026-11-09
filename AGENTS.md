# Project documentation

`docs/README.md` is the source of truth for how this project is built. Read
its golden rules, `docs/reference/architecture.md` and
`docs/standards/workflow.md` before the first task, and the document for each
area you change (UI, performance,
persistence, security, reliability, results, UX). If a request contradicts the
documentation, stop and ask instead of improvising. Follow `docs/standards/`;
mandatory requirements in `docs/reference/` remain binding. Plans and research
do not grant implementation approval. Documents that track gaps end with a
*Known deviations* table: new code follows the standard, and existing code is
migrated only when that is the goal of the work.

- UI changes follow `docs/standards/ui-guidelines.md` and must not break a
  budget in `docs/standards/performance.md`; report numbers when the touched
  path is measured.
- Do not add or upgrade dependencies, change core contracts, the SQLite schema
  or an ADR without the maintainer's approval. When a dependency would serve
  better than our own code, propose it
  (see `docs/standards/stack-and-dependencies.md`)
  instead of narrowing the feature or hand-rolling a replacement to avoid it.

- Keep documentation and filenames in English, using `kebab-case` for document
  names (release versions and numbered ADRs retain their conventions).
- File documents by purpose as described in `docs/README.md`: standards,
  reference, plans, research and ADRs. Keep approval separate from implementation
  progress and update links when documents move.

# Verifying changes

Read `docs/standards/testing.md` when changing code or the validation infrastructure.

- Before delivering a task that changes code, run `cargo xtask check`.
- Changes to drivers, execution contracts or release preparation require
  `cargo xtask check --full`.
- Dependency changes also require `cargo deny --locked check` (already
  included in full mode).
- Keep regression tests and doctests. Do not remove, ignore or weaken tests,
  or silence lints, to pass the gate.
- Use dedicated fixtures and synthetic data. Never use production databases or
  credentials in tests.
- For visual changes, open the application and check the affected flow,
  keyboard and focus where relevant. Record that check separately.
- Report checks as passed, failed or not run, explaining limitations and
  ignored tests. An audit failure or unavailable infrastructure is not a
  passed gate. Reviewing the diff is still required.
