//! SQL text handling that needs no window: tokens, statement splitting,
//! layout normalisation, preview checks and formatting.
//!
//! One tokenizer serves every function here, so splitting, highlighting and
//! formatting always agree on where a literal or a comment ends. Its rules
//! follow the connection's [`SqlDialect`]; a tab without a connection uses
//! generic rules (`None`).
//!
//! Everything here is pure and synchronous: the same input gives the same
//! output, and SQL is never sent anywhere or rewritten for execution.
use crate::drivers::SqlDialect;
use crate::tr;
use std::ops::Range;

/// What a token is, for highlighting and formatting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Comment,
    /// A MySQL executable comment (`/*! … */`): code the server runs, kept
    /// whole like a comment.
    Executable,
    String,
    Number,
    Word,
    Keyword,
    Function,
    Symbol,
}

/// The lexical rules of a dialect. Generic rules (no dialect) accept what
/// any of the three databases accepts where they do not conflict.
#[derive(Clone, Copy)]
struct Rules {
    dialect: Option<SqlDialect>,
}

impl Rules {
    fn mysql(self) -> bool {
        matches!(self.dialect, Some(SqlDialect::MySql))
    }

    fn postgres(self) -> bool {
        matches!(self.dialect, Some(SqlDialect::Postgres))
    }

    fn databricks(self) -> bool {
        matches!(self.dialect, Some(SqlDialect::DatabricksSql))
    }

    /// PostgreSQL quotes strings with `$$ … $$` and `$tag$ … $tag$`.
    fn dollar_quotes(self) -> bool {
        !self.mysql() && !self.databricks()
    }

    /// Whether a backslash escapes the next character inside `quote`.
    /// PostgreSQL treats it as an escape only in `E'…'` strings
    /// (`standard_conforming_strings`, on by default).
    fn backslash(self, quote: u8, escape_string: bool) -> bool {
        if self.postgres() {
            escape_string
        } else {
            quote != b'`'
        }
    }

    /// MySQL also starts a line comment with `#`.
    fn hash_comments(self) -> bool {
        self.mysql()
    }

    /// MySQL block comments do not nest; the others do.
    fn nested_comments(self) -> bool {
        !self.mysql()
    }

    /// MySQL identifiers may contain `$`.
    fn word_char(self, ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_' || (ch == '$' && self.mysql())
    }
}

/// Longest first, so `->>` wins over `->`.
const OPERATORS: &[&str] = &[
    "->>", "#>>", "<=>", "->", "#>", "<=", ">=", "<>", "!=", "::", "||", "=>", ":=", "@>", "<@",
    "&&", "<<", ">>", "!~", "~*",
];

/// The end of the line that `from` is on (the offset of its `\n`, or the end
/// of the text).
fn line_end(sql: &str, from: usize) -> usize {
    sql[from..].find('\n').map_or(sql.len(), |at| from + at)
}

/// The end of a quoted literal opened at `start`; unterminated literals run
/// to the end of the text.
fn quoted_end(sql: &str, start: usize, quote: u8, backslash: bool) -> usize {
    let bytes = sql.as_bytes();
    let mut pos = start + 1;
    while pos < bytes.len() {
        let byte = bytes[pos];
        if backslash && byte == b'\\' {
            pos += 1;
            pos += sql[pos..].chars().next().map_or(0, char::len_utf8);
        } else if byte == quote {
            pos += 1;
            if bytes.get(pos) == Some(&quote) {
                pos += 1;
            } else {
                return pos;
            }
        } else {
            pos += sql[pos..].chars().next().map_or(1, char::len_utf8);
        }
    }
    sql.len()
}

