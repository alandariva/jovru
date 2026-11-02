# Verifying changes

Read `docs/testing.md` when changing code or the validation infrastructure.

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
