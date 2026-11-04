//! Result presentation grouped by value type family. Formatting never
//! mutates source values or converts exact database numbers through floats.
use crate::result::ResultData;
use crate::tr;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueFamily {
    Text,
    Number,
    Boolean,
    Temporal,
    Binary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderIcon {
    Number,
    String,
    Boolean,
    Date,
    Time,
    Code,
}

pub fn header_icon(db_type: &str) -> HeaderIcon {
    let name = db_type.trim().to_ascii_uppercase();
    if name.ends_with("[]")
        || ["ARRAY", "STRUCT", "MAP", "JSON", "VARIANT"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
    {
        return HeaderIcon::Code;
    }
    match family(db_type) {
        ValueFamily::Number => HeaderIcon::Number,
        ValueFamily::Boolean => HeaderIcon::Boolean,
        ValueFamily::Binary => HeaderIcon::Code,
        ValueFamily::Temporal if name.starts_with("INTERVAL") => HeaderIcon::String,
        ValueFamily::Temporal if name.starts_with("TIME") && !name.starts_with("TIMESTAMP") => {
            HeaderIcon::Time
        }
        ValueFamily::Temporal => HeaderIcon::Date,
        _ => HeaderIcon::String,
    }
}

pub fn family(db_type: &str) -> ValueFamily {
    let name = db_type.trim().to_ascii_uppercase();
    if name.ends_with("[]")
        || name.starts_with("ARRAY")
        || name.starts_with("STRUCT")
        || name.starts_with("MAP")
    {
        return ValueFamily::Text;
    }
    let base = name.split(['(', ' ', '<']).next().unwrap_or("");
    match base {
        "INT" | "INTEGER" | "INT2" | "INT4" | "INT8" | "TINYINT" | "SMALLINT" | "MEDIUMINT"
        | "BIGINT" | "LONG" | "SHORT" | "BYTE" | "OID" | "SERIAL" | "BIGSERIAL" | "SMALLSERIAL"
        | "YEAR" | "DECIMAL" | "NUMERIC" | "NUMBER" | "MONEY" | "FLOAT" | "FLOAT4" | "FLOAT8"
        | "REAL" | "DOUBLE" => ValueFamily::Number,
        "BOOL" | "BOOLEAN" => ValueFamily::Boolean,
        "DATE" | "TIME" | "TIMETZ" | "TIMESTAMP" | "TIMESTAMPTZ" | "TIMESTAMP_NTZ"
        | "TIMESTAMP_LTZ" | "DATETIME" | "INTERVAL" => ValueFamily::Temporal,
        "BYTEA" | "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB"
        | "BIT" => ValueFamily::Binary,
        _ => ValueFamily::Text,
    }
}

pub fn display(value: Option<&str>, family: ValueFamily, numbers_pt_br: bool) -> String {
    let Some(value) = value else {
        return "NULL".into();
    };
    if value.is_empty() {
        return tr!("(empty)").into();
    }
    if value.chars().all(char::is_whitespace) {
        return tr!("␠ {0} spaces", value.chars().count());
    }
    match family {
        ValueFamily::Number if numbers_pt_br => number_pt_br(value),
        ValueFamily::Boolean => match value.to_ascii_lowercase().as_str() {
            "t" | "true" | "1" => "true".into(),
            "f" | "false" | "0" => "false".into(),
            _ => value.into(),
        },
        ValueFamily::Binary if value.starts_with("\\x") => format!("0x{}", &value[2..]),
        _ => value.into(),
    }
}

/// A count for user-facing text, with digits grouped in the UI language
/// (`100,000` in English, `100.000` in Portuguese).
pub fn count(value: usize) -> String {
    let digits = value.to_string();
    let separator = if crate::i18n::locale().starts_with("pt") {
        '.'
    } else {
        ','
    };
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(separator);
        }
        grouped.push(digit);
    }
    grouped
}