/// The end of a block comment opened at `start`, nested when the dialect
/// nests them.
fn block_comment_end(sql: &str, start: usize, nested: bool) -> usize {
    let mut depth = 1;
    let mut pos = start + 2;
    while pos < sql.len() {
        let rest = &sql[pos..];
        if nested && rest.starts_with("/*") {
            depth += 1;
            pos += 2;
        } else if rest.starts_with("*/") {
            depth -= 1;
            pos += 2;
            if depth == 0 {
                return pos;
            }
        } else {
            pos += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
    sql.len()
}

/// The first token at or after byte `pos`, skipping whitespace.
fn next_token(sql: &str, mut pos: usize, rules: Rules) -> Option<(Range<usize>, TokenKind)> {
    let bytes = sql.as_bytes();
    let ch = loop {
        let ch = sql.get(pos..)?.chars().next()?;
        if !ch.is_whitespace() {
            break ch;
        }
        pos += ch.len_utf8();
    };
    let start = pos;
    let rest = &sql[start..];
    let next = bytes.get(start + 1).copied();
    let (end, kind) = if rest.starts_with("--") || (ch == '#' && rules.hash_comments()) {
        (line_end(sql, start), TokenKind::Comment)
    } else if rest.starts_with("/*!") && rules.mysql() {
        // Executable comments do not nest.
        (block_comment_end(sql, start, false), TokenKind::Executable)
    } else if rest.starts_with("/*") {
        (
            block_comment_end(sql, start, rules.nested_comments()),
            TokenKind::Comment,
        )
    } else if matches!(ch, '\'' | '"' | '`') {
        let quote = ch as u8;
        (
            quoted_end(sql, start, quote, rules.backslash(quote, false)),
            TokenKind::String,
        )
    } else if matches!(ch, 'e' | 'E') && next == Some(b'\'') && rules.postgres() {
        // A PostgreSQL escape string, where a backslash escapes.
        (
            quoted_end(sql, start + 1, b'\'', rules.backslash(b'\'', true)),
            TokenKind::String,
        )
    } else if ch == '$' && rules.dollar_quotes() {
        let tag = &sql[start + 1..];
        let tag_len = tag
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(tag.len());
        if tag[tag_len..].starts_with('$') && !tag.starts_with(|c: char| c.is_ascii_digit()) {
            // Dollar quoting: `$$…$$` or `$tag$…$tag$`.
            let delimiter = &sql[start..start + tag_len + 2];
            let body = start + delimiter.len();
            let end = sql[body..]
                .find(delimiter)
                .map_or(sql.len(), |close| body + close + delimiter.len());
            (end, TokenKind::String)
        } else {
            // Positional parameter such as `$1`.
            (start + 1 + tag_len, TokenKind::Word)
        }
    } else if ch.is_ascii_digit() {
        let len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        (start + len, TokenKind::Number)
    } else if rules.word_char(ch) {
        let len = rest
            .find(|c: char| !rules.word_char(c))
            .unwrap_or(rest.len());
        let end = start + len;
        let kind = match sql[start..end].to_ascii_uppercase().as_str() {
            "SELECT" | "FROM" | "WHERE" | "JOIN" | "LEFT" | "RIGHT" | "INNER" | "OUTER" | "ON"
            | "AS" | "AND" | "OR" | "NOT" | "NULL" | "IS" | "IN" | "BETWEEN" | "LIKE" | "GROUP"
            | "BY" | "ORDER" | "DESC" | "ASC" | "LIMIT" | "OFFSET" | "WITH" | "DISTINCT"
            | "CASE" | "WHEN" | "THEN" | "ELSE" | "END" | "HAVING" => TokenKind::Keyword,
            "COUNT" | "SUM" | "AVG" | "ROUND" | "MAX" | "MIN" | "COALESCE" | "DATE"
            | "DATE_FORMAT" | "DATE_TRUNC" | "EXPLAIN" | "ANALYZE" => TokenKind::Function,
            _ => TokenKind::Word,
        };
        (end, kind)
    } else {
        // Multi-character operators are one token, so formatting never
        // splits `>=` into `> =` or `::` into `: :`.
        let len = OPERATORS
            .iter()
            .find(|operator| rest.starts_with(*operator))
            .map_or(ch.len_utf8(), |operator| operator.len());
        (start + len, TokenKind::Symbol)
    };
    Some((start..end, kind))
}

/// Byte ranges of every token with its kind, by the rules of `dialect`.
/// Whitespace is not a token; unterminated comments and literals run to the
/// end of the text.
pub fn tokens(sql: &str, dialect: Option<SqlDialect>) -> Vec<(Range<usize>, TokenKind)> {
    let rules = Rules { dialect };
    let mut tokens = Vec::new();
    let mut pos = 0;
    while let Some((range, kind)) = next_token(sql, pos, rules) {
        pos = range.end;
        tokens.push((range, kind));
    }
    tokens
}

/// The word of a token, upper case, or `None` for any other kind.
fn word(sql: &str, token: &(Range<usize>, TokenKind)) -> Option<String> {
    matches!(
        token.1,
        TokenKind::Word | TokenKind::Keyword | TokenKind::Function
    )
    .then(|| sql[token.0.clone()].to_ascii_uppercase())
}

/// The next token after `pos` that is not a comment.
fn next_code(sql: &str, pos: usize, rules: Rules) -> Option<(Range<usize>, TokenKind)> {
    let mut pos = pos;
    loop {
        let token = next_token(sql, pos, rules)?;
        if token.1 != TokenKind::Comment {
            return Some(token);
        }
        pos = token.0.end;
    }
}

/// The statement being read by [`statements`].
#[derive(Default)]
struct Pending {
    start: Option<usize>,
    /// The first word, upper case.
    first: Option<String>,
    /// A `CREATE PROCEDURE/FUNCTION/TRIGGER/EVENT`, whose body may hold
    /// semicolons.
    routine: bool,
    /// Open `BEGIN … END` and `CASE … END` blocks of a routine body.
    depth: usize,
    /// The previous word was an `END` that the next word completes
    /// (`END CASE`, `END IF`), so that word opens nothing.
    ending: bool,
}

impl Pending {
    /// Follows the blocks of a routine body, so its semicolons do not end
    /// the statement. `word` is upper case.
    fn track(&mut self, word: &str, sql: &str, end: usize, rules: Rules) {
        if std::mem::take(&mut self.ending) {
            return;
        }
        let first = self.first.is_none();
        if first {
            self.first = Some(word.to_owned());
        }
        let creating = self.first.as_deref() == Some("CREATE");
        if creating
            && self.depth == 0
            && matches!(word, "PROCEDURE" | "FUNCTION" | "TRIGGER" | "EVENT")
        {
            self.routine = true;
        }
        let following = || next_code(sql, end, rules).and_then(|token| self::word(sql, &token));
        match word {
            "BEGIN" => {
                let opens = if self.depth > 0 {
                    !rules.postgres()
                } else if rules.mysql() {
                    self.routine
                } else if rules.postgres() {
                    following().as_deref() == Some("ATOMIC")
                } else if rules.databricks() {
                    // SQL scripting: a compound statement, or a routine body.
                    (first || self.routine)
                        && !matches!(following().as_deref(), Some("TRANSACTION" | "WORK"))
                        && next_code(sql, end, rules).is_some_and(|token| &sql[token.0] != ";")
                } else {
                    false
                };
                if opens {
                    self.depth += 1;
                }
            }
            "CASE" if self.depth > 0 => self.depth += 1,
            // `END IF`, `END LOOP` and the like close a control statement,
            // which never opened a block here.
            "END" if self.depth > 0 => {
                let next = following();
                if !matches!(
                    next.as_deref(),
                    Some("IF" | "LOOP" | "WHILE" | "REPEAT" | "FOR")
                ) {
                    self.depth -= 1;
                }
                self.ending = matches!(
                    next.as_deref(),
                    Some("IF" | "LOOP" | "WHILE" | "REPEAT" | "FOR" | "CASE")
                );
            }
            _ => {}
        }
    }
}

/// Executable ranges, by the rules of `dialect`, excluding leading comments
/// and empty statements.
///
/// - A `;` outside literals, comments and dollar quotes ends a statement and
///   belongs to it.
/// - The body of a MySQL `CREATE PROCEDURE/FUNCTION/TRIGGER/EVENT`, a
///   PostgreSQL `BEGIN ATOMIC … END` and a Databricks `BEGIN … END` compound
///   statement keep their semicolons.
/// - MySQL: a `DELIMITER` line changes the delimiter and is not a statement
///   (the server never sees it); a custom delimiter is not part of the
///   statement. Executable comments (`/*! … */`) are code.
pub fn statements(sql: &str, dialect: Option<SqlDialect>) -> Vec<Range<usize>> {
    let rules = Rules { dialect };
    let mut ranges = Vec::new();
    let mut delimiter = String::from(";");
    let mut pending = Pending::default();
    let mut pos = 0;
    while let Some(token) = next_token(sql, pos, rules) {
        let (range, kind) = token.clone();
        pos = range.end;
        if kind == TokenKind::Comment {
            continue;
        }
        let name = word(sql, &token);
        if pending.start.is_none() && rules.mysql() && name.as_deref() == Some("DELIMITER") {
            let end = line_end(sql, range.end);
            if let Some(value) = sql[range.end..end].split_whitespace().next() {
                value.clone_into(&mut delimiter);
            }
            pos = end;
            continue;
        }
        let found = if kind == TokenKind::String || kind == TokenKind::Executable {
            None
        } else if sql[range.start..].starts_with(delimiter.as_str()) {
            Some(range.start)
        } else {
            sql[range.clone()]
                .find(delimiter.as_str())
                .map(|at| range.start + at)
        };
        match found {
            // Inside a routine body, `;` separates its statements.
            Some(at) if !(delimiter == ";" && pending.depth > 0) => {
                if at > range.start {
                    pending.start.get_or_insert(range.start);
                }
                if let Some(start) = pending.start {
                    let end = if delimiter == ";" {
                        at + 1
                    } else {
                        start + sql[start..at].trim_end().len()
                    };
                    ranges.push(start..end);
                }
                pending = Pending::default();
                pos = at + delimiter.len();
            }
            _ => {
                pending.start.get_or_insert(range.start);
                if delimiter == ";"
                    && let Some(name) = name
                {
                    pending.track(&name, sql, range.end, rules);
                }
            }
        }
    }
    if let Some(start) = pending.start {
        ranges.push(start..sql.trim_end().len().max(start));
    }
    ranges
}

/// The statement Ctrl+Enter runs with the cursor at byte `cursor`, as an
/// index into [`statements`]:
///
/// - inside a statement, or right after its `;`, that statement;
/// - after a statement on its last line (spaces or a comment in between),
///   the statement that ended there;
/// - before a statement on its first line (indentation), that statement.
///
/// `None` when the cursor is on no statement (a blank line, a comment line,
/// before the first or after the last one) or there is nothing to run: the
/// caller asks the user to point at the statement instead of guessing.
pub fn statement_at(sql: &str, cursor: usize, dialect: Option<SqlDialect>) -> Option<usize> {
    let ranges = statements(sql, dialect);
    let cursor = cursor.min(sql.len());
    if let Some(index) = ranges
        .iter()
        .position(|range| range.start <= cursor && cursor <= range.end)
    {
        return Some(index);
    }
    let next = ranges.iter().position(|range| range.start > cursor);
    let previous = match next {
        Some(next) => next.checked_sub(1),
        None => ranges.len().checked_sub(1),
    };
    let same_line = |gap: Option<&str>| gap.is_some_and(|gap| !gap.contains('\n'));
    if let Some(previous) = previous
        && same_line(sql.get(ranges[previous].end..cursor))
    {
        return Some(previous);
    }
    next.filter(|next| same_line(sql.get(cursor..ranges[*next].start)))
}

/// The zero-based line on which each of [`statements`] starts. One pass over
/// the text, so a view can keep the result while the text is unchanged
/// instead of counting lines on every frame.
pub fn statement_lines(sql: &str, dialect: Option<SqlDialect>) -> Vec<usize> {
    let bytes = sql.as_bytes();
    let mut line = 0;
    let mut counted = 0;
    statements(sql, dialect)
        .into_iter()
        .map(|range| {
            line += bytes[counted..range.start]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count();
            counted = range.start;
            line
        })
        .collect()
}

/// Executable statements with runs of whitespace collapsed, so layout-only
/// edits (new lines, indentation) do not count as a different statement.
pub fn normalized_statements(sql: &str, dialect: Option<SqlDialect>) -> Vec<String> {
    statements(sql, dialect)
        .into_iter()
        .map(|range| sql[range].split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// Why the demo would refuse to run `sql`, or `None` when it accepts it. The
/// demo only simulates read queries; real connections never use this check.
pub fn preview_error(sql: &str) -> Option<&'static str> {
    let first = tokens(sql, None).into_iter().next()?;
    let command = sql[first.0].to_ascii_uppercase();
    if !matches!(
        command.as_str(),
        "SELECT" | "WITH" | "EXPLAIN" | "SHOW" | "DESCRIBE" | "VALUES"
    ) {
        Some(tr!(
            "The demo accepts only read queries (SELECT, WITH, EXPLAIN, SHOW, DESCRIBE or VALUES)."
        ))
    } else if command == "SELECT"
        && sql
            .trim_end_matches([';', ' ', '\n', '\r'])
            .trim()
            .eq_ignore_ascii_case("SELECT")
    {
        Some(tr!("SELECT needs an expression or column."))
    } else {
        None
    }
}

/// Why [`format`] could not format the SQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatError {
    /// The formatter failed on this input, or its result would change more
    /// than layout and keyword case; the SQL is left as it was.
    Failed,
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
const FORMAT_RULES: &str =
    "LT01,LT02,LT03,LT04,LT05,LT06,LT07,LT08,LT09,LT10,LT11,LT12,LT13,LT14,LT15,CP01";

/// The formatter for `dialect`, built once: building one sets up its
/// dialect's grammar, which takes long. `None` when it could not be built.
fn formatter(
    dialect: Option<SqlDialect>,
) -> Option<&'static sqruff_lib::core::linter::core::Linter> {
    use sqruff_lib::core::{config::FluffConfig, linter::core::Linter};
    use std::sync::OnceLock;
    static FORMATTERS: [OnceLock<Option<Linter>>; 4] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    let (slot, name) = match dialect {
        Some(SqlDialect::Postgres) => (0, "postgres"),
        Some(SqlDialect::MySql) => (1, "mysql"),
        Some(SqlDialect::DatabricksSql) => (2, "databricks"),
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

/// Lays out `sql` by the rules of `dialect` with sqruff: spacing,
/// indentation, line breaks and upper-case keywords. Only whitespace and the
/// case of keywords change, so literals, comments and identifiers keep their
/// meaning and [`map_offset`] finds any position again; a result that would
/// change anything else is refused. What does not parse in the dialect is
/// left as it was (`partial`).
///
/// It is CPU work that grows with the SQL, and the first call per dialect
/// builds its grammar: callers run it off the UI thread.
pub fn format(sql: &str, dialect: Option<SqlDialect>) -> Result<Formatted, FormatError> {
    // A failure of the formatter on some input must not take the
    // application down: it is reported as such and the SQL stays.
    let run = std::panic::AssertUnwindSafe(|| {
        let linted = formatter(dialect)?.lint_string(sql, None, true).ok()?;
        let partial = linted.has_parse_or_templating_errors();
        Some(Formatted {
            sql: linted.fix_string(),
            partial,
        })
    });
    match std::panic::catch_unwind(run) {
        Ok(Some(formatted)) if same_text(sql, &formatted.sql) => Ok(formatted),
        _ => Err(FormatError::Failed),
    }
}

/// Where byte `offset` of `before` is in `after`, when the two differ only
/// in whitespace and letter case (as [`format`] guarantees): the position
/// after the same number of non-whitespace characters. Used to keep the
/// caret and the selection on the same text after formatting.
pub fn map_offset(before: &str, after: &str, offset: usize) -> usize {
    let offset = offset.min(before.len());
    let kept = before
        .char_indices()
        .take_while(|(at, _)| *at < offset)
        .filter(|(_, ch)| !ch.is_whitespace())
        .count();
    if kept == 0 {
        return 0;
    }
    after
        .char_indices()
        .filter(|(_, ch)| !ch.is_whitespace())
        .nth(kept - 1)
        .map_or(after.len(), |(at, ch)| at + ch.len_utf8())
}

#[cfg(test)]
mod tests {
    use super::{
        SqlDialect, TokenKind, format, map_offset, normalized_statements, preview_error,
        statement_at, statement_lines, statements, tokens,
    };
    use std::time::{Duration, Instant};

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
    fn normalized_statements_ignore_layout_only() {
        assert_eq!(
            normalized_statements("SELECT\n    1;\n\n  SELECT  2;\n", None),
            ["SELECT 1;", "SELECT 2;"]
        );
    }

    #[test]
    fn finds_queries_without_splitting_literals_or_comments() {
        let sql = "-- résumé start\nSELECT 'a;''b', \"x;y\";\n/* ; /* nested */ */ SELECT $$a;b$$; -- end\n;";
        let queries = statements(sql, None);
        assert_eq!(queries.len(), 2);
        assert_eq!(&sql[queries[0].clone()], "SELECT 'a;''b', \"x;y\";");
        assert_eq!(&sql[queries[1].clone()], "SELECT $$a;b$$;");
        assert!(statements("-- comment\n/* only */ ;", None).is_empty());
        assert_eq!(
            split(sql, POSTGRES),
            ["SELECT 'a;''b', \"x;y\";", "SELECT $$a;b$$;"]
        );
    }

    #[test]
    fn mysql_routines_delimiters_and_executable_comments() {
        // A procedure body keeps its semicolons, even without DELIMITER.
        let sql = "CREATE PROCEDURE p()\nBEGIN\n  DECLARE x INT;\n  IF x THEN SELECT 1; END IF;\n  \
                   CASE x WHEN 1 THEN SELECT 2; END CASE;\nEND;\nSELECT 3;";
        let parts = split(sql, MYSQL);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].starts_with("CREATE PROCEDURE") && parts[0].ends_with("END;"));
        assert_eq!(parts[1], "SELECT 3;");
        // DELIMITER lines are not sent, and the custom delimiter is not part
        // of the statement.
        let sql = "DELIMITER //\nCREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW\nBEGIN\n  SET @a = 1;\nEND//\nDELIMITER ;\nSELECT 1;";
        let parts = split(sql, MYSQL);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].starts_with("CREATE TRIGGER") && parts[0].ends_with("END"));
        assert_eq!(parts[1], "SELECT 1;");
        let sql =
            "DELIMITER $$\nCREATE FUNCTION f() RETURNS INT BEGIN RETURN 1; END$$\nDELIMITER ;";
        assert_eq!(
            split(sql, MYSQL),
            ["CREATE FUNCTION f() RETURNS INT BEGIN RETURN 1; END"]
        );
        // A leading executable comment is code, not a comment to skip.
        assert_eq!(
            split("/*!40101 SET NAMES utf8 */;\n# note\nSELECT 1;", MYSQL),
            ["/*!40101 SET NAMES utf8 */;", "SELECT 1;"]
        );
        // A transaction BEGIN is an ordinary statement.
        assert_eq!(
            split("BEGIN; UPDATE t SET a = 1; COMMIT;", MYSQL),
            ["BEGIN;", "UPDATE t SET a = 1;", "COMMIT;"]
        );
        // Without a dialect, the generic rules split on every semicolon.
        assert_eq!(
            split("CREATE PROCEDURE p() BEGIN SELECT 1; END;", None).len(),
            2
        );
    }

    #[test]
    fn postgres_begin_atomic_and_databricks_compound_statements() {
        let sql = "CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql\nBEGIN ATOMIC\n  SELECT CASE WHEN a > 0 THEN 1 ELSE 0 END;\n  SELECT 2;\nEND;\nSELECT 3;";
        let parts = split(sql, POSTGRES);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].ends_with("END;"));
        assert_eq!(
            split("BEGIN; SELECT 1; COMMIT;", POSTGRES),
            ["BEGIN;", "SELECT 1;", "COMMIT;"]
        );
        let sql = "BEGIN\n  DECLARE x INT DEFAULT 0;\n  WHILE x < 2 DO SET x = x + 1; END WHILE;\nEND;\nSELECT 1;";
        let parts = split(sql, DATABRICKS);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[1], "SELECT 1;");
        assert_eq!(
            split("BEGIN TRANSACTION; SELECT 1;", DATABRICKS),
            ["BEGIN TRANSACTION;", "SELECT 1;"]
        );
    }

    #[test]
    fn the_cursor_picks_the_statement_to_run() {
        let sql = "SELECT * FROM users; SELECT 1;\n\nSELECT * FROM teste";
        let at = |marker: &str, offset: usize| {
            let cursor = sql.find(marker).expect("marker in the script") + offset;
            statement_at(sql, cursor, None).map(|index| &sql[statements(sql, None)[index].clone()])
        };
        assert_eq!(at("users", 2), Some("SELECT * FROM users;"));
        assert_eq!(at("SELECT 1;", "SELECT 1;".len()), Some("SELECT 1;"));
        // A blank line between statements runs nothing.
        assert_eq!(at("\n\n", 1), None);
        assert_eq!(at("teste", 3), Some("SELECT * FROM teste"));
        assert_eq!(at("teste", 5), Some("SELECT * FROM teste"));
        // Right after a `;` that touches the next statement, the one that ended.
        assert_eq!(at("users;", 6), Some("SELECT * FROM users;"));
        assert_eq!(at(" SELECT 1", 1), Some("SELECT 1;"));
    }

    #[test]
    fn the_cursor_between_or_around_statements() {
        // Spaces or a comment after the `;` keep the statement of that line.
        let sql = "SELECT 1;   -- done\nSELECT 2;";
        assert_eq!(statement_at(sql, sql.find("--").unwrap(), None), Some(0));
        assert_eq!(statement_at(sql, sql.find('\n').unwrap(), None), Some(0));
        assert_eq!(
            statement_at(sql, sql.find('\n').unwrap() + 1, None),
            Some(1)
        );
        // Indentation before a statement belongs to it.
        let sql = "SELECT 1;\n    SELECT 2;";
        assert_eq!(statement_at(sql, sql.find("    ").unwrap(), None), Some(1));
        // A comment line above a statement is on no statement.
        let sql = "SELECT 1;\n-- next\nSELECT 2;";
        assert_eq!(statement_at(sql, sql.find("next").unwrap(), None), None);
        // Blank lines before or after the only statement are on none.
        let sql = "\n\n  SELECT 1;\n\n";
        assert_eq!(statement_at(sql, 0, None), None);
        assert_eq!(statement_at(sql, sql.len(), None), None);
        assert_eq!(statement_at(sql, sql.len() + 10, None), None);
        assert_eq!(statement_at(sql, sql.find("  S").unwrap(), None), Some(0));
        // Semicolons in literals and comments do not split.
        let sql = "SELECT 'a;b'; /* ; */ SELECT $$;$$";
        assert_eq!(statement_at(sql, sql.find("$$").unwrap(), None), Some(1));
        assert_eq!(statement_at(sql, 9, None), Some(0));
        for empty in ["", "   ", "-- only a comment\n", "/* ; */ ;"] {
            assert_eq!(statement_at(empty, 0, None), None, "{empty:?}");
        }
    }

    #[test]
    fn statement_lines_skip_comments_and_follow_multiline_literals() {
        let sql =
            "-- intro\nSELECT 1;\n\n/* a\n b */ SELECT 'x;\ny';\nSELECT\n  2; SELECT $$\n;$$;";
        assert_eq!(statement_lines(sql, None), [1, 4, 6, 7]);
        // The same lines as counting from the start for every statement.
        let counted: Vec<_> = statements(sql, None)
            .into_iter()
            .map(|range| sql[..range.start].matches('\n').count())
            .collect();
        assert_eq!(statement_lines(sql, None), counted);
        assert!(statement_lines("-- only a comment\n", None).is_empty());
    }

    #[test]
    fn a_long_script_maps_its_statements_to_lines_within_budget() {
        // Counting lines from the start for every statement is quadratic in
        // the script; this is one pass, paid once per edit.
        let script: String = (0..10_000)
            .map(|index| format!("-- statement {index}\nSELECT 'a;b' FROM t{index};\n"))
            .collect();
        let start = Instant::now();
        let lines = statement_lines(&script, None);
        let elapsed = start.elapsed();
        assert_eq!(lines.len(), 10_000);
        assert_eq!(lines[9_999], 19_999);
        assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
    }

    #[test]
    fn handles_unicode_and_unterminated_queries() {
        let sql = "SELECT 'ação;';\n SELECT $tag$é;$tag$";
        assert_eq!(statements(sql, None).len(), 2);
        assert_eq!(statements("SELECT 'é", None), vec![0..10]);
        assert!(preview_error("SELECT;").is_some());
        assert!(preview_error("DELETE FROM orders;").is_some());
        assert!(preview_error("SELECT 1;").is_none());
    }

    #[test]
    fn tokens_classify_keywords_literals_comments_and_operators() {
        let sql = "select count(x) -- c\nfrom t where a->>'k' = 1.5";
        let kinds: Vec<_> = tokens(sql, None)
            .into_iter()
            .map(|(range, kind)| (&sql[range], kind))
            .collect();
        assert_eq!(kinds[0], ("select", TokenKind::Keyword));
        assert_eq!(kinds[1], ("count", TokenKind::Function));
        assert!(kinds.contains(&("-- c", TokenKind::Comment)));
        assert!(kinds.contains(&("->>", TokenKind::Symbol)));
        assert!(kinds.contains(&("'k'", TokenKind::String)));
        assert!(kinds.contains(&("1.5", TokenKind::Number)));
        // The same input always gives the same output.
        assert_eq!(tokens(sql, None), tokens(sql, None));
        // MySQL: `#` comments and `$` inside identifiers.
        let sql = "SELECT a$b # note";
        let kinds: Vec<_> = tokens(sql, MYSQL)
            .into_iter()
            .map(|(range, kind)| (&sql[range], kind))
            .collect();
        assert_eq!(kinds[1], ("a$b", TokenKind::Word));
        assert_eq!(kinds[2], ("# note", TokenKind::Comment));
    }
}
