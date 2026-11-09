# Specification — Open SQL dialect catalog data format

**Approval:** Proposed, draft 1 · November 4, 2026.
**Implementation:** Partial: local catalog generation/loading exists; the separate repository and publishing pipeline remain proposed.

This originally defined a **separate** repository publishing keywords, built-in functions and types per dialect, with entry validity ranges. It was intended as that repository's SPEC.md. The opening “nothing has been created” statement described the original proposal snapshot; section 10.3 records later local adoption. See the [current reference](../reference/sql-catalogs.md) for implemented behavior and the [editor investigation](../research/sql-editor-investigation.md) for context.

Markers: **[V]** verified in the cited source; **[H]** hypothesis requiring implementation validation. Source/version/license observations below belong to the original investigation date.

## 1. Goal

Ready-to-consume, language-independent data for dialect-aware autocomplete, highlighting, formatting, hover and identifier quoting.

Format requirements:

1. **Plain text, no specific library:** read JSON Lines one line at a time.
2. **Incremental:** new database versions add/close only changed entries; unchanged microversions update the manifest only.
3. **Readable diffs:** each change is an added/changed Git line; every PR gets human review.
4. **Per-entry provenance/license:** all data identifies its source; distribute text only where licensing permits.

## 2. Scope

**Format version 1:** PostgreSQL, MySQL, Databricks SQL; keywords, overloaded built-in functions, types.

**Out of scope:** user tables/functions, complete manuals, grammar rules such as words allowed after FROM, online services/APIs. The repository publishes files only.

Operators and other dialects may be added compatibly later (6.1).

## 3. Repository layout

```text
SPEC.md                    # this document
LICENSE                    # original-content license (CC0-1.0, section 7.3)
schema/                    # JSON Schema for each file type
  manifest.schema.json
  keyword.schema.json
  function.schema.json
  type.schema.json
postgresql/
  manifest.json
  keywords.jsonl
  functions.jsonl
  types.jsonl
  LICENSE-postgresql       # notice required for extracted text
mysql/
  manifest.json
  keywords.jsonl
  functions.jsonl
  types.jsonl
databricks/
  manifest.json
  keywords.jsonl
  functions.jsonl
  types.jsonl
  NOTICE-spark             # if Apache Spark text is included
generator/                 # generation/update code (outside the format)
CHANGELOG.md               # corrections and format changes
```

Dialect directories are **self-contained**; PostgreSQL-only consumers read only postgresql/. Generators copy shared data such as Databricks' Spark inheritance instead of cross-directory references.

## 4. Version model

### 4.1 Per-dialect version scheme

| Dialect | version_scheme | Tracked granularity | Example | Reason |
| --- | --- | --- | --- | --- |
| PostgreSQL | numeric | Major | 18 | pg_proc.dat, kwlist.h, pg_type.dat have identical blobs in 17.0/17.11 and 18.0/18.6 [V] |
| MySQL | numeric | Release | 9.7.0, 26.10.0 | Functionality changes in point releases |
| Databricks | date | Observation date | 2026-11-04 | SQL engine continuously changes without published engine versions |

**Comparison:** numeric components are integers, padding shorter versions with zeros: 18 = [18,0] < 18.6 = [18,6] < 19. ISO YYYY-MM-DD dates compare lexically.

### 4.2 Entry validity

Optional since/until define **half-open** ranges: since ≤ t < until.

- Missing since means the first manifest version; missing until means still valid.
- Both endpoints must occur in manifest.versions.
- Same-key ranges **never overlap**.
- Never delete entries because the database changed; close them with until.

### 4.3 versions and verified_through

- versions: sorted generated versions, including the initial version and only later versions that **changed** data.
- verified_through: latest checked version. Processing is sequential, so **every** preceding version was checked, including unchanged releases absent from versions.

### 4.4 Resolving a requested version

For server version r:

1. Choose greatest t in versions with t ≤ r; e.g. [14,15,16,17,18] resolves 18.6 to 18.
2. Before the first tracked version: use the first and warn “version predates tracked versions”.
3. Beyond verified_through: use the last and warn “version not yet verified”.
4. Unknown server version: use the last.

An entry applies when (since absent or since ≤ t) and (until absent or t < until).

## 5. Files

### 5.1 General rules

