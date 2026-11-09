//! Where a Databricks error is in the statement. The SQL Statement
//! Execution API and the command API have no position field; Spark writes it
//! into the message instead, with a one-based line and a zero-based
//! character in that line:
//!
//! - a syntax error: `[PARSE_SYNTAX_ERROR] … (line 1, pos 9)`, before an
//!   echo of the statement under `== SQL ==`;
//! - an analysis error: `[TABLE_OR_VIEW_NOT_FOUND] … SQLSTATE: 42P01; line 1 pos 14`;
//! - a query context, from Spark 3.4: `== SQL(line 1, position 15) ==`, whose
//!   character is one-based. A context in a view (`== SQL of VIEW v(…) ==`)
//!   points into the view's text, not the statement, and is not read.
use jykre_core::DriverError;

/// Adds the position the message names to `error`, when the server gave
/// none and the position falls inside `sql`, the statement as sent. Any
/// other error is returned as it is.
pub(crate) fn locate(mut error: DriverError, sql: &str) -> DriverError {
    if let Some(server) = error.server.as_mut()
        && server.position.is_none()
    {
        server.position = error_position(&error.message, sql);
    }
    error
}

/// The one-based position, in characters, in `sql` of the line and
/// character the message names.
pub(crate) fn error_position(message: &str, sql: &str) -> Option<usize> {
    let (line, character) = annotation(message)?;
    position_in(sql, line, character)
}

/// The line (one-based) and the character in it (zero-based) the message
/// names. Spark's own annotation comes first; the query context is read
/// only when there is none.
fn annotation(message: &str) -> Option<(usize, usize)> {
    // The statement echoed after `== SQL ==` may hold anything.
    let message = message.split("== SQL ==").next().unwrap_or(message);
    let mut rest = message;
    while let Some(at) = rest.find("line ") {
        let after = &rest[at + "line ".len()..];
        if let Some(found) = line_and_pos(after) {
            return Some(found);
        }
        rest = after;
    }
    let (_, context) = message.split_once("== SQL(line ")?;
    let (line, context) = number(context)?;
    let (character, _) = number(context.strip_prefix(", position ")?)?;
    Some((line, character.checked_sub(1)?))
}

/// `<line>, pos <char>` or `<line> pos <char>`.
fn line_and_pos(text: &str) -> Option<(usize, usize)> {
    let (line, rest) = number(text)?;
    let rest = rest
        .strip_prefix(", pos ")
        .or_else(|| rest.strip_prefix(" pos "))?;
    let (character, _) = number(rest)?;
    Some((line, character))
}

/// The decimal number `text` starts with, and what follows it.
fn number(text: &str) -> Option<(usize, &str)> {
    let end = text
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(text.len());
    let value = text[..end].parse().ok()?;
    Some((value, &text[end..]))
}

