# Automation Engine — Architecture review

**Subject:** [Automation engine specification](../plans/automation-engine-specification.md), version 1.0
**Date:** November 7, 2026
**Status:** Proposed changes, incorporated into [version 1.1](../plans/automation-engine-specification.md) of the specification; the maintainer decisions in section 5 are still open
**Codebase baseline:** `release/0.2.0`, commit `fa97819`

> This review is dated evidence. Section references (`§`) point to version
> 1.0 of the specification, which remains in the Git history; Appendix B of
> version 1.1 maps them to where each change landed.

## 1. Overall assessment

**The specification needs substantial revision before it becomes an implementation contract.**

The direction suits a local SQL workspace: native SQL without processes/TCP, sessions separate from tabs, protocol reserved for externals, stdout/stderr outside the protocol, and no claim that JJP permissions provide sandboxing. Preserve these choices.

Three problem categories:

1. **Existing-rule conflicts:** persisted query values in outputs/events/cache/artifacts conflict with [Persistence](../reference/persistence.md), audited lines 5/299. Credential fallback conflicts with [Security](../standards/security.md), line 31, and ADR-0004. SQL/migrations escape storage, UI mixes with the engine, and a second root (`Jykre Studio\jykre.db` instead of `JykreStudio\config.db`) appears. Each needs a maintainer decision or ADR, never implicit implementation resolution.
2. **Unsupported guarantees:** CA-05 read-only manifests have no driver enforcement; there is no assumed session pool, transaction_mode auto rests on inferred state (TD-001), process-tree cleanup is absent and page success is mistaken for statement completion.
3. **Scope:** RF-001–RF-010 describes an integration platform: SQL, external processes, 11-method JJP, user questions, artifacts, DAG and a native_flow expression language despite section 1.4's non-goal. The normative SFMC example needs Evolution 1 cache/SecretRef. Much duplicates reports, named parameters and backend::query_in_context.

**Recommendation:** start with a **manual SQL Job** reusing ParameterSpec/ParameterKind, driver binds, guard::risk, CancellationToken, query_in_context-style sessions and an in-memory grid. Add durable operational history in automation.db without result values and an application-owned engine service. External processes, JJP, chaining and capabilities come later with process-tree supervision, persistence ADRs and explicit grants. Automation **does not belong in 0.2.0**, whose thesis excludes new capabilities; see the [release plan](../plans/releases/0.2.0.md).

## 2. Findings by severity

Findings are ordered by severity. File and line references identify the codebase baseline above; they are evidence for the proposed changes, not current document line numbers.

### Critical

| ID | Spec section | Problem | Evidence |
| --- | --- | --- | --- |
| P‑01 | §6.3, §10.2, §12, §13, §14.3 | The engine persists query results on its own (outputs_json, events, cache, artifacts), contrary to current policy. Scalars, including COUNT(*), are results too. | `../reference/persistence.md:5`, `:299`; ADR‑0003 |
| P‑02 | §6.4, §9.3, §16.4, CA‑05 | CA-05 promises that jobs without db.write cannot write, but nothing enforces this. reads_only misses side-effecting functions; SQL can revert default_transaction_read_only; ExecutionOptions/DriverCapabilities have no read-only mode. | `crates/sql/src/lib.rs:716-726`; `crates/core/src/result.rs:34`; `src/guard.rs:1-20` |

### High

