//! SQL text handling that the execution of queries needs and the editor
//! shares: tokens, statement splitting, layout normalisation, `EXPLAIN` and
//! position mapping, by the rules of each dialect.
//!
//! One tokenizer serves every function here, so splitting, highlighting and
//! formatting always agree on where a literal or a comment ends. Its rules
//! follow the connection's [`SqlDialect`]; a tab without a connection uses
//! generic rules (`None`).
//!
//! Everything here is pure and synchronous: the same input gives the same
//! output, and SQL is never sent anywhere or rewritten for execution. This
//! crate knows nothing about the editor's language features (completion,
//! highlighting, formatting), which live in `jykre-sql-language` and can be
//! removed without touching what runs queries.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod names;

pub use jykre_core::SqlDialect;
use std::ops::Range;

/// What a token is, lexically. Whether a word is a keyword, a function or a
/// name depends on the dialect's catalog, which the editor's language
/// features know and this crate does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Comment,
    /// A MySQL executable comment (`/*! … */`): code the server runs, kept
    /// whole like a comment.
    Executable,
    String,
    /// A name in the dialect's identifier quotes: `"x"` in PostgreSQL,
    /// `` `x` `` in MySQL and Databricks, both without a dialect, and
    /// `"x"`, `` `x` `` and `[x]` in SQLite.
    QuotedIdentifier,
    Number,
    /// A keyword, a function or a name.
    Word,
    /// A placeholder or a variable: `$1` (PostgreSQL), `?` (MySQL and
    /// Databricks), `:name` (Databricks), `@var` and `@@var` (MySQL);
    /// `?`, `?NNN`, `:name`, `@name` and `$name` (SQLite).
    Parameter,
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

    fn sqlite(self) -> bool {
        matches!(self.dialect, Some(SqlDialect::Sqlite))
    }

    /// PostgreSQL quotes strings with `$$ … $$` and `$tag$ … $tag$`.
    fn dollar_quotes(self) -> bool {
        !self.mysql() && !self.databricks() && !self.sqlite()
    }

    /// Whether a backslash escapes the next character inside `quote`.
    /// PostgreSQL treats it as an escape only in `E'…'` strings
    /// (`standard_conforming_strings`, on by default); SQLite never does.
    fn backslash(self, quote: u8, escape_string: bool) -> bool {
        if self.postgres() {
            escape_string
        } else if self.sqlite() {
            false
        } else {
            quote != b'`'
        }
    }

    /// MySQL also starts a line comment with `#`.
    fn hash_comments(self) -> bool {
        self.mysql()
    }

    /// MySQL and SQLite block comments do not nest; the others do.
    fn nested_comments(self) -> bool {
        !self.mysql() && !self.sqlite()
    }

    /// MySQL identifiers may contain `$`.
    fn word_char(self, ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_' || (ch == '$' && self.mysql())
    }

    /// Whether `quote` opens a name rather than a string: `"` quotes strings
    /// in MySQL (without `ANSI_QUOTES`) and Databricks, and PostgreSQL has no
    /// backtick quotes.
    fn identifier_quote(self, quote: char) -> bool {
        match quote {
            '"' => !self.mysql() && !self.databricks(),
            '`' => !self.postgres(),
            _ => false,
        }
    }

    /// `?` is a placeholder except in PostgreSQL, where it is an operator
    /// (`jsonb ? 'key'`) and placeholders are `$1`.
    fn question_placeholder(self) -> bool {
        !self.postgres()
    }

    /// `:name` is a named parameter in Databricks and SQLite (and without
    /// a dialect).
    fn colon_parameters(self) -> bool {
        self.databricks() || self.sqlite() || self.dialect.is_none()
    }

    /// SQLite quotes names in brackets too (`[order]`), as MS Access does;
    /// there are no arrays to confuse them with.
    fn bracket_identifiers(self) -> bool {
        self.sqlite()
    }
}

