//! What each token of the editor shows as: the tokenizer's lexical kind
//! refined with the dialect's catalog. Only what the text alone tells:
//!
//! - a word the dialect reserves (or one of the built-in clause words) is a
//!   keyword;
//! - a word before `(` is a function, unless it is a keyword that takes
//!   parentheses (`IN (`, `VALUES (`);
//! - a word after `::` is a type;
//! - a word next to a `.` is part of a qualified name.
//!
//! Which table, column or alias a name refers to needs the statement's
//! structure and the metadata cache, which this does not use.
use crate::catalog::{DialectCatalog, is_builtin_keyword};
use jykre_sql::TokenKind;
use std::ops::Range;

/// How a token is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Highlight {
    Keyword,
    Function,
    Type,
    String,
    Number,
    Comment,
    /// A MySQL executable comment, which the server runs.
    Executable,
    /// A name in identifier quotes.
    QuotedIdentifier,
    Parameter,
    Operator,
}

/// Keywords that take parentheses, so `(` after them does not make a
/// function.
const PAREN_KEYWORDS: &[&str] = &[
    "in", "values", "exists", "any", "all", "some", "over", "filter", "within", "using", "on",
    "as", "and", "or", "not", "from", "join", "select", "where", "table", "into", "with",
];

/// Symbols that only separate or group, shown as plain text.
const PUNCTUATION: &[&str] = &["(", ")", ",", ";", ".", "[", "]", "{", "}"];

/// The previous and next tokens of `index` that are not comments.
fn neighbours(
    tokens: &[(Range<usize>, TokenKind)],
    index: usize,
) -> (Option<usize>, Option<usize>) {
    let code = |at: &usize| !matches!(tokens[*at].1, TokenKind::Comment);
    let previous = (0..index).rev().find(code);
    let next = (index + 1..tokens.len()).find(code);
    (previous, next)
}

