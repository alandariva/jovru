//! Format: lays SQL out by the connection's dialect for the editor's Format
//! button. Only whitespace and the case of keywords change, which a check
//! of every result enforces.
//!
//! With the `sqruff` feature (on by default) sqruff-lib lays each statement
//! out; without it, a simple built-in layout does.
#[cfg(not(feature = "sqruff"))]
use crate::catalog::{is_builtin_function, is_builtin_keyword};
use jykre_sql::{SqlDialect, statements};
#[cfg(not(feature = "sqruff"))]
use jykre_sql::{TokenKind, tokens};
use std::ops::Range;

/// Why [`format`] could not format the SQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatError {
    /// The formatter failed on this input, or its result would change more
    /// than layout and keyword case; the SQL is left as it was.
    Failed,
    /// The caller asked [`format_until`] to stop; nothing was formatted.
    Cancelled,
}

/// The SQL as [`format`] laid it out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Formatted {
    pub sql: String,
    /// Part of the SQL does not parse in the dialect and was left as it was.
    pub partial: bool,
}

/// The rules [`format`] applies: layout (spacing, indentation, line breaks,
/// blank lines) and keyword capitalisation. Each changes only whitespace
/// and the case of keywords; rules that rewrite SQL (aliases, quoting,
/// semicolons, identifier case) are left out.
#[cfg(feature = "sqruff")]
const FORMAT_RULES: &str =
    "LT01,LT02,LT03,LT04,LT05,LT06,LT07,LT08,LT09,LT10,LT11,LT12,LT13,LT14,LT15,CP01";

/// The formatter for `dialect`, built once: building one sets up its
/// dialect's grammar, which takes long. `None` when it could not be built.
#[cfg(feature = "sqruff")]
fn formatter(
    dialect: Option<SqlDialect>,
) -> Option<&'static sqruff_lib::core::linter::core::Linter> {
    use sqruff_lib::core::{config::FluffConfig, linter::core::Linter};
    use std::sync::OnceLock;
    static FORMATTERS: [OnceLock<Option<Linter>>; 5] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    let (slot, name) = match dialect {
        Some(SqlDialect::Postgres) => (0, "postgres"),
        Some(SqlDialect::MySql) => (1, "mysql"),
        Some(SqlDialect::DatabricksSql) => (2, "databricks"),
        Some(SqlDialect::Sqlite) => (4, "sqlite"),
        _ => (3, "ansi"),
    };
    FORMATTERS[slot]
        .get_or_init(|| {
            // In memory only: no configuration file is read, and files of any
            // size are formatted.
            let source = format!(
                "[sqruff]\ndialect = {name}\nrules = {FORMAT_RULES}\nlarge_file_skip_byte_limit = 0\n\n\
                 [sqruff:rules:capitalisation.keywords]\ncapitalisation_policy = upper\n"
            );
            let config = FluffConfig::try_from_source(&source, None).ok()?;
            Linter::new(config, None, None, false).ok()
        })
        .as_ref()
}

/// Whether `after` holds the same text as `before` but for whitespace and
/// ASCII letter case.
fn same_text(before: &str, after: &str) -> bool {
    let text = |sql: &str| {
        sql.chars()
            .filter(|ch| !ch.is_whitespace())
            .map(|ch| ch.to_ascii_lowercase())
            .collect::<Vec<_>>()
    };
    text(before) == text(after)
}

/// One statement laid out by sqruff, without the whitespace around it, or
/// `None` when the formatter failed or changed more than layout and keyword
/// case. The flag tells that part of it does not parse in the dialect.
#[cfg(feature = "sqruff")]
fn format_statement(statement: &str, dialect: Option<SqlDialect>) -> Option<(String, bool)> {
    // A failure of the formatter on some input must not take the
    // application down: the statement is left as it was.
    let run = std::panic::AssertUnwindSafe(|| {
        let linted = formatter(dialect)?
            .lint_string(statement, None, true)
            .ok()?;
        let partial = linted.has_parse_or_templating_errors();
        Some((linted.fix_string().trim().to_owned(), partial))
    });
    match std::panic::catch_unwind(run) {
        Ok(Some((formatted, partial))) if same_text(statement, &formatted) => {
            Some((formatted, partial))
        }
        _ => None,
    }
}

