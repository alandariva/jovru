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
                    // SQL Server's `[x]` closes with `]`; the others with the
                    // quote they open with.
                    let (open, close) = match text.get(..1) {
                        Some("[") => ("[", "]"),
                        Some(quote) => (quote, quote),
                        None => ("", ""),
                    };
                    let inner = text
                        .strip_prefix(open)
                        .map(|rest| rest.strip_suffix(close).unwrap_or(rest))
                        .unwrap_or(text);
                    Part::Quoted(inner.replace(&format!("{close}{close}"), close))
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
/// the dialect folds it (PostgreSQL and Unity Catalog to lower case, MySQL
/// and SQL Server as written).
pub fn stored(name: &Name, dialect: SqlDialect) -> String {
    if name.quoted {
        return name.text.clone();
    }
    match dialect {
        SqlDialect::MySql | SqlDialect::SqlServer => name.text.clone(),
        _ => name.text.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlserver_names_in_brackets_keep_their_text_and_case() {
        let parts = parts(
            "SELECT * FROM [My]]Db].dbo.\"Order\"",
            Some(SqlDialect::SqlServer),
        );
        let (name, _) = name_at(&parts, 3).unwrap();
        assert_eq!(
            name,
            [
                Name {
                    text: "My]Db".into(),
                    quoted: true
                },
                Name {
                    text: "dbo".into(),
                    quoted: false
                },
                Name {
                    text: "Order".into(),
                    quoted: true
                },
            ]
        );
        let unquoted = Name {
            text: "Orders".into(),
            quoted: false,
        };
        assert_eq!(stored(&unquoted, SqlDialect::SqlServer), "Orders");
        assert_eq!(stored(&unquoted, SqlDialect::Postgres), "orders");
    }
}
