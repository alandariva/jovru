//! The copy formats of a result's selection: tab-separated text, CSV, JSON,
//! JSON Lines, a Markdown or HTML table, `INSERT` statements, an `IN` list,
//! values separated by commas and the column names
//! ([Results](../docs/results.md#copying)).
//!
//! Values are written from the database's text, never reinterpreted: a
//! number of a number column is written as the database wrote it, a
//! boolean as `TRUE`/`FALSE`, NULL as `NULL` (`null` in JSON) and anything
//! else as a string, escaped as the connection's dialect reads strings
//! ([`crate::sql::string_literal`]). Names are quoted by the caller, which
//! knows which words the dialect reserves.
//!
//! [`CopyOptions`] are the choices of the copy menu: the format `Ctrl C`
//! copies in and how the delimited formats write their fields. They are kept
//! in the settings as `copy.preferences` ([Persistence](../docs/persistence.md)).
use crate::cell_format::{self, ValueFamily};
use crate::decimal;
use crate::drivers::DriverResult;
use crate::export::{Quoting, Separator, push_csv_field};
use crate::grid::{CellSelection, selection_block};
use crate::result::ResultData;
use crate::sql::string_literal;
use jykre_core::SqlDialect;
use std::collections::HashSet;
use std::path::Path;

mod job;
pub use job::{CopyRequest, CopySource, Progress, generate};

/// The formats a selection copies in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CopyFormat {
    /// Tab-separated text, which spreadsheets paste as a grid; a single
    /// cell as its exact value.
    Text,
    /// Delimited text with the separator and quoting of [`CopyOptions`].
    Csv,
    /// An array with one object per selected row.
    Json,
    /// One JSON object per selected row, one per line.
    JsonLines,
    /// A Markdown table with the column names as its header.
    Markdown,
    /// An HTML `<table>` with the column names as its header.
    Html,
    /// One `INSERT INTO … VALUES (…);` per selected row.
    Insert,
    /// One `INSERT INTO … VALUES (…), (…);` for every selected row.
    InsertValues,
    /// `(1, 'a', NULL)`, the distinct values for `IN`.
    InList,
    /// Every value as a literal, separated by commas.
    ValueList,
    /// The selected columns' names, separated by commas.
    ColumnNames,
}

impl CopyFormat {
    pub const ALL: [Self; 11] = [
        Self::Text,
        Self::Csv,
        Self::Json,
        Self::JsonLines,
        Self::Markdown,
        Self::Html,
        Self::Insert,
        Self::InsertValues,
        Self::InList,
        Self::ValueList,
        Self::ColumnNames,
    ];

    /// The name stored in the preferences.
    fn key(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Csv => "csv",
            Self::Json => "json",
            Self::JsonLines => "jsonl",
            Self::Markdown => "markdown",
            Self::Html => "html",
            Self::Insert => "insert",
            Self::InsertValues => "insert_values",
            Self::InList => "in_list",
            Self::ValueList => "value_list",
            Self::ColumnNames => "column_names",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.key() == key)
    }

    /// Whether the format can leave out the column names. The others always
    /// name the columns (JSON, tables, `INSERT`s) or never do (lists).
    pub fn has_optional_header(self) -> bool {
        matches!(self, Self::Text | Self::Csv)
    }

    /// Whether NULL is written as [`CopyOptions::null`] says. The other
    /// formats have a NULL of their own.
    pub fn has_null_choice(self) -> bool {
        matches!(self, Self::Text | Self::Csv)
    }
}

/// How the delimited formats write NULL.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NullText {
    /// An empty field.
    Empty,
    /// The word `NULL`, never quoted, so it stays apart from the text
    /// `'NULL'` when every value is quoted.
    Null,
}

/// The choices of the copy menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CopyOptions {
    /// The format `Ctrl C` and **Copy** use.
    pub format: CopyFormat,
    /// Text and CSV: the column names first. `Ctrl Shift C` and **Copy
    /// with headers** add them regardless.
    pub header: bool,
    /// CSV only.
    pub separator: Separator,
    /// CSV only. Text quotes only the values holding a tab, a line break or
    /// a quote, so it pastes into spreadsheets as cells.
    pub quoting: Quoting,
    /// Text and CSV.
    pub null: NullText,
}

