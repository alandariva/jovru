# Workflow

Code is written by the maintainer and by AI agents working under the
maintainer's direction. Quality comes from one automated gate, the same
locally and in CI, and from the maintainer reviewing the diff before it is
committed.

The project is young, so the workflow stays light: a piece of work may bundle
several related changes, span several commits, and be verified once for the
whole batch before it is committed.

## How work flows

1. Read [docs/README.md](../README.md) and the documents of the areas you will
   change.
2. Make the changes, with their tests, and update `docs/` when behaviour, a
   contract, the schema or configuration changes.
3. Run the [gate](#verification-gate) before committing. One run can cover
   everything since the last commit.
4. The maintainer reviews the diff (and runs the application for UI changes),
   then commits. Several changes can go in one commit, and one piece of work
   can produce several commits.

## Rules for agents

Required:

- Follow `docs/`. If the documentation is wrong or contradictory, **stop and
  ask** instead of improvising.
- Report every check as passed, failed or not run, with the reason. An
  unavailable tool or fixture is a check not run, never a pass.
- Write tests for the behaviour you add or fix.
- Say what you assumed and where you deviated from the docs.

Ask the maintainer first:

- Adding or upgrading dependencies ([Stack and dependencies](stack-and-dependencies.md)).
- Changing core contracts (`Driver`, `Session`, `RowSink`,
  `IntrospectionSink`, `SecretStore`), the SQLite schema or an ADR.
- Committing, pushing, rewriting history or switching branches. The one
  exception is a release run through the `deploy-release` skill
  (`.claude/skills/deploy-release/SKILL.md`): the agent announces what it will
  do and does it, without asking at each step (see [Git](#git)).

Never:

- Make the gate pass around a problem: `#[allow(...)]` to silence a lint,
  `#[ignore]` or deleting a failing test, weakening an assertion, a
  "temporary" `unwrap()`.
- Edit a released migration ([Persistence](../reference/persistence.md#migrations)).
- Invent library APIs; check the documentation or the source of the version
  in `Cargo.lock` (`~/.cargo/registry`).
- Use production databases, real credentials or real customer data
  ([Testing](testing.md)).

## Verification gate

| Command | Content | When |
| --- | --- | --- |
| `cargo xtask check` | Architecture check, `cargo fmt --check`, Clippy with `-D warnings` (examples included), `cargo test --workspace` with doctests | Before committing code changes |
| `cargo xtask check --full` | The above, plus live tests against PostgreSQL 14/18 and MySQL 8.4/9.7 in Docker, the benchmarks in quick mode and `cargo deny --locked check` | When the batch touches drivers, execution contracts or dependencies, and before a release |

- CI runs the basic gate on Windows, the full gate on Linux and a build with
  the minimum Rust version on every push to `main` and every pull request; the
  audit also runs weekly.
- The pre-commit hook in `.githooks/` checks formatting
  (`git config core.hooksPath .githooks`). It does not replace the gate.
- UI changes also need a look at the affected flow in the application,
  keyboard and focus included.

## Review checklist

- [ ] Logic is in the right place, and crate dependency rules hold
      ([Architecture](../reference/architecture.md)).
- [ ] No new `unwrap`/`expect`/`panic` on production paths (the workspace
      lints catch them) and no new `#[allow]` for those lints without
      approval; errors carry the right `ErrorCode`.
- [ ] Long operations are cancellable, time-limited and show progress.
- [ ] No pattern from [Rejected patterns](performance.md#rejected-patterns),
      and budgets in the touched areas still hold.
- [ ] The [security checklist](security.md#security-review-checklist).
- [ ] Tests cover the change and were not weakened; no lint silenced.

## Task report

What an agent hands back, kept short:

```markdown
## Summary
<what changed>

## Checks
- `cargo xtask check`: passed | failed | not run (reason)
- `--full` / `cargo deny`: when relevant
- Visual check: <flows looked at, or not run and why>
- Performance: <before/after numbers, when a measured path changed>

## Notes
<assumptions, deviations from the docs, what is left>
```

## Git

- `main` is the default branch; `release/<version>` branches prepare a
  release. A release goes in two phases: the agent bumps the version, runs
  the full gate, commits, pushes the release branch and opens the pull
  request, then stops until the maintainer merges it into `main`. Only then
  does it tag the merged `main` (`v<version>`, annotated) and push that one
  tag. Pushing a `v*` tag builds the binaries, signs them and drafts a GitHub
  release for review; the agent checks that the workflow started, gives the
  maintainer its link, and does not monitor it. The maintainer approves the
  signing stage and publishes the draft; the agent fills the draft's
  description from the commits since the previous published release. The
  `sign` job needs the
  `ZIPSIGN_PRIVATE_KEY` secret and verifies the archives with
  `assets/update/release-key.pub` before the draft
  ([ADR-0033](../adr/0033-update-check-and-signed-self-install.md)); a
  release without it fails rather than ship unsigned. Reviewing a draft
  includes opening the Windows zip in Explorer and 7-Zip, since the
  signature sits in front of the zip's data.
- Group related changes as you see fit. Never commit with the gate failing.
- Commit messages: a short imperative summary, a blank line, then a body in
  plain English describing the user-visible changes, usually as a bullet list
  (see `git log`).
- History that has been pushed is not rewritten.

## Done

Work is ready to commit when the gate passed (and `--full` when relevant),
the change has tests, the docs reflect it, the UI was checked when it
changed, and the maintainer reviewed the diff.

## Architecture decisions

- Changing what an ADR or [Architecture](../reference/architecture.md) decides needs a new
  ADR ([template](../adr/0000-template.md)).
- Agents may **propose** an ADR; only the maintainer accepts it. Accepted ADRs
  are not rewritten; a later one supersedes them.
- A change needs an ADR when it affects more than one crate, a core contract,
  the persistence schema or the stack.