/// How long something took, in the scale that reads best: milliseconds
/// below a second, seconds with one decimal (in the UI language's decimal
/// separator) below a minute, then minutes and seconds, then hours and
/// minutes. Truncated, never rounded up, so a run is never shown as longer
/// than it was.
pub fn duration(ms: u64) -> String {
    if ms < 1_000 {
        return crate::tr!("{0} ms", ms);
    }
    let seconds = ms / 1_000;
    if seconds < 60 {
        let separator = if crate::i18n::locale().starts_with("pt") {
            ','
        } else {
            '.'
        };
        return crate::tr!(
            "{0} s",
            format!("{seconds}{separator}{}", (ms % 1_000) / 100)
        );
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return crate::tr!("{0} min {1} s", minutes, seconds % 60);
    }
    crate::tr!("{0} h {1} min", minutes / 60, minutes % 60)
}

/// A file size for user-facing text, in the scale that reads best: bytes,
/// then KB, MB and GB (powers of 1,024), with one decimal from MB on in the
/// UI language's decimal separator. Truncated, never rounded up.
pub fn size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes < KB {
        return crate::tr!("{0} B", bytes);
    }
    if bytes < MB {
        return crate::tr!("{0} KB", bytes / KB);
    }
    let (unit, scale) = if bytes < GB { (MB, "MB") } else { (GB, "GB") };
    let separator = if crate::i18n::locale().starts_with("pt") {
        ','
    } else {
        '.'
    };
    let tenths = bytes / (unit / 10);
    let value = format!("{}{separator}{}", tenths / 10, tenths % 10);
    if scale == "MB" {
        crate::tr!("{0} MB", value)
    } else {
        crate::tr!("{0} GB", value)
    }
}

