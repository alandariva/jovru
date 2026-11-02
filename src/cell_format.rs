//! Result presentation grouped by value type family. Formatting never
//! mutates source values or converts exact database numbers through floats.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