impl Default for CopyOptions {
    /// As `Ctrl C` always copied: tab-separated text without the column
    /// names, NULL as an empty field. CSV as the CSV copy writes: `;`, every
    /// value quoted.
    fn default() -> Self {
        Self {
            format: CopyFormat::Text,
            header: false,
            separator: Separator::Semicolon,
            quoting: Quoting::All,
            null: NullText::Empty,
        }
    }
}

impl CopyOptions {
    /// The same options in `format`, with the column names first when
    /// `header` (otherwise as chosen).
    pub fn with(self, format: CopyFormat, header: bool) -> Self {
        Self {
            format,
            header: self.header || header,
            ..self
        }
    }

    /// The JSON kept in the settings.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "format": self.format.key(),
            "header": self.header,
            "separator": self.separator.char().to_string(),
            "quoting": match self.quoting {
                Quoting::All => "all",
                Quoting::Needed => "needed",
            },
            "null": match self.null {
                NullText::Empty => "empty",
                NullText::Null => "null",
            },
        })
    }

    /// Reads what [`CopyOptions::to_value`] wrote; anything missing or
    /// unknown takes its default, so older or newer settings keep loading.
    pub fn from_value(value: Option<&serde_json::Value>) -> Self {
        let defaults = Self::default();
        let Some(value) = value else {
            return defaults;
        };
        let text = |key: &str| value.get(key).and_then(serde_json::Value::as_str);
        Self {
            format: text("format")
                .and_then(CopyFormat::from_key)
                .unwrap_or(defaults.format),
            header: value
                .get("header")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(defaults.header),
            separator: text("separator")
                .and_then(Separator::from_char)
                .unwrap_or(defaults.separator),
            quoting: match text("quoting") {
                Some("all") => Quoting::All,
                Some("needed") => Quoting::Needed,
                _ => defaults.quoting,
            },
            null: match text("null") {
                Some("empty") => NullText::Empty,
                Some("null") => NullText::Null,
                _ => defaults.null,
            },
        }
    }
}

/// The settings key of the copy menu's choices.
pub const PREFERENCES_KEY: &str = "copy.preferences";

/// The writes of the copy preferences: the last one asked for wins.
pub static PREFERENCE_WRITES: crate::connection_store::LatestWrite =
    crate::connection_store::LatestWrite::new();

/// Reads the copy preferences kept in the data folder `dir`.
pub async fn load_preferences(dir: &Path) -> DriverResult<CopyOptions> {
    let repository = jykre_storage::ConfigRepository::open(dir).await?;
    let value = repository.setting(PREFERENCES_KEY).await;
    repository.close().await;
    Ok(CopyOptions::from_value(value?.as_ref()))
}

/// Saves the copy preferences for the write `ticket` of
/// [`PREFERENCE_WRITES`]; preferences superseded by newer ones are not
/// written.
pub async fn save_preferences(dir: &Path, options: CopyOptions, ticket: u64) -> DriverResult<()> {
    PREFERENCE_WRITES
        .run(ticket, async {
            let repository = jykre_storage::ConfigRepository::open(dir).await?;
            let result = repository
                .set_setting(PREFERENCES_KEY, &options.to_value())
                .await;
            repository.close().await;
            result
        })
        .await
        .unwrap_or(Ok(()))
}

/// How SQL formats write names and strings for the tab's connection.
pub struct SqlStyle<'a> {
    pub dialect: Option<SqlDialect>,
    /// Quotes a name when the dialect needs it.
    pub quote_name: &'a dyn Fn(&str) -> String,
    /// The table `INSERT`s name, already quoted; a placeholder when the
    /// result is not a table's.
    pub table: &'a str,
}

/// The selected cells, by row, with their columns' names and families.
pub struct Block<'a> {
    pub names: Vec<&'a str>,
    pub families: Vec<ValueFamily>,
    pub rows: Vec<Vec<Option<&'a str>>>,
}