/// The one-based character position of `character` (zero-based) on `line`
/// (one-based) of `sql`, if that line exists and is that long.
fn position_in(sql: &str, line: usize, character: usize) -> Option<usize> {
    let index = line.checked_sub(1)?;
    let mut before = 0;
    for (number, text) in sql.split('\n').enumerate() {
        let length = text.chars().count();
        if number == index {
            return (character <= length).then_some(before + character + 1);
        }
        // The line and its line break.
        before += length + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use jykre_core::{ErrorCode, ServerDiagnostic};

    fn at(sql: &str, token: &str) -> usize {
        sql[..sql.find(token).unwrap_or(sql.len())].chars().count() + 1
    }

    #[test]
    fn a_syntax_error_gives_its_line_and_character() {
        let sql = "SELECT * FORM orders";
        let message = "[PARSE_SYNTAX_ERROR] Syntax error at or near 'FORM'. SQLSTATE: 42601 \
                       (line 1, pos 9)\n\n== SQL ==\nSELECT * FORM orders\n---------^^^\n";
        assert_eq!(error_position(message, sql), Some(at(sql, "FORM")));
        // Older runtimes put the annotation before the SQLSTATE.
        let message = "\n[PARSE_SYNTAX_ERROR] Syntax error at or near 'FORM'.(line 1, pos 9)\n\n\
                       == SQL ==\nSELECT * FORM orders\n---------^^^\n";
        assert_eq!(error_position(message, sql), Some(at(sql, "FORM")));
    }

    #[test]
    fn an_analysis_error_gives_its_line_and_character() {
        let sql = "SELECT * FROM missing";
        let message = "[TABLE_OR_VIEW_NOT_FOUND] The table or view `missing` cannot be found. \
                       Verify the spelling and correctness of the schema and catalog. \
                       SQLSTATE: 42P01; line 1 pos 14";
        assert_eq!(error_position(message, sql), Some(at(sql, "missing")));
        // On a later line, after characters of more than one byte; the
        // logical plan that may follow is not read.
        let sql = "SELECT 'ação' AS a,\n       amount_x\nFROM sales";
        let message = "[UNRESOLVED_COLUMN.WITH_SUGGESTION] A column, variable, or function \
                       parameter with name `amount_x` cannot be resolved. Did you mean one of \
                       the following? [`amount`]. SQLSTATE: 42703; line 2 pos 7;\n'Project \
                       ['amount_x]\n+- 'UnresolvedRelation [sales], [], false\n";
        assert_eq!(error_position(message, sql), Some(at(sql, "amount_x")));
    }

    #[test]
    fn a_query_context_counts_its_character_from_one() {
        let sql = "SELECT 1 / 0 AS x";
        let message = "[DIVIDE_BY_ZERO] Division by zero. Use `try_divide` to tolerate divisor \
                       being 0 and return NULL instead. SQLSTATE: 22012\n== SQL(line 1, \
                       position 8) ==\nSELECT 1 / 0 AS x\n       ^^^^^\n";
        assert_eq!(error_position(message, sql), Some(at(sql, "1 / 0")));
        // A context inside a view points into the view's text.
        let message = "[DIVIDE_BY_ZERO] Division by zero. SQLSTATE: 22012\n\
                       == SQL of VIEW v(line 1, position 8) ==\nSELECT 1 / 0\n       ^^^^^\n";
        assert_eq!(error_position(message, sql), None);
    }

    #[test]
    fn a_position_outside_the_statement_or_no_annotation_gives_none() {
        let sql = "SELECT 1\nFROM t";
        for message in [
            "[TABLE_OR_VIEW_NOT_FOUND] The table or view `t` cannot be found. SQLSTATE: 42P01",
            "Error; line 3 pos 0",
            "Error; line 2 pos 7",
            "Error; line 0 pos 0",
            "The pipeline line 1 is down",
            "Error; line 1 pos -1",
        ] {
            assert_eq!(error_position(message, sql), None, "{message}");
        }
        // The end of a line is a position.
        assert_eq!(error_position("Error; line 2 pos 6", sql), Some(16));
    }

    #[test]
    fn a_located_error_keeps_its_message_and_codes() {
        let sql = "SELECT * FROM missing";
        let error = DriverError::from_server(
            ErrorCode::Database,
            "[TABLE_OR_VIEW_NOT_FOUND] The table or view `missing` cannot be found. \
             SQLSTATE: 42P01; line 1 pos 14",
            ServerDiagnostic {
                sqlstate: Some("42P01".into()),
                vendor_code: Some("BAD_REQUEST".into()),
                position: None,
            },
        );
        let located = locate(error.clone(), sql);
        assert_eq!(located.message, error.message);
        let server = located.server.unwrap_or_default();
        assert_eq!(server.position, Some(15));
        assert_eq!(server.sqlstate.as_deref(), Some("42P01"));
        let plain = DriverError::new(ErrorCode::Protocol, "line 1 pos 1");
        assert_eq!(locate(plain.clone(), sql), plain);
    }
}
