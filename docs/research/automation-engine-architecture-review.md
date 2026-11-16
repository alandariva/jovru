# Automation Engine — Architecture review

**Subject:** [Automation engine specification](../plans/automation-engine-specification.md), version 1.0
**Date:** November 7, 2026 (pruned 2026-11-11)
**Status:** Incorporated into [version 1.1](../plans/automation-engine-specification.md) of the specification and implemented for 0.3.0 (see [Automation](../reference/automation.md) and [Decisions: Automation](../decisions/automation.md)). This file keeps only the findings still open, the design requirements for the phases not built yet, and one-line stubs so that the `P-xx`, `Dxx` and `Q-xx` IDs cited by the specification still land. The full 1.0 critique remains in the Git history.
**Codebase baseline:** `release/0.2.0`, commit `fa97819`

> Section references (`§`) point to version 1.0 of the specification, which
> remains in the Git history; Appendix B of version 1.1 maps them to where
> each change landed.

## 1. Overall assessment

Direction kept: native SQL without processes or TCP, sessions separate from
tabs, the protocol reserved for external programs, stdout/stderr outside it,
and no claim that `jjp/1` permissions are a sandbox. The review's central
recommendation (a manual SQL Job first, durable history in `automation.db`
without values, an application-owned engine service, external processes and
chaining only after process-tree supervision) was adopted; the maintainer
widened the first delivery to SQL → script (Q-01).

## 2. Findings by severity

All findings were acted on or deliberately deferred. Disposition per ID:

| ID | Finding (short) | Disposition |
| --- | --- | --- |
| P‑01 | Engine persisted query results (outputs, events, cache, artifacts) against Persistence policy | Resolved: nothing persisted except declared outputs ([D-0035](../decisions/automation.md#d-0035)) and program logs ([D-0037](../decisions/automation.md#d-0037)); reference Rules |
| P‑02 | CA-05 read-only manifests had no enforcement | Resolved: no engine read-only mode; CA-05 rewritten (plan §4.5) |
| P‑03 | MVP was another product (cache, `native_flow`, `jobs.run`, `${}`) | Resolved: phased scope; `native_flow` removed; linear chain only |
| P‑04 | Assumed session pool, `transaction_mode: auto`, unpinned context | Resolved: new session per run, pinned context, autocommit, one statement |
| P‑05 | Completion inferred from page success | Resolved: completion rule, `effect_unknown`, never replay; measured in [Automation](../reference/automation.md#measurements) |
| P‑06 | Rows keyed by column name; JSON numbers lose precision | Resolved: ordered columns, exact text values, NULL distinct |
| P‑07 | Cancellation does not end process trees | Resolved: `process-wrap` ([D-0044](../decisions/stack-and-dependencies.md#d-0044)); Windows validation pending |
| P‑08 | Cancellation order for SQL/HTTP/ask; grace before SQL cancel | Resolved: immediate SQL cancellation, grace only for programs |
| P‑09 | No arbitration among terminal events; no run owner | Resolved: run owner decides the first observed event; lock per data folder |
| P‑10 | SQL outside `jovru-storage`, UI mixed in, second data root | Resolved: [D-0034](../decisions/automation.md#d-0034) |
| P‑11 | Credential fallback vs D-0004 | Resolved: removed |
| P‑12 | JJP too large and bidirectional | Resolved: minimal `jjp/1`, jobs are pure clients |
| P‑13 | No backpressure or quotas | Resolved: bounded channels, log quotas, always drain |
| P‑14 | `jobs.run` deadlock at concurrency 1 | Resolved: no dynamic `jobs.run`; chains release the previous session |
| P‑15 | Approvals not bound to content; unattended runs vs password and production prompts | Resolved for schedules ([D-0041](../decisions/automation.md#d-0041) grants); open for `db.query` grants (section 3) |
| P‑16 | SecretRef authorizes the holder, not the destination | Open, phase 7 (section 3) |
| P‑17 | TOCTOU between path validation and copy | Open, phase 6 (section 3) |
| P‑18 | Job logs may reach tracing/OTLP | Resolved: logs on disk only, never in telemetry |
| P‑19 | Environment inheritance | Resolved differently: inherit minus a denylist (Q-27) |
| P‑20 | JSON Schema as a second type system | Deferred: `ParameterSpec`/`ParameterKind`; a mature library only if an external contract needs objects |
| P‑21 | Immutable version on every save | Resolved: versions only on publish ([D-0036](../decisions/automation.md#d-0036)) |
| P‑22 | One session per run without pooling; no defaults | Resolved: 2 active, 1 per connection, queue 8/4/60 s; **revisit after measurement** |
| P‑23 | Cache/singleflight semantics | Open, phase 7 (section 3) |
| P‑24 | Entrypoint hash does not freeze executed code | Accepted: hash plus `PlanChanged`; no copy of the file (Q-16, [D-0038](../decisions/automation.md#d-0038)) |
| P‑25 | No grammar for manifest/protocol versions | Deferred with `jjp/1` stabilization |
| P‑26 | UX implied another Workspace | Resolved: list/detail with Editor, Inputs, Runs; editor reused |
| P‑27 | Engine must survive closing its screen | Resolved: application-root service ([D-0034](../decisions/automation.md#d-0034)) |
| P‑28 | YAML (`serde_yaml` archived) | Resolved: canonical JSON in SQLite |
| P‑29 | 30 s vs the 300 s default | Resolved: 300 s |
| P‑30 | Transports and TypeScript interfaces inflate v1 | Resolved: JS example in `examples/jjp/`, no SDK |
| P‑31 | Per-`ConnectionId` limits do not serialize physical resources (two SQLite profiles on one file) | **Open, documented**: [Automation](../reference/automation.md#service-and-lock) says limits count profiles |

## 3. Still open: requirements for phases 6 and 7

Excluded until an explicit decision (plan §9.6): `native_flow`, dynamic
`jobs.run`, subworkflows, retry/compensation, JSON Schema, loopback TLS,
alternative transports and execution while the application is closed. Each
capability below needs its own ADR first; these are the review's constraints
for it.

- **`db.query` grant (P-15).** Dynamic SQL is never authorized by a script's
  hash; allow it only with real enforcement. Grants are explicit, revocable
  and bound to snapshot, connection and revision, context and environment
  (D26). Never promote a manual confirmation into a durable grant.
- **Artifacts (P-17, D27).** Sources are regular files inside the run
  directory; copy through a verified handle without reopening the path
  (symlinks, junctions and growing files), count quotas while copying, then
  stage, rename, commit and reconcile; acknowledge only after confirmation.
  Needs its own exception ADR for writing to disk. Test: TOCTOU, quota and
  a crash between copy and index.
- **Secret broker and cache (P-16, P-23, D28).** Keep the ordinary cache and
  the secret broker separate. Secrets stay in memory (or in new accounts of
  the existing vault, never a second vault). Bind a SecretRef to origin,
  audience and authentication type; no redirects, explicit proxy, no
  overriding auth headers, never reveal the value. Define generation, TTL,
  consumer ACL re-evaluation and waiter cancellation only for a concrete
  consumer.
- **`ask`/confirm (`WaitingInput`).** Questions must not block readers or
  cancellation; late answers never alter a run; a declined answer is a
  business outcome, not an infrastructure failure.
- **Same-user visibility of the run token (Q-12).** Accepted with token
  consumption at `hello`; the risk stays documented.
- **Windows and Databricks validation** (plan Q-23, Q-24) is tracked in the
  specification's checklist, not here.

## 4. Recommended design and trade-offs

The plan cites these rows ("each D-ID cited here applies that row"). Status
is as of 2026-11-11; "see D-00xx" means the decision entry carries the
reasoning and rejected alternatives.

| ID | Recommended design | Status |
| --- | --- | --- |
| D01 | First delivery a manual SQL Job; flow, `jobs.run`, cache, SecretRef, broker, HTTP, KV, palette, visual editor, packages, SDK, TLS, sandbox, agent excluded | Resolved: widened to SQL → script (Q-01); schedules added ([D-0041](../decisions/automation.md#d-0041)) |
| D02 | No automation in 0.2.0 | Resolved: shipped in 0.3.0 |
| D03 | Spec v1.1 first: non-normative SFMC, phase acceptance, CA-05 rewritten | Resolved |
| D04 | Core DTOs, storage repository, `src/automation`, `src/app/automation`; no new crate; enum executor | Resolved ([Automation](../reference/automation.md#where-it-lives)) |
| D05 | Separate `automation.db`, `open_pool`, own migrations | Resolved: [D-0034](../decisions/automation.md#d-0034) |
| D06 | Canonical JSON in SQLite, no file tree or YAML | Resolved |
| D07 | Application-root service; screen closure does not cancel; coordinated quit | Resolved: [D-0034](../decisions/automation.md#d-0034) |
| D08 | New run session, pinned context, autocommit, one statement | Resolved |
| D09 | `ParameterSpec`/`ParameterKind` plus driver binding; no JSON Schema | Resolved |
| D10 | No engine read-only mode | Resolved (plan §4.5) |
| D11 | Reads truncate; other statements drain or end `effect_unknown`; never replay | Resolved; drain times measured |
| D12 | 300 s timeout; immediate cancellation; timeout differs from cancellation | Resolved |
| D13 | Two active globally, one per connection, refusal | Resolved, then extended with a queue (D25) |
| D14 | Running/Succeeded/Failed/Cancelled/Interrupted plus cause and `effect_unknown` | Resolved |
| D15 | Immutable run snapshot; durable admission before I/O | Resolved |
| D16 | Never persist inputs, server messages, cells or scalars | Resolved, amended by [D-0035](../decisions/automation.md#d-0035) and [D-0037](../decisions/automation.md#d-0037) |
| D17 | Engine lock per data directory; second instance read-only; recovery never replays | Resolved: [D-0034](../decisions/automation.md#d-0034) |
| D18 | Job logs outside tracing/OTLP; allowlisted spans; no v1 metrics | Resolved |
| D19 | Process-tree supervision as prerequisite | Resolved: [D-0044](../decisions/stack-and-dependencies.md#d-0044) |
| D20 | No shell, bounded drained pipes, executed snapshot | Resolved except the snapshot (P-24) and the environment (denylist, Q-27) |
| D21 | Non-JJP processes before JJP | Resolved |
| D22 | Minimal `jjp/1`: hello inputs, no `execute`; host only cancels | Resolved |
| D23 | Always drain; drop excess logs; abuse ends the run | Resolved |
| D24 | Testable JS example, no package | Resolved: `examples/jjp/` |
| D25 | Structured linear chain, one confirmed plan, 8/4/60 s queue | Resolved: [D-0036](../decisions/automation.md#d-0036) |
| D26 | Durable grants only through explicit revocable consent; `db.query` only with real enforcement | Resolved for schedules ([D-0041](../decisions/automation.md#d-0041)); `db.query` open (section 3) |
| D27 | Artifact exception ADR, verified handle, quotas | Open (section 3) |
| D28 | Separate caches; memory secrets; destination-bound SecretRef | Open (section 3) |
| D29 | Persisted-value defaults: 4 KiB per run, values 7 days, metadata 90 days, opt-in per job, extra production confirmation | Resolved: [D-0035](../decisions/automation.md#d-0035) |

## 5. Decisions required from the maintainer

Q-01…Q-14 of this review were answered on 2026-11-08 and are recorded,
together with Q-15…Q-32, in
[section 12 of the specification](../plans/automation-engine-specification.md#12-decisions-required-from-the-maintainer).
Q-08 (typed remote-completion metadata in `DriverOutcome`) closed as
"conservative, no contract change" after the drain measurements; Q-14
(durable scheduling consent) was answered by [D-0041](../decisions/automation.md#d-0041).