impl<'a> Block<'a> {
    /// The visible part of `selection`, or `None` when nothing visible is
    /// selected.
    pub fn new(
        result: &'a ResultData,
        visible: &[usize],
        selection: Option<CellSelection>,
        families: &[ValueFamily],
    ) -> Option<Self> {
        let (rows, cols) = selection_block(selection?, visible)?;
        Some(Self {
            names: cols
                .clone()
                .filter_map(|col| result.columns().get(col))
                .map(|column| column.name.as_str())
                .collect(),
            families: cols
                .clone()
                .map(|col| families.get(col).copied().unwrap_or(ValueFamily::Text))
                .collect(),
            rows: rows
                .filter_map(|position| visible.get(position))
                .map(|row| cols.clone().map(|col| result.value(*row, col)).collect())
                .collect(),
        })
    }

    /// The number of cells.
    pub fn cells(&self) -> usize {
        self.rows.len() * self.names.len()
    }
}

/// The block in `options.format`.
pub fn copy_as(options: &CopyOptions, block: &Block, style: &SqlStyle) -> String {
    match options.format {
        CopyFormat::Text => text(block, options),
        CopyFormat::Csv => csv(block, options),
        CopyFormat::Json => json(block),
        CopyFormat::JsonLines => json_lines(block),
        CopyFormat::Markdown => markdown(block),
        CopyFormat::Html => html(block),
        CopyFormat::Insert => inserts(block, style),
        CopyFormat::InsertValues => insert_values(block, style),
        CopyFormat::InList => in_list(block, style.dialect),
        CopyFormat::ValueList => value_list(block, style.dialect),
        CopyFormat::ColumnNames => quoted_names(block, style),
    }
}

/// What a NULL becomes in the delimited formats.
fn null_text(options: &CopyOptions) -> Option<&'static str> {
    match options.null {
        NullText::Empty => None,
        NullText::Null => Some("NULL"),
    }
}

/// Tab-separated text ([`cell_format::copy_cells`]).
fn text(block: &Block, options: &CopyOptions) -> String {
    let headers = options.header.then_some(block.names.as_slice());
    match null_text(options) {
        None => cell_format::copy_cells(headers, &block.rows),
        Some(null) => {
            let rows: Vec<Vec<Option<&str>>> = block
                .rows
                .iter()
                .map(|row| row.iter().map(|value| value.or(Some(null))).collect())
                .collect();
            cell_format::copy_cells(headers, &rows)
        }
    }
}

/// Delimited text, one line per row with no line break after the last, as
/// the CSV export writes its fields.
fn csv(block: &Block, options: &CopyOptions) -> String {
    let separator = options.separator.char();
    let null = null_text(options);
    let line = |cells: &mut dyn Iterator<Item = Option<&str>>| {
        let mut out = String::new();
        for (index, cell) in cells.enumerate() {
            if index > 0 {
                out.push(separator);
            }
            match (cell, null) {
                (None, Some(null)) => out.push_str(null),
                (cell, _) => push_csv_field(&mut out, cell, separator, options.quoting),
            }
        }
        out
    };
    let mut lines = Vec::with_capacity(block.rows.len() + 1);
    if options.header {
        lines.push(line(&mut block.names.iter().map(|name| Some(*name))));
    }
    lines.extend(block.rows.iter().map(|row| line(&mut row.iter().copied())));
    lines.join("\n")
}

