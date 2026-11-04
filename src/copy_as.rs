//! **Copy as** formats for the selected block of a result: an `IN` list,
//! `INSERT` statements, JSON, a Markdown table and the column names.
//!
//! Values are written from the database's text, never reinterpreted: a
//! number of a number column is written as the database wrote it, a
//! boolean as `TRUE`/`FALSE`, NULL as `NULL` (`null` in JSON) and anything
//! else as a string, escaped as the connection's dialect reads strings
//! ([`crate::sql::string_literal`]). Names are quoted by the caller, which
//! knows which words the dialect reserves.
use crate::cell_format::ValueFamily;
use crate::decimal;
use crate::grid::{CellSelection, selection_block};
use crate::result::ResultData;
use crate::sql::string_literal;
use jykre_core::SqlDialect;
use std::collections::HashSet;

/// The formats **Copy as** offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFormat {
    /// `(1, 'a', NULL)`, the distinct values for `IN`.
    InList,
    /// One `INSERT INTO … VALUES (…);` per selected row.
    Insert,
    /// An array with one object per selected row.
    Json,
    /// A Markdown table with the column names as its header.
    Markdown,
    /// The selected columns' names, separated by commas.
    ColumnNames,
}

impl CopyFormat {
    pub const ALL: [Self; 5] = [
        Self::InList,
        Self::Insert,
        Self::Json,
        Self::Markdown,
        Self::ColumnNames,
    ];
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

/// The block in `format`.
pub fn copy_as(format: CopyFormat, block: &Block, style: &SqlStyle) -> String {
    match format {
        CopyFormat::InList => in_list(block, style.dialect),
        CopyFormat::Insert => inserts(block, style),
        CopyFormat::Json => json(block),
        CopyFormat::Markdown => markdown(block),
        CopyFormat::ColumnNames => block
            .names
            .iter()
            .map(|name| (style.quote_name)(name))
            .collect::<Vec<_>>()
            .join(", "),
    }
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

fn inserts(block: &Block, style: &SqlStyle) -> String {
    let columns = block
        .names
        .iter()
        .map(|name| (style.quote_name)(name))
        .collect::<Vec<_>>()
        .join(", ");
    block
        .rows
        .iter()
        .map(|row| {
            let values = row
                .iter()
                .zip(&block.families)
                .map(|(value, family)| literal(*value, *family, style.dialect))
                .collect::<Vec<_>>()
                .join(", ");
            format!("INSERT INTO {} ({columns}) VALUES ({values});", style.table)
        })
        .collect::<Vec<_>>()
        .join("\n")
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

fn json(block: &Block) -> String {
    let keys = json_keys(block.names.iter().copied());
    let objects: Vec<String> = block
        .rows
        .iter()
        .map(|row| {
            let fields = row
                .iter()
                .zip(&block.families)
                .zip(&keys)
                .map(|((value, family), key)| format!("{key}: {}", json_value(*value, *family)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("  {{{fields}}}")
        })
        .collect();
    if objects.is_empty() {
        "[]".into()
    } else {
        format!("[\n{}\n]", objects.join(",\n"))
    }
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
            copy_as(CopyFormat::InList, &block, &style(&quote)),
            "(1.50, 'O''Brien', 2, 'a|b\nc')"
        );
    }

    #[test]
    fn inserts_write_literals_in_the_dialect() {
        let result = result();
        let quote = |name: &str| format!("\"{name}\"");
        let text = copy_as(CopyFormat::Insert, &all(&result), &style(&quote));
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
        assert!(copy_as(CopyFormat::Insert, &all(&result), &mysql).contains("'O\\'Brien'"));
    }

    #[test]
    fn json_keeps_numbers_as_written_and_nulls_as_null() {
        let result = result();
        let quote = |name: &str| name.to_owned();
        assert_eq!(
            copy_as(CopyFormat::Json, &all(&result), &style(&quote)),
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
        let text = copy_as(CopyFormat::Markdown, &all(&result), &style(&quote));
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
            copy_as(CopyFormat::ColumnNames, &all(&result), &style(&quote)),
            "id, \"name\", active"
        );
    }
}