/// Lays out `sql` by the rules of `dialect` (with sqruff, or the built-in
/// layout without the `sqruff` feature): spacing,
/// indentation, line breaks and upper-case keywords. Each statement (as
/// [`statements`] splits them) is formatted on its own, and the text between
/// statements (blank lines, comments, MySQL `DELIMITER` lines) is kept as it
/// was, so the script keeps its separation and every statement keeps its own
/// first line. Only whitespace and the case of keywords change, so literals,
/// comments and identifiers keep their meaning and
/// [`map_offset`](jykre_sql::map_offset) finds any
/// position again; a result that would change anything else is refused. A
/// statement that does not parse in the dialect is left as it was
/// (`partial`).
///
/// It is CPU work that grows with the SQL, and the first call per dialect
/// builds its grammar: callers run it off the UI thread.
pub fn format(sql: &str, dialect: Option<SqlDialect>) -> Result<Formatted, FormatError> {
    format_until(sql, dialect, &|| false)
}

/// [`format`], asking `stop` before each statement: once it answers `true`
/// the work ends with [`FormatError::Cancelled`] and nothing is returned. A
/// statement already being formatted (or the dialect's grammar being built)
/// finishes first, so stopping waits for at most one statement.
pub fn format_until(
    sql: &str,
    dialect: Option<SqlDialect>,
    stop: &dyn Fn() -> bool,
) -> Result<Formatted, FormatError> {
    let mut formatted = String::with_capacity(sql.len() + sql.len() / 8);
    let mut partial = false;
    let mut end = 0;
    for range in statements(sql, dialect) {
        if stop() {
            return Err(FormatError::Cancelled);
        }
        formatted.push_str(&sql[end..range.start]);
        match format_statement(&sql[range.clone()], dialect) {
            Some((statement, unparsed)) => {
                formatted.push_str(&statement);
                partial |= unparsed;
            }
            None => {
                formatted.push_str(&sql[range.clone()]);
                partial = true;
            }
        }
        end = range.end;
    }
    formatted.push_str(&sql[end..]);
    if same_text(sql, &formatted) {
        Ok(Formatted {
            sql: formatted,
            partial,
        })
    } else {
        Err(FormatError::Failed)
    }
}

/// [`format`] applied to `range` of `sql` only, as the Format button does
/// with a selection: the text outside it is kept as it was. Returns the
/// whole text and the range the formatted part now takes.
pub fn format_range(
    sql: &str,
    range: Range<usize>,
    dialect: Option<SqlDialect>,
) -> Result<(Formatted, Range<usize>), FormatError> {
    format_range_until(sql, range, dialect, &|| false)
}

/// [`format_range`], stopping as [`format_until`] does.
pub fn format_range_until(
    sql: &str,
    range: Range<usize>,
    dialect: Option<SqlDialect>,
    stop: &dyn Fn() -> bool,
) -> Result<(Formatted, Range<usize>), FormatError> {
    let part = sql.get(range.clone()).ok_or(FormatError::Failed)?;
    let Formatted {
        sql: formatted,
        partial,
    } = format_until(part, dialect, stop)?;
    let whole = format!("{}{formatted}{}", &sql[..range.start], &sql[range.end..]);
    let end = range.start + formatted.len();
    Ok((
        Formatted {
            sql: whole,
            partial,
        },
        range.start..end,
    ))
}

/// One statement laid out by the built-in layout, used without the `sqruff`
/// feature. It never fails to parse, so nothing is left as it was.
#[cfg(not(feature = "sqruff"))]
fn format_statement(statement: &str, dialect: Option<SqlDialect>) -> Option<(String, bool)> {
    let formatted = builtin(statement, dialect);
    same_text(statement, &formatted).then_some((formatted, false))
}

