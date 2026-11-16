# Decisions: Automation

Why this area is built as it is. Each entry records what was decided, why, and which alternatives lost. The rules themselves are in [Automation](../reference/automation.md). Entries are numbered `D-NNNN`, and the numbers are stable.

<a id="d-0034"></a>
## D-0034: Automation in `automation.db`, run by an application-root service under a data-folder lock

*Accepted 2026-11-08 · implemented*

**Decision.** Job definitions and run history live in their own file,
`automation.db`, with forward-only migrations. The engine is a service of
the application root, so a run outlives its view, and the first instance to
open Automation takes an operating-system lock on `automation.lock`; a second
instance is read-only. See [Service and lock](../reference/automation.md#service-and-lock) and
[Storage](../reference/automation.md#storage).

**Why.** `config.db` holds the only copy of the connection profiles, while
run history grows and is pruned; a bad migration or a large prune must not
put the profiles at risk. Job definitions are the user's work, hence
`synchronous=FULL`. Recovery must never replay a run and two processes must
never recover each other's runs. The maintainer asked that a lock can never
be left behind for good: an operating-system lock is released by the system
however the process ends (crash, kill, power loss), so nothing needs cleaning
by hand and no clock matters. The limits (2 active, 1 per connection, a 300 s timeout) were the
maintainer's answers (Q-07); a run beyond them was refused at once until the
queue arrived with chains.
Quitting asks, then stops the runs and waits (about 11 s at most) so a write
is not silently interrupted and left with an unknown effect.

**Rejected.**
- Tables in `config.db`: history growth and pruning next to the only copy of
  the profiles.
- History only in memory: no history after a restart, no recovery.
- Lock by file existence or PID file: left behind by a crash, a reused PID
  can look alive.
- Lock with a lease renewed by time: a slow disk or a suspended laptop
  expires a live lock, and correctness would depend on clocks.
- A single-instance application: changes the whole application for one
  feature.
- Runs owned by their view: a run would die when its screen closes.

**Two limits of the engine, recorded with it (review items P-02 and P-05).**
The engine has no read-only mode: `reads_only` and `guard::risk` classify
text, so they warn but are not a security boundary (SQL can change session
settings such as `default_transaction_read_only`). To protect data, use a
database user without write privileges. And a delivered page does not prove
that a statement finished, so completion follows one rule, and a run whose
effect is unknown is never replayed (measured in
[Automation](../reference/automation.md#measurements)).
- Quit cancels without asking: interrupts writes silently, leaving
  `effect_unknown`.

<a id="d-0035"></a>
## D-0035: Declared outputs persisted for chains, small, opt-in and short-lived

*Accepted 2026-11-08 · implemented*

**Decision.** A job's declared scalar outputs are stored in `run_outputs`
only when the job opts in, at most 4 KiB per run, all or none, values
emptied after 7 days and their metadata after 90. Nothing undeclared is ever
kept. See [Declared outputs](../reference/automation.md#declared-outputs).

**Why.** A chain step may need an earlier step's output after a restart, and
a step run again on its own needs the outputs it was fed. But outputs are
results (a `COUNT(*)` is data and may be personal or confidential), and the
specification and the persistence rules forbade writing values to disk. The
maintainer approved the review's defaults (Q-03): 4 KiB, 7 days of values, 90
of metadata, opt-in per job and an extra confirmation for production. This
decision amends the specification's rule on what is never persisted for this
one case. A quota overflow stores nothing and does not fail the run, because
the chain can still use the values in memory.

**Rejected.**
- Outputs in memory only: no chain across restarts, and a step run again
  alone has no inputs.
- Never persist, for good: the same, permanently.
- A larger quota or longer retention: more data at rest for longer.
- Outputs as JSON inside `runs`: harder to expire values while keeping the
  metadata, and quota checks inside JSON; a table keeps each rule a column.

<a id="d-0036"></a>
## D-0036: Published job versions and chain runs

*Accepted 2026-11-08 · implemented*

**Decision.** Publishing copies a job's definition into an immutable
`job_versions` row; a chain step names `(job_id, version)`. Chains are jobs of
kind `chain`, their runs live in `chain_runs`, and each step's run stays an
ordinary `runs` row linked to it. Queued state is never persisted. See
[Versions and chain runs](../reference/automation.md#versions-and-chain-runs).

**Why.** The plan the user confirmed must be what runs: editing a job must
never change a chain already confirmed, nor a run already in the history. A
chain run has no connection, statement or counters, which `runs` is full of,
so mixing it into `runs` would add `NULL`s and weaken the `CHECK`s. A queued
run has done no I/O, so there is nothing to recover. Recovery stays one rule
per table and never replays. `jobs` is rebuilt in the migration only because
its `kind` constraint widens.

**Rejected.**
- Chains name jobs, not versions: editing a job silently changes every chain
  using it.
- Snapshot only, no published versions: nothing stops a chain picking up an
  edit between two runs, and there is no stable thing to name.
- Chain runs as rows of `runs`: more `NULL`s and weaker `CHECK`s.
- Persisting `queued`: a row for a run that did nothing.
- Resuming an interrupted chain: needs effects known up to the step and risks
  replay.

<a id="d-0037"></a>
## D-0037: External programs' logs kept on disk for 7 days

*Accepted 2026-11-08, amended 2026-11-09 (code-page decoding) · implemented*

**Decision.** Each program run writes `stdout.log`, `stderr.log` and, under
`jjp/1`, `jjp.log` in `automation/logs/<run-id>/`, at most 1 MiB each, deleted
7 days after the run ended, and never copied into `tracing`, telemetry or
`automation.db`. See [External programs and their logs](../reference/automation.md#external-programs-and-their-logs).

**Why.** A program's stdout and stderr must always be drained so a full pipe
never blocks it, and what it prints is not under Jovru's control: it may hold
input values, rows or tokens. Logs are the only way to diagnose a failed or
crashed run after a restart, so the maintainer chose to keep them always
(Q-15), bounded and with a visible warning. 7 days matches D-0035. The
amendment of 2026-11-09 shows a non-UTF-8 stream in the console's code page
(OEM on Windows, such as CP850 on pt-BR) so a console program's accents read
as written, without rewriting the file. The cost is up to 3 MiB per run, about
3 GiB for a thousand runs a week at the limits. This decision amends the
specification's rule on what is never persisted for program logs only.

**Rejected.**
- Memory only: lost on quit or crash, when diagnosing matters most.
- Memory by default, disk opt-in per job: the first failure of a job would
  have no logs.
- Logs in `automation.db`: large blobs next to the history, slower pruning,
  values beside the run metadata.
- Longer retention: more exposure of whatever programs print.

<a id="d-0038"></a>
## D-0038: External program runs in `automation.db`

*Accepted 2026-11-08 · implemented*

**Decision.** SQL and program runs share one `runs` table with a `kind`
column; columns of the other kind are `NULL` and `CHECK`s keep each kind's
required columns. A program run records its exit code or signal, process
group and start time, entrypoint hash, `fail` code and log sizes. See
[External programs and their logs](../reference/automation.md#external-programs-and-their-logs).

**Why.** History, pruning, recovery and the chain links of D-0036 stay one
code path. Recording the leader's start time with the process group lets
recovery tell a live orphan from a reused ID, which the maintainer required
(Q-29): it never ends a process whose start time does not match. The
version's file path and hash sit in the version's definition JSON because they
are part of what was published. `jobs` and `runs` are rebuilt in one
migration because a `CHECK` widens and `connection_id` becomes nullable.

**Rejected.**
- A table per kind: history, pruning, recovery and chain links split in two,
  and a chain step would point at one of two tables.
- Program details as JSON in a column: recovery and pruning would query inside
  JSON, with no constraints.
- Recording the PID instead of group and start time: a reused PID could be
  killed.
- Copying the program file into the database: the maintainer chose no copy
  (Q-16).

Programs are **pure clients** of `jjp/1` (review item P-12): the protocol has
no `execute`, `accepted` or `ping`, and the host sends only the `cancel`
notification.

<a id="d-0041"></a>
## D-0041: Schedules and their grants in `automation.db`, with runs that record the schedule

*Accepted 2026-11-08, with a change: missed times and overlap are a policy per schedule · implemented*

**Decision.** `schedules` holds one schedule per job or chain (rule, zone,
both policies, bookkeeping), `grants` and `grant_connections` hold the consent
bound to exactly what runs, and `runs` and `chain_runs` record their origin,
schedule, due time and catch-up marker. Time-zone arithmetic uses `jiff` 0.2.
See [Schedules and grants](../reference/automation.md#schedules-and-grants).

**Why.** A grant must say exactly what was consented to so any difference is
detectable without reading values, which is why a schedule stores a rule, a
zone and references and uses the version's defaults (Q-36): no input value
may be stored. A refused scheduled run leaves no run row, yet the user must
see it, so the schedule keeps counters and its last outcome, not a row per
time: a schedule every minute missed through a weekend would otherwise write
thousands of rows. The next due time is computed, because a stored one goes
stale after a zone database update, a clock change or a lock takeover. Queued
runs, waiting catch-ups and the held due time stay in memory, since a stored
catch-up would start at an unexpected moment after a restart. Origin had to
widen from `manual`, which SQLite cannot do in place, so `runs` and
`chain_runs` are rebuilt, and the origin tells who started a run. Recovery
does not change: nothing is replayed, and a catch-up is a new run for a due
time no run started for. The maintainer replaced the proposal's fixed rules
(never catch up, skip on overlap) with per-schedule policies (Q-34, Q-35),
because some jobs must run after a closed night (a daily load) or must not
lose a time while a long run ends, keeping *ignore* and *skip* as defaults;
the policies are part of the consent. Bindings are a child table so the
grants a saved connection affects come from one indexed query. The
notification choice (Q-40) changes nothing that runs, so it raises no
revision.

**Rejected.**
- Keeping `origin = 'manual'` and adding only `schedule_id`: the origin would
  lie, and no `CHECK` could tie `due_at` to it.
- Grant columns in `schedules`: consent would mix with bookkeeping, and later
  `jjp/1` capability grants would need their own table anyway.
- Bindings as JSON in the grant: finding the grants a connection affects means
  scanning every grant.
- A row per occurrence: an audit that carries nothing a counter does not.
- Storing the next due time: stale after zone, clock or lock changes.
- Storing input values with the schedule: breaks the rule that no value is
  persisted.
- Fixed rules for missed times and overlap: one behaviour could not fit every
  job.
- Storing the catch-up times waiting their turn: they would start at an
  unexpected moment after a restart, and a table per time returns.
