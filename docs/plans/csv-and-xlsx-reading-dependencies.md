# Proposal: `csv`, `calamine` and `encoding_rs` for reading CSV and XLSX files

**Date:** November 8, 2026
**Approval:** Approved by the maintainer on 2026-11-08, together with
[ADR-0040](../adr/0040-importing-files-into-a-table.md), with every
recommendation of the open decisions [below](#open-decisions-of-a11)
(I-1 (a), I-2 (c), I-3 (a), I-4 (b), I-5 `.xlsx` only).
**Implementation:** Not started.
**For:** [0.3.0](releases/0.3.0.md), milestone A11 (importing a CSV or Excel
file into a table)

Facts below carry their source:

- **[lock]**: the worktree's `Cargo.lock` at `release/0.3.0` (`f85ac9b`).
- **[reg]**: the crate's source in `~/.cargo/registry`.
- **[cio]**: `https://crates.io/api/v1/crates/<name>` and its
  `/<version>/dependencies`.
- **[gh]**: the GitHub API for the repository.
- **[docs]**: docs.rs for that version.

All were read on 2026-11-08. `cargo tree` could not run offline, so which
target or feature turns each crate on is read from `Cargo.lock` and **not
verified**.

## Request

Add three workspace dependencies, used only by `jykre::import` (the root
crate, beside `jykre::export`):

```toml
# [workspace.dependencies]
csv = "1.4.0"
calamine = { version = "0.38.0", default-features = false }
encoding_rs = "0.8.44"
```

`encoding_rs` is already in the tree (below), so it only becomes direct.

## 1. Why

The application exports CSV and XLSX but reads neither, and nothing in the
workspace reads them ([0.3.0](releases/0.3.0.md), A11). A11 forbids
answering a rejected dependency with a hand-written reader that narrows the
feature.

| Need | Without the crates | With them |
| --- | --- | --- |
| CSV with quotes, doubled quotes, line breaks inside quoted fields, any one-byte delimiter, a strict field count | Our own state machine and its tests (RFC 4180 and the variants Excel and other tools write) | `csv::ReaderBuilder` (`delimiter`, `quote`, `double_quote`, `escape`, `flexible`, `has_headers`, `terminator`, `comment`) **[docs]**, on `csv-core`'s allocation-free state machine |
| CSV streamed with bounded memory | Same | `Reader::read_byte_record(&mut ByteRecord)` reuses one allocation per record. The reader buffers 8 KiB by default **[reg `csv-1.4.0/src/reader.rs`]** |
| CSV that is not UTF-8 (Windows-1252, UTF-16, ISO-8859-x, Shift_JIS…) | Our own decoders | `encoding_rs`: the WHATWG Encoding Standard's encodings, with streaming decoders that report malformed input instead of replacing it (`Decoder::decode_to_utf8_without_replacement`) **[docs]** |
| XLSX: the zip container, the workbook and its relationships, the shared strings table, inline strings, styles and number formats to tell a date from a number, the 1900 and 1904 epochs, Excel's 1900-02-29 | Our own reader on `zip` (already a direct dependency) and `quick-xml` (not a dependency of ours). Several hundred lines, plus the corner cases of files written by Excel, LibreOffice, Google Sheets and libraries | `calamine::Xlsx` |
| XLSX streamed with bounded memory | Same | `Xlsx::worksheet_cells_reader(name)` gives an `XlsxCellReader`. It opens the sheet's entry as XML and reads nothing up front. `next_cell()` yields one cell at a time in file order and reuses its buffers **[docs]**, **[reg `calamine-0.38.0/src/xlsx/cells_reader.rs`]**. See [memory](#memory-and-streaming) for what stays in memory |
| A list of sheets for the user to choose | Same | `Reader::sheet_names()` **[docs]** |

The import pipeline is ours either way: choosing the file, the header row,
the mapping, batching, the session, progress and cancellation. The crates
only turn bytes into fields.

## 2. Licence

Checked against `deny.toml`'s allow-list, which includes MIT, Apache-2.0,
BSD-3-Clause and Unlicense:

| Crate | Licence | Source |
| --- | --- | --- |
| `csv` 1.4.0, `csv-core` 0.1.13 | `Unlicense/MIT` (either) | [cio] |
| `calamine` 0.38.0 | MIT | [cio] |
| `encoding_rs` 0.8.44 | `(Apache-2.0 OR MIT) AND BSD-3-Clause` | [cio] |
| `quick-xml` 0.42.0 | MIT | [cio] |
| `atoi_simd` 0.18.1, `fast-float2` 0.2.4 | MIT OR Apache-2.0 | [cio] |
| `codepage` 0.1.3 | Apache-2.0 OR MIT | [cio] |

None is GPL, AGPL or SSPL. `cargo deny --locked check` has **not been run**
with them. That is part of the implementation's `--full` gate.

## 3. Health

| Crate | Latest release | Downloads (total / recent) | Repository | Minimum Rust |
| --- | --- | --- | --- | --- |
| `csv` | 1.4.0, 2025-10-17 | 286.8 M / 63.9 M | `BurntSushi/rust-csv`: not archived, 108 open issues and PRs, last push 2026-08-04 [gh] | 1.73 |
| `calamine` | 0.38.0, 2026-11-02. Earlier: 0.37.0 (10-18), 0.36.x (07), 0.35.0 (05), 0.34.0 (03) | 17.7 M / 6.85 M | `tafia/calamine`: not archived, 52 open issues and PRs, last push 2026-11-05, 2,464 stars [gh] | 1.88 |
| `encoding_rs` | 0.8.44, 2026-10-30. 0.8.41–0.8.44 released September–October 2026 | 627 M / — | `hsivonen/encoding_rs` | 1.88 |

Ours is 1.95. No advisory is known for these versions. That is to be
confirmed by `cargo deny` with the crates in the lock.

`csv` changes rarely because it is finished, not abandoned. Its maintainer
also maintains `jiff`, `regex` and `ripgrep`. `calamine` publishes a 0.x
minor release every one to two months. Its API changed between 0.3x
releases (the cells reader gained `next_cell_with_formula`), so upgrades
are their own small task.

## 4. Weight

| | Crates |
| --- | --- |
| Already in `Cargo.lock` | `encoding_rs` 0.8.44 (through `reqwest` 0.13, `gpui-pre-reqwest` and `xim-ctext`), `zip` 9.3.0 (ours, `rust_xlsxwriter`, `self_update`), `byteorder`, `log`, `serde`, `itoa`, `ryu`, `serde_core`, `memchr` [lock] |
| New | `csv`, `csv-core`, `calamine`, `atoi_simd`, `fast-float2`, `codepage`, and **`quick-xml` 0.42.0** [lock, cio] |
| Duplicates | `quick-xml` 0.41.0 is in the lock through `wayland-scanner` and `xcb` (Linux), so Linux would build 0.41 and 0.42 and `cargo deny` warns (`multiple-versions = "warn"`). On Windows and macOS 0.42 is the only copy. Which targets compile 0.41 is inferred, **not verified** |
| `zip` features | `calamine` asks for `zip` 9 with `deflate`. `self_update`'s `compression-zip-deflate` probably compiles it already. **Not verified**; at worst it adds the `flate2` decoder, already in the tree |

Nothing here brings a second async runtime, TLS stack or date library.
`calamine`'s `chrono` feature stays off (section 5). The new code is a CSV
state machine, an OOXML reader and two number parsers.

## 5. Features

- `csv`: default features (it has none that pull optional dependencies
  [cio]).
- `calamine`: `default-features = false`. Its default is already empty
  (`default = []`) [cio]. `dates` and `chrono` stay off: dates come as
  `ExcelDateTime`, whose `to_ymd_hms_milli()` gives the calendar date and
  time without `chrono` **[docs]**. `picture` stays off.
- `encoding_rs`: default features. `fast-legacy-encode` and `simd-accel`
  stay off: encoding is not needed and `simd-accel` requires nightly
  **[docs]**.

## Memory and streaming

- **CSV:** one record at a time, in a reused `ByteRecord`. The decoder sits
  between the file and the reader with fixed buffers. Memory is bounded by
  the reader's buffer plus the longest record. The import refuses a record
  above 64 MiB rather than grow without limit: `csv` reads a quoted field
  until its closing quote, so a missing quote would otherwise swallow the
  file.
- **XLSX:** the cells reader streams the sheet. Three things are read whole
  when the workbook opens [reg `src/xlsx/mod.rs`]:
  - **the shared strings table**, into a `Vec<String>`, because a
    shared-string cell is only an index into it. Each distinct text value is
    held once, so a workbook whose text is mostly repeated is cheap, and one
    with millions of distinct strings is not;
  - the styles;
  - the workbook's list of sheets.

  So the import checks the size of `xl/sharedStrings.xml` with `zip` (a
  direct dependency) **before** handing the file to `calamine`. It refuses a
  workbook whose uncompressed table exceeds 256 MiB, the per-page memory
  ceiling of [Results](../reference/results.md#limits), and says so,
  suggesting a CSV. `worksheet_range` is never used: it collects every cell
  of the sheet into a `Vec` [reg].
- **Cells arrive in file order.** In files written by Excel and the usual
  libraries that is row-major. A cell whose row goes backwards stops the
  import with its address, never a silent reordering.
- `.xls`, `.xlsb` and `.ods`: `calamine` reads `.xls` and `.ods` whole when
  it opens them (`ods.rs` builds every sheet at open; `.xls` from the
  source's structure, **not verified**). `.xlsb` has a cells reader too.
  This proposal covers **`.xlsx` only** (decision I-5).

## Encodings

- The user chooses the encoding from `encoding_rs`'s list. UTF-8 is the
  default. A byte-order mark (UTF-8, UTF-16LE, UTF-16BE) overrides the
  choice, as `encoding_rs` sniffs it (`Encoding::for_bom`).
- Decoding uses `Decoder::decode_to_utf8_without_replacement`. **Malformed
  input stops the import**, naming the byte offset and suggesting another
  encoding. Nothing is replaced with `U+FFFD`, because text from a CSV must
  reach the table as written (A11).
- `encoding_rs_io` 0.1.8 (MIT OR Apache-2.0, in the lock through
  `serde-saphyr`) wraps a decoder as a `Read`. It **always** replaces
  malformed bytes with `U+FFFD` **[docs `DecodeReaderBytesBuilder`]**, so it
  does not fit. The `Read` adapter around `encoding_rs`'s decoder is ours, a
  few dozen lines.
- Guessing the encoding (`chardetng` 1.0.0, Apache-2.0 OR MIT, not in the
  lock [cio]) is **not** proposed. The preview of the first rows shows the
  effect of the chosen encoding, and a wrong guess would silently change the
  text.

## Values

What each format gives, and how it reaches the table, is set by
[ADR-0040](../adr/0040-importing-files-into-a-table.md). In short:

- **CSV fields are text**, bound as written. The database converts them to
  the column's type.
- `csv` does not tell a quoted empty field (`""`) from an empty one. So the
  NULL that `jykre::export` writes as an empty field and the empty text it
  writes as `""` read the same. That is decision I-4.
- **XLSX numbers are IEEE doubles**: `Data::Float` and `DataRef::Float`
  **[docs]**, parsed by `fast-float2`. The import binds the shortest decimal
  text that reads back as the same double, Rust's `{}` for `f64`: `0.1`
  stays `0.1`. A value Excel itself could not hold exactly, such as a
  17-digit account number typed as a number, is already rounded in the
  file. The import dialog says that XLSX stores numbers as floating point.
- **Dates and times:** a cell whose number format is a date comes as
  `DateTime(ExcelDateTime)`. Its date and time are written as text without
  a time zone (`YYYY-MM-DD HH:MM:SS[.mmm]`), with the 1904 epoch applied
  (`Xlsx::has_1904_epoch`) **[docs]**. Excel keeps milliseconds at most.
- **Formulas:** the cached value the file stores is imported, never
  recalculated. A file saved by a tool that writes no cached values gives
  empty cells. The preview shows it.
- **Error cells** (`#N/A`, `#DIV/0!`) stop the import at their address.

## Alternatives considered

| Alternative | Licence, health | Why not |
| --- | --- | --- |
| `umya-spreadsheet` 3.1.1 | MIT, 1.49 M downloads, 4.0.0-beta.1 on 2026-10-26 [cio] | A read-and-write workbook model. `read` loads the workbook. It has a `read_sheet_by_name_stream` added recently, whose memory use and dependencies were **not verified**. Heavier than a reader |
| Our own XLSX reader on `zip` and `quick-xml` | `quick-xml` MIT, 532 M downloads [cio] | `quick-xml` is a new dependency on Windows anyway. We would re-implement shared strings, styles, date formats, epochs, inline strings and the relationship mapping that `calamine` maintains. A11 forbids narrowing it to avoid asking |
| `xlsx_reader` 3.1.1, `xlsx_batch_reader` 0.4.21 | Last release 2019; 59 recent downloads, hosted on gitee [cio] | Unmaintained or too small |
| `spreadsheet-ods` 1.0.4 | MIT/Apache-2.0, 121 k downloads [cio] | ODS only |
| `csv-async` 1.3.1 | MIT [cio] | Not needed: reading is CPU and file I/O on the blocking pool, not network I/O |
| `encoding_rs_io` 0.1.8 | MIT OR Apache-2.0, already in the lock | Replaces malformed input ([Encodings](#encodings)) |
| Our own CSV parser | — | `csv-core` is small, exhaustively tested and allocation-free. Ours would need the same tests, for no gain |

## Residual risks

- A workbook with a huge number of distinct strings is refused instead of
  streamed, with a message that names the limit and suggests a CSV.
- A CSV whose fields hold line breaks inside quotes is read correctly. One
  with an unbalanced quote fails at the 64 MiB record limit, not at the
  quote.
- `calamine` is 0.x. A minor upgrade may change the cells reader's API.
- Linux builds two `quick-xml` versions until `wayland-scanner` and `xcb`
  move to 0.42.

## Open decisions of A11

The release plan leaves three decisions open. This proposal adds two that
come from the formats. They are restated with the design in
[ADR-0040](../adr/0040-importing-files-into-a-table.md#decisions-for-the-maintainer).

| ID | Question | Options | Recommendation |
| --- | --- | --- | --- |
| I-1 | All or nothing, or commits in batches? | **(a) All or nothing:** one transaction for the whole file. A failure or a cancellation rolls it back, and the table is as it was. (b) Commit every batch. A failure leaves the rows of the committed batches, and the message says how many. (c) Choose per import, defaulting to (a) | **(a)** for 0.3.0. One rule to explain, and no half-imported table. The cost is a transaction as long as the file (locks held, undo or WAL growth), which the dialog states |
| I-2 | May the import create the table? | (a) No: existing tables only. (b) Yes: the dialog proposes `CREATE TABLE` with a type per column that the user edits; the driver writes the DDL per dialect. **(c) No, but "New table from this file…" opens a draft `CREATE TABLE` in a new query tab**, built from the header with the driver's quoting and a text type, for the user to edit and run before importing | **(c)**. It keeps the import to DML, and the DDL is visible and the user's own. MySQL's DDL commits implicitly, so a table created inside the import could not be rolled back with it |
| I-3 | Is Databricks included? | (a) No. (b) SQL warehouses only: bound parameters exist, but there is no multi-statement transaction, so each `INSERT` is a Delta commit and (a) of I-1 is impossible. Classic clusters cannot bind | **(a)** for 0.3.0 |
| I-4 | What does an empty CSV field mean? | (a) Always empty text. **(b) A choice per import, "Empty fields are NULL" (default) or "empty text"**, shown in the preview. (c) NULL only for a chosen marker (`\N`, `NULL`) | **(b)**. It matches `jykre::export`, which writes NULL as an empty field. Typing `NULL` in a file stays the four-letter text, unless (c) is added later |
| I-5 | Which workbook formats? | **(a) `.xlsx` only.** (b) Also `.xlsb` (streamed). (c) Also `.xls` and `.ods`, read whole and within the memory limit | **(a)**, the format A11 names and the one `jykre::export` writes. The others are refused with a reason |

## What approval allows

Declaring the three dependencies as above, used only by `jykre::import`,
with the tests and the gate of ADR-0040 (`--full`, which includes
`cargo deny --locked check`). The
[stack table](../standards/stack-and-dependencies.md) gets its row when the
dependencies land.
