# Decisions: Stack and dependencies

Why this area is built as it is. Each entry records what was decided, why, and which alternatives lost. The rules themselves are in [Stack and dependencies](../standards/stack-and-dependencies.md). Entries are numbered `D-NNNN`, and the numbers are stable.

<a id="d-0043"></a>
## D-0043: Reading CSV and XLSX files with `csv`, `calamine` and `encoding_rs`

*Approved by the maintainer 2026-11-08 · implemented*

**Decision.** `csv` 1.4.0, `calamine` 0.38.0 without default features and `encoding_rs` 0.8.44 with its defaults are workspace dependencies, used only by `jovru::import` (row above). `calamine`'s `dates`, `chrono` and `picture` features and `encoding_rs`'s `fast-legacy-encode` and `simd-accel` stay off, and `calamine`'s `worksheet_range` is never called: it collects the whole sheet. `zip` is also a direct dependency of the `jovru` crate, for the shared strings check. The release profile's debug assertions for `debug_unsafe` and `atoi_simd` and overflow checks for `calamine` (root `Cargo.toml`) must stay: files are untrusted, and a malformed one must fail the read, not index unchecked. Import rules: [D-0040](drivers.md#d-0040).
**Why.** The application exported CSV and XLSX but read neither, and a hand-written reader that narrows the feature was ruled out. Our own readers would need a CSV state machine, an OOXML reader (shared strings, styles and date formats, the 1900 and 1904 epochs) and decoders for many encodings. Licences pass `deny.toml`. New crates: `csv-core`, `atoi_simd`, `debug_unsafe`, `fast-float2`, `codepage` and `quick-xml` 0.42 (a second copy beside 0.41 on Linux, a `cargo deny` warning). `calamine` is 0.x and its API moves between minors, so upgrading it is its own task.
**Rejected.** `umya-spreadsheet`: its stream callback cannot stop early or be cancelled, it panics on malformed XML and adds 18 crates. Our own XLSX reader on `zip` and `quick-xml`: re-implements what `calamine` maintains. `xlsx_reader`, `xlsx_batch_reader`: unmaintained. `spreadsheet-ods`: ODS only. `encoding_rs_io`: always replaces malformed bytes. `chardetng` (guessing the encoding): a wrong guess silently changes text. Our own CSV parser: `csv-core` is small and exhaustively tested.

<a id="d-0044"></a>
## D-0044: Process-tree supervision with `process-wrap`

*Approved by the maintainer 2026-11-08 · partly: validated on Linux, Windows pending*

**Decision.** `process-wrap` 10.0.1 without default features, with `tokio1`, `job-object`, `kill-on-drop`, `process-group` and `creation-flags`, is a workspace dependency used only by `jovru::automation::process` to start an automation program (row above). Its `tracing` feature stays off: its spans would carry fields outside the telemetry allowlist. Windows is pending: the Job Object, the hidden console window and the kill of the whole tree have not been checked there, and must be before a release relies on them.
**Why.** Stop, a deadline and a crashed Jovru must end a program and everything it started (D-0038). Tokio's `kill_on_drop` signals only the direct child. A spike on Linux with a shell that ignores `SIGTERM` and starts `sleep 300` showed the grandchild killed with `ProcessGroup::leader()` and still running with `KillOnDrop` alone. Doing it ourselves needs `pre_exec` and Win32 Job Object calls, `unsafe` that the workspace forbids. One new crate: `nix`, `windows`, `futures` and `tokio` were in the tree. `Apache-2.0 OR MIT`; maintained by the watchexec organisation. A descendant that calls `setsid` or `setpgid` leaves the group and survives Stop, and this supervises, not sandboxes: the program runs with the user's privileges.
**Rejected.** Our own process groups and Job Objects: `unsafe` FFI on two platforms and races between spawn and assignment. Tokio's `kill_on_drop` alone: leaves grandchildren running.

<a id="d-0045"></a>
## D-0045: Time zones of schedules with `jiff`, without a cron grammar

*Approved by the maintainer 2026-11-08 · implemented*

**Decision.** `jiff` 0.2.39 with its default features is a workspace dependency used only by `jovru::automation::schedule` and the schedule views (row above). A schedule's rule is a structured value edited in a form (an interval, or times of day on chosen days) and evaluated by our code. No cron or RRULE crate may be added. A time inside a daylight-saving gap moves forward by the gap and a time in a fold takes the earlier instant, once (`Disambiguation::Compatible`, RFC 5545's).
**Why.** The standard library has no time zones and Windows ships no IANA database, and the gaps and folds of every zone are not code to write ourselves. `jiff` was already in the lock through `serde_with`, so no new crate. Linux and macOS read the system's database; Windows uses the bundled one (about 207 KB), so a zone whose rules change after a release, as Brazil's did in 2019, needs an application update there. A zone name missing from a newer database suspends the schedule with that reason. `Unlicense OR MIT`. `jiff` is 0.x; upgrading is its own task.
**Rejected.** `croner` with its `jiff` feature: pulls `derive_builder` and `strum` copies and its DST fixes were weeks old. `cron` and `jiff-cron`: a fall-back transition panicked. `rrule`: unmaintained since April 2025 and brings `chrono-tz`. `cronexpr`: no documented gap or fold policy. `chrono` with `chrono-tz`: embeds the whole database and a second date library on this path.
