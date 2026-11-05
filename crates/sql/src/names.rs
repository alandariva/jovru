//! Qualified names as the tokenizer reads them: the parts of a statement
//! reduced to words, quoted identifiers and symbols, the dotted names among
//! them and how each dialect stores an unquoted name.
use crate::{SqlDialect, TokenKind, tokens};

/// One part of a qualified name, unquoted, and whether it was quoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    pub text: String,
    pub quoted: bool,
}

/// A token reduced to what the classification needs.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    /// An unquoted word, upper-cased for comparison, with its text.
    Word(String, String),
    /// A quoted identifier, unquoted.
    Quoted(String),
    Symbol(String),
    Other,
}

/// The tokens of `sql` by the rules of `dialect` reduced to what name
/// reading needs, comments left out.
pub fn parts(sql: &str, dialect: Option<SqlDialect>) -> Vec<Part> {
    tokens(sql, dialect)
        .into_iter()
        .filter(|(_, kind)| *kind != TokenKind::Comment)
        .map(|(range, kind)| {
            let text = &sql[range];
            match kind {
                TokenKind::Word => Part::Word(text.to_ascii_uppercase(), text.to_owned()),
                TokenKind::QuotedIdentifier => {
                    let quote = &text[..1];
                    let inner = text
                        .strip_prefix(quote)
                        .map(|rest| rest.strip_suffix(quote).unwrap_or(rest))
                        .unwrap_or(text);
                    Part::Quoted(inner.replace(&format!("{quote}{quote}"), quote))
                }
                TokenKind::Symbol => Part::Symbol(text.to_owned()),
                _ => Part::Other,
            }
        })
        .collect()
}

/// Reads the name starting at `at`: words or quoted identifiers joined by
/// dots. Returns it and the position after it.
pub fn name_at(parts: &[Part], mut at: usize) -> Option<(Vec<Name>, usize)> {
    let mut name = Vec::new();
    loop {
        match parts.get(at)? {
            Part::Word(_, text) => name.push(Name {
                text: text.clone(),
                quoted: false,
            }),
            Part::Quoted(text) => name.push(Name {
                text: text.clone(),
                quoted: true,
            }),
            _ => return None,
        }
        at += 1;
        if parts.get(at) == Some(&Part::Symbol(".".into())) {
            at += 1;
        } else {
            return Some((name, at));
        }
    }
}

/// How the server stores a name: quoted as written; unquoted folded the way
/// the dialect folds it (PostgreSQL and Unity Catalog to lower case, MySQL as
/// written).
pub fn stored(name: &Name, dialect: SqlDialect) -> String {
    if name.quoted {
        return name.text.clone();
    }
    match dialect {
        SqlDialect::MySql => name.text.clone(),
        _ => name.text.to_lowercase(),
    }
}
