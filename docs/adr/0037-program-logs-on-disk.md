# ADR-0037: External programs' logs kept on disk for 7 days

- **Status:** Accepted (by the maintainer on 2026-11-08; not implemented)
- **Date:** 2026-11-08
- **Authors:** Alan de Oliveira (decision Q-15), Claude (draft)

## Context

Phase 3 of the [automation specification](../plans/automation-engine-specification.md)
runs external programs whose stdout and stderr must always be drained, so a
full pipe never blocks them (section 9.1, P-13); phase 4 adds the `log`
message of `jjp/1`. What a program prints is not under Studio's control: it
may hold input values, rows or tokens. Section 6.4 and CA-13 forbid writing
values to disk, and [ADR-0035](0035-persisted-declared-outputs.md) is the
only exception so far. On 2026-11-08 the maintainer answered Q-15: program
logs are **always kept on disk**, at most 1 MiB per stream, deleted after
7 days, with a warning.

## Decision

1. **Where.** Each run of an external program writes up to three files in
   `automation/logs/<run-id>/` under the data folder (`JYKRE_CONFIG_DIR`
   respected): `stdout.log`, `stderr.log` and, under `jjp/1`, `jjp.log`.
   Nothing else is written there.
2. **Bounded.** At most **1 MiB per file**. Past it the stream is still read
   and discarded, and the file ends with one marker line giving the bytes
   dropped. Bytes are written as received; the viewer decodes them as UTF-8
   with replacement and never interprets them.
3. **Short-lived.** A run's log folder is deleted **7 days** after the run
   finished, by the same background pruning as runs, and with the run's row
   if that is pruned first. A run's history offers **Delete logs** to remove
   them at once. A log folder whose run no longer exists (a crash between the
   two deletions) is removed at the next pruning.
4. **Nowhere else.** Log content never reaches `tracing`, the application's
   log files, OTLP, error reports or `automation.db`; the database records
   only whether logs exist and their sizes. The interface reads the files
   only when the user opens a run's log.
5. **Said plainly.** The program editor, the first-run warning and every log
   view state that logs are kept for 7 days on this computer and that the
   program should not print passwords, tokens or personal data
   (specification, section 8.1.1).

This ADR amends the specification's section 6.4 and CA-13 for program logs
only, as ADR-0035 does for declared outputs.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Memory only | Nothing at rest | Lost on quit or crash, when diagnosing matters most | Q-15 |
| Memory by default, disk opt-in per job | Values at rest only by choice | One more setting; the first failure of a job has no logs | Q-15 chose always |
| Logs in `automation.db` | One file to back up | Large blobs in the history database, slower pruning, values next to the run metadata | Files are bounded, deletable on their own, and keep the database small |
| Longer retention | Older failures can be read | More exposure of whatever programs print | 7 days matches ADR-0035 |

## Consequences

- Positive: a failed or crashed run can be diagnosed after a restart; the
  database stays free of program output.
- Negative or costs: whatever a program prints stays on disk for up to
  7 days, readable by the user's account; at most 3 MiB per run, so about
  3 GiB for a thousand runs a week at the limits.
- What becomes required: tests that a stream past 1 MiB is still drained and
  ends with the marker, that folders are deleted at 7 days and with their
  run, that **Delete logs** removes them, and that no sentinel printed by a
  program reaches the application's logs, spans or `automation.db`.
- What becomes forbidden: reading log content into telemetry or error
  reports; keeping logs past their run or past 7 days.

## How to validate

The tests above, on Windows and Linux separately, with a program that prints
a synthetic sentinel and more than 1 MiB without newlines.