/// How token `index` of `tokens` (the tokens of `sql`) is shown, or `None`
/// for plain text.
pub fn classify(
    sql: &str,
    tokens: &[(Range<usize>, TokenKind)],
    index: usize,
    catalog: &DialectCatalog,
) -> Option<Highlight> {
    let (range, kind) = tokens.get(index)?;
    let text = sql.get(range.clone())?;
    match kind {
        TokenKind::Comment => Some(Highlight::Comment),
        TokenKind::Executable => Some(Highlight::Executable),
        TokenKind::String => Some(Highlight::String),
        TokenKind::QuotedIdentifier => Some(Highlight::QuotedIdentifier),
        TokenKind::Number => Some(Highlight::Number),
        TokenKind::Parameter => Some(Highlight::Parameter),
        TokenKind::Symbol => (!PUNCTUATION.contains(&text)).then_some(Highlight::Operator),
        TokenKind::Word => {
            let (previous, next) = neighbours(tokens, index);
            let at = |token: Option<usize>| token.and_then(|at| sql.get(tokens[at].0.clone()));
            let (before, after) = (at(previous), at(next));
            if before == Some(".") || after == Some(".") {
                return None;
            }
            let word = text.to_lowercase();
            if before == Some("::") {
                return Some(Highlight::Type);
            }
            let keyword = catalog.keyword(&word).is_some();
            if after == Some("(") {
                return if PAREN_KEYWORDS.contains(&word.as_str())
                    || (keyword
                        && catalog.functions(&word).is_empty()
                        && catalog.is_reserved(&word))
                {
                    Some(Highlight::Keyword)
                } else {
                    Some(Highlight::Function)
                };
            }
            (catalog.is_reserved(&word) || is_builtin_keyword(&word)).then_some(Highlight::Keyword)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Highlight, classify};
    use crate::catalog::{DialectCatalog, DialectFiles, parse};
    use jykre_sql::{SqlDialect, tokens};

    fn postgres() -> DialectCatalog {
        parse(
            DialectFiles {
                manifest: r#"{"schema_version":1,"version_scheme":"numeric","versions":["18"],"verified_through":"18"}"#,
                keywords: concat!(
                    r#"{"reserved":true,"source":"s","word":"user"}"#,
                    "\n",
                    r#"{"reserved":true,"source":"s","word":"left"}"#,
                    "\n",
                    r#"{"reserved":false,"source":"s","word":"name"}"#,
                    "\n"
                ),
                functions: concat!(
                    r#"{"id":"left(text,int4)","kind":"scalar","name":"left","signature":"left(text, integer)","source":"s"}"#,
                    "\n"
                ),
                types: "",
            },
            None,
        )
        .unwrap()
    }

    fn shown(
        sql: &str,
        dialect: Option<SqlDialect>,
        catalog: &DialectCatalog,
    ) -> Vec<(String, Option<Highlight>)> {
        let tokens = tokens(sql, dialect);
        (0..tokens.len())
            .map(|index| {
                (
                    sql[tokens[index].0.clone()].to_owned(),
                    classify(sql, &tokens, index, catalog),
                )
            })
            .collect()
    }

    fn of(shown: &[(String, Option<Highlight>)], text: &str) -> Option<Highlight> {
        shown
            .iter()
            .find(|(token, _)| token == text)
            .and_then(|(_, highlight)| *highlight)
    }

    #[test]
    fn words_are_keywords_functions_types_or_names() {
        let catalog = postgres();
        let sql = "select count(x), left(name, 2), t.user, user, a::int4, x in (1) from t -- note";
        let shown = shown(sql, Some(SqlDialect::Postgres), &catalog);
        assert_eq!(of(&shown, "select"), Some(Highlight::Keyword));
        assert_eq!(of(&shown, "count"), Some(Highlight::Function));
        // Reserved, but a function before `(`.
        assert_eq!(of(&shown, "left"), Some(Highlight::Function));
        // Not reserved, so a name.
        assert_eq!(of(&shown, "name"), None);
        assert_eq!(of(&shown, "int4"), Some(Highlight::Type));
        assert_eq!(of(&shown, "in"), Some(Highlight::Keyword));
        assert_eq!(of(&shown, "-- note"), Some(Highlight::Comment));
        assert_eq!(of(&shown, "::"), Some(Highlight::Operator));
        assert_eq!(of(&shown, ","), None);
        // `user` after `t.` is a column; on its own, the reserved word.
        let tokens = tokens(sql, Some(SqlDialect::Postgres));
        let users: Vec<_> = (0..tokens.len())
            .filter(|at| &sql[tokens[*at].0.clone()] == "user")
            .map(|at| classify(sql, &tokens, at, &catalog))
            .collect();
        assert_eq!(users, [None, Some(Highlight::Keyword)]);
    }

    #[test]
    fn literals_quoted_names_and_parameters_follow_the_dialect() {
        let catalog = DialectCatalog::builtin();
        let pg = shown(
            "SELECT \"Name\", 'text', 42, $1 FROM t",
            Some(SqlDialect::Postgres),
            &catalog,
        );
        assert_eq!(of(&pg, "\"Name\""), Some(Highlight::QuotedIdentifier));
        assert_eq!(of(&pg, "'text'"), Some(Highlight::String));
        assert_eq!(of(&pg, "42"), Some(Highlight::Number));
        assert_eq!(of(&pg, "$1"), Some(Highlight::Parameter));
        let mysql = shown(
            "SELECT `name`, \"text\", @total, ? FROM t",
            Some(SqlDialect::MySql),
            &catalog,
        );
        assert_eq!(of(&mysql, "`name`"), Some(Highlight::QuotedIdentifier));
        assert_eq!(of(&mysql, "\"text\""), Some(Highlight::String));
        assert_eq!(of(&mysql, "@total"), Some(Highlight::Parameter));
        assert_eq!(of(&mysql, "?"), Some(Highlight::Parameter));
        let databricks = shown(
            "SELECT raw:owner FROM t WHERE id = :id",
            Some(SqlDialect::DatabricksSql),
            &catalog,
        );
        assert_eq!(of(&databricks, ":id"), Some(Highlight::Parameter));
        assert_eq!(of(&databricks, ":owner"), None);
    }
}
