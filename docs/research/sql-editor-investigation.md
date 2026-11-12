# Architectural investigation — SQL editor: autocomplete, highlighting and formatting

Date: November 4, 2026 · Branch release/0.1.0, commit c876fc3. Pruned 2026-11-11: what the investigation proposed and the project built (the catalogs, the background providers, the theme-aware highlighter, Format with sqruff, the scope model) is described in [Architecture](../reference/architecture.md), [SQL catalogs](../reference/sql-catalogs.md), [Decisions: Architecture](../decisions/architecture.md#d-0020) and [Decisions: SQL dialect catalogs](../decisions/sql-catalogs.md#d-0021). This file keeps the evidence behind the choices not recorded there (why tree-sitter and external language servers were not adopted, the measurements) and what is still open. Versions, activity and license observations are dated November 4, 2026.

**Reading markers:** [V] verified in project code, dependency source or official repositories; [I] inference from verified facts; [H] hypothesis requiring a PoC.

## 1. Outcome

The investigation found no mature embeddable Rust/GPUI library, language server or engine giving semantic completion for **all of PostgreSQL, MySQL and Databricks**: the strong engines are single-database (Postgres Language Server, Squawk) or need a Go/TypeScript runtime and omit Databricks. It proposed a hybrid (project lexer as lexical authority, tree-sitter for the cursor statement, sqruff for Format). The PoC rejected tree-sitter (section 3); the rest was built.

| Proposed | Status |
| --- | --- |
| Phase 1 — no-dependency foundations (P1–P4 highlighter, P5/P6/P9 providers off the UI thread, dialect parts and Ctrl+Space, P8 keywords/functions and ranking) | Done |
| Phase 2 — dialect catalogs (keywords, signatures, types; function hover) | Done: [D-0021](../decisions/sql-catalogs.md#d-0021); H6 passed |
| Phase 3 — structural analysis with tree-sitter | Not adopted; replaced by the token scope model (section 3) |
| Phase 4 — Format with sqruff layout and keyword-case rules | Done (`sqruff-lib` 0.44, approved 2026-11-04); fallback built in |
| Phase 5 — diagnostics and code actions | **Open, optional** (section 2) |

**P1–P11** (problems in the code as audited: whole-document relex per edit, quoted identifiers coloured as strings, 33 fixed keywords, fixed hex colours, synchronous providers, `ddl` tokenizing generically, flat `references`, no keyword candidates, no manual trigger, duplicated per-tab column caches, shallow formatting) — resolved by phases 1, 2 and 4 and the scope model; **P10** (per-tab column cache duplicated across same-connection tabs) is not re-verified here.

## 2. Still open

| Item | Detail | Constraint |
| --- | --- | --- |
| Semantic highlighting | Colour resolved tables, views, columns, aliases, CTEs and parameters through the kit's `DocumentRangeSemanticTokensProvider` (100 ms debounce, theme names resolved at paint). The scope model already builds every query's relations | Not built. Reject the kit's tree-sitter SQL highlighter: its grammar misses string backslash escapes, `#` comments and nested comments [V], disagreeing with execution splitting |
| Diagnostics, code actions | Report unresolved names only in fully introspected namespaces; FK-driven `JOIN ON`, expand `*`, qualify names. Zero false positives is the acceptance bar | Do not use `tree-sitter-sequel` for syntax errors: about a third of valid MySQL/Databricks cases end in `ERROR`. Sqruff dialect parsing or server validation may fit better [H] |
| `JOIN ON` suggestions from the cached foreign keys | `sqls` does this [V]; the cache already holds `ForeignKey` | Not built |
| User functions and procedures in completion | Needs new core `ObjectKind`s (signatures, return types, comments) and driver introspection of the selected scope | Approval required (core contract). **Inspect** reads them from the server on demand today |
| Signature help and snippets | Absent from `gpui-kit` 0.9 (`InsertTextFormat`, `sort_text`, formatting provider too) [V] | Signatures go in detail, documentation and hover; propose a `SignatureHelpProvider` upstream |
| Completion latency and memory at scale | Criterion target was completion p95 ≤ 10 ms on a 1 MB script with 500,000 objects, and a connection-shared name index within the 200 MB budget (H7) | **Not measured**; no completion row in [Performance](../standards/performance.md) |
| Static catalogs versus older servers | Entries carry version ranges; PostgreSQL 14/18 and MySQL 8.4/9.7 are the validated sources | Suggestions can be missing for older servers |

## 3. PoC results and decision on tree-sitter (November 4, 2026)

**Method.** A disposable Cargo project over `crates/sql` and `crates/sql-language`, tree-sitter 0.26.13 and `tree-sitter-sequel` 0.3.11, none added to this project. The dialect lexer masked strings, comments, parameters and non-ASCII bytes with equal-length neutral text, inserted a sentinel where no word was being typed, closed parentheses opened before the cursor, and extracted the nearest statement's relations, aliases, ancestor CTEs, derived sources and cursor clause. Compared with the completion of the time and a name index holding every table of the cases.

**Cases** were written by the implementation authors, so figures are optimistic: a 64-statement corpus (25 PostgreSQL, 19 MySQL, 20 Databricks), a 33-case set (outer cursors around subqueries, CTEs, `UNION`, derived tables, correlation, `UPDATE … FROM`, `DELETE … USING`, multi-table DML, `MERGE`) and a 24-case set written before the scope model first ran. Earlier heuristic figures were corrected after eight wrongly expected answers.

Correct tables at the cursor, complete statements:

| Path | Corpus 63 | New 33 | New 24 | Total 120 |
| --- | ---: | ---: | ---: | ---: |
| Previous one-table-list heuristic | 58 | 15 | 15 | 88 (73%) |
| tree-sitter | 62 | 22 | 20 | 104 (87%) |
| Clean tree plus heuristic fallback | 62 | 23 | 20 | 105 (88%) |
| Token scopes, first execution | 63 | 31 | 23 | 117 (98%) |
| Scope model as integrated | 63 | 33 | 24 | 120 (100%) |

Typing at the end of every prefix of the populated statements:

| Dialect | Name-expected positions | Clean tree context | Without closing parentheses | Heuristic responds |
| --- | ---: | ---: | ---: | ---: |
| PostgreSQL | 320 | 87% | 74% | 98–100% |
| MySQL | 196 | 90% | 79% | 98–100% |
| Databricks | 234 | 80% | 69% | 98–100% |

Token scopes answer every prefix without a valid tree and agree with clean tree contexts in 70–97% (disagreement samples favoured tokens or concerned positions where columns are irrelevant).

**Grammar gaps [V]:** PostgreSQL `DISTINCT ON`; MySQL `STRAIGHT_JOIN` and multi-table `DELETE`; Databricks `LATERAL VIEW`, `PIVOT`, `QUALIFY` and `MERGE … UPDATE SET *`; in every dialect `DELETE FROM t alias` ends in an error.

**Cost** (release, maintainer hardware):

| Measurement | tree-sitter | Scope model |
| --- | --- | --- |
| Statements up to about 400 bytes | p50 19 µs, p95 0.1 ms | p95 10–25 µs |
| 19.6 KB statement | p95 4.2–6.0 ms | p95 0.8–1.1 ms |
| About 100 KB | — | 3.6 ms (24 ms debug) |
| Executable weight | PoC 2.9 MB against 69.4 MB, ≤ +4% | none |

**Hypotheses.** H1 (lexer masking makes sequel usable for all three dialects) failed: 87% overall, 50–86% per dialect on the new sets. H2 (≤ 2 ms p95 for 20 KB) failed at 4.2–6.0 ms. H3/H4 (sqruff accuracy and weight) were superseded by the maintainer's approval of sqruff. H5 (window relex ≤ 1 ms/key at 10 MB) failed at 5.3 ms/key against 57 ms for a full relex; the rest was token-vector copying, later removed (see [Performance](../standards/performance.md#the-editor-with-a-large-sql-file)). H6 passed. H7 was not measured.

**Decision.** Do not adopt tree-sitter. Completion models each statement's lexer-token scopes (`crates/sql-language/src/completion/scopes.rs`) instead of one flat table list: queries (statement, subqueries, CTE bodies, each `UNION` branch), CTE and derived-table columns (first `UNION` branch, explicit column lists, `*` imports traced to the source column), outer aliases for correlation, Databricks `SEMI`/`ANTI JOIN` right-hand relations visible only in the join condition, and incomplete text (an open parenthesis keeps the query open to the statement end) with no second path. Reasons: more accurate in every set including the grammar gaps; about five times cheaper without mask, sentinel and fallback; no dependency or `links = "tree-sitter"` version coupling with `gpui-kit`; scope modelling is needed even with a tree. Regression: 121 cases in `crates/sql-language/tests/fixtures/scopes.txt` ([Testing](../standards/testing.md#completion-scopes)). **Excluded:** unaliased expressions (`count(*)`, `x::int`) are not offered; Databricks `* EXCEPT` still offers all columns; computed aliased columns have no inferred type.

**Revisit tree-sitter** only for a capability that needs a complete tree and a grammar supporting all three dialects.

## 4. Alternatives evaluated and rejected

Scores are the investigation's weighted totals (1–5; functional quality 25%, dialects 20%, Rust/GPUI integration 15%, maturity 15%, performance 10%, extensibility 10%, maintenance 5%).

**Architectures:** external language servers (A1) 2.40, ready-made libraries only (A2) 3.40, improved heuristics (A3) 3.40, hybrid (A4, tree-sitter plus custom engine) 4.05. A1 failed on integration (no LSP client in `gpui-kit`, platform binaries, third-party credentials, servers re-introspecting and ignoring the local cache and the user's introspection scope, duplicate schema caches against the 200 MB budget, no Databricks). A2 adds three lexers that disagree with ours on literals and comments.

| Feature | Option | Verdict and reason |
| --- | --- | --- |
| Completion | Postgres Language Server (`pgls_completions`) | Design reference only (sentinel, ranking, snapshot tests): PostgreSQL-only, unpublished 0.0.0 crates, own sqlx catalog |
| Completion | `sqlparser-rs` plus sentinel | Syntax-only; no partial tree on error, so an incomplete `JOIN` can invalidate the statement [I] |
| Completion | `sqls` (Go) | No `CREATE`/`ALTER`, no Databricks; own connections contradict the vault and the introspection-scope rules |
| Completion | `sql-language-server`, `dt-sql-parser`, Squawk, `duck-sqllsp`, `banshee`, `polyglot-sql` | TypeScript/JS runtime, PostgreSQL focus, too young, or for complete SQL only |
| Highlighting | `gpui-component` tree-sitter SQL | 3.30; inherits grammar defects (MySQL/Databricks escaped strings, `#` comments, PostgreSQL nested comments) and a fork cannot read one backslash as both a MySQL escape and a PostgreSQL literal |
| Highlighting | `syntect` | 2.80; generic regex grammar, no dialects |
| Highlighting | Zed's editor crates | GPL-3.0-or-later, outside `deny.toml` |
| Formatting | `sqlformat-rs` | 3.40; only Generic/PostgreSql/SQLServer dialects; kept as the fallback idea if sqruff failed |
| Formatting | `sqlparser` pretty print, `polyglot`, JS `sql-formatter`, `pgFormatter`, `sqlfluff`, `squawk-fmt` | Drop comments, may normalize syntax, lack Databricks, need Perl/Python, or PostgreSQL-only. `squawk-fmt`'s token-validation design informed the Format guard |
| Formatting | `sqruff-lib` | **Chosen** (3.90): real grammars for all three dialects; heavy dependency (`fancy-regex`, `rayon`, `icu_casemap`, deprecated `serde_yaml`); 0.x API aimed at CLI/LSP, so isolated behind the `sqruff` feature of `jovru-sql-language`. Only layout and keyword/function/type capitalization rules run; never identifier case (MySQL table names may be case-sensitive) |

**Reusable design references:** pgcli's `suggest_type`, DBeaver's semantic model, Squawk's `validate_no_dropped_tokens`, `validate_parses` and idempotence checks.

## 5. `gpui-kit` 0.9 editor facts used by the design

Verified in `gpui-base`/`gpui-component` 0.9.0 source: pluggable `InputHighlighter`; `CompletionProvider` returning a `Task`, newer requests superseding older and stale results discarded; `ShowCompletions` has no default shortcut (the project binds Ctrl+Space); hover, definition, code-action and document-colour providers; `DocumentRangeSemanticTokensProvider` with a 100 ms debounce; `DiagnosticSet`. The kit's tree-sitter highlighter (`tree-sitter-sql` feature, 2 ms synchronous parse up to 256 KB, background reparse after 150 ms) is not enabled and absent from the lockfile. Missing: signature help, snippets, `sort_text`, a formatting provider, an LSP process client.