| ID | Spec section | Problem | Evidence |
| --- | --- | --- | --- |
| P‑03 | §2.1, §11, §18, §20.1 | The MVP is another product: normative cache: reuse/token_ref workflows need Evolution 1. native_flow, workflows and jobs.run are three orchestrators plus a ${} language. | `spec:741-757`, `:1391`; ADR‑0024 (reports already cover parameterized SQL) |
| P‑04 | §6.4, §17.6 | The spec assumes a session pool, transaction_mode: auto and shared_session, but user-session pooling does not exist and transaction state is inferred (TD-001). Catalog/schema context is not pinned in definitions. | `src/backend.rs:1133`, `:1222`; `../reference/architecture.md` (deviations); ADR‑0014 |
| P‑05 | §6.3, §6.5 | Statement completion is inferred from page success. Full PostgreSQL pages retain an open stream; the next command cancels through pg_cancel_backend. UPDATE … RETURNING above the limit can therefore have unknown effects, as can a memory-ceiling Stop. | `crates/driver-postgres/src/runtime.rs:780-805`, `:826`; `src/result.rs:319-321` (collector returns `SinkControl::Stop`) |
| P‑06 | §6.3 | Rows keyed by column name break duplicate names (SELECT a.id, b.id); JSON numbers lose precision, violating golden rule 9. | `crates/core/src/result.rs:308`, `:314`; `src/result.rs:45` |
| P‑07 | §7.3, §8.6, §15.4, §19.2 | Cancellation/timeouts do not terminate process trees. kill_on_drop kills only the direct child; Python/Node also spawn descendants. Unsafe is forbidden; Job Object/killpg require an approved dependency. | `Cargo.toml:32`; `crates/driver-databricks/src/auth.rs:118-120` |
| P‑08 | §15.2–15.4, §17.2 | Cancellation omits ongoing SQL/HTTP/ask operations and waits for grace before cancelling SQL. Dropping the backend::run await does not cancel the task. | `src/backend.rs:1057`; `crates/driver-postgres/src/runtime.rs:824` |
| P‑09 | §8.7, §15, §19.2 | No arbitration among complete/fail/cancel/deadline/exit, and no run owner. Recovery omits states and can interrupt another instance sharing the data directory. | `crates/storage/src/metadata.rs:886` (locking precedent) |
| P‑10 | §14, §17.1 | SQL/migrations are outside jykre-storage, UI is mixed into the engine, a different data root/database is introduced, and definitions are duplicated in files and SQLite. | `../reference/persistence.md:13`, Rules; `../reference/architecture.md:86-112`; `src/connection_store.rs:20` (`APP_DIR = "JykreStudio"`) |
| P‑11 | §1.2, §12.3 | The explicit secure credential fallback conflicts with ADR-0004; a separate secret_store implies a second vault. | `../standards/security.md:31`; `crates/secrets/src/lib.rs:1`, `:75` |
| P‑12 | §8 | JJP is too large for v1 (11 methods) and bidirectional: jobs must also act as JSON-RPC servers. ID types, multiplexing, preallocation limits, pending calls and reconnection are unspecified. | `spec:426`, `:447`, `:482`, `:492` |
| P‑13 | §2.2, §15.3, §17.4 | Missing backpressure, quotas and saturation rules. UI coalescing is not measurable. Full pipes block children; unbounded RAM collection can crash Studio. Existing UI budgets apply immediately. | `../standards/performance.md:17-21` |
| P‑14 | §11.4–11.5 | jobs.run may deadlock without recursion: at concurrency 1, the parent occupies the slot while waiting for its child. | `src/reports/run.rs:27` (current scheduler has no nesting support) |
| P‑15 | §5.2, §11.4, §16.3 | Approvals/grants are not bound to content, connection or revision. Unattended execution cannot satisfy UI password requests (needs_password) or view-level production confirmation (guard::risk). | `src/connection_store.rs:737-746`; `src/app/workspace.rs:1162`; `src/app/report_view.rs:968` |
| P‑16 | §12.2–12.3 | SecretRef authorizes the holder, not the use destination; wildcard hosts turn the broker into an exfiltration channel. | `spec:650`, `:822` |
| P‑17 | §13.3 | Path validation followed by copying leaves a TOCTOU window: symlinks, junctions and growing files. | `spec:878-880` |
| P‑18 | §7.3, §9.2, §16.5, §18.2 | Free-form job logs may enter tracing/OTLP. Example 18.2 logs external_key, an input value, through job.log. | `../standards/security.md:94`, Telemetry; `src/telemetry/filter.rs:11` |
| P‑19 | §7.2, §16.2–16.3 | Inheriting the entire child environment copies third-party secrets; an empty environment breaks Node/Python (SystemRoot/PATH/TEMP on Windows, HOME/LANG on Unix). | `spec:1128` ("where possible") |