/// Breaks lines before the main clauses and between the items of a select
/// list, and upper-cases keywords. Comments and literals are kept whole.
#[cfg(not(feature = "sqruff"))]
fn builtin(sql: &str, dialect: Option<SqlDialect>) -> String {
    let mut output = String::new();
    let mut depth = 0usize;
    let mut previous = "";
    let mut select_list = false;
    for (range, kind) in tokens(sql, dialect) {
        let original = &sql[range];
        let upper = original.to_ascii_uppercase();
        let lower = original.to_ascii_lowercase();
        let function = kind == TokenKind::Word && is_builtin_function(&lower);
        let keyword = kind == TokenKind::Word && is_builtin_keyword(&lower);
        let token = if keyword || function {
            upper.as_str()
        } else {
            original
        };
        let newline = kind == TokenKind::Comment
            || depth == 0
                && matches!(
                    token,
                    "SELECT"
                        | "FROM"
                        | "WHERE"
                        | "JOIN"
                        | "LEFT"
                        | "RIGHT"
                        | "INNER"
                        | "GROUP"
                        | "ORDER"
                        | "HAVING"
                        | "LIMIT"
                        | "AND"
                        | "OR"
                );
        if newline && !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        if token == "SELECT" {
            select_list = true;
        } else if token == "FROM" {
            select_list = false;
        }
        let space = !output.is_empty()
            && !output.ends_with([' ', '\n'])
            && !matches!(token, "," | "." | ")" | ";")
            && previous != "."
            && previous != "("
            && !(token == "("
                && kind == TokenKind::Symbol
                && !matches!(previous, "AS" | "IN" | "FROM" | "WITH"));
        if space {
            output.push(' ');
        }
        output.push_str(token);
        if kind == TokenKind::Comment {
            output.push('\n');
        }
        if token == "(" {
            depth += 1;
        } else if token == ")" {
            depth = depth.saturating_sub(1);
        }
        if token == "," && depth == 0 && select_list {
            output.push_str("\n    ");
        }
        previous = if function { "FUNCTION" } else { original };
    }
    output.trim_end().to_string()
}

#[cfg(test)]
mod stop_tests {
    use super::{FormatError, format, format_range_until, format_until};
    use std::cell::Cell;

    const SCRIPT: &str = "select 1;

select a from t where b = 2;
-- kept
select 3;";

    #[test]
    fn stopping_ends_before_the_next_statement_and_returns_nothing() {
        // Asked before every statement: stopping at once formats none.
        assert_eq!(
            format_until(SCRIPT, None, &|| true),
            Err(FormatError::Cancelled)
        );
        // Stopping after the first statement still returns nothing partial.
        let asked = Cell::new(0);
        let stop = || {
            asked.set(asked.get() + 1);
            asked.get() > 1
        };
        assert_eq!(
            format_until(SCRIPT, None, &stop),
            Err(FormatError::Cancelled)
        );
        assert_eq!(asked.get(), 2);
        // Never stopping is Format itself.
        assert_eq!(format_until(SCRIPT, None, &|| false), format(SCRIPT, None));
        assert_eq!(
            format_range_until(SCRIPT, 0..9, None, &|| true),
            Err(FormatError::Cancelled)
        );
    }
}

#[cfg(all(test, feature = "sqruff"))]
mod tests {
    use super::format;
    use jykre_sql::{SqlDialect, TokenKind, map_offset, statements, tokens};

    const MYSQL: Option<SqlDialect> = Some(SqlDialect::MySql);
    const POSTGRES: Option<SqlDialect> = Some(SqlDialect::Postgres);
    const DATABRICKS: Option<SqlDialect> = Some(SqlDialect::DatabricksSql);

    fn split(sql: &str, dialect: Option<SqlDialect>) -> Vec<&str> {
        statements(sql, dialect)
            .into_iter()
            .map(|range| &sql[range])
            .collect()
    }

    fn formatted(sql: &str, dialect: Option<SqlDialect>) -> String {
        format(sql, dialect).expect("the formatter runs").sql
    }

    #[test]
    fn sqlite_keeps_its_bracket_names_and_parameters() {
        const SQLITE: Option<SqlDialect> = Some(SqlDialect::Sqlite);
        let bracketed = formatted(
            "select [order id], \"b\" from t where c = :who and d = ?2 and e = @x",
            SQLITE,
        );
        for kept in ["[order id]", "\"b\"", ":who", "?2", "@x"] {
            assert!(bracketed.contains(kept), "{kept:?} in {bracketed:?}");
        }
        // Without brackets, which sqruff's SQLite grammar may not read, the
        // whole statement is laid out.
        let plain = formatted("select a, \"b\" from t where c = :who", SQLITE);
        assert!(plain.starts_with("SELECT"), "{plain:?}");
        assert!(plain.contains(":who"), "{plain:?}");
    }

