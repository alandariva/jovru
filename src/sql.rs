//! SQL text handling that needs no window: statement splitting, layout
//! normalisation, preview checks, tokens for highlighting and formatting.
//!
//! Everything here is pure and synchronous: the same input gives the same
//! output, and SQL is never sent anywhere or rewritten for execution.
use crate::tr;
use std::ops::Range;

/// What a token is, for highlighting and formatting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Comment,
    String,
    Number,
    Word,
    Keyword,
    Function,
    Symbol,
}

/// Executable ranges, excluding leading comments and empty statements.
/// Quoted semicolons (including PostgreSQL dollar quotes) are not separators.
pub fn statements(sql: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = None;
    let mut pos = 0;
    let bytes = sql.as_bytes();
    while pos < bytes.len() {
        if bytes[pos].is_ascii_whitespace() {
            pos += 1;
            continue;
        }
        if sql[pos..].starts_with("--") {
            pos += sql[pos..].find('\n').unwrap_or(sql.len() - pos);
            continue;
        }
        if sql[pos..].starts_with("/*") {
            let mut depth = 1;
            pos += 2;
            while pos < bytes.len() && depth > 0 {
                if sql[pos..].starts_with("/*") {
                    depth += 1;
                    pos += 2;
                } else if sql[pos..].starts_with("*/") {
                    depth -= 1;
                    pos += 2;
                } else {
                    pos += sql[pos..].chars().next().map_or(1, char::len_utf8);
                }
            }
            continue;
        }
        if bytes[pos] == b';' {
            if let Some(begin) = start.take() {
                ranges.push(begin..pos + 1);
            }
            pos += 1;
            continue;
        }
        start.get_or_insert(pos);
        let quote = bytes[pos];
        // End of a PostgreSQL dollar-quote opener such as `$$` or `$tag$`.
        let dollar = (quote == b'$')
            .then(|| sql[pos + 1..].find('$'))
            .flatten()
            .filter(|end| {
                sql[pos + 1..pos + 1 + end]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
            .map(|end| pos + 2 + end);
        if matches!(quote, b'\'' | b'"' | b'`') {
            pos += 1;
            while pos < bytes.len() {
                if bytes[pos] == b'\\' {
                    pos = (pos + 2).min(bytes.len());
                } else if bytes[pos] == quote {
                    pos += 1;
                    if bytes.get(pos) == Some(&quote) {
                        pos += 1;
                    } else {
                        break;
                    }
                } else {
                    pos += 1;
                }
            }
        } else if let Some(end) = dollar {
            let delimiter = &sql[pos..end];
            pos = sql[end..]
                .find(delimiter)
                .map_or(sql.len(), |close| end + close + delimiter.len());
        } else {
            pos += sql[pos..].chars().next().map_or(1, char::len_utf8);
        }
    }
    if let Some(begin) = start {
        ranges.push(begin..sql.trim_end().len());
    }
    ranges
}

/// The statement Ctrl+Enter runs with the cursor at byte `cursor`, as an
/// index into [`statements`]:
///
/// - inside a statement, or right after its `;`, that statement;
/// - after a `;` on the same line (spaces or a comment in between), the
///   statement that ended there;
/// - on a line between two statements, the next one;
/// - before the first or after the last statement, the nearest one.
///
/// `None` when there is nothing to run.
pub fn statement_at(sql: &str, cursor: usize) -> Option<usize> {
    let ranges = statements(sql);
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
    match (previous, next) {
        (Some(previous), Some(next)) => {
            let gap = sql.get(ranges[previous].end..cursor).unwrap_or("\n");
            Some(if gap.contains('\n') { next } else { previous })
        }
        (previous, next) => previous.or(next),
    }
}

/// The zero-based line on which each of [`statements`] starts. One pass over
/// the text, so a view can keep the result while the text is unchanged
/// instead of counting lines on every frame.
pub fn statement_lines(sql: &str) -> Vec<usize> {
    let bytes = sql.as_bytes();
    let mut line = 0;
    let mut counted = 0;
    statements(sql)
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
pub fn normalized_statements(sql: &str) -> Vec<String> {
    statements(sql)
        .into_iter()
        .map(|range| sql[range].split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// Why the demo would refuse to run `sql`, or `None` when it accepts it. The
/// demo only simulates read queries; real connections never use this check.
pub fn preview_error(sql: &str) -> Option<&'static str> {
    let first = tokens(sql).into_iter().next()?;
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

/// Longest first, so `->>` wins over `->`.
const OPERATORS: &[&str] = &[
    "->>", "#>>", "<=>", "->", "#>", "<=", ">=", "<>", "!=", "::", "||", "=>", ":=", "@>", "<@",
    "&&", "<<", ">>", "!~", "~*",
];

/// Byte ranges of every token with its kind. Whitespace is not a token;
/// unterminated comments and literals run to the end of the text.
pub fn tokens(sql: &str) -> Vec<(Range<usize>, TokenKind)> {
    let mut tokens = Vec::new();
    let mut chars = sql.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        let kind = if ch == '-' && chars.peek().is_some_and(|(_, c)| *c == '-') {
            chars.next();
            while chars.peek().is_some_and(|(_, c)| *c != '\n') {
                chars.next();
            }
            TokenKind::Comment
        } else if ch == '/' && chars.peek().is_some_and(|(_, c)| *c == '*') {
            chars.next();
            while let Some((_, next)) = chars.next() {
                if next == '*' && chars.peek().is_some_and(|(_, c)| *c == '/') {
                    chars.next();
                    break;
                }
            }
            TokenKind::Comment
        } else if ch == '\'' || ch == '"' || ch == '`' {
            while let Some((_, next)) = chars.next() {
                if next == ch {
                    if chars.peek().is_some_and(|(_, c)| *c == ch) {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            TokenKind::String
        } else if ch == '$' {
            let rest = &sql[start + 1..];
            let tag_len = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if rest[tag_len..].starts_with('$')
                && !rest[..tag_len].starts_with(|c: char| c.is_ascii_digit())
            {
                // PostgreSQL dollar quoting: `$$…$$` or `$tag$…$tag$`.
                let delimiter = &sql[start..start + tag_len + 2];
                let body = start + delimiter.len();
                let end = sql[body..]
                    .find(delimiter)
                    .map_or(sql.len(), |close| body + close + delimiter.len());
                while chars.peek().is_some_and(|(offset, _)| *offset < end) {
                    chars.next();
                }
                TokenKind::String
            } else {
                // Positional parameter such as `$1`.
                while chars
                    .peek()
                    .is_some_and(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
                {
                    chars.next();
                }
                TokenKind::Word
            }
        } else if ch.is_ascii_digit() {
            while chars
                .peek()
                .is_some_and(|(_, c)| c.is_ascii_digit() || *c == '.')
            {
                chars.next();
            }
            TokenKind::Number
        } else if ch.is_alphabetic() || ch == '_' {
            while chars
                .peek()
                .is_some_and(|(_, c)| c.is_alphanumeric() || *c == '_')
            {
                chars.next();
            }
            let end = chars.peek().map_or(sql.len(), |(offset, _)| *offset);
            let word = sql[start..end].to_ascii_uppercase();
            match word.as_str() {
                "SELECT" | "FROM" | "WHERE" | "JOIN" | "LEFT" | "RIGHT" | "INNER" | "OUTER"
                | "ON" | "AS" | "AND" | "OR" | "NOT" | "NULL" | "IS" | "IN" | "BETWEEN"
                | "LIKE" | "GROUP" | "BY" | "ORDER" | "DESC" | "ASC" | "LIMIT" | "OFFSET"
                | "WITH" | "DISTINCT" | "CASE" | "WHEN" | "THEN" | "ELSE" | "END" | "HAVING" => {
                    TokenKind::Keyword
                }
                "COUNT" | "SUM" | "AVG" | "ROUND" | "MAX" | "MIN" | "COALESCE" | "DATE"
                | "DATE_FORMAT" | "DATE_TRUNC" | "EXPLAIN" | "ANALYZE" => TokenKind::Function,
                _ => TokenKind::Word,
            }
        } else {
            // Multi-character operators are one token, so formatting never
            // splits `>=` into `> =` or `::` into `: :`.
            if let Some(operator) = OPERATORS
                .iter()
                .find(|operator| sql[start..].starts_with(*operator))
            {
                for _ in 1..operator.len() {
                    chars.next();
                }
            }
            TokenKind::Symbol
        };
        let end = chars.peek().map_or(sql.len(), |(offset, _)| *offset);
        tokens.push((start..end, kind));
    }
    tokens
}

/// Format token boundaries while preserving comments and string contents.
pub fn format(sql: &str) -> String {
    let mut output = String::new();
    let mut depth = 0usize;
    let mut previous = "";
    let mut select_list = false;
    for (range, kind) in tokens(sql) {
        let original = &sql[range];
        let upper = original.to_ascii_uppercase();
        let token = if kind == TokenKind::Keyword || kind == TokenKind::Function {
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
        previous = if kind == TokenKind::Function {
            "FUNCTION"
        } else {
            original
        };
    }
    output.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        TokenKind, format, normalized_statements, preview_error, statement_at, statement_lines,
        statements, tokens,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn format_keeps_operators_literals_and_parameters_intact() {
        let sql = "select a from t where a>=1 and b<=2 and c<>3 and d!=4 and e::int=5 \
                   and f||'x'='y' and g->>'k'=$1 and h=$$a >= b;$$ and i=$q$x$q$";
        let formatted = format(sql);
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
        assert!(!formatted.contains(": :"), "{formatted:?}");
        // Formatting is stable, so a second run changes nothing.
        assert_eq!(format(&formatted), formatted);
    }

    #[test]
    fn normalized_statements_ignore_layout_only() {
        assert_eq!(
            normalized_statements("SELECT\n    1;\n\n  SELECT  2;\n"),
            ["SELECT 1;", "SELECT 2;"]
        );
    }

    #[test]
    fn finds_queries_without_splitting_literals_or_comments() {
        let sql = "-- résumé start\nSELECT 'a;''b', \"x;y\";\n/* ; /* nested */ */ SELECT $$a;b$$; -- end\n;";
        let queries = statements(sql);
        assert_eq!(queries.len(), 2);
        assert_eq!(&sql[queries[0].clone()], "SELECT 'a;''b', \"x;y\";");
        assert_eq!(&sql[queries[1].clone()], "SELECT $$a;b$$;");
        assert!(statements("-- comment\n/* only */ ;").is_empty());
    }

    #[test]
    fn the_cursor_picks_the_statement_to_run() {
        let sql = "SELECT * FROM users; SELECT 1;\n\nSELECT * FROM teste";
        let at = |marker: &str, offset: usize| {
            let cursor = sql.find(marker).expect("marker in the script") + offset;
            statement_at(sql, cursor).map(|index| &sql[statements(sql)[index].clone()])
        };
        assert_eq!(at("users", 2), Some("SELECT * FROM users;"));
        assert_eq!(at("SELECT 1;", "SELECT 1;".len()), Some("SELECT 1;"));
        assert_eq!(at("\n\n", 1), Some("SELECT * FROM teste"));
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
        assert_eq!(statement_at(sql, sql.find("--").unwrap()), Some(0));
        assert_eq!(statement_at(sql, sql.find('\n').unwrap()), Some(0));
        assert_eq!(statement_at(sql, sql.find('\n').unwrap() + 1), Some(1));
        // A comment line above a statement belongs to the next one.
        let sql = "SELECT 1;\n-- next\nSELECT 2;";
        assert_eq!(statement_at(sql, sql.find("next").unwrap()), Some(1));
        // Before or after the only statement, that statement.
        let sql = "\n\n  SELECT 1;\n\n";
        assert_eq!(statement_at(sql, 0), Some(0));
        assert_eq!(statement_at(sql, sql.len()), Some(0));
        assert_eq!(statement_at(sql, sql.len() + 10), Some(0));
        // Semicolons in literals and comments do not split.
        let sql = "SELECT 'a;b'; /* ; */ SELECT $$;$$";
        assert_eq!(statement_at(sql, sql.find("$$").unwrap()), Some(1));
        assert_eq!(statement_at(sql, 9), Some(0));
        for empty in ["", "   ", "-- only a comment\n", "/* ; */ ;"] {
            assert_eq!(statement_at(empty, 0), None, "{empty:?}");
        }
    }

    #[test]
    fn statement_lines_skip_comments_and_follow_multiline_literals() {
        let sql =
            "-- intro\nSELECT 1;\n\n/* a\n b */ SELECT 'x;\ny';\nSELECT\n  2; SELECT $$\n;$$;";
        assert_eq!(statement_lines(sql), [1, 4, 6, 7]);
        // The same lines as counting from the start for every statement.
        let counted: Vec<_> = statements(sql)
            .into_iter()
            .map(|range| sql[..range.start].matches('\n').count())
            .collect();
        assert_eq!(statement_lines(sql), counted);
        assert!(statement_lines("-- only a comment\n").is_empty());
    }

    #[test]
    fn a_long_script_maps_its_statements_to_lines_within_budget() {
        // Counting lines from the start for every statement is quadratic in
        // the script; this is one pass, paid once per edit.
        let script: String = (0..10_000)
            .map(|index| format!("-- statement {index}\nSELECT 'a;b' FROM t{index};\n"))
            .collect();
        let start = Instant::now();
        let lines = statement_lines(&script);
        let elapsed = start.elapsed();
        assert_eq!(lines.len(), 10_000);
        assert_eq!(lines[9_999], 19_999);
        assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
    }

    #[test]
    fn handles_unicode_and_unterminated_queries() {
        let sql = "SELECT 'ação;';\n SELECT $tag$é;$tag$";
        assert_eq!(statements(sql).len(), 2);
        assert_eq!(statements("SELECT 'é"), vec![0..10]);
        assert!(preview_error("SELECT;").is_some());
        assert!(preview_error("DELETE FROM orders;").is_some());
        assert!(preview_error("SELECT 1;").is_none());
    }

    #[test]
    fn tokens_classify_keywords_literals_comments_and_operators() {
        let sql = "select count(x) -- c\nfrom t where a->>'k' = 1.5";
        let kinds: Vec<_> = tokens(sql)
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
        assert_eq!(tokens(sql), tokens(sql));
    }
}