### Medium

| ID | Spec section | Problem | Evidence |
| --- | --- | --- | --- |
| P‑20 | §5.1, §10.1 | JSON Schema adds another type system alongside ParameterKind and either a dependency or a handwritten validator. | `crates/core/src/result.rs:58`; `src/reports/mod.rs:92`; ADR‑0023 |
| P‑21 | §5.2, §4.3 | Immutable versions on every save punish daily SQL editing; rerunning masked inputs fails or resends placeholders. | ADR‑0024 (reports do not version definitions) |
| P‑22 | §15.3, §20.4 | Without pooling, each run opens a real session: 100 jobs means 100 sessions. Concurrency defaults are missing. | `../reference/ux.md` (at most 4 panels); `../standards/reliability.md` |
| P‑23 | §12.1 | Cache/singleflight does not define waiter cancellation, credential generations or expiring references. | `spec:806`, `:1480` |
| P‑24 | §5.2, §7.2 | Entrypoint hashes do not freeze executed code: local imports, node_modules and PATH-resolved runtimes remain mutable. | `spec:267`, `:389` |
| P‑25 | §5.1, §8.2, §8.7 | Manifest/protocol versioning has no normative grammar or unknown-field/method rules. | `spec:229`, `:467` |
| P‑26 | §4.1–4.2, §18.1 | UX starts with five sections and a full editor implying another Workspace. Palette execution appears in examples but is assigned to Evolution 2. | `../reference/architecture.md:102` (ADR‑0020) |
| P‑27 | — | The engine must survive closing its screen, conflicting with the current rule that dropping a view cancels its operations. | `../reference/architecture.md:440` |

### Low

| ID | Spec section | Problem | Evidence |
| --- | --- | --- | --- |
| P‑28 | Examples | YAML definitions: serde_yaml is archived and only appears transitively through sqruff. | — |
| P‑29 | §5.1, §6.1 | Examples use 30 s; the actual ExecutionOptions::default is 300 s. | — |
| P‑30 | §8.8, §9.1, §17.5 | Alternative transports, TypeScript interfaces and onExecute inflate v1; empty Transport implementations would be premature abstraction. | — |
| P‑31 | §15.3 | Per-ConnectionId limits do not serialize physical resources: two SQLite profiles can target one file, and recovery opens control connections. | — |

## 3. Proposed changes

Revise the specification as version 1.1 using the changes below. Section 4 summarizes the recommended design and its trade-offs.

### 3.1 Scope and roadmap (§1.2, §2.1, §20)

- **MVP = manual SQL Job** (P-03): immediate value using mature named-parameter, guard, session and cancellation contracts, without unused native_flow/cache/schedule/grant tables.
- Make the SFMC examples (§11.2, §12.2, §18) non-normative future scenarios. Phase acceptance so CA-03 onward cannot block CA-01/02.
- Remove native_flow as an independent executor/language. If reintroduced, compile it into the same chaining plan, never a sibling engine. Honor the no-new-language non-goal.

### 3.2 Architecture and code placement (§14, §17)

- Minimal pure IDs/states/run-record DTOs in jykre-core; AutomationRepository/migrations in jykre-storage; admission/state/executor/deadline/supervisor engine in `src/automation/`; thin views in `src/app/automation/`. No new crate or SqlJobConnectionService trait: a backend function following query_in_context suffices. Use executor/event enums and bounded Tokio channels. This obeys xtask's graph and storage-only SQLite rule.
- **automation.db** in the current data directory, respecting JYKRE_CONFIG_DIR, opened through open_pool with its own forward-only migrations. Separate from config.db because history volume/lifecycle differs from connection profiles; config.db uses synchronous=FULL and is their only copy.
- **Canonical JSON in SQLite** is the single definition source. No jobs/version file tree or YAML (P-28). Preserve unknown keys like reports; reject unsupported major versions.
- **Application-root engine service** (P-27): screens subscribe; closing them does not cancel. Application exit asks, then coordinates cancellation/shutdown before quitting. Record this deliberate deviation from “dropping a view cancels” in an ADR.

