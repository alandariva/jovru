# Proposal: `process-wrap` for process-tree supervision

**Date:** November 8, 2026
**Approval:** Approved by the maintainer on 2026-11-08, as requested below (Q-11 of the [automation specification](automation-engine-specification.md#12-decisions-required-from-the-maintainer))
**Implementation:** Complete on Linux, as approved: the dependency is declared with exactly the features requested (no `tracing` feature) and is used only by `jykre::automation::process`. **Windows is not validated yet**: the Job Object, `CREATE_NO_WINDOW` and the kill of the tree are checked by the maintainer's [checklist](releases/0.3.0-windows-checklist.md)
**For:** [0.3.0](releases/0.3.0.md), milestone A3 (phase 3 — external program)

## Request

Add [`process-wrap`](https://crates.io/crates/process-wrap) 10.0.1
(`Apache-2.0 OR MIT`) as a workspace dependency of the `jykre` crate, used
only by the automation engine to start external programs:

```toml
# [workspace.dependencies]
process-wrap = { version = "10.0.1", default-features = false, features = ["tokio1", "job-object", "kill-on-drop", "process-group", "creation-flags"] }
```

## 1. Why

The specification requires that Stop and the deadline end the program **and
everything it started** (section 9.1, CA-03, P-07). Tokio alone cannot do
it: `kill_on_drop` and `Child::kill` signal only the direct child.

| Need | Without the crate | With it |
| --- | --- | --- |
| Kill grandchildren on Unix | Own code calling `setpgid` before `exec` and `killpg`: needs `unsafe` (`pre_exec`), which the workspace forbids (`unsafe_code = "forbid"`) | `ProcessGroup::leader()` |
| Kill grandchildren on Windows | Own Win32 Job Object code (`CreateJobObjectW`, `AssignProcessToJobObject`, suspended start to avoid an escape window, completion port to wait for the tree): FFI and `unsafe` throughout | `JobObject` with `KillOnDrop` (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) |
| Tree killed when Studio crashes (Windows) | Same Job Object code | The system closes the job handle and kills the tree |
| No console window flashing on Windows | `creation_flags` by hand | `CreationFlags` composes with `JobObject` |
| Reap the whole group (no zombies) on Unix | Own `waitpid` loop | `ProcessGroupChild::wait` |

Writing it ourselves means `unsafe` FFI on two platforms, an exception to the
workspace lint, and tests for races between spawn and assignment, which is
exactly what this crate maintains.

**Spike (Linux, 2026-11-08).** A shell that starts `sleep 300` in the
background and ignores `SIGTERM`, killed through `process-wrap`:

| Wrapper | Grandchild after kill |
| --- | --- |
| `ProcessGroup::leader()` + `KillOnDrop` | Killed |
| `KillOnDrop` only (what Tokio gives) | **Still running** |

Windows has not been checked yet; milestone A3 requires it.

## 2. License

`Apache-2.0 OR MIT`, both in the `deny.toml` allow-list. Its whole tree
passes the licence check.

## 3. Health

- 25.1 million downloads, 14.5 million in the last 90 days; first release
  2024-03, latest 10.0.1 on 2026-09-23 (10.0.0 on 2026-08-24, 9.x in 2026).
- Maintained by the [watchexec](https://github.com/watchexec/process-wrap)
  organisation, successor of its `command-group`; repository pushed
  2026-11-02, not archived, 3 open issues.
- Minimum Rust 1.87 (ours: 1.95).
- `cargo deny --locked check` on a project with exactly these features:
  advisories ok, bans ok, licenses ok, sources ok.

## 4. Weight

| | Crates |
| --- | --- |
| New crates in `Cargo.lock` | **1** (`process-wrap`) |
| Its other dependencies | `nix` 0.31.3, `windows` 0.62.2 (and its split crates), `futures` 0.3.34, `tokio`: all already in `Cargo.lock` at these versions |
| New duplicate versions | None |

What grows is compile time from a few more features of `nix` (`fs`,
`signal`, `process`) and of `windows` (`Win32_System_JobObjects` and
neighbours) being built. No second runtime, TLS stack or date library.

The `unsafe` it needs (Win32 calls, `pre_exec`) stays in the dependency; our
crates keep `unsafe_code = "forbid"`.

## 5. Features

`default-features = false`, then only:

- `tokio1`: the Tokio API (the engine runs on the application's runtime).
- `process-group`: Unix process groups.
- `job-object`, `creation-flags`: Windows Job Objects, and flags such as
  `CREATE_NO_WINDOW`.
- `kill-on-drop`: kill the tree when the handle is dropped, and kill-on-close
  for the Job Object.

Left out: `tracing` (its debug spans would carry fields outside the
telemetry allowlist), `process-session`, `reset-sigmask`, `std`.

## Residual risks (documented, not solved by any crate)

- **Unix:** a descendant that calls `setsid` or `setpgid` leaves the group
  and survives Stop. If Studio itself crashes, the group survives; recovery
  records the group ID and start time, warns, and never kills an ID that may
  have been reused (specification, section 9.1).
- **Windows:** a program can escape only if the job allows breakaway, which
  this configuration does not.
- External code still runs with the user's privileges; this supervises, it
  does not sandbox (Q-10).

## What approval allows

Declaring the dependency as above, used only by the automation engine's
external-program executor, with the phase 3 tests of the specification's
section 11, run on Windows and Linux separately. The
[stack table](../standards/stack-and-dependencies.md) gets its row when the
dependency lands.
