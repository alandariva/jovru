//! Small, local SQL presentation helpers for the visual prototype.
use super::ui::{BLUE, FAINT, MINT, PURPLE};
use gpui_kit::component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, Rope,
};
use gpui_kit::*;
use jykre::tr;
use std::ops::Range;

#[derive(Default)]
pub struct SqlHighlighter {
    tokens: Vec<(Range<usize>, u32)>,
}

impl InputHighlighter for SqlHighlighter {
    fn language(&self) -> SharedString {
        "sql".into()
    }

    fn update(
        &mut self,
        _: Option<InputEdit>,
        text: &Rope,
        _: bool,
        _: &mut Window,
        _: &mut Context<EditorState>,
    ) {
        self.tokens = tokenize(&text.to_string())
            .into_iter()
            .filter_map(|(range, kind)| {
                let color = match kind {
                    Kind::Comment => FAINT,
                    Kind::String => MINT,
                    Kind::Number => 0xd9b584,
                    Kind::Word => return None,
                    Kind::Keyword => PURPLE,
                    Kind::Function => BLUE,
                    Kind::Symbol => return None,
                };
                Some((range, color))
            })
            .collect();
    }

    fn styles(
        &self,
        range: &Range<usize>,
        _: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut runs = Vec::new();
        let mut cursor = range.start;
        for (token, color) in &self.tokens {
            let start = token.start.max(range.start);
            let end = token.end.min(range.end);
            if start >= end {
                continue;
            }
            if cursor < start {
                runs.push((cursor..start, HighlightStyle::default()));
            }
            runs.push((
                start..end,
                HighlightStyle {
                    color: Some(rgb(*color).into()),
                    ..Default::default()
                },
            ));
            cursor = end;
        }
        if cursor < range.end {
            runs.push((cursor..range.end, HighlightStyle::default()));
        }
        runs
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
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
                    pos += sql[pos..].chars().next().unwrap().len_utf8();
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
        } else if quote == b'$'
            && sql[pos + 1..].find('$').is_some_and(|end| {
                sql[pos + 1..pos + 1 + end]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        {
            let end = pos + 2 + sql[pos + 1..].find('$').unwrap();
            let delimiter = &sql[pos..end];
            pos = sql[end..]
                .find(delimiter)
                .map_or(sql.len(), |close| end + close + delimiter.len());
        } else {
            pos += sql[pos..].chars().next().unwrap().len_utf8();
        }
    }
    if let Some(begin) = start {
        ranges.push(begin..sql.trim_end().len());
    }
    ranges
}

/// Executable statements with runs of whitespace collapsed, so layout-only
/// edits (new lines, indentation) do not count as a different statement.
pub fn normalized_statements(sql: &str) -> Vec<String> {
    statements(sql)
        .into_iter()
        .map(|range| sql[range].split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

pub fn preview_error(sql: &str) -> Option<&'static str> {
    let first = tokenize(sql).into_iter().next()?;
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

fn tokenize(sql: &str) -> Vec<(Range<usize>, Kind)> {
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
            Kind::Comment
        } else if ch == '/' && chars.peek().is_some_and(|(_, c)| *c == '*') {
            chars.next();
            while let Some((_, next)) = chars.next() {
                if next == '*' && chars.peek().is_some_and(|(_, c)| *c == '/') {
                    chars.next();
                    break;
                }
            }
            Kind::Comment
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
            Kind::String
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
                Kind::String
            } else {
                // Positional parameter such as `$1`.
                while chars
                    .peek()
                    .is_some_and(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
                {
                    chars.next();
                }
                Kind::Word
            }
        } else if ch.is_ascii_digit() {
            while chars
                .peek()
                .is_some_and(|(_, c)| c.is_ascii_digit() || *c == '.')
            {
                chars.next();
            }
            Kind::Number
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
                    Kind::Keyword
                }
                "COUNT" | "SUM" | "AVG" | "ROUND" | "MAX" | "MIN" | "COALESCE" | "DATE"
                | "DATE_FORMAT" | "DATE_TRUNC" | "EXPLAIN" | "ANALYZE" => Kind::Function,
                _ => Kind::Word,
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
            Kind::Symbol
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
    for (range, kind) in tokenize(sql) {
        let original = &sql[range];
        let upper = original.to_ascii_uppercase();
        let token = if kind == Kind::Keyword || kind == Kind::Function {
            upper.as_str()
        } else {
            original
        };
        let newline = kind == Kind::Comment
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
                && kind == Kind::Symbol
                && !matches!(previous, "AS" | "IN" | "FROM" | "WITH"));
        if space {
            output.push(' ');
        }
        output.push_str(token);
        if kind == Kind::Comment {
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
        previous = if kind == Kind::Function {
            "FUNCTION"
        } else {
            original
        };
    }
    output.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::{format, normalized_statements, preview_error, statements};

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
    fn handles_unicode_and_unterminated_queries() {
        let sql = "SELECT 'ação;';\n SELECT $tag$é;$tag$";
        assert_eq!(statements(sql).len(), 2);
        assert_eq!(statements("SELECT 'é"), vec![0..10]);
        assert!(preview_error("SELECT;").is_some());
        assert!(preview_error("DELETE FROM orders;").is_some());
        assert!(preview_error("SELECT 1;").is_none());
    }
}
