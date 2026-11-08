//! Where a MySQL syntax error is. MySQL reports no position field: its
//! syntax errors (1064 and the like) end with `near '<text>' at line <n>`,
//! where `<text>` is the statement from the token it stopped at, cut at 80
//! characters, and `<n>` the line of that token in the statement.
use jykre_core::DriverError;

/// How many characters of the statement MySQL quotes after `near`.
const QUOTED: usize = 80;

/// Adds the position of a syntax error to `error`, from its message, when
/// the server gave none and the quoted text is found in `sql`, the statement
/// as sent. Any other error is returned as it is.
pub(crate) fn locate(mut error: DriverError, sql: &str) -> DriverError {
    if let Some(server) = error.server.as_mut()
        && server.position.is_none()
    {
        server.position = syntax_error_position(&error.message, sql);
    }
    error
}

/// The one-based position, in characters, of the token a syntax error
/// stopped at, read from MySQL's message and found in `sql`.
pub(crate) fn syntax_error_position(message: &str, sql: &str) -> Option<usize> {
    let (near, line) = near_and_line(message)?;
    let offset = offset_of(near, line, sql)?;
    Some(sql[..offset].chars().count() + 1)
}

/// The quoted text and the line of `… near '<text>' at line <n>`.
fn near_and_line(message: &str) -> Option<(&str, usize)> {
    const NEAR: &str = " near '";
    const AT_LINE: &str = "' at line ";
    let start = message.find(NEAR)? + NEAR.len();
    // The quoted text may hold quotes and even "' at line ", so the line is
    // read from the last one, which ends the message.
    let end = message.rfind(AT_LINE)?;
    let digits = message[end + AT_LINE.len()..].trim_end();
    if end < start || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let line = digits.parse().ok().filter(|line| *line > 0)?;
    Some((&message[start..end], line))
}

/// The byte offset in `sql` where `near` starts, on line `line` (one-based).
fn offset_of(near: &str, line: usize, sql: &str) -> Option<usize> {
    let line_of = |offset: usize| sql[..offset].matches('\n').count() + 1;
    // MySQL drops the spaces and semicolons that end a statement it reads
    // as text, so the end of the statement is also tried without them.
    let trimmed = sql.trim_end_matches(|ch: char| ch.is_whitespace() || ch == ';');
    if near.chars().count() < QUOTED {
        // Not cut: the quoted text is the rest of the statement. Its line is
        // the token's, or a later one for a token that spans lines.
        return [trimmed, sql]
            .into_iter()
            .filter(|text| text.ends_with(near))
            .map(|text| text.len() - near.len())
            .find(|offset| line_of(*offset) <= line);
    }
    // Cut at 80 characters, possibly in the middle of one the message could
    // not decode, which then ends in replacement characters.
    let near = near.trim_end_matches('\u{FFFD}');
    if near.is_empty() {
        return None;
    }
    let found: Vec<usize> = sql.match_indices(near).map(|(at, _)| at).collect();
    found
        .iter()
        .copied()
        .find(|at| line_of(*at) == line)
        .or_else(|| found.iter().copied().rfind(|at| line_of(*at) < line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jykre_core::{ErrorCode, ServerDiagnostic};

    /// MySQL's message for error 1064, as the server writes it.
    fn message(near: &str, line: usize) -> String {
        format!(
            "You have an error in your SQL syntax; check the manual that corresponds to your \
             MySQL server version for the right syntax to use near '{near}' at line {line}"
        )
    }

    fn at(sql: &str, token: &str) -> usize {
        sql[..sql.find(token).unwrap_or(sql.len())].chars().count() + 1
    }

    #[test]
    fn the_quoted_rest_of_the_statement_gives_the_token() {
        let sql = "SELECT * FORM orders";
        assert_eq!(
            syntax_error_position(&message("FORM orders", 1), sql),
            Some(at(sql, "FORM"))
        );
        // On a later line, with characters of more than one byte before it.
        let sql = "SELECT 'ação',\n  id\nFROM orders\nWHERE id = = 1";
        assert_eq!(
            syntax_error_position(&message("= 1", 4), sql),
            Some(at(sql, "= 1"))
        );
    }

    #[test]
    fn the_end_of_the_statement_is_found_without_its_trailing_spaces_and_semicolon() {
        let sql = "SELECT * FROM orders WHERE;  \n";
        // Nothing after the token: the error is at the end of the statement.
        assert_eq!(
            syntax_error_position(&message("", 1), sql),
            Some("SELECT * FROM orders WHERE".chars().count() + 1)
        );
        let sql = "SELECT 1 +;\n";
        assert_eq!(syntax_error_position(&message("", 1), sql), Some(11));
        // A prepared statement keeps its semicolon in the quote.
        let sql = "SELECT * FORM t;";
        assert_eq!(
            syntax_error_position(&message("FORM t;", 1), sql),
            Some(at(sql, "FORM"))
        );
    }

    #[test]
    fn a_quote_inside_the_text_does_not_end_it() {
        let sql = "SELECT 'it''s' x y FROM t WHERE a = 'b' at line 9";
        let near = "y FROM t WHERE a = 'b' at line 9";
        assert_eq!(
            syntax_error_position(&message(near, 1), sql),
            Some(at(sql, "y FROM"))
        );
    }

    #[test]
    fn a_cut_quote_is_found_on_its_line() {
        let long = "x".repeat(100);
        // The same text on line 1 and line 2: the line tells them apart.
        let sql = format!("SELECT {long}\nSELECT {long}");
        let near: String = format!("SELECT {long}").chars().take(QUOTED).collect();
        let second = sql.rfind("SELECT").unwrap_or_default();
        assert_eq!(
            syntax_error_position(&message(&near, 2), &sql),
            Some(sql[..second].chars().count() + 1)
        );
        // Cut in a character the message could not decode.
        let sql = format!("SELECT * FORM {}ção", "y".repeat(80));
        let near = format!("FORM {}\u{FFFD}", "y".repeat(74));
        assert_eq!(
            syntax_error_position(&message(&near, 1), &sql),
            Some(at(&sql, "FORM"))
        );
    }

    #[test]
    fn other_messages_and_text_that_is_not_there_give_no_position() {
        let sql = "SELECT * FROM missing";
        for other in [
            "Table 'jykre.missing' doesn't exist".to_owned(),
            "Unknown column 'x' in 'field list'".to_owned(),
            message("FORM t", 0),
            message("not in the statement", 1),
            "near 'FROM' at line one".to_owned(),
        ] {
            assert_eq!(syntax_error_position(&other, sql), None, "{other}");
        }
        // The text is there, but after the line MySQL named.
        assert_eq!(
            syntax_error_position(&message("missing", 1), "SELECT 1\nFROM missing"),
            None
        );
    }

    #[test]
    fn a_located_error_keeps_its_message_and_codes() {
        let sql = "SELECT * FORM t";
        let error = DriverError::from_server(
            ErrorCode::Database,
            message("FORM t", 1),
            ServerDiagnostic {
                sqlstate: Some("42000".into()),
                vendor_code: Some("1064".into()),
                position: None,
            },
        );
        let located = locate(error.clone(), sql);
        assert_eq!(located.message, error.message);
        let server = located.server.unwrap_or_default();
        assert_eq!(server.position, Some(10));
        assert_eq!(server.vendor_code.as_deref(), Some("1064"));
        // An error without the server's details stays as it is.
        let plain = DriverError::new(ErrorCode::Protocol, message("FORM t", 1));
        assert_eq!(locate(plain.clone(), sql), plain);
    }
}