    #[test]
    fn format_keeps_operators_literals_and_parameters_intact() {
        let sql = "select a from t where a>=1 and b<=2 and c<>3 and d!=4 and e::int=5 \
                   and f||'x'='y' and g->>'k'=$1 and h=$$a >= b;$$ and i=$q$x$q$";
        let formatted = formatted(sql, POSTGRES);
        for kept in [
            ">= 1",
            "<= 2",
            "<> 3",
            "!= 4",
            "::",
            "||",
            "->>",
            "$1",
            "$$a >= b;$$",
            "$q$x$q$",
        ] {
            assert!(formatted.contains(kept), "{kept:?} in {formatted:?}");
        }
        assert!(formatted.starts_with("SELECT"), "{formatted:?}");
        assert!(!formatted.contains("> ="), "{formatted:?}");
        assert!(!formatted.contains(": :"), "{formatted:?}");
        // Formatting is stable, so a second run changes nothing.
        assert_eq!(super::format(&formatted, POSTGRES).unwrap().sql, formatted);
    }

    #[test]
    fn format_keeps_mysql_backslash_escapes_and_the_statements_apart() {
        // `'a\'b'` is one literal in MySQL: formatting must not treat the
        // rest of the line as a string and change what follows it.
        let sql = "select 'a\\'b' as x, 'c' from t where y = 'd;e';\n\ninsert into t values (1);";
        let formatted = formatted(sql, MYSQL);
        assert!(formatted.contains("'a\\'b' AS x"), "{formatted:?}");
        assert!(formatted.contains("'d;e'"), "{formatted:?}");
        assert!(formatted.contains("INSERT INTO t"), "{formatted:?}");
        // Statements stay apart, with the blank line between them.
        assert!(formatted.contains(";\n\nINSERT"), "{formatted:?}");
        assert_eq!(split(&formatted, MYSQL).len(), 2);
        let kinds: Vec<_> = tokens(sql, MYSQL)
            .into_iter()
            .map(|(range, kind)| (&sql[range], kind))
            .collect();
        assert_eq!(kinds[1], ("'a\\'b'", TokenKind::String));
        // In PostgreSQL a backslash is literal, except in E'' strings.
        assert_eq!(split("SELECT 'C:\\'; SELECT 2;", POSTGRES).len(), 2);
        assert_eq!(split("SELECT E'a\\';b'; SELECT 2;", POSTGRES).len(), 2);
    }

    #[test]
    fn what_does_not_parse_is_left_as_it_was() {
        let sql = "select a from t;\nselect * from orders where";
        let result = format(sql, POSTGRES).unwrap();
        assert!(result.partial);
        assert!(
            result.sql.starts_with("SELECT a FROM t;"),
            "{:?}",
            result.sql
        );
        assert!(result.sql.contains("where"), "{:?}", result.sql);
        let clean = format("select 1", DATABRICKS).unwrap();
        assert!(!clean.partial);
        assert_eq!(clean.sql.trim_end(), "SELECT 1");
        // Without a connection the generic rules apply.
        assert_eq!(formatted("select 1", None).trim_end(), "SELECT 1");
    }

    #[test]
    fn only_layout_and_keyword_case_may_change() {
        assert!(super::same_text("select  a\nfrom t", "SELECT a FROM t"));
        assert!(!super::same_text("select 'a b'", "select 'a', 'b'"));
        assert!(!super::same_text("select a", "select a;"));
    }

