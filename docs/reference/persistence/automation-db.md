# Persistence: automation.db

Part of [Persistence](README.md).

## Automation (`automation.db`)

`jovru-storage` owns the file through `AutomationRepository`
(`crates/storage/src/automation.rs`); no other crate opens it. It is separate
from `config.db` because history grows and is pruned while `config.db` holds
the only copy of the connection profiles
(D-0034).

| Table | Content |
| --- | --- |
| `jobs` | A job's canonical-JSON definition (kind `sql`, `process` or `chain`) and its `revision`, which grows with every save |
| `job_versions` | A published version of a job: its frozen definition and fingerprint (D-0036). Deleting a job is refused while a chain names any of its versions |
| `runs` | One row per admitted run: the origin (`manual` or `schedule`, with the schedule, the due time and the catch-up marker), the snapshot (no values), state, cause, `effect_unknown`, counters (rows delivered and discarded, rows affected, `truncated`), the SQLSTATE or vendor code, exit code or signal, a program's `fail` code, log sizes, the program's process group and start time, and the SHA-256 of the program's entrypoint (D-0038) |
| `chain_runs` | One row per chain run, with the step that stopped it |
| `schedules` | At most one per job or chain (D-0041): the published version it runs (none for a chain), the rule as canonical JSON, the IANA zone, the missed-time policy with its N, the overlap policy, the opt-in notification, `revision` (grows with the rule, the zone or a policy), whether it is on, why it is suspended, and the bookkeeping: the last handled due time, the last outcome and reason, the run it started, the missed count and the last missed time. The next due time is never stored. Its version cannot be deleted; deleting its job deletes it |
| `grants` / `grant_connections` | A schedule's consent: the target's fingerprint (the version's, or the chain plan's), the chain's revision, the schedule revision consented to, whether a production confirmation was part of it, and each connection with its profile revision and environment label. Turning a schedule off or deleting it deletes the grant; a suspension keeps it to say what changed |
| `run_outputs` | The declared outputs of a run whose job opted in: name, kind, size and, until it expires, the value (D-0035) |

- **Configuration.** WAL, `foreign_keys=ON`, `synchronous=FULL` (it holds job
  definitions) and `secure_delete=ON`, so SQLite overwrites deleted content
  and an expired output value does not linger in the file's free pages
  (`open_pool_with`). The file opens only through the repository, after
  `PRAGMA quick_check`.
- **Migrations.** Forward-only, by `PRAGMA user_version`, each step in one
  transaction on a connection with foreign keys off, checked with
  `foreign_key_check` before the commit: 1 (jobs and runs,
  D-0034),
  2 (`run_outputs` and `runs.outputs_stored`,
  D-0035), 3 (kind `process`
  and the program's columns, D-0038;
  `jobs` and `runs` are rebuilt) and 4 (kind `chain`, `job_versions`,
  `chain_runs` and the chain columns of `runs`,
  D-0036) and 5
  (`schedules`, `grants` and `grant_connections`; `runs` and `chain_runs`
  rebuilt so the origin takes `schedule` with `schedule_id`, `due_at` and
  `catch_up`, D-0041). A file whose
  version is above 5 is refused untouched: automation is off with a message
  and the rest of the application works. There is no backup copy before a
  migration (D-0034).
- **Corruption.** A file that cannot be opened and is confirmed corrupt (not
  a database, or `quick_check` fails) is set aside with its `-wal` and `-shm`
  files (those first) as `automation.db.corrupt-<millis>` and an empty one is
  created; the job definitions in it are lost and the application says so
  (`Recovery::recreated`). Permission and I/O errors never trigger this.
  An empty marker, `automation.db.recovering`, is written before anything
  moves and removed once the new file opened: a recovery cut short by a
  failure or a crash is finished at the next open and still reported as
  recreated, never as a ready, empty file.
- **The lock.** `automation.lock` holds an exclusive operating-system lock
  (`std::fs::File::try_lock`: `flock` on Unix, `LockFileEx` on Windows) for as
  long as the engine lives. The system releases it when the process ends,
  however it ends, so a lock is **never stranded**: the empty file left on
  disk carries no meaning, and there is no PID file, timestamp or lease. A
  second instance on the same folder cannot run or save anything: its
  automation is read-only until the first one ends and **Try again** takes the
  lock over. It opens `automation.db` read-only
  (`AutomationRepository::open_read_only`), after the same `quick_check` (a
  file that fails it is an error and stays where it is), and never creates,
  migrates or sets the file aside; a file it finds missing or not yet migrated shows no
  history until the lock holder has prepared it. A process that is alive but hung keeps the lock, which is
  correct. **Network file systems may not honour the lock**, so keep the
  data folder on a local disk; when it is not, two instances could both
  believe they hold it.
- **Recovery.** The lock holder, before admitting anything, marks every run
  and chain run still `running` as `interrupted` and never replays one.
  `effect_unknown` is set on every interrupted program and on every SQL run
  that was not `reads_only`; an interrupted chain has it when any of its
  steps has. On Linux, a program run whose recorded process group still has
  a leader with the recorded start time is listed as an **orphan** for the
  user to end; a PID with another start time is never touched. Other systems
  record no start time, so they list none; on Windows the Job Object ends the
  tree with Jovru. A scheduled run left `running` becomes `interrupted` like
  any other and is never run again: a catch-up (D-0041) is always a new
  run for a due time no run started for.
- **Retention.** Pruned in the background about once an hour, in transactions
  of at most 500 rows that never block admission: output values are emptied
  7 days after their run ended (name, kind and size stay, and
  `secure_delete` overwrites the value); runs and chain runs (with their
  steps and outputs) are deleted 90 days after they ended; a program's log
  folder is deleted 7 days after its run ended, or when its run's row is gone
  (a log folder is never deleted while its run is active). **Delete logs**
  removes one run's folder at once and forgets its sizes. Schedules and grants
  are never pruned; a pruned run leaves its schedule's `last_run_id` empty.
  Missed, skipped and refused due times are counters and the last outcome on
  the schedule, never a row per time.

**What is never stored.** Input values, result cells, scalar results and
server or program messages are written to no file of automation: not to
`automation.db`, not to the application's log, not to telemetry (CA-13); the
history says what ran, where, when and how it ended. The snapshot holds the
job's definition (SQL text, parameter *specifications*, options) and the
connection's ID and revision, never a password or a value. There are two
exceptions, each bounded and visible to the user:

1. **Declared outputs** (D-0035):
   only for a job that opts in, at most 4 KiB of values per run and stored
   all or none, readable for 7 days.
2. **Program logs** (D-0037): what a
   program prints, and its `jjp/1` `log` and `fail` messages, are written
   as received under `automation/logs/<run-id>/` because they are the only
   way to diagnose it. At most 1 MiB per file, kept 7 days, never read except
   when the user opens a run's log. The user is warned that a program should
   print no secrets.