- UTF-8 without BOM, LF and final newline.
- JSONL: one canonical JSON object per line, [RFC 8785](https://www.rfc-editor.org/rfc/rfc8785) JCS, lexically sorted keys, no whitespace; identical information gives identical text.
- Sort records by key, then since.
- Omit default-valued fields: false, empty lists, absent values.
- Lowercase key words/names; consumers choose display spelling.
- Human-readable manifest JSON: sorted keys, two-space indentation.
- Optional new fields do not change schema_version; removing/changing meaning does.

Common fields:

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| since | string | No | Section 4.2 |
| until | string | No | Section 4.2 |
| source | string | Yes | manifest.sources ID identifying generation source or curated |
| note | string | No | Short project-written note |

### 5.2 manifest.json

| Field | Type | Meaning |
| --- | --- | --- |
| schema_version | integer | Format version, 1 |
| dialect | string | Identifier matching directory name |
| name | string | Display name |
| version_scheme | numeric or date | Section 4.1 |
| versions | string list | Section 4.3 |
| verified_through | string | Section 4.3 |
| series_components | integer | Components forming documentation series: PG 1 gives 18, MySQL 2 gives 8.4 |
| doc_base | string | Documentation URL template with {series} |
| files | object | Filename per record type |
| sources | list | Sources below |
| generator | object | Generator name/version |
| updated_at | string | ISO date of last change |

Each source:

| Field | Meaning |
| --- | --- |
| id | Referenced by source/summary_source |
| title | Short description |
| url | Origin, optionally with {series} |
| license | SPDX ID, proprietary, or facts for text-free server facts |
| redistributable | Whether source **text** may be included in summaries/documentation |
| notice | Required notice file within dialect directory |

Example:

```json
{
  "dialect": "postgresql",
  "doc_base": "https://www.postgresql.org/docs/{series}/",
  "files": {
    "functions": "functions.jsonl",
    "keywords": "keywords.jsonl",
    "types": "types.jsonl"
  },
  "generator": { "name": "catalog-gen", "version": "0.1.0" },
  "name": "PostgreSQL",
  "schema_version": 1,
  "series_components": 1,
  "sources": [
    {
      "id": "kwlist.h",
      "license": "PostgreSQL",
      "notice": "LICENSE-postgresql",
      "redistributable": true,
      "title": "Parser keyword list",
      "url": "https://github.com/postgres/postgres/blob/REL_{series}_STABLE/src/include/parser/kwlist.h"
    },
    {
      "id": "pg_proc.dat",
      "license": "PostgreSQL",
      "notice": "LICENSE-postgresql",
      "redistributable": true,
      "title": "Built-in catalog functions",
      "url": "https://github.com/postgres/postgres/blob/REL_{series}_STABLE/src/include/catalog/pg_proc.dat"
    },
    {
      "id": "curated",
      "license": "CC0-1.0",
      "redistributable": true,
      "title": "Project-authored content"
    }
  ],
  "updated_at": "2026-11-04",
  "verified_through": "18.6",
  "version_scheme": "numeric",
  "versions": ["14", "15", "16", "17", "18"]
}
```

### 5.3 keywords.jsonl

Key: word.

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| word | string | Yes | Lowercase keyword |
| reserved | boolean | Yes | Cannot be an unquoted table/column identifier; guides editor quoting |
| category | string | No | Original category; PG unreserved/col_name/type_func_name/reserved; last two imply reserved=true |
| bare_label | boolean | No | PG column label without AS |
| conditional | boolean | No | Reserved only in modes such as Databricks ANSI; explain in note |

Actual kwlist.h examples [V]: json_table is absent in REL_16_0 and appears in REL_17_0:

```json
{"bare_label":true,"category":"col_name","reserved":false,"since":"17","source":"kwlist.h","word":"json_table"}
{"category":"reserved","reserved":true,"source":"kwlist.h","word":"order"}
{"bare_label":true,"category":"reserved","reserved":true,"source":"kwlist.h","word":"select"}
```

order is AS_LABEL, so bare_label=false is omitted.

### 5.4 functions.jsonl

Key: id. **One entry per overload.**

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| id | string | Yes | Stable name(type,type,…) with known canonical dialect types, e.g. PG typname; without formal types use signature parameter names |
| name | string | Yes | Lowercase name |
| kind | string | Yes | scalar, aggregate, window, table, procedure |
| args | list | No | Argument name/type (types.jsonl key)/optional/variadic |
| returns | string | No | types.jsonl return-type key |
| returns_set | boolean | No | Returns a row set |
| signature | string | Yes | Display signature |
| summary | string | No | Up to 200 characters |
| summary_source | string | With summary | Text source with redistributable=true |
| doc | string | No | Path relative to doc_base or absolute URL |
| doc_file | string | No | Long repository documentation from redistributable sources only |

Actual pg_proc.dat oid 1284 example [V]:

```json
{"args":[{"type":"text"},{"type":"timestamptz"},{"type":"text"}],"doc":"functions-datetime.html","id":"date_trunc(text,timestamptz,text)","kind":"scalar","name":"date_trunc","returns":"timestamptz","signature":"date_trunc(text, timestamp with time zone, text) → timestamp with time zone","source":"pg_proc.dat","summary":"truncate timestamp with time zone to specified units in specified time zone","summary_source":"pg_proc.dat"}
```

### 5.5 types.jsonl

Key: name.

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| name | string | Yes | Canonical dialect name, PG typname |
| display | string | Yes | SQL display spelling |
| aliases | string list | No | Other accepted spellings |
| params | string | No | Accepted parameters, e.g. (precision, scale) |
| category | string | No | numeric/string/datetime/boolean/binary/json/array/composite/network/geometric/other |
| doc | string | No | Same as function documentation |

Illustrative example:

```json
{"category":"datetime","display":"timestamp with time zone","name":"timestamptz","params":"(precision)","source":"pg_type.dat"}
```

## 6. Updates

### 6.1 Generated and curated fields

Generators own source-derived fields; people curate original summaries, notes and MySQL/Databricks types. Each generator declares its fields.

- Regeneration compares **only owned fields**, preserving curated data.
- New changed-entry lines inherit curated fields.
- Never modify source: curated records automatically.

### 6.2 Weekly cycle

1. **Detect releases:** PG/MySQL through [endoflife.date](https://endoflife.date/api/v1/products/postgresql/) (PG 18.6 and MySQL 26.10/26.7/9.7 at the investigation date [V]), repository tags or official Docker images. Databricks manually or scheduled against a workspace, section 8.
2. **PG shortcut:** if new-tag kwlist.h/pg_proc.dat/pg_type.dat blobs match the last processed tag, only advance verified_through.
3. **Generate full new-version v state** in memory without validity ranges.
4. **Resolve current state** at the latest versions entry (4.4).
5. **Compare by key:** new-only adds since:v; old-only closes until:v; changed generator fields close the old line and add a new one, copying curation; unchanged does nothing.
6. **Write:** add v to versions only if data changed; always update verified_through=v and updated_at. Sorted canonical rewrites keep diffs limited to changes.
7. **Publish:** data changes always get human-reviewed PRs with summaries such as “mysql 9.8.0: +12 keywords, 3 functions changed”. Manifest-only updates remain an open direct-bot-commit versus grouped-PR decision (section 10).

### 6.3 Corrections

Fix generator/curation errors **in place**, without since/until, and record CHANGELOG.md. until means a database-version removal, never a corrected mistake.

## 7. Documentation and licenses

### 7.1 Rule

Only redistributable sources supply summary/doc_file text. Names, signatures, reserved flags and types are factual data usable from any source under this conservative project policy, not a legal opinion. Obtain qualified advice before publishing the repository.

### 7.2 Known-source status

| Source | Redistributable text? | Basis |
| --- | --- | --- |
| PG source/manual | Yes, retaining copyright/license paragraphs in LICENSE-postgresql | Legal notice permits use/copy/modify/distribution of software and documentation for any purpose [V] |
| Apache Spark source/generated docs | Yes, Apache-2.0 plus NOTICE | Repository license [V] |
| MySQL manual/help tables | No | 8.0 is not GPL and forbids publishing/distributing; 8.4/9.7 reserve ungranted rights [V] |
| Databricks docs | No without explicit license | No reuse license found; Learn is often CC BY 4.0 but Azure Databricks article repository was not found [H] |
| Server output: INFORMATION_SCHEMA.KEYWORDS, sql_keywords(), SHOW SYSTEM FUNCTIONS | Facts only; usage text follows product-documentation rules | license: facts |
| Project-authored content | Yes | CC0-1.0 |

### 7.3 Repository license

Proposed original curated data, summaries, schemas and this document use **CC0-1.0** for broad reuse. Extracted permissive text retains its license/notices in per-dialect LICENSE-*/NOTICE-*. generator/ code may use a separate MIT license.

## 8. Per-dialect generation sources

| Dialect | Keywords | Functions | Types |
| --- | --- | --- | --- |
| PostgreSQL | kwlist.h at REL_{major}_{minor}: word/category/BARE_LABEL/AS_LABEL [V] | pg_proc.dat proname/proargtypes/prorettype/prokind/proretset/descr [V]; filter internal I/O/operator functions and internal/cstring/trigger arguments [H: exact rules] | pg_type.dat plus curated typname-to-SQL mapping, e.g. timestamptz → timestamp with time zone [H] |
| MySQL | Official version Docker image, INFORMATION_SCHEMA.KEYWORDS WORD/RESERVED [V] | mysql.help_topic names/syntax lines, no descriptions [H: syntax format] | Curated |
| Databricks | Workspace sql_keywords(), Databricks SQL/DBR 13.3+ [V] | SHOW SYSTEM FUNCTIONS [V], DESCRIBE FUNCTION EXTENDED signatures only [H], Spark sql-expression-schema.md Apache-2.0 base [V] | Curated |

Databricks generation needs a workspace, run manually, e.g. monthly, or in CI with a secret token and its associated cost/risk.

## 9. CI validation

The proposed repository rejects PRs when:

1. Files fail their JSON Schema.
2. JSON is noncanonical or unsorted.
3. Same-key validity ranges overlap.
4. since/until is absent from versions or since ≥ until.
5. source/summary_source is absent from manifest sources.
6. Nonredistributable source text is included.
7. Entries disappear without until or a CHANGELOG correction.
8. Latest-version regeneration differs from committed files, detecting drift.

## 10. Releases and consumption

### 10.1 Releases

Date tags: vYYYY.MM.DD. CI attaches **generated** artifacts excluded from Git:

- snapshots/<dialect>/<version>.json with resolved keywords/functions/types, no ranges;
- Optional catalog.sqlite with the same tables/ranges;
- SHA256SUMS.

schema_version changes require CHANGELOG notice.

### 10.2 Consuming source files

```text
read <dialect>/manifest.json
t = resolved version (section 4.4)
for each line of keywords.jsonl, functions.jsonl and types.jsonl:
    keep if (since absent or since ≤ t) and (until absent or t < until)
```

Consumers avoiding ranges can use version snapshots.

### 10.3 Jykre Studio consumption

Until the separate repository exists, local catalog/ data is generated through cargo xtask catalogs; see [ADR-0021](../adr/0021-sql-catalogs-read-at-run-time.md).

- Read **at runtime**, at startup off the UI thread, from the first directory containing a catalog: JYKRE_SQL_CATALOG_DIR, catalog beside the executable (included in releases), then repository catalog in development.
- Pointing JYKRE_SQL_CATALOG_DIR at an external checkout simulates the final arrangement.
- Currently choose each dialect's latest version; server-specific filtering requires per-session driver version reporting (Q9).
- Network only through explicit xtask commands, never during build/application execution.

## 11. Open questions

| ID | Question |
| --- | --- |
| Q1 | Project name and external repository language; English helps adoption |
| Q2 | Wrap manifest in [Frictionless Data Package](https://specs.frictionlessdata.io/data-package/), datapackage.json, to reuse validators; verify JSON Lines handling |
| Q3 | Manifest-only updates: direct bot commit or grouped PR? |
| Q4 | Databricks manual runs or CI workspace? |
| Q5 | PostgreSQL internal-function filtering rules [H] |
| Q6 | MySQL help-table syntax-line format [H] |
| Q7 | Can DESCRIBE FUNCTION signatures and Spark usage text safely use Apache-2.0 source attribution? [H] |
| Q8 | Initial versions: PG 14+, MySQL 8.0 or 8.4+, Databricks from first observation? |
| Q9 | Can each Jykre session report server version for catalog filtering? |

## Sources


- PostgreSQL: [kwlist.h](https://github.com/postgres/postgres/blob/HEAD/src/include/parser/kwlist.h), [pg_proc.dat](https://github.com/postgres/postgres/blob/HEAD/src/include/catalog/pg_proc.dat), [pg_type.dat](https://github.com/postgres/postgres/blob/HEAD/src/include/catalog/pg_type.dat), [legal notice](https://www.postgresql.org/docs/current/legalnotice.html), [system information functions](https://www.postgresql.org/docs/current/functions-info.html)
- MySQL: [KEYWORDS](https://dev.mysql.com/doc/refman/8.0/en/information-schema-keywords-table.html), [server help](https://dev.mysql.com/doc/refman/8.4/en/server-side-help-support.html), legal notices [8.0](https://dev.mysql.com/doc/refman/8.0/en/preface.html), [8.4](https://docs.oracle.com/cd/E17952_01/mysql-8.4-en/preface.html), [9.7](https://dev.mysql.com/doc/refman/9.7/en/preface.html)
- Databricks: [sql_keywords](https://docs.databricks.com/aws/sql/language-manual/functions/sql_keywords), [SHOW FUNCTIONS](https://docs.databricks.com/aws/en/sql/language-manual/sql-ref-syntax-aux-show-functions), [DESCRIBE FUNCTION](https://learn.microsoft.com/en-my/azure/databricks/sql/language-manual/sql-ref-syntax-aux-describe-function), [function index](https://docs.databricks.com/aws/en/sql/language-manual/sql-ref-functions-builtin-alpha)
- Spark: [sql-expression-schema.md](https://github.com/apache/spark/blob/HEAD/sql/core/src/test/resources/sql-functions/sql-expression-schema.md)
- Formats and versions: [RFC 8785 (canonical JSON)](https://www.rfc-editor.org/rfc/rfc8785), [JSON Lines](https://jsonlines.org/), [endoflife.date](https://endoflife.date/)