pub fn number_pt_br(value: &str) -> String {
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    let mut parts = unsigned.split('.');
    let integer = parts.next().unwrap_or("");
    let fraction = parts.next();
    if integer.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || fraction
            .is_some_and(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
        || parts.next().is_some()
    {
        return value.into();
    }
    let mut result = String::with_capacity(value.len() + integer.len() / 3);
    if unsigned.len() != value.len() {
        result.push(value.as_bytes()[0] as char);
    }
    for (index, digit) in integer.chars().enumerate() {
        if index > 0 && (integer.len() - index).is_multiple_of(3) {
            result.push('.');
        }
        result.push(digit);
    }
    if let Some(fraction) = fraction {
        result.push(',');
        result.push_str(fraction);
    }
    result
}

/// NULL exports as an empty field; the literal string "NULL" remains quoted.
pub fn csv_field(value: Option<&str>) -> String {
    value
        .map(|text| format!("\"{}\"", text.replace('"', "\"\"")))
        .unwrap_or_default()
}

/// The whole result as CSV with headers: `;` between fields, every value
/// quoted, NULL as an empty field. Values are the originals, regardless of
/// how the grid presents them.
pub fn result_csv(result: &ResultData) -> String {
    let mut csv = result
        .columns()
        .iter()
        .map(|column| csv_field(Some(&column.name)))
        .collect::<Vec<_>>()
        .join(";");
    for row in 0..result.row_count() {
        csv.push('\n');
        csv.push_str(&result.row(row).map(csv_field).collect::<Vec<_>>().join(";"));
    }
    csv
}

/// Copies a block of cells as tab-separated text, the clipboard format
/// spreadsheets and SQL tools paste as a grid. A single cell is copied as its
/// exact value. Fields with tabs, line breaks or quotes are quoted with
/// doubled quotes; NULL becomes an empty field, as in [`csv_field`].
pub fn copy_cells(headers: Option<&[&str]>, rows: &[Vec<Option<&str>>]) -> String {
    if headers.is_none() && rows.len() == 1 && rows[0].len() == 1 {
        return rows[0][0].unwrap_or_default().to_owned();
    }
    let field = |value: &str| {
        if value.contains(['\t', '\n', '\r', '"']) {
            format!("\"{}\"", value.replace('"', "\"\""))
        } else {
            value.to_owned()
        }
    };
    let mut lines = Vec::with_capacity(rows.len() + 1);
    if let Some(headers) = headers {
        lines.push(
            headers
                .iter()
                .map(|name| field(name))
                .collect::<Vec<_>>()
                .join("\t"),
        );
    }
    for row in rows {
        lines.push(
            row.iter()
                .map(|value| value.map(field).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\t"),
        );
    }
    lines.join("\n")
}

/// JSON text laid out with two-space indentation for the value viewer, every
/// token kept as written (numbers are not parsed, so none changes a digit).
/// `None` when the text is not an object or an array whose brackets balance.
pub fn pretty_json(text: &str) -> Option<String> {
    let text = text.trim();
    if !text.starts_with(['{', '[']) {
        return None;
    }
    let mut out = String::with_capacity(text.len() * 2);
    // The closing bracket each open one expects.
    let mut open: Vec<char> = Vec::new();
    let (mut in_string, mut escaped) = (false, false);
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", depth));
    };
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        // Something after the outermost closing bracket: not one value.
        if open.is_empty() && !out.is_empty() && !ch.is_whitespace() {
            return None;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '{' | '[' => {
                out.push(ch);
                while chars.next_if(|next| next.is_whitespace()).is_some() {}
                let close = if ch == '{' { '}' } else { ']' };
                if chars.next_if_eq(&close).is_some() {
                    out.push(close);
                } else {
                    open.push(close);
                    newline(&mut out, open.len());
                }
            }
            '}' | ']' => {
                if open.pop() != Some(ch) {
                    return None;
                }
                newline(&mut out, open.len());
                out.push(ch);
            }
            ',' => {
                out.push(ch);
                newline(&mut out, open.len());
            }
            ':' => out.push_str(": "),
            ch if ch.is_whitespace() => {}
            ch => out.push(ch),
        }
    }
    (open.is_empty() && !in_string).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_laid_out_without_changing_its_tokens() {
        assert_eq!(
            pretty_json(r#"{"a":1.50,"b":[true, null,{}],"c":"x, \"y\": {z}"}"#).as_deref(),
            Some(
                "{\n  \"a\": 1.50,\n  \"b\": [\n    true,\n    null,\n    {}\n  ],\n  \"c\": \"x, \\\"y\\\": {z}\"\n}"
            )
        );
        assert_eq!(pretty_json("[]").as_deref(), Some("[]"));
        assert_eq!(pretty_json("plain text"), None);
        assert_eq!(pretty_json("{\"a\": [1}"), None);
        assert_eq!(pretty_json("[1}"), None);
        assert_eq!(pretty_json("{} {}"), None);
    }

    #[test]
    fn durations_read_in_their_own_scale() {
        assert_eq!(duration(0), "0 ms");
        assert_eq!(duration(999), "999 ms");
        assert_eq!(duration(1_000), "1.0 s");
        assert_eq!(duration(59_999), "59.9 s");
        assert_eq!(duration(183_421), "3 min 3 s");
        assert_eq!(duration(3_599_999), "59 min 59 s");
        assert_eq!(duration(3_600_000), "1 h 0 min");
        assert_eq!(duration(7_380_000), "2 h 3 min");
    }

    #[test]
    fn copies_cells_as_spreadsheet_text() {
        assert_eq!(copy_cells(None, &[vec![Some("a\tb")]]), "a\tb");
        assert_eq!(copy_cells(None, &[vec![None]]), "");
        assert_eq!(
            copy_cells(
                Some(&["id", "note"]),
                &[vec![Some("1"), Some("say \"hi\"")], vec![Some("2"), None]]
            ),
            "id\tnote\n1\t\"say \"\"hi\"\"\"\n2\t"
        );
        assert_eq!(
            copy_cells(None, &[vec![Some("x"), Some("line\nbreak")]]),
            "x\t\"line\nbreak\""
        );
    }

    #[test]
    fn recognizes_types_from_all_three_drivers() {
        for name in [
            "INT8",
            "int4",
            "BIGINT UNSIGNED",
            "DECIMAL(38,10)",
            "NUMERIC",
            "DOUBLE PRECISION",
        ] {
            assert_eq!(family(name), ValueFamily::Number, "{name}");
        }
        for name in [
            "timestamp with time zone",
            "TIMESTAMP_NTZ",
            "DATETIME",
            "interval",
        ] {
            assert_eq!(family(name), ValueFamily::Temporal);
        }
        for name in [
            "jsonb",
            "uuid",
            "NUMERIC[]",
            "ARRAY<INT>",
            "STRUCT<a:INT>",
            "VARCHAR(200)",
        ] {
            assert_eq!(family(name), ValueFamily::Text);
        }
        assert_eq!(family("BOOLEAN"), ValueFamily::Boolean);
        assert_eq!(family("BYTEA"), ValueFamily::Binary);
        for (kind, icon) in [
            ("INT8", HeaderIcon::Number),
            ("NUMERIC(38,10)", HeaderIcon::Number),
            ("JSONB", HeaderIcon::Code),
            ("INT[]", HeaderIcon::Code),
            ("BOOLEAN", HeaderIcon::Boolean),
            ("TIMESTAMP_NTZ", HeaderIcon::Date),
            ("TIME", HeaderIcon::Time),
            ("UUID", HeaderIcon::String),
        ] {
            assert_eq!(header_icon(kind), icon);
        }
    }
    #[test]
    fn null_empty_whitespace_and_literal_null_remain_distinct() {
        assert_eq!(display(None, ValueFamily::Text, false), "NULL");
        assert_eq!(display(Some(""), ValueFamily::Text, false), "(empty)");
        assert_eq!(display(Some("   "), ValueFamily::Text, false), "␠ 3 spaces");
        assert_eq!(csv_field(None), "");
        assert_eq!(csv_field(Some("NULL")), "\"NULL\"");
        assert_eq!(csv_field(Some("a;\"b\n")), "\"a;\"\"b\n\"");
    }
    #[test]
    fn exact_numbers_keep_every_digit_sign_and_trailing_zero() {
        assert_eq!(
            number_pt_br("-9007199254740993.123456789000"),
            "-9.007.199.254.740.993,123456789000"
        );
        assert_eq!(number_pt_br("+1234"), "+1.234");
        for value in ["NaN", "Infinity", "1.5e20", "12.3.4", "1."] {
            assert_eq!(number_pt_br(value), value);
        }
        assert_eq!(
            display(Some("2026-11-02 12:34:56+03"), ValueFamily::Temporal, true),
            "2026-11-02 12:34:56+03"
        );
        assert_eq!(display(Some("t"), ValueFamily::Boolean, false), "true");
    }

    #[test]
    fn result_csv_quotes_every_value_and_leaves_null_empty() {
        use crate::result::ResultColumn;
        let result = ResultData::from_rows(
            vec![
                ResultColumn::new("id", "int8"),
                ResultColumn::new("no\"te", "text"),
            ],
            [vec![Some("1"), None], vec![Some("2"), Some("a;\"b\"\nc")]],
        );
        assert_eq!(
            result_csv(&result),
            "\"id\";\"no\"\"te\"\n\"1\";\n\"2\";\"a;\"\"b\"\"\nc\""
        );
    }

    #[test]
    fn sizes_read_in_their_own_scale() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1_023), "1023 B");
        assert_eq!(size(1_536), "1 KB");
        assert_eq!(size(13_002_342), "12.4 MB");
        assert_eq!(size(1024 * 1024 - 1), "1023 KB");
        assert_eq!(size(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
    }

    #[test]
    fn counts_group_digits_in_threes() {
        // Unit tests run in the source language.
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1,000");
        assert_eq!(count(100_000), "100,000");
        assert_eq!(count(1_234_567), "1,234,567");
    }
}