    #[test]
    fn map_offset_follows_the_text_through_formatting() {
        let sql = "select a,\n   b from t\nwhere   a = 1";
        let formatted = formatted(sql, POSTGRES);
        for word in ["a,", "b", "t", "where", "1"] {
            let at = sql.find(word).unwrap();
            let mapped = map_offset(sql, &formatted, at + word.len());
            assert!(
                formatted[..mapped]
                    .to_ascii_lowercase()
                    .ends_with(&word.to_ascii_lowercase()),
                "{word:?} -> {mapped} in {formatted:?}"
            );
        }
        assert_eq!(map_offset(sql, &formatted, 0), 0);
        assert_eq!(
            map_offset(sql, &formatted, sql.len()),
            formatted.trim_end().len()
        );
        assert_eq!(map_offset("é  x", "é x", "é  x".len()), "é x".len());
    }
    #[test]
    fn format_keeps_blank_lines_comments_and_each_statement_on_its_own_line() {
        let sql = "insert into t values (1); -- first\n\n\n-- the second\ndelete from t where a = 1;\nupdate t set a = 2;";
        let formatted = formatted(sql, POSTGRES);
        assert!(
            formatted.starts_with(
                "INSERT INTO t VALUES (1); -- first\n\n\n-- the second\nDELETE FROM t"
            ),
            "{formatted:?}"
        );
        assert!(formatted.contains("= 1;\nUPDATE t"), "{formatted:?}");
        // Every statement still starts a line of its own, never sharing one
        // with the statement before it.
        assert_eq!(split(&formatted, POSTGRES).len(), 3);
        for range in statements(&formatted, POSTGRES).into_iter().skip(1) {
            assert!(formatted[..range.start].ends_with('\n'), "{formatted:?}");
        }
    }

    #[test]
    fn format_range_changes_only_the_selection() {
        let sql = "select a from t;\nselect b   from u;\nselect c from v;";
        let start = sql.find("select b").unwrap();
        let end = sql.find("u;").unwrap() + 2;
        let (result, range) = super::format_range(sql, start..end, POSTGRES).unwrap();
        assert!(!result.partial);
        assert_eq!(
            result.sql,
            "select a from t;\nSELECT b FROM u;\nselect c from v;"
        );
        assert_eq!(&result.sql[range], "SELECT b FROM u;");
        assert!(super::format_range(sql, 0..sql.len() + 1, POSTGRES).is_err());
    }
}

#[cfg(all(test, not(feature = "sqruff")))]
mod builtin_tests {
    use super::{format, format_range, same_text};
    use jykre_sql::{SqlDialect, map_offset, statements};

    const MYSQL: Option<SqlDialect> = Some(SqlDialect::MySql);

    fn formatted(sql: &str, dialect: Option<SqlDialect>) -> String {
        format(sql, dialect).expect("the built-in layout runs").sql
    }

    #[test]
    fn the_built_in_layout_keeps_operators_literals_and_parameters_intact() {
        let sql = "select a from t where a>=1 and b<=2 and c<>3 and d!=4 and e::int=5 \
                   and f||'x'='y' and g->>'k'=$1 and h=$$a >= b;$$ and i=$q$x$q$";
        let formatted = formatted(sql, None);
        for kept in [
            ">= 1",
            "<= 2",
            "<> 3",
            "!= 4",
            "::",
            "||",
            "->>",
            "$1",
            "$$a >= b;$$",
            "$q$x$q$",
        ] {
            assert!(formatted.contains(kept), "{kept:?} in {formatted:?}");
        }
        assert!(!formatted.contains("> ="), "{formatted:?}");
        // Formatting is stable, so a second run changes nothing.
        assert_eq!(super::format(&formatted, None).unwrap().sql, formatted);
    }

    #[test]
    fn the_built_in_layout_keeps_mysql_backslash_escapes() {
        let sql = "select 'a\\'b' as x, 'c' from t where y = 'd;e'";
        let formatted = formatted(sql, MYSQL);
        assert!(formatted.contains("'a\\'b' AS x"), "{formatted:?}");
        assert!(formatted.contains("'d;e'"), "{formatted:?}");
        assert_eq!(statements(&formatted, MYSQL).len(), 1);
    }

    #[test]
    fn the_built_in_layout_changes_only_whitespace_and_keyword_case() {
        let sql = "select a,\n   b from t\nwhere   a = 1;\nselect c from u;";
        let formatted = formatted(sql, None);
        assert!(same_text(sql, &formatted));
        let at = sql.find("where").unwrap() + "where".len();
        assert!(formatted[..map_offset(sql, &formatted, at)].ends_with("WHERE"));
        let start = sql.find("select c").unwrap();
        let (result, range) = format_range(sql, start..sql.len(), None).unwrap();
        assert!(
            result.sql.starts_with("select a,\n   b from t"),
            "{:?}",
            result.sql
        );
        assert_eq!(&result.sql[range], "SELECT c\nFROM u;");
    }
}