### 3.3 SQL Job (§6)

- New session per run, opened through Driver::session_connection like query_in_context, with **definition-pinned context**, never a tab session, closed afterward. One logical statement, autocommit, no pool/shared_session/managed transaction_mode (P-04). Per-run sessions prevent TD-002 set_config values leaking between runs.
- Reuse ParameterSpec/ParameterKind, parameters::bind and driver binding including classic Databricks exceptions (ADR-0023/0026). No v1 JSON Schema (P-20).
- No engine read-only mode. Rewrite CA-05: manifests cannot replace database permissions; restricted DB users protect data and guard::risk still confirms production (P-02).
- **Completion rule (P-05):** at row/memory limits, if reads_only accepts the statement, truncate, request closure and mark truncated; residual nontransactional SELECT function effects match tab behavior. Otherwise drain to a discard sink within the deadline, without collector Stop/early settle, until confirmed completion. Without confirmation, finish with effect_unknown and **never replay**.

| Driver | Current behavior | Executor responsibility |
| --- | --- | --- |
| PostgreSQL | Full page retains stream; next command calls recover/pg_cancel_backend | Fetch until complete |
| MySQL | Limit drains for up to 5 s, then may KILL QUERY; runtime.rs:633,730–751 | Consume and observe late errors before success |
| SQLite | Limit waits for idle, interrupts only at deadline, rolling back writes; runtime.rs:617–630 | Consume until completion |
| Databricks | Warehouse/classic return rows only after terminal state; has_more is local remainder | Discarding remainder cannot change commit; effect_unknown only without terminal confirmation |

Distinguishing confirmed remote completion from result delivery needs a typed DriverOutcome contract change (Q-08). Until approved, handle uncertainty conservatively.

- Future structured outputs: ordered columns, array rows, exact values as text, distinct NULL, explicit scalar mapping for zero/one/multiple rows. Never key by column name (P-06).
- Definition-stored default timeout **300 s**, from ExecutionOptions::default (P-29). Immediate CancellationToken cancellation with drivers' 5 s recovery window; grace applies only to external-process cooperation (P-08).

### 3.4 State, persistence and recovery (§14.4, §15, §19)