/// Whether the `:` at `start` opens a named parameter: a name follows, and
/// what comes before is not something a Databricks path could continue
/// (`raw:owner` reads a field of `raw`).
fn named_parameter(sql: &str, start: usize) -> bool {
    let follows = sql[start + 1..]
        .chars()
        .next()
        .is_some_and(|ch| ch.is_alphabetic() || ch == '_');
    let continues = sql[..start]
        .chars()
        .next_back()
        .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '_' | ')' | ']' | '`' | '"' | ':'));
    follows && !continues
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
        let kind = if rules.identifier_quote(ch) {
            TokenKind::QuotedIdentifier
        } else {
            TokenKind::String
        };
        (
            quoted_end(sql, start, quote, rules.backslash(quote, false)),
            kind,
        )
    } else if matches!(ch, 'e' | 'E') && next == Some(b'\'') && rules.postgres() {
        // A PostgreSQL escape string, where a backslash escapes.
        (
            quoted_end(sql, start + 1, b'\'', rules.backslash(b'\'', true)),
            TokenKind::String,
        )
    } else if ch == '[' && rules.bracket_identifiers() {
        // No escape inside brackets: the first `]` closes the name.
        let end = sql[start + 1..]
            .find(']')
            .map_or(sql.len(), |close| start + 1 + close + 1);
        (end, TokenKind::QuotedIdentifier)
    } else if matches!(ch, '$' | '@') && rules.sqlite() {
        // SQLite's named parameters `$name` and `@name`.
        let len = rest[1..]
            .find(|c: char| !rules.word_char(c))
            .unwrap_or(rest.len() - 1);
        if len > 0 {
            (start + 1 + len, TokenKind::Parameter)
        } else {
            (start + 1, TokenKind::Symbol)
        }
    } else if ch == '?' && rules.sqlite() {
        // `?` or a numbered `?NNN`.
        let len = rest[1..]
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len() - 1);
        (start + 1 + len, TokenKind::Parameter)
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
        } else if tag_len > 0 && tag[..tag_len].bytes().all(|byte| byte.is_ascii_digit()) {
            // Positional parameter such as `$1`.
            (start + 1 + tag_len, TokenKind::Parameter)
        } else {
            (start + 1 + tag_len, TokenKind::Word)
        }
    } else if ch == '?' && rules.question_placeholder() {
        (start + 1, TokenKind::Parameter)
    } else if ch == ':' && rules.colon_parameters() && named_parameter(sql, start) {
        let len = rest[1..]
            .find(|c: char| !rules.word_char(c))
            .unwrap_or(rest.len() - 1);
        (start + 1 + len, TokenKind::Parameter)
    } else if ch == '@' && rules.mysql() {
        // A user variable (`@total`) or a system one (`@@autocommit`).
        let at = if rest.starts_with("@@") { 2 } else { 1 };
        let len = rest[at..]
            .find(|c: char| !rules.word_char(c))
            .unwrap_or(rest.len() - at);
        if len > 0 {
            (start + at + len, TokenKind::Parameter)
        } else {
            (start + 1, TokenKind::Symbol)
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
        (start + len, TokenKind::Word)
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

/// Where an edit replaced bytes `start..old_end` of the previous text with
/// bytes `start..new_end` of the new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub old_end: usize,
    pub new_end: usize,
}

/// The tokens of `sql` (the text after `edit`) from `old`, the tokens of the
/// text before it, reading only around the edit: the tokens that end before
/// it are kept, reading restarts after them and stops at the first token
/// that meets one of the old ones moved by the edit, after which the old
/// tokens are kept, moved. The result is always what [`tokens`] gives for
/// `sql`; only opening a literal or a comment reads to the end of the text.
pub fn relex(
    old: &[(Range<usize>, TokenKind)],
    sql: &str,
    edit: Edit,
    dialect: Option<SqlDialect>,
) -> Vec<(Range<usize>, TokenKind)> {
    let mut tokens = old.to_vec();
    relex_in_place(&mut tokens, sql, edit, dialect);
    tokens
}

/// [`relex`] on the tokens it is given: only the tokens around the edit are
/// replaced and the ones after it moved, so a keystroke in a long script
/// costs no new list of every token.
pub fn relex_in_place(
    tokens: &mut Vec<(Range<usize>, TokenKind)>,
    sql: &str,
    edit: Edit,
    dialect: Option<SqlDialect>,
) {
    let rules = Rules { dialect };
    // A token ending before the edit is read up to a character before it,
    // so the edit cannot change it.
    let keep = tokens.partition_point(|(range, _)| range.end < edit.start);
    let mut pos = keep.checked_sub(1).map_or(0, |last| tokens[last].0.end);
    let shift = |at: usize| at - edit.old_end + edit.new_end;
    let moved =
        |(range, kind): &(Range<usize>, TokenKind)| (shift(range.start)..shift(range.end), *kind);
    let mut candidate = tokens.partition_point(|(range, _)| range.start < edit.old_end);
    let mut fresh = Vec::new();
    while let Some(token) = next_token(sql, pos, rules) {
        if token.0.start >= edit.new_end {
            while candidate < tokens.len() && moved(&tokens[candidate]).0.start < token.0.start {
                candidate += 1;
            }
            if candidate < tokens.len() && moved(&tokens[candidate]) == token {
                // In step again: the text from here on is the old text,
                // moved, so its tokens are too.
                for (range, _) in &mut tokens[candidate..] {
                    *range = shift(range.start)..shift(range.end);
                }
                tokens.splice(keep..candidate, fresh);
                return;
            }
        }
        pos = token.0.end;
        fresh.push(token);
    }
    tokens.truncate(keep);
    tokens.extend(fresh);
}

/// The word of a token, upper case, or `None` for any other kind.
fn word(sql: &str, token: &(Range<usize>, TokenKind)) -> Option<String> {
    (token.1 == TokenKind::Word).then(|| sql[token.0.clone()].to_ascii_uppercase())
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
                } else if rules.mysql() || rules.sqlite() {
                    // A trigger body (SQLite has no procedures); a `BEGIN`
                    // on its own opens a transaction.
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
        let found = if matches!(
            kind,
            TokenKind::String | TokenKind::QuotedIdentifier | TokenKind::Executable
        ) {
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

/// Executable statements with the whitespace between tokens collapsed to one
/// space, so layout-only edits (new lines, indentation) do not count as a
/// different statement. Whitespace inside a token (a literal, a quoted
/// identifier, a comment) is kept: `'a b'` and `'a  b'` are different
/// statements.
pub fn normalized_statements(sql: &str, dialect: Option<SqlDialect>) -> Vec<String> {
    statements(sql, dialect)
        .into_iter()
        .map(|range| {
            let text = &sql[range];
            let mut normalized = String::with_capacity(text.len());
            let mut end = 0;
            for (token, _) in tokens(text, dialect) {
                if token.start > end && !normalized.is_empty() {
                    normalized.push(' ');
                }
                normalized.push_str(&text[token.clone()]);
                end = token.end;
            }
            normalized
        })
        .collect()
}

/// Why [`explain`] gives no statement to explain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainRefused {
    /// The tab has no connection, so there is no dialect to explain in.
    NoDialect,
    /// The cursor is on no statement, or the selection holds none.
    NoStatement,
    /// The selection holds more than one statement.
    Several,
    /// The statement is not a query or a data change whose plan `EXPLAIN`
    /// shows without running it (or it already is an `EXPLAIN`).
    NotExplainable,
}

/// The `EXPLAIN` of the statement at the cursor, or of the selection when it
/// holds one statement, in `dialect`.
///
/// PostgreSQL, MySQL and Databricks all show the plan of `EXPLAIN
/// <statement>` without running the statement (unlike `EXPLAIN ANALYZE`).
/// SQLite's `EXPLAIN` lists the bytecode instead, so SQLite gets `EXPLAIN
/// QUERY PLAN`, which does not run the statement either. Only queries and data changes are explained, so the words after
/// `EXPLAIN` can never be read as one of its options.
pub fn explain(
    sql: &str,
    selection: Range<usize>,
    dialect: Option<SqlDialect>,
) -> Result<String, ExplainRefused> {
    if dialect.is_none() {
        return Err(ExplainRefused::NoDialect);
    }
    let statement = if selection.is_empty() {
        let index =
            statement_at(sql, selection.start, dialect).ok_or(ExplainRefused::NoStatement)?;
        let range = statements(sql, dialect)
            .into_iter()
            .nth(index)
            .ok_or(ExplainRefused::NoStatement)?;
        &sql[range]
    } else {
        let text = sql
            .get(selection.clone())
            .ok_or(ExplainRefused::NoStatement)?;
        let ranges = statements(text, dialect);
        match ranges.as_slice() {
            [] => return Err(ExplainRefused::NoStatement),
            [range] => &text[range.clone()],
            _ => return Err(ExplainRefused::Several),
        }
    };
    let rules = Rules { dialect };
    let first = next_code(statement, 0, rules)
        .and_then(|token| word(statement, &token))
        .ok_or(ExplainRefused::NotExplainable)?;
    if !matches!(
        first.as_str(),
        "SELECT"
            | "WITH"
            | "VALUES"
            | "TABLE"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "MERGE"
            | "REPLACE"
    ) {
        return Err(ExplainRefused::NotExplainable);
    }
    let body = statement.trim_end().trim_end_matches(';').trim_end();
    if rules.sqlite() {
        return Ok(format!("EXPLAIN QUERY PLAN {body}"));
    }
    Ok(format!("EXPLAIN {body}"))
}

/// Whether `sql` is one statement that only reads: it starts as a query
/// (`SELECT`, `WITH`, `VALUES`, `TABLE`, `SHOW`, `DESCRIBE`, `DESC` or
/// `EXPLAIN`) and no word of its code writes, locks or runs something else
/// (`INSERT`, `UPDATE`, `DELETE`, `MERGE`, `INTO`, `FOR UPDATE`, `CALL`,
/// `ANALYZE`, ...). Words are read by the dialect's tokens, so a string or a
/// comment holding `DELETE` does not count; a name such as a column called
/// `update` does, and the statement is refused: the answer errs on the side
/// of not running. A MySQL executable comment (`/*! … */`) is code, so it
/// refuses too. Functions with side effects (`nextval`) cannot be told
/// apart, which is why running a statement again for an export also asks
/// the user.
pub fn reads_only(sql: &str, dialect: Option<SqlDialect>) -> bool {
    const STARTS: &[&str] = &[
        "SELECT", "WITH", "VALUES", "TABLE", "SHOW", "DESCRIBE", "DESC", "EXPLAIN",
    ];
    const WRITES: &[&str] = &[
        "INSERT",
        "UPDATE",
        "DELETE",
        "MERGE",
        "UPSERT",
        "REPLACE",
        "CREATE",
        "DROP",
        "ALTER",
        "TRUNCATE",
        "RENAME",
        "COMMENT",
        "GRANT",
        "REVOKE",
        "CALL",
        "EXEC",
        "EXECUTE",
        "DO",
        "COPY",
        "INTO",
        "LOCK",
        "SHARE",
        "ANALYZE",
        "ANALYSE",
        "VACUUM",
        "OPTIMIZE",
        "REFRESH",
        "CLUSTER",
        "REINDEX",
        "SET",
        "RESET",
        "BEGIN",
        "START",
        "COMMIT",
        "ROLLBACK",
        "SAVEPOINT",
        "PREPARE",
        "DEALLOCATE",
        "HANDLER",
        "LOAD",
        "NOTIFY",
        "LISTEN",
        "USE",
    ];
    if statements(sql, dialect).len() != 1 {
        return false;
    }
    let mut first = true;
    for token in tokens(sql, dialect) {
        match token.1 {
            TokenKind::Comment => continue,
            TokenKind::Executable => return false,
            _ => {}
        }
        let word = word(sql, &token);
        if first {
            first = false;
            if !word.as_deref().is_some_and(|word| STARTS.contains(&word)) {
                return false;
            }
            continue;
        }
        if word.as_deref().is_some_and(|word| WRITES.contains(&word)) {
            return false;
        }
    }
    !first
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

/// `value` as a string literal of `dialect`, for SQL the application
/// writes from values (a copy as an `IN` list or as `INSERT`s). Where a
/// backslash escapes inside strings (MySQL, Databricks and generic rules),
/// backslashes and quotes are escaped with one, since Databricks reads two
/// adjacent quotes as two literals; PostgreSQL doubles the quote.
pub fn string_literal(value: &str, dialect: Option<SqlDialect>) -> String {
    let backslash = Rules { dialect }.backslash(b'\'', false);
    let mut literal = String::with_capacity(value.len() + 2);
    literal.push('\'');
    for ch in value.chars() {
        match ch {
            '\'' if backslash => literal.push_str("\\'"),
            '\'' => literal.push_str("''"),
            '\\' if backslash => literal.push_str("\\\\"),
            ch => literal.push(ch),
        }
    }
    literal.push('\'');
    literal
}

/// A `:name` parameter marker of a report (ADR-0023).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedMarker {
    pub name: String,
    /// The marker's bytes, colon included.
    pub range: Range<usize>,
}

/// The `:name` markers of `sql`, in order, by the rules of `dialect`. Only
/// code counts: strings, comments and quoted names are skipped whole. A
/// marker is a `:` right before a name (ASCII letters, digits and `_`, not
/// starting with a digit) and not right after something it could continue:
/// `x::int`, `a := 1`, `raw:owner`, `arr[1:2]`, `arr[lo:hi]` and `arr[:hi]`
/// hold no marker. Finding markers changes nothing: the driver binds the
/// values ([`jykre_core::QueryParameter`]).
pub fn named_markers(sql: &str, dialect: Option<SqlDialect>) -> Vec<NamedMarker> {
    let tokens = tokens(sql, dialect);
    let mut markers = Vec::new();
    for (index, (range, kind)) in tokens.iter().enumerate() {
        let marker = match kind {
            TokenKind::Parameter if sql.get(range.clone()).is_some_and(|t| t.starts_with(':')) => {
                range.clone()
            }
            TokenKind::Symbol if sql.get(range.clone()) == Some(":") => {
                let Some((word, TokenKind::Word)) = tokens.get(index + 1) else {
                    continue;
                };
                if word.start != range.end || !named_parameter(sql, range.start) {
                    continue;
                }
                range.start..word.end
            }
            _ => continue,
        };
        // `arr[:hi]` slices an array.
        if sql
            .get(..marker.start)
            .is_some_and(|before| before.ends_with('['))
        {
            continue;
        }
        let Some(name) = sql.get(marker.start + 1..marker.end) else {
            continue;
        };
        if jykre_core::is_parameter_name(name) {
            markers.push(NamedMarker {
                name: name.to_owned(),
                range: marker,
            });
        }
    }
    markers
}

#[cfg(test)]
mod named_marker_tests {
    use super::{SqlDialect, named_markers};

    const DIALECTS: [Option<SqlDialect>; 5] = [
        Some(SqlDialect::Postgres),
        Some(SqlDialect::MySql),
        Some(SqlDialect::DatabricksSql),
        Some(SqlDialect::Sqlite),
        None,
    ];

    fn names(sql: &str, dialect: Option<SqlDialect>) -> Vec<String> {
        named_markers(sql, dialect)
            .into_iter()
            .map(|marker| {
                assert_eq!(&sql[marker.range.clone()], format!(":{}", marker.name));
                marker.name
            })
            .collect()
    }

    #[test]
    fn markers_are_found_in_every_dialect_in_order_with_repeats() {
        for dialect in DIALECTS {
            assert_eq!(
                names(
                    "SELECT * FROM t WHERE a = :start AND b <= :_end2 OR c = :start",
                    dialect
                ),
                ["start", "_end2", "start"],
                "{dialect:?}"
            );
            assert_eq!(names("SELECT (:a)+:b", dialect), ["a", "b"], "{dialect:?}");
        }
    }

    #[test]
    fn code_that_only_looks_like_a_marker_holds_none() {
        for dialect in DIALECTS {
            for sql in [
                "SELECT x::int, y :: text",
                "SELECT ':a', ' :b '",
                "SELECT 1 -- :a",
                "SELECT /* :a */ 1",
                "SELECT raw:owner FROM t",
                "SELECT arr[1:2], arr[lo:hi], arr[:hi] FROM t",
                "SELECT : a",
                "SELECT :1",
                "SELECT x:=1",
            ] {
                assert!(names(sql, dialect).is_empty(), "{dialect:?} {sql}");
            }
        }
        assert!(names("SELECT $$ :a $$, E'\\' :b'", Some(SqlDialect::Postgres)).is_empty());
        assert!(names("SELECT \":a\"", Some(SqlDialect::Postgres)).is_empty());
        assert!(names("SELECT `:a`", Some(SqlDialect::MySql)).is_empty());
        assert!(names("SELECT 1 # :a", Some(SqlDialect::MySql)).is_empty());
    }

    #[test]
    fn names_outside_ascii_are_not_markers() {
        assert!(names("SELECT :ação", Some(SqlDialect::Postgres)).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SqlDialect, TokenKind, map_offset, normalized_statements, statement_at, statement_lines,
        statements, string_literal, tokens,
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

    #[test]
    fn string_literals_escape_as_the_dialect_reads_them() {
        let value = r"it's C:\temp";
        assert_eq!(string_literal(value, POSTGRES), r"'it''s C:\temp'");
        assert_eq!(string_literal(value, MYSQL), r"'it\'s C:\\temp'");
        assert_eq!(string_literal(value, DATABRICKS), r"'it\'s C:\\temp'");
        // Each literal is one string under its own dialect's rules.
        for dialect in [POSTGRES, MYSQL, DATABRICKS, None] {
            let sql = format!("SELECT {};", string_literal(value, dialect));
            assert_eq!(statements(&sql, dialect).len(), 1, "{sql}");
        }
    }

    #[test]
    fn map_offset_follows_the_text_through_a_layout_change() {
        let before = "select a,\n   b from t\nwhere   a = 1";
        let after = "SELECT\n    a,\n    b\nFROM t\nWHERE a = 1";
        for word in ["a,", "b", "t", "where", "1"] {
            let at = before.find(word).unwrap();
            let mapped = map_offset(before, after, at + word.len());
            assert!(
                after[..mapped]
                    .to_ascii_lowercase()
                    .ends_with(&word.to_ascii_lowercase()),
                "{word:?} -> {mapped} in {after:?}"
            );
        }
        assert_eq!(map_offset(before, after, 0), 0);
        assert_eq!(map_offset(before, after, before.len()), after.len());
        assert_eq!(map_offset("é  x", "é x", "é  x".len()), "é x".len());
    }

    #[test]
    fn normalized_statements_ignore_layout_only() {
        assert_eq!(
            normalized_statements("SELECT\n    1;\n\n  SELECT  2;\n", None),
            ["SELECT 1;", "SELECT 2;"]
        );
        // Spaces inside a literal are part of the statement: the gutter
        // status of `SELECT 'a b'` does not carry to `SELECT 'a  b'`.
        assert_ne!(
            normalized_statements("SELECT 'a b';", None),
            normalized_statements("SELECT 'a  b';", None)
        );
        assert_ne!(
            normalized_statements("SELECT \"a b\" FROM t;", POSTGRES),
            normalized_statements("SELECT \"a  b\" FROM t;", POSTGRES)
        );
        assert_eq!(
            normalized_statements("SELECT 'a  b'\n  FROM t;", None),
            normalized_statements("SELECT 'a  b' FROM    t;", None)
        );
    }

    #[test]
    fn only_a_single_query_that_writes_nothing_reads_only() {
        use super::reads_only;
        for sql in [
            "SELECT * FROM t WHERE note = 'DELETE' -- update later",
            "  /* insert? */ with x as (select 1) select * from x;",
            "VALUES (1), (2)",
            "SHOW TABLES",
            "EXPLAIN SELECT 1",
            "SELECT nextval('s')",
        ] {
            assert!(reads_only(sql, POSTGRES), "{sql}");
        }
        for sql in [
            "WITH gone AS (DELETE FROM t RETURNING *) SELECT * FROM gone",
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t FOR SHARE",
            "SELECT * INTO copy_of_t FROM t",
            "EXPLAIN ANALYZE SELECT 1",
            "UPDATE t SET a = 1",
            "SELECT 1; DELETE FROM t",
            "CALL refresh()",
            // A column named like a keyword errs on the side of refusing.
            "SELECT update FROM t",
            "",
            "-- only a comment",
        ] {
            assert!(!reads_only(sql, POSTGRES), "{sql}");
        }
        assert!(!reads_only("SELECT 1 /*! , (DELETE FROM t) */", MYSQL));
        assert!(!reads_only("SELECT * FROM t INTO OUTFILE '/tmp/x'", MYSQL));
        assert!(!reads_only("SELECT * FROM t LOCK IN SHARE MODE", MYSQL));
        assert!(reads_only("SELECT `delete` FROM t", MYSQL));
        assert!(reads_only("SELECT * FROM `cat`.`sch`.`t`", DATABRICKS));
    }

    #[test]
    fn explain_wraps_one_query_or_data_change_in_the_dialect() {
        use super::{ExplainRefused, explain};
        let sql = "SELECT 1;\n\nDELETE FROM t WHERE a = 1;\nCREATE TABLE x (a int);";
        let cursor = |marker: &str| {
            let at = sql.find(marker).unwrap();
            at..at
        };
        assert_eq!(
            explain(sql, cursor("SELECT"), POSTGRES).as_deref(),
            Ok("EXPLAIN SELECT 1")
        );
        assert_eq!(
            explain(sql, cursor("DELETE"), MYSQL).as_deref(),
            Ok("EXPLAIN DELETE FROM t WHERE a = 1")
        );
        assert_eq!(
            explain(sql, cursor("CREATE"), POSTGRES),
            Err(ExplainRefused::NotExplainable)
        );
        let blank = sql.find("\n\n").unwrap() + 1;
        assert_eq!(
            explain(sql, blank..blank, POSTGRES),
            Err(ExplainRefused::NoStatement)
        );
        assert_eq!(
            explain(sql, 0..sql.len(), POSTGRES),
            Err(ExplainRefused::Several)
        );
        let select = sql.find("DELETE").unwrap()..sql.find("CREATE").unwrap();
        assert_eq!(
            explain(sql, select, DATABRICKS).as_deref(),
            Ok("EXPLAIN DELETE FROM t WHERE a = 1")
        );
        // Without a connection, and for statements that already explain or
        // analyse, nothing is sent.
        assert_eq!(
            explain(sql, cursor("SELECT"), None),
            Err(ExplainRefused::NoDialect)
        );
        for statement in ["EXPLAIN ANALYZE DELETE FROM t;", "ANALYZE t;", "VACUUM t;"] {
            assert_eq!(
                explain(statement, 0..0, POSTGRES),
                Err(ExplainRefused::NotExplainable),
                "{statement}"
            );
        }
        assert_eq!(
            explain(
                "-- why\nWITH a AS (SELECT 1) SELECT * FROM a;",
                8..8,
                POSTGRES
            )
            .as_deref(),
            Ok("EXPLAIN WITH a AS (SELECT 1) SELECT * FROM a")
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
    fn one_keystroke_in_a_long_script_rereads_only_around_it() {
        // Reading the whole script again on every keystroke grows with the
        // script; reading around the edit does not.
        let script: String = (0..10_000)
            .map(|index| {
                format!(
                    "-- statement {index}
SELECT 'a;b' FROM t{index};
"
                )
            })
            .collect();
        let old = tokens(&script, None);
        let middle = script.find("t5000").unwrap();
        let mut edited = script.clone();
        edited.insert(middle, 'x');
        let edit = super::Edit {
            start: middle,
            old_end: middle,
            new_end: middle + 1,
        };
        let start = Instant::now();
        let relexed = super::relex(&old, &edited, edit, None);
        let elapsed = start.elapsed();
        assert_eq!(relexed, tokens(&edited, None));
        assert!(elapsed < Duration::from_millis(50), "took {elapsed:?}");
    }

    #[test]
    fn a_keystroke_in_a_4_mib_script_relexes_in_place_within_budget() {
        // The highlighter's tokens for the largest file a tab opens: a
        // keystroke replaces the tokens around it and moves the rest, with
        // no new list of every token (about 1.5 ms in release).
        let line = "SELECT 00000000 AS n, 'synthetic row' AS label, 42 AS answer FROM dual;\n";
        let mut script = line.repeat(4 * 1024 * 1024 / line.len());
        let mut current = tokens(&script, None);
        let middle = script.len() / 2;
        let middle = (middle..script.len())
            .find(|at| script.is_char_boundary(*at))
            .unwrap();
        script.insert(middle, 'x');
        let edit = super::Edit {
            start: middle,
            old_end: middle,
            new_end: middle + 1,
        };
        let start = Instant::now();
        super::relex_in_place(&mut current, &script, edit, None);
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(100), "took {elapsed:?}");
        assert_eq!(current, tokens(&script, None));
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
    }

    #[test]
    fn relex_after_any_edit_matches_reading_the_whole_text() {
        let scripts = [
            "SELECT 'a;b', \"x\" FROM t -- c\nWHERE a >= $1 AND b::int = 2;\n/* x /* y */ */ SELECT $$a$$;",
            "SELECT `a`, \"s\", 'it\\'s', @v, @@autocommit # note\nFROM t WHERE x = ?;",
            "SELECT raw:owner, :id FROM t WHERE c = 'é' AND d = `q`;",
        ];
        let inserts = [
            "'", "\"", "--", "/*", "*/", "$$", "x", " ", "\n", ";", "é", "`", "@", ":",
        ];
        // A small linear congruential generator keeps the test repeatable.
        let mut seed: u64 = 0x5eed;
        let mut next = |bound: usize| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) as usize % bound.max(1)
        };
        for (script, dialect) in scripts.iter().zip([POSTGRES, MYSQL, DATABRICKS]) {
            let mut text = (*script).to_owned();
            let mut current = tokens(&text, dialect);
            for _ in 0..400 {
                let boundaries: Vec<usize> = (0..=text.len())
                    .filter(|at| text.is_char_boundary(*at))
                    .collect();
                let start = boundaries[next(boundaries.len())];
                let ends: Vec<usize> = boundaries
                    .iter()
                    .copied()
                    .filter(|at| *at >= start && *at <= start + 6)
                    .collect();
                let old_end = ends[next(ends.len())];
                let inserted = if next(3) == 0 {
                    ""
                } else {
                    inserts[next(inserts.len())]
                };
                text.replace_range(start..old_end, inserted);
                let edit = super::Edit {
                    start,
                    old_end,
                    new_end: start + inserted.len(),
                };
                super::relex_in_place(&mut current, &text, edit, dialect);
                assert_eq!(current, tokens(&text, dialect), "{text:?} after {edit:?}");
            }
        }
    }

    #[test]
    fn quoted_names_and_parameters_follow_the_dialect() {
        let kinds = |sql: &str, dialect| -> Vec<(String, TokenKind)> {
            tokens(sql, dialect)
                .into_iter()
                .map(|(range, kind)| (sql[range].to_owned(), kind))
                .collect()
        };
        let pg = kinds("SELECT \"a;b\", `x`, $1, data ? 'k' FROM t", POSTGRES);
        assert!(pg.contains(&("\"a;b\"".into(), TokenKind::QuotedIdentifier)));
        assert!(pg.contains(&("`x`".into(), TokenKind::String)));
        assert!(pg.contains(&("$1".into(), TokenKind::Parameter)));
        // In PostgreSQL `?` is a jsonb operator.
        assert!(pg.contains(&("?".into(), TokenKind::Symbol)));
        let mysql = kinds(
            "SELECT `a`, \"s\", @x, @@session.autocommit, ? FROM t",
            MYSQL,
        );
        assert!(mysql.contains(&("`a`".into(), TokenKind::QuotedIdentifier)));
        assert!(mysql.contains(&("\"s\"".into(), TokenKind::String)));
        assert!(mysql.contains(&("@x".into(), TokenKind::Parameter)));
        assert!(mysql.contains(&("@@session".into(), TokenKind::Parameter)));
        assert!(mysql.contains(&("?".into(), TokenKind::Parameter)));
        let databricks = kinds("SELECT raw:owner, a::int FROM t WHERE id = :id", DATABRICKS);
        assert!(databricks.contains(&(":id".into(), TokenKind::Parameter)));
        assert!(databricks.contains(&(":".into(), TokenKind::Symbol)));
        assert!(databricks.contains(&("::".into(), TokenKind::Symbol)));
        // A semicolon inside a quoted name does not end the statement.
        assert_eq!(split("SELECT \"a;b\" FROM t; SELECT 2;", POSTGRES).len(), 2);
        assert_eq!(split("SELECT `a;b` FROM t; SELECT 2;", MYSQL).len(), 2);
    }

    #[test]
    fn a_mysql_variable_named_like_a_keyword_opens_no_block() {
        // `@begin` is a variable: it must not open a block in the routine.
        let sql =
            "CREATE PROCEDURE p()\nBEGIN\n  SET @begin = 1;\n  SELECT @begin;\nEND;\nSELECT 2;";
        let parts = split(sql, MYSQL);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[1], "SELECT 2;");
    }

    #[test]
    fn tokens_mark_words_literals_comments_and_operators() {
        let sql = "select count(x) -- c\nfrom t where a->>'k' = 1.5";
        let kinds: Vec<_> = tokens(sql, None)
            .into_iter()
            .map(|(range, kind)| (&sql[range], kind))
            .collect();
        assert_eq!(kinds[0], ("select", TokenKind::Word));
        assert_eq!(kinds[1], ("count", TokenKind::Word));
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

#[cfg(test)]
mod sqlite_tests {
    use super::{
        SqlDialect, TokenKind, explain, named_markers, statements, string_literal, tokens,
    };

    const SQLITE: Option<SqlDialect> = Some(SqlDialect::Sqlite);

    fn kinds(sql: &str) -> Vec<(&str, TokenKind)> {
        tokens(sql, SQLITE)
            .into_iter()
            .map(|(range, kind)| (&sql[range], kind))
            .collect()
    }

    #[test]
    fn names_are_quoted_three_ways_and_backslashes_do_not_escape() {
        assert_eq!(
            kinds(r#"SELECT [order], "a""b", `c`, 'C:\' FROM t"#),
            [
                ("SELECT", TokenKind::Word),
                ("[order]", TokenKind::QuotedIdentifier),
                (",", TokenKind::Symbol),
                (r#""a""b""#, TokenKind::QuotedIdentifier),
                (",", TokenKind::Symbol),
                ("`c`", TokenKind::QuotedIdentifier),
                (",", TokenKind::Symbol),
                (r"'C:\'", TokenKind::String),
                ("FROM", TokenKind::Word),
                ("t", TokenKind::Word),
            ]
        );
        assert_eq!(string_literal(r"it's C:\", SQLITE), r"'it''s C:\'");
    }

    #[test]
    fn every_parameter_form_is_one_token() {
        let found: Vec<_> = kinds("SELECT ?, ?12, :a, @b, $c, $")
            .into_iter()
            .filter(|(_, kind)| *kind == TokenKind::Parameter)
            .map(|(text, _)| text)
            .collect();
        assert_eq!(found, ["?", "?12", ":a", "@b", "$c"]);
        let names: Vec<_> = named_markers("SELECT :a, [x:y], ':b' WHERE c = :c", SQLITE)
            .into_iter()
            .map(|marker| marker.name)
            .collect();
        assert_eq!(names, ["a", "c"]);
    }

    #[test]
    fn block_comments_do_not_nest() {
        assert_eq!(
            statements("/* a /* b */ SELECT 1; SELECT 2;", SQLITE).len(),
            2
        );
    }

    #[test]
    fn a_trigger_body_keeps_its_semicolons_and_begin_alone_is_a_transaction() {
        let sql = "CREATE TRIGGER t AFTER INSERT ON a BEGIN\n  UPDATE b SET n = n + 1;\n  \
                   SELECT CASE WHEN 1 THEN 2 END;\nEND;\nBEGIN;\nINSERT INTO a VALUES (1);\nCOMMIT;";
        let found: Vec<_> = statements(sql, SQLITE)
            .into_iter()
            .map(|range| &sql[range])
            .collect();
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(found[0].starts_with("CREATE TRIGGER") && found[0].ends_with("END;"));
        assert_eq!(
            &found[1..],
            ["BEGIN;", "INSERT INTO a VALUES (1);", "COMMIT;"]
        );
    }

    #[test]
    fn explain_asks_for_the_query_plan() {
        assert_eq!(
            explain("SELECT * FROM t", 0..0, SQLITE).as_deref(),
            Ok("EXPLAIN QUERY PLAN SELECT * FROM t")
        );
    }

    #[test]
    fn bracketed_names_are_read_without_their_brackets() {
        use crate::names::{Part, parts};
        assert_eq!(
            parts("[my table]", SQLITE),
            [Part::Quoted("my table".into())]
        );
    }
}
