# Proposal: `jiff` for schedules' time zones, with a structured rule instead of a cron grammar

**Date:** November 8, 2026
**Approval:** Proposed. It awaits the maintainer's decision (Q-33 of the
[specification's scheduling revision](automation-engine-specification.md#959-decisions-for-the-maintainer)),
together with [ADR-0041](../adr/0041-schedules-and-grants.md).
**Implementation:** Not started.
**For:** [0.3.0](releases/0.3.0.md), milestone A13 (schedules while the
application is open)

Facts below carry their source:

- **[lock]**: the worktree's `Cargo.lock` at `release/0.3.0` (`f85ac9b`).
- **[reg]**: `~/.cargo/registry`.
- **[cio]**: the crates.io API for the crate and its dependencies.
- **[gh]**: the GitHub API and issue search.
- **[docs]**: docs.rs for that version.

All were read on 2026-11-08. `cargo tree` could not run offline, so whether
a crate in the lock is compiled for a given target is **not verified**.

## Request

Add [`jiff`](https://crates.io/crates/jiff) as a workspace dependency of the
`jykre` crate, used by the automation engine's scheduler
(`src/automation/schedule.rs`) and by the views that show a schedule's next
run:

```toml
# [workspace.dependencies]
jiff = "0.2.39"   # the version already in Cargo.lock
```

Do **not** add a cron or RRULE crate. A schedule's rule is a structured
value edited through a form (an interval, or times of day on chosen days),
which our code evaluates with `jiff`. The
[specification](automation-engine-specification.md#952-the-rule) defines the
rule.

## 1. Why

A13 asks for schedules defined in a time zone, with the next run shown in
that zone. The standard library has no time zones, and Windows ships no IANA
database. Something must read the tz database and apply its daylight-saving
rules. That is not code to write ourselves: the database's binary format,
its updates and the gaps and folds of every zone.

| Need | Without a dependency | With `jiff` |
| --- | --- | --- |
| Turn "every day at 02:30 in `Europe/Berlin`" into instants | Our own TZif parser and rules, plus a bundled database for Windows | `TimeZone::get(name)`, then `TimeZone::to_ambiguous_zoned(civil::DateTime)` **[reg `jiff-0.2.39/src/tz/timezone.rs`]** |
| Daylight-saving gaps and folds | Same | `AmbiguousZoned` tells a gap and a fold apart, and `disambiguate(Disambiguation)` resolves them **[reg `src/tz/ambiguous.rs`]**. The specification's rule is `Disambiguation::Compatible` (RFC 5545's): **a time in a gap moves forward by the gap's length, and a time in a fold takes the earlier instant**, once |
| The user's own zone | Platform code per OS | `TimeZone::system()` **[reg `src/tz/timezone.rs`]** |
| Show the next run in its zone with its abbreviation and offset | Our own formatting | `Zoned` and `strftime` **[docs]** |
| Arithmetic that crosses DST (an interval in absolute time) | Error-prone by hand | `Timestamp` arithmetic is absolute, `Zoned` arithmetic is calendar-aware **[docs]** |

A side effect, not part of this request: [Results](../reference/results.md)
notes that **Messages** shows relative times because showing the local
clock time "needs a date library, which is a dependency decision". `jiff`
would allow it later.

## 2. The grammar: three choices

The specification proposes a rule edited through a form, not typed as a
cron string. This is the choice the maintainer is asked to make (Q-33):

| Option | What it brings | Time zones and DST | Weight | Assessment |
| --- | --- | --- | --- | --- |
| **(a) Structured rule + `jiff`** (recommended) | Our rule type and its next-occurrence function, a few hundred lines with tests, over `jiff`'s tz arithmetic. The form *is* the grammar | `jiff`'s, documented. Our scheduler states the gap and fold policy and tests it with real zones | `jiff` only (below) | A form is what A13's "interval or calendar rule" needs. It cannot hold an expression the interface cannot show, and every rule it accepts has a sentence that describes it |
| (b) `croner` 4.0.3 with its `jiff` feature, plus `jiff` | Cron strings with seconds, `L`, `W`, `#`, `?` and nicknames; day-of-month OR day-of-week unless asked otherwise **[gh README]** | Documented policy (OCPS/Vixie): a fixed time in a gap runs at the first valid moment; in a fold, a fixed job runs once and an interval job runs each time **[gh README]**. DST fixes are recent: #67/#68 (a job ran twice before a spring-forward gap) fixed in 4.0.3 on 2026-11-04; #70 (4.x changed interval behaviour) open **[gh]** | MIT. 12.5 M downloads, 4 open issues. Pulls `derive_builder` 0.20 (with `darling` 0.20, duplicating the lock's 0.24.1) and `strum` 0.27 (duplicating 0.28) **[cio, lock]** | Worth it only if the maintainer wants users to type cron. Its DST semantics are newly settled |
| (c) `cron` 0.17.0 or `jiff-cron` 0.3.0 | Seven-field cron expressions | `cron` (chrono): open #87, a fall-back transition in `Europe/Paris` panics with "invalid time"; #145 (repeated hour skipped) fixed on 2026-06-21 **[gh]**. `jiff-cron` is a fork with 9 stars; its DST tests were **not verified** | `cron`: MIT OR Apache-2.0, deps already in the lock **[cio, lock]** | A panic on a DST change is disqualifying for an unattended feature |
| (d) `rrule` 0.14.0 | RFC 5545 recurrence rules | `chrono-tz` zones; DST behaviour **not verified** | Brings `chrono-tz` (new). Last push 2025-04-20, 36 open issues **[gh]** | Unmaintained since April 2025, and far more grammar than A13 needs |
| (e) `cronexpr` 1.7.1 | Five-field cron with an IANA zone in the expression, on `jiff` only | **No documented gap or fold policy** **[docs]**. Reading its source, a fixed time inside a gap seems to be skipped that day (**not verified** by a test) | Apache-2.0, 89 k downloads **[cio]** | An undocumented DST policy |

For time zones alone, `chrono` with `chrono-tz` is the alternative to
`jiff`. `chrono` 0.4.45 is in the lock through GPUI. `chrono-tz` 0.10.4
(MIT OR Apache-2.0) is not. It embeds the whole tz database at build time
and was last released on 2025-07-11 **[cio]**. `jiff` is already in the
tree and updates the database on Unix from the system (below), so it is
preferred.

## 3. Licence

`jiff` 0.2.x is `Unlicense OR MIT` **[cio]**. `jiff-tzdb` and
`jiff-tzdb-platform` are under the same terms. Both licences are in
`deny.toml`'s allow-list.

## 4. Health

- 0.2.41 released 2026-11-08, with releases roughly weekly to monthly.
- 268 M downloads, 110.6 M in the last 90 days **[cio]**.
- `BurntSushi/jiff`: not archived, 31 open issues, last push 2026-11-08,
  3,007 stars **[gh]**.
- Minimum Rust 1.70 (ours: 1.95).
- No advisory is known. `cargo deny` with `jiff` as a direct dependency has
  **not been run**.

## 5. Weight

| | Crates |
| --- | --- |
| Already in `Cargo.lock` | `jiff` 0.2.39, `jiff-static` 0.2.39, `jiff-tzdb-platform` 0.1.3, `jiff-tzdb` 0.1.9, through `serde_with` 3.24.0, which `genai` 0.7.3 uses **[lock]** |
| New crates | **None**, if `jiff = "0.2.39"` resolves to the locked version |
| Compiled today? | **Not verified.** `serde_with`'s `std` feature enables `jiff`'s system tz features **[reg `serde_with-3.24.0/Cargo.toml`]**, but whether `genai` turns `serde_with`'s jiff support on was not confirmed offline. If it does not, `jiff` and, on Windows, `jiff-tzdb` become newly compiled, with no new crate in the lock |
| Size of the bundled database | `jiff-tzdb` 0.1.9's `concatenated-zoneinfo.dat` is **206,788 bytes** (tzdb 2026e) **[reg]**. `jiff` declares `jiff-tzdb-platform` only for Windows and wasm **[reg `jiff-0.2.39/Cargo.toml`]**, so Linux and macOS read `/usr/share/zoneinfo` |

There is no second date library on the scheduler's path. `chrono` stays only
where GPUI uses it.

## 6. Features

The default features: `std`, `tz-system`, `tz-fat`,
`tzdb-bundle-platform`, `tzdb-zoneinfo`, `tzdb-concatenated` and
`perf-inline` **[reg `jiff-0.2.39/Cargo.toml`]**. They give the system zone, the system database on
Unix and the bundled one on Windows. `tzdb-bundle-always` stays off: its own
documentation discourages it on Unix, where the system database is kept up
to date. `serde` stays off: rules are stored as our own canonical JSON.

## Residual risks

- **Windows' database ages with the build.** A zone whose rules change
  after a release (a country dropping DST, as Brazil did in 2019) needs an
  application update on Windows. Linux follows its system's `tzdata`. The
  schedule shows its next run in its zone, so a change is at least visible.
- **A zone name that disappears** from a newer database (rare; names are
  normally kept as links) suspends the schedule with that reason rather
  than guessing (specification, section 9.5.3).
- `jiff` is 0.x; its authors aim at 1.0 with a published plan. A minor
  upgrade is its own small task.

## What approval allows

Declaring `jiff` as above, used by the scheduler and the schedule views of
the `jykre` crate only, with the engine tests of the
[specification's section 9.5.10](automation-engine-specification.md#9510-tests-and-acceptance).
No cron or RRULE crate is added. The
[stack table](../standards/stack-and-dependencies.md) gets its row when the
dependency lands.