fn quoted_names(block: &Block, style: &SqlStyle) -> String {
    block
        .names
        .iter()
        .map(|name| (style.quote_name)(name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A value as a SQL literal. Shared with [`crate::export`], so a copy and an
/// export write the same literal.
pub(crate) fn literal(
    value: Option<&str>,
    family: ValueFamily,
    dialect: Option<SqlDialect>,
) -> String {
    let Some(value) = value else {
        return "NULL".into();
    };
    match family {
        ValueFamily::Number if decimal::is_number(value) => value.trim().to_owned(),
        ValueFamily::Boolean => match value.to_ascii_lowercase().as_str() {
            "t" | "true" | "1" => "TRUE".into(),
            "f" | "false" | "0" => "FALSE".into(),
            _ => string_literal(value, dialect),
        },
        _ => string_literal(value, dialect),
    }
}

/// The distinct values of the block, in the order they first appear, as
/// `(…)` for `IN`. NULLs are left out: `x IN (NULL)` matches nothing.
fn in_list(block: &Block, dialect: Option<SqlDialect>) -> String {
    let mut seen = HashSet::new();
    let values: Vec<String> = block
        .rows
        .iter()
        .flat_map(|row| row.iter().zip(&block.families))
        .filter_map(|(value, family)| value.map(|value| (value, *family)))
        .map(|(value, family)| literal(Some(value), family, dialect))
        .filter(|literal| seen.insert(literal.clone()))
        .collect();
    format!("({})", values.join(", "))
}

/// A row's values as SQL literals, separated by commas.
fn row_literals(block: &Block, row: &[Option<&str>], dialect: Option<SqlDialect>) -> String {
    row.iter()
        .zip(&block.families)
        .map(|(value, family)| literal(*value, *family, dialect))
        .collect::<Vec<_>>()
        .join(", ")
}

fn inserts(block: &Block, style: &SqlStyle) -> String {
    let columns = quoted_names(block, style);
    block
        .rows
        .iter()
        .map(|row| {
            format!(
                "INSERT INTO {} ({columns}) VALUES ({});",
                style.table,
                row_literals(block, row, style.dialect)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One `INSERT` for every row, each row on a line of its own.
fn insert_values(block: &Block, style: &SqlStyle) -> String {
    let rows = block
        .rows
        .iter()
        .map(|row| format!("  ({})", row_literals(block, row, style.dialect)))
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "INSERT INTO {} ({}) VALUES\n{rows};",
        style.table,
        quoted_names(block, style)
    )
}

/// Every value of the block, row by row, as a literal (NULL included),
/// separated by commas.
fn value_list(block: &Block, dialect: Option<SqlDialect>) -> String {
    block
        .rows
        .iter()
        .map(|row| row_literals(block, row, dialect))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether `text` is a JSON number as written (no `+`, leading zeros, or a
/// bare `.`), so it can be copied as one without changing a digit.
fn json_number(text: &str) -> bool {
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (unsigned, None),
    };
    let (integer, fraction) = match mantissa.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (mantissa, None),
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    digits(integer)
        && (integer == "0" || !integer.starts_with('0'))
        && fraction.is_none_or(digits)
        && exponent
            .is_none_or(|exponent| digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)))
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

/// The JSON keys of the columns `names`, already quoted. Repeated names
/// (`a.id`, `b.id`) get a suffix, so no key hides another. Shared with
/// [`crate::export`].
pub(crate) fn json_keys<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut used = HashSet::new();
    names
        .into_iter()
        .map(|name| {
            let mut key = name.to_owned();
            let mut copy = 1;
            while !used.insert(key.clone()) {
                copy += 1;
                key = format!("{name}_{copy}");
            }
            json_string(&key)
        })
        .collect()
}

/// A value as JSON: a number of a number column as written when it is a
/// valid JSON number, a boolean as `true`/`false`, NULL as `null`, anything
/// else as a string. Shared with [`crate::export`].
pub(crate) fn json_value(value: Option<&str>, family: ValueFamily) -> String {
    match (value, family) {
        (None, _) => "null".to_owned(),
        (Some(value), ValueFamily::Number) if json_number(value) => value.to_owned(),
        (Some(value), ValueFamily::Boolean) => match value.to_ascii_lowercase().as_str() {
            "t" | "true" | "1" => "true".into(),
            "f" | "false" | "0" => "false".into(),
            _ => json_string(value),
        },
        (Some(value), _) => json_string(value),
    }
}

/// One row as a JSON object under `keys` ([`json_keys`]).
fn json_object(keys: &[String], families: &[ValueFamily], row: &[Option<&str>]) -> String {
    let fields = row
        .iter()
        .zip(families)
        .zip(keys)
        .map(|((value, family), key)| format!("{key}: {}", json_value(*value, *family)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{{fields}}}")
}

fn json(block: &Block) -> String {
    let keys = json_keys(block.names.iter().copied());
    let objects: Vec<String> = block
        .rows
        .iter()
        .map(|row| format!("  {}", json_object(&keys, &block.families, row)))
        .collect();
    if objects.is_empty() {
        "[]".into()
    } else {
        format!("[\n{}\n]", objects.join(",\n"))
    }
}

fn json_lines(block: &Block) -> String {
    let keys = json_keys(block.names.iter().copied());
    block
        .rows
        .iter()
        .map(|row| json_object(&keys, &block.families, row))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A value inside an HTML cell: escaped, line breaks as `<br>`.
fn html_cell(value: &str) -> String {
    crate::reports::export::escape(value)
        .replace("\r\n", "<br>")
        .replace(['\n', '\r'], "<br>")
}

/// An HTML table: the names in `<thead>`, NULL as `NULL`, numbers to the
/// right, every text escaped.
fn html(block: &Block) -> String {
    let align = |family: &ValueFamily| {
        if *family == ValueFamily::Number {
            " style=\"text-align: right\""
        } else {
            ""
        }
    };
    let mut out = String::from("<table>\n  <thead>\n    <tr>");
    for (name, family) in block.names.iter().zip(&block.families) {
        out.push_str(&format!("<th{}>{}</th>", align(family), html_cell(name)));
    }
    out.push_str("</tr>\n  </thead>\n  <tbody>\n");
    for row in &block.rows {
        out.push_str("    <tr>");
        for (value, family) in row.iter().zip(&block.families) {
            let text = value.map_or_else(|| "NULL".to_owned(), html_cell);
            out.push_str(&format!("<td{}>{text}</td>", align(family)));
        }
        out.push_str("</tr>\n");
    }
    out.push_str("  </tbody>\n</table>");
    out
}

/// A value inside a Markdown table cell: `\` and `|` escaped, line breaks
/// as `<br>`. Shared with [`crate::export`].
pub(crate) fn markdown_cell(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace("\r\n", "<br>")
        .replace(['\n', '\r'], "<br>")
}

/// The alignment row's cell for a column: numbers to the right.
pub(crate) fn markdown_alignment(family: ValueFamily) -> &'static str {
    if family == ValueFamily::Number {
        "---:"
    } else {
        "---"
    }
}

fn markdown(block: &Block) -> String {
    let cell = markdown_cell;
    let mut lines = vec![
        format!(
            "| {} |",
            block
                .names
                .iter()
                .map(|name| cell(name))
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        format!(
            "| {} |",
            block
                .families
                .iter()
                .map(|family| markdown_alignment(*family))
                .collect::<Vec<_>>()
                .join(" | ")
        ),
    ];
    for row in &block.rows {
        lines.push(format!(
            "| {} |",
            row.iter()
                .map(|value| value.map_or_else(|| "NULL".to_owned(), cell))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ResultColumn;

    fn result() -> ResultData {
        ResultData::from_rows(
            vec![
                ResultColumn::new("id", "numeric"),
                ResultColumn::new("name", "text"),
                ResultColumn::new("active", "bool"),
            ],
            [
                [Some("1.50"), Some("O'Brien"), Some("t")],
                [Some("2"), None, Some("f")],
                [Some("1.50"), Some("a|b\nc"), None],
            ],
        )
    }

    fn families() -> Vec<ValueFamily> {
        vec![ValueFamily::Number, ValueFamily::Text, ValueFamily::Boolean]
    }

    fn all(result: &ResultData) -> Block<'_> {
        let families = families();
        Block::new(
            result,
            &[0, 1, 2],
            CellSelection::all(&[0, 1, 2], 3),
            &families,
        )
        .expect("a block")
    }

    fn copied(format: CopyFormat, block: &Block, style: &SqlStyle) -> String {
        copy_as(&CopyOptions::default().with(format, false), block, style)
    }

    fn style<'a>(quote: &'a dyn Fn(&str) -> String) -> SqlStyle<'a> {
        SqlStyle {
            dialect: Some(SqlDialect::Postgres),
            quote_name: quote,
            table: "public.people",
        }
    }

    #[test]
    fn an_in_list_has_each_value_once_and_no_nulls() {
        let result = result();
        let quote = |name: &str| name.to_owned();
        let block = Block::new(
            &result,
            &[0, 1, 2],
            Some(CellSelection {
                anchor: (0, 0),
                focus: (2, 1),
            }),
            &families(),
        )
        .expect("a block");
        assert_eq!(
            copied(CopyFormat::InList, &block, &style(&quote)),
            "(1.50, 'O''Brien', 2, 'a|b\nc')"
        );
    }

    #[test]
    fn inserts_write_literals_in_the_dialect() {
        let result = result();
        let quote = |name: &str| format!("\"{name}\"");
        let text = copied(CopyFormat::Insert, &all(&result), &style(&quote));
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(
            lines[0],
            "INSERT INTO public.people (\"id\", \"name\", \"active\") VALUES (1.50, 'O''Brien', TRUE);"
        );
        assert_eq!(
            lines[1],
            "INSERT INTO public.people (\"id\", \"name\", \"active\") VALUES (2, NULL, FALSE);"
        );
        let mysql = SqlStyle {
            dialect: Some(SqlDialect::MySql),
            ..style(&quote)
        };
        assert!(copied(CopyFormat::Insert, &all(&result), &mysql).contains("'O\\'Brien'"));
    }

    #[test]
    fn json_keeps_numbers_as_written_and_nulls_as_null() {
        let result = result();
        let quote = |name: &str| name.to_owned();
        assert_eq!(
            copied(CopyFormat::Json, &all(&result), &style(&quote)),
            "[\n  {\"id\": 1.50, \"name\": \"O'Brien\", \"active\": true},\n  \
             {\"id\": 2, \"name\": null, \"active\": false},\n  \
             {\"id\": 1.50, \"name\": \"a|b\\nc\", \"active\": null}\n]"
        );
        assert!(json_number("-0.5e+3") && json_number("0"));
        assert!(
            !json_number("+1") && !json_number("007") && !json_number("1.") && !json_number("NaN")
        );
        let repeated = Block {
            names: vec!["id", "id"],
            families: vec![ValueFamily::Number; 2],
            rows: vec![vec![Some("+1"), Some("2")]],
        };
        assert_eq!(json(&repeated), "[\n  {\"id\": \"+1\", \"id_2\": 2}\n]");
    }

    #[test]
    fn markdown_escapes_pipes_and_line_breaks() {
        let result = result();
        let quote = |name: &str| name.to_owned();
        let text = copied(CopyFormat::Markdown, &all(&result), &style(&quote));
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "| id | name | active |");
        assert_eq!(lines[1], "| ---: | --- | --- |");
        assert_eq!(lines[3], "| 2 | NULL | f |");
        assert_eq!(lines[4], "| 1.50 | a\\|b<br>c | NULL |");
    }

    #[test]
    fn column_names_are_quoted_by_the_caller() {
        let result = result();
        let quote = |name: &str| {
            if name == "name" {
                format!("\"{name}\"")
            } else {
                name.to_owned()
            }
        };
        assert_eq!(
            copied(CopyFormat::ColumnNames, &all(&result), &style(&quote)),
            "id, \"name\", active"
        );
    }

    fn options(format: CopyFormat) -> CopyOptions {
        CopyOptions::default().with(format, false)
    }

    #[test]
    fn text_copies_tab_separated_and_writes_null_as_chosen() {
        let result = result();
        let quote = |name: &str| name.to_owned();
        let block = all(&result);
        let text = copy_as(&options(CopyFormat::Text), &block, &style(&quote));
        assert_eq!(text, "1.50\tO'Brien\tt\n2\t\tf\n1.50\t\"a|b\nc\"\t");
        let headed = CopyOptions {
            header: true,
            null: NullText::Null,
            ..CopyOptions::default()
        };
        let text = copy_as(&headed, &block, &style(&quote));
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "id\tname\tactive");
        assert_eq!(lines[2], "2\tNULL\tf");
        // A single cell is its exact value, a NULL one as chosen.
        let cell = Block {
            names: vec!["name"],
            families: vec![ValueFamily::Text],
            rows: vec![vec![None]],
        };
        assert_eq!(
            copy_as(&options(CopyFormat::Text), &cell, &style(&quote)),
            ""
        );
        assert_eq!(
            copy_as(&headed.with(CopyFormat::Text, false), &cell, &style(&quote))
                .lines()
                .nth(1),
            Some("NULL")
        );
    }

    #[test]
    fn csv_follows_the_separator_quoting_header_and_null_chosen() {
        let result = result();
        let quote = |name: &str| name.to_owned();
        let block = all(&result);
        let quoted = copy_as(&options(CopyFormat::Csv), &block, &style(&quote));
        assert_eq!(
            quoted,
            "\"1.50\";\"O'Brien\";\"t\"\n\"2\";;\"f\"\n\"1.50\";\"a|b\nc\";"
        );
        let plain = CopyOptions {
            format: CopyFormat::Csv,
            header: true,
            separator: Separator::Comma,
            quoting: Quoting::Needed,
            null: NullText::Null,
        };
        assert_eq!(
            copy_as(&plain, &block, &style(&quote)),
            "id,name,active\n1.50,O'Brien,t\n2,NULL,f\n1.50,\"a|b\nc\",NULL"
        );
        // Empty text stays apart from an empty NULL.
        let empty = Block {
            names: vec!["a", "b"],
            families: vec![ValueFamily::Text; 2],
            rows: vec![vec![Some(""), None]],
        };
        let needed = CopyOptions {
            null: NullText::Empty,
            header: false,
            ..plain
        };
        assert_eq!(copy_as(&needed, &empty, &style(&quote)), "\"\",");
    }

    #[test]
    fn json_lines_write_one_object_per_row() {
        let result = result();
        let quote = |name: &str| name.to_owned();
        let text = copied(CopyFormat::JsonLines, &all(&result), &style(&quote));
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], "{\"id\": 2, \"name\": null, \"active\": false}");
        for line in lines {
            serde_json::from_str::<serde_json::Value>(line).expect("a JSON value");
        }
    }

    #[test]
    fn html_escapes_every_text_and_aligns_numbers() {
        let result = result();
        let quote = |name: &str| name.to_owned();
        let block = Block {
            names: vec!["n", "<b>"],
            families: vec![ValueFamily::Number, ValueFamily::Text],
            rows: vec![vec![Some("1"), Some("a & \"b\"\nc")], vec![None, None]],
        };
        assert_eq!(
            copied(CopyFormat::Html, &block, &style(&quote)),
            "<table>\n  <thead>\n    <tr><th style=\"text-align: right\">n</th><th>&lt;b&gt;</th></tr>\n  </thead>\n  <tbody>\n    \
             <tr><td style=\"text-align: right\">1</td><td>a &amp; &quot;b&quot;<br>c</td></tr>\n    \
             <tr><td style=\"text-align: right\">NULL</td><td>NULL</td></tr>\n  </tbody>\n</table>"
        );
        let _ = result;
    }

    #[test]
    fn one_insert_holds_every_row_and_a_value_list_every_value() {
        let result = result();
        let quote = |name: &str| name.to_owned();
        let block = all(&result);
        assert_eq!(
            copied(CopyFormat::InsertValues, &block, &style(&quote)),
            "INSERT INTO public.people (id, name, active) VALUES\n  \
             (1.50, 'O''Brien', TRUE),\n  (2, NULL, FALSE),\n  (1.50, 'a|b\nc', NULL);"
        );
        assert_eq!(
            copied(CopyFormat::ValueList, &block, &style(&quote)),
            "1.50, 'O''Brien', TRUE, 2, NULL, FALSE, 1.50, 'a|b\nc', NULL"
        );
    }

    #[test]
    fn the_preferences_read_back_and_unknown_values_take_defaults() {
        let options = CopyOptions {
            format: CopyFormat::Html,
            header: true,
            separator: Separator::Tab,
            quoting: Quoting::Needed,
            null: NullText::Null,
        };
        assert_eq!(CopyOptions::from_value(Some(&options.to_value())), options);
        for format in CopyFormat::ALL {
            let value = options.with(format, false).to_value();
            assert_eq!(CopyOptions::from_value(Some(&value)).format, format);
        }
        assert_eq!(CopyOptions::from_value(None), CopyOptions::default());
        assert_eq!(
            CopyOptions::from_value(Some(&serde_json::json!({
                "format": "yaml", "separator": "|", "quoting": "some", "null": 0, "header": "yes"
            }))),
            CopyOptions::default()
        );
        // Ctrl Shift C adds the names; it never takes them away.
        assert!(options.with(CopyFormat::Csv, false).header);
        assert!(CopyOptions::default().with(CopyFormat::Csv, true).header);
    }
}