- Persist Running/Succeeded/Failed/Cancelled/Interrupted. Stable cause codes: Timeout is a Failed cause like tab ErrorCode::Timeout; effect_unknown is independent. Cancelling/Completing exist only in memory; Queued/WaitingInput arrive with queues/ask.
- Immutable per-run snapshot replaces published MVP versions: exact SQL, parameter specifications without values, context/effective options, a defined fingerprint representation, connection_id/revision. Admission is durable before I/O; success records only after confirmation/local commit.
- Never persist input values (Security's audited line 94), server messages, cells or scalar results. Record operational delivered-row/rows_affected counts, truncated and SQLSTATE. Full messages/pages remain bounded in memory (P-01).
- Exclusive engine lock per data directory, following metadata locking, acquired before migration/recovery/admission. Second instances remain usable but cannot run automation. Recovery marks nonterminals Interrupted, adds effect_unknown when applicable and never replays or uses leases.

### 3.5 Security (§12, §16)

- Remove credential fallback. Without a vault, no persistent job secrets; explain in UI. New secrets use new accounts in existing jykre-secrets, never another vault (P-11).
- MVP manual runs confirm like tabs. Future manual chains confirm the entire plan once, like reports: exactly the confirmed plan runs. Durable scheduling/JJP grants require explicit revocable consent bound to snapshot, connection/revision, context/environment. Never silently promote manual confirmation. Unattended runs without remembered passwords fail Authentication without dialogs. Dynamic db.query SQL is never authorized by script hash (P-15).
- Job logs are not tracing events and never reach OTLP; engine spans use allowlisted attributes only. Example 18.2 must not log inputs (P-18).
- Bind SecretRef to origin/audience/authentication type; no redirects, explicit proxy, no overriding auth headers or revealing values (P-16).
- Artifact sources are regular files within run directories; copy through a verified handle without reopening paths, count quotas while copying, stage/rename/commit/reconcile, acknowledge only after confirmation (P-17).

### 3.6 External processes and JJP (§7, §8)

- **Process-tree supervision is a prerequisite** (P-07): Windows kill-on-close Job Object; Unix process-group signals. An approved dependency is required; evaluate process-wrap's license, weight, advisories/features. No approval means no external phase. Document Unix residual risk (Studio crashes do not kill the group); recovery records group/start time, warns and never kills reused PID/PGID values.
- Spawn without shell, with argument vectors. Environment is an OS/runtime allowlist plus explicit passthrough and run JJP keys only; no default NODE_OPTIONS/PYTHONPATH or JYKRE_CONFIG_DIR inheritance. Execute snapshots, not original paths. Drain stdout/stderr into bounded files, then discard excess with markers (P-19/P-24).
- Non-JJP processes with exit-code success come before the protocol.
- Minimal jjp/1 (P-12): hello returns inputs/deadline/max_message_bytes; no execute/ping/accepted phase. Jobs are pure clients, removing bidirectional ID collisions. Host sends only cancel notifications. Methods: hello/log/progress/complete/fail. One 127.0.0.1 listener, created on the first external run rather than cold startup. NDJSON 1 MiB preallocation limit, 256-bit atomically consumed per-run token, one session/run, no reconnect/batches. Hello request/response are private token/input payloads.
- Always drain. Drop over-quota logs with a counter; control/terminal traffic is outside the quota. Persistent abuse ends the run; cancellation never depends on reading a remote terminal (P-13).
- Testable JS example in examples/, no published SDK package (P-30).

### 3.7 Chaining and future capabilities (§10–§13)

- Manual linear chain with structured from_step/from_output/to_input references, no ${} or text-inferred dependencies. Pin snapshots and confirm the full plan at startup. Release the previous session before the next step (P-14). Introduce queues here; dynamic jobs.run stays excluded.
- Separate ordinary cache and secret broker; define generation, TTL, consumer ACL reevaluation and waiter cancellation only for a concrete consumer (P-23).
- JSON Schema only when an external contract requires objects/arrays, then propose a mature library, never a handwritten subset (P-20/P-25).

### 3.8 UX and performance (§4, §19.3, §20.4)

- Initial list/detail UI has Editor, Inputs, Runs tabs. Reuse the dialect-aware component through src/app/sql_editor, not another Workspace. SQL-tab execution shortcuts never trigger jobs. Text belongs in locales/; identify draft runs in history (P-26).
- Coalesce subscriptions to state/progress/counters; virtualize lists. Existing [Performance](../standards/performance.md) budgets apply immediately: frame ≤ 8 ms p95, stall ≤ 16 ms, publication ≤ 2 ms. No v1 engine metrics (P-13).

## 4. Recommended design and trade-offs

The following design choices are recommendations for the revised specification. They require the approvals listed in section 5; listing them here does not authorize implementation.

| ID | Recommended design | Trade-off |
| --- | --- | --- |
| D01 | First delivery: manual SQL Job. Exclude flow/jobs.run/cache/SecretRef/broker/HTTP/KV/scheduling/palette/visual editor/packages/SDK/TLS/UDS/pipes/sandbox/agent | Scripts/connectors wait; golden rules remain and initial schema has no unused tables |
| D02 | No automation in 0.2.0 | Delays capability; protects daily-use thesis |
| D03 | Spec v1.1 first: non-normative SFMC, phase acceptance, rewritten CA-05 | One additional document iteration |
| D04 | Core DTOs, storage repository, src/automation engine, src/app/automation views; no new crate; enum executor | jykre grows; a future GPUI-free binary may need extraction |
| D05 | Separate automation.db, open_pool, own migrations and ADR | Another backup/migration file; history separate from profiles |
| D06 | Canonical SQLite JSON, no file tree/YAML | Less hand-readable; one source of truth |
| D07 | Application-root service; screen closure does not cancel; coordinated application shutdown does | ADR records deviation from view-owned cancellation |
| D08 | New run session, pinned context, autocommit, one statement | More connections; no residual state/new pool |
| D09 | ParameterSpec/ParameterKind plus driver binding; no JSON Schema | No nested v1 inputs |
| D10 | No engine read-only mode; rewrite CA-05 | Fewer advertised guarantees, no false promises |
| D11 | Reads truncate; other statements drain or finish effect_unknown; never replay | Large writes cost time/I/O; no false success |
| D12 | Definition timeout 300 s; immediate cancellation; timeout differs from cancellation | Long jobs depend on visible Stop |
| D13 | Two active global runs, one per ConnectionId, immediate refusal | Less parallelism; revise defaults after measurement |
| D14 | Running/Succeeded/Failed/Cancelled/Interrupted plus cause/effect_unknown | Transient labels stay in memory |
| D15 | Immutable run snapshot; durable admission before I/O | Small SQL duplication; no versioning product |
| D16 | Never persist inputs, server messages, cells or scalars | History shows what/when/outcome, not returned values |
| D17 | Engine lock per data directory; second instance cannot execute; recovery never replays | Second window cannot run jobs |
| D18 | Job logs outside tracing/OTLP; allowlisted spans; no v1 metrics | Less remote diagnosis |
| D19 | Approved process-tree supervision prerequisite | External phase waits for dependency approval |
| D20 | No shell, allowlisted environment, executed snapshot, bounded drained pipes | Exotic runtimes need passthrough configuration |
| D21 | Non-JJP processes before JJP | — |
| D22 | Minimal jjp/1: hello inputs, no execute; host only cancels | Reusable workers require a new protocol version |
| D23 | Always drain; drop excess logs; abuse terminates | Excess logs lost with a counter |
| D24 | Testable JS example, no package | Authors copy the example until stabilization |
| D25 | Structured linear chain, one confirmed plan, 8/4/60 s queue | More verbose YAML/JSON than ${}, no language to maintain |
| D26 | Durable grants only through explicit revocable consent; db.query only with real enforcement | More scheduling/JJP modeling |
| D27 | Artifact exception ADR, verified handle, quotas/reconciliation | More careful platform-specific implementation |
| D28 | Separate caches; memory secrets; destination-bound SecretRef; existing-vault accounts | More restrictive broker |
| D29 | No ADR means zero persisted values. Proposed defaults: 4 KiB/run, values 7 days, metadata 90 days, job opt-in, extra production job/connection confirmation | Cross-app-session chaining limited to small values |

## 5. Decisions required from the maintainer

Project policy requires maintainer approval for schema changes, core contracts, ADRs and dependencies. Resolve these questions before starting the phases they block.

| ID | Question | Review recommendation | Blocks |
| --- | --- | --- | --- |
| Q-01 | First indispensable flow: reusable manual SQL, SQL → script or SFMC/HTTP connector? | Manual SQL, D01 | Phase 0 |
| Q-02 | Approve automation ADR: automation.db, minimal schema, recorded run data D05/D14–D16? | Approve as described | Phase 1 |
| Q-03 | Allow declared scalar-output persistence by ADR, with which defaults? | D29; no values without ADR | Chaining |
| Q-04 | Approve minimal pure automation DTOs in core? | Approve | Phase 1 |
| Q-05 | Application service and coordinated quit, deviating from audited architecture line 440? | Accept D07 | Phase 1 |
| Q-06 | Second instance cannot run automation in same data directory? | Accept D17 | Phase 1 |
| Q-07 | Initial 2 global/1 per connection/refusal and 300 s timeout? | Confirm, measure and revisit | Phase 1 |
| Q-08 | Typed remote-completion metadata in DriverOutcome or conservative uncertainty? | Conservative phase 1; contract when measured need appears | Phase 1, optional |
| Q-09 | Keep native_flow product or replace with linear chaining? | Replace; if revived, compile to same plan | Roadmap |
| Q-10 | Explicitly trusted external code, with UI explaining JJP grants protect Studio APIs rather than containing processes? | Yes; warn on import/first run | Phase 3 |
| Q-11 | Evaluate/approve process-supervision dependency, candidate process-wrap? | Formal license/weight/advisory/feature proposal | Phase 3 |
| Q-12 | Environment IPC tokens visible to same-user processes during jobs? | Accept with hello consumption and documented risk | Phase 4 |
| Q-13 | Keep SDK outside repository until jjp/1 stabilizes? | Yes, D24 | Phase 4 |
| Q-14 | Durable scheduling consent and execution while app closed? | Design with scheduling | Evolution |

## 6. Proposed implementation plan

Every phase ends with cargo xtask check, adding full mode for driver/execution-contract/dependency changes, synthetic fixtures and diff review. UI phases need separate flow/keyboard/focus checks and affected-path budget measurements. No dependency without approval.

| Phase | Priority | Delivery | Exit criterion |
| --- | --- | --- | --- |
| 0 — Decisions | P0 | Q-01…Q-07, automation ADR for persistence/service/lock, section 3 spec v1.1 | Approved ADR, conflict-free spec, initial DDL only for phase 1 |
| 1 — Durable SQL slice | P0 | Core DTOs, repository/migration, automation.db, lock, durable admission, D14 states; new session/pinned context/current bind/one statement/production guard/password flow; immediate cancellation, timeout, D11 completion, Interrupted recovery | Tests: bind with ::/comments/strings/dollar quotes, multi-statement rejection, unchanged open tab transaction CA-02, duplicate columns, decimal precision, RETURNING above limit, no replay after timeout/cancel, disk failures before/after commit, second-instance lock, zero input/result values in SQLite |
| 2 — Minimal UI | P0 | List/detail Editor/Inputs/Runs, ParameterKind form, in-memory grid, Stop, password/risk prompts, localized text | Create/run/cancel/error/navigation/keyboard/focus visual checks; measured stall ≤ 16 ms and publication ≤ 2 ms; usable SQL Job MVP |
| 3 — External process without JJP | P1 | Approved tree supervisor Q-11, shell-free spawn, environment allowlist, snapshot, bounded pipes, exit-code success | Kill grandchildren on cancel/timeout; ignored signals/retained pipes cannot trap Studio; parent sentinel not inherited; newline-free spam cannot exhaust RAM/disk; validate per OS, never infer Linux/macOS from Windows |
| 4 — Minimal JJP | P1 | One listener; hello inputs; log/progress/complete/fail/cancel; quotas; JS example | Framing chunks/UTF-8/preallocation cap; invalid/reused tokens, duplicate hello, conflicting/duplicate terminals, disconnect, saturated-queue cancellation, method allowlist, sentinels absent from logs/OTLP |
| 5 — Linear chain | P1 | Structured references, pinned snapshots, whole-plan confirmation, 8/4/60 s queue, published versions, memory outputs or Q-03-approved persistence | SQL → SQL/process; reject cycles/missing inputs; cascade cancellation/failure; global limit 1 progresses without deadlock |
| 6 — On-demand capabilities | P2 | D27 artifacts, ask/confirm WaitingInput, D26 db.query; ADRs for disk/vault/schema/dependencies | P-17 TOCTOU/quota/copy-index crash requirements; questions cannot block readers/cancellation; late answers never alter runs |
| 7 — Evolution | P3 | Scheduling with consent/timezone/overlap/missed triggers, D28 cache/broker, HTTP, palette, SDK, transports, sandbox, agent | Complete each capability's semantics before stable schema/protocol |

**Excluded until explicit decision:** native_flow, dynamic jobs.run, subworkflows, retry/compensation, JSON Schema, loopback TLS, alternative transports and execution while the application is closed.
