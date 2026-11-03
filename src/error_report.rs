//! How a failure reads in the UI: a short title chosen from the error's code,
//! the database's own message, a suggested action and the details a report
//! needs ([UX](../docs/ux.md#error-messages)).
use crate::drivers::{DriverError, ErrorCode};
use crate::tr;
use std::time::{SystemTime, UNIX_EPOCH};

/// A failure ready to show. It holds no credentials: only what the driver's
/// error already allowed (the database's own message and codes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorReport {
    pub code: ErrorCode,
    /// A short human title ("Could not sign in", "Syntax error").
    pub title: String,
    /// The database's own message, or the driver's when there is none.
    pub message: String,
    /// What the user can do next, when there is something to suggest.
    pub action: Option<String>,
    pub sqlstate: Option<String>,
    pub vendor_code: Option<String>,
    /// One-based position, in characters, in the statement as sent.
    pub position: Option<usize>,
    /// Seconds since the Unix epoch when the failure was recorded.
    pub at: u64,
}

impl ErrorReport {
    pub fn new(error: &DriverError) -> Self {
        let server = error.server.clone().unwrap_or_default();
        Self {
            code: error.code,
            title: title(error.code, server.sqlstate.as_deref()),
            message: error.message.clone(),
            action: action(error.code, server.sqlstate.as_deref()),
            sqlstate: server.sqlstate,
            vendor_code: server.vendor_code,
            position: server.position,
            at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        }
    }

    /// A failure the application found itself, such as the demo's checks.
    pub fn database(message: impl Into<String>) -> Self {
        Self::new(&DriverError::new(ErrorCode::Database, message))
    }

    /// One line for a status: the title, the message and the action.
    pub fn summary(&self) -> String {
        let mut text = format!("{}: {}", self.title, self.message.replace('\n', " "));
        if let Some(codes) = self.codes() {
            text.push_str(&format!(" ({codes})"));
        }
        if let Some(action) = &self.action {
            text.push(' ');
            text.push_str(action);
        }
        text
    }

    /// The server's codes, as "SQLSTATE 42P01" or "1146 · SQLSTATE 42S02".
    pub fn codes(&self) -> Option<String> {
        let state = self
            .sqlstate
            .as_ref()
            .map(|state| format!("SQLSTATE {state}"));
        match (&self.vendor_code, state) {
            (Some(vendor), Some(state)) => Some(format!("{vendor} · {state}")),
            (Some(vendor), None) => Some(vendor.clone()),
            (None, state) => state,
        }
    }

    /// The line and column (both one-based) of the position in `sql`, for
    /// the message next to the error.
    pub fn line_and_column(&self, sql: &str) -> Option<(usize, usize)> {
        let offset = self.offset_in(sql)?;
        let before = &sql[..offset];
        let line = before.matches('\n').count() + 1;
        let column = before
            .rsplit_once('\n')
            .map_or(before, |(_, line)| line)
            .chars()
            .count()
            + 1;
        Some((line, column))
    }

    /// The byte offset in `sql` (the statement as sent) of the position the
    /// server reported, if it falls inside the statement.
    pub fn offset_in(&self, sql: &str) -> Option<usize> {
        let position = self.position?.checked_sub(1)?;
        match sql.char_indices().nth(position) {
            Some((offset, _)) => Some(offset),
            None if position == sql.chars().count() => Some(sql.len()),
            None => None,
        }
    }

    /// Plain text for **Copy details**: what a report needs, so no
    /// screenshot is required. `context` names the run and the statement.
    pub fn details(&self, context: &str) -> String {
        let mut text = format!(
            "{}\n{}\nJykre Studio {}\nTime (UTC): {}\n",
            self.title,
            self.message,
            env!("CARGO_PKG_VERSION"),
            utc(self.at)
        );
        if !context.is_empty() {
            text.push_str(context);
            text.push('\n');
        }
        text.push_str(&format!("Error code: {:?}\n", self.code));
        if let Some(codes) = self.codes() {
            text.push_str(&format!("Server code: {codes}\n"));
        }
        if let Some(position) = self.position {
            text.push_str(&format!("Position: {position}\n"));
        }
        text
    }
}

/// The title for an error, from its code and, for database errors, the
/// SQLSTATE class.
pub fn title(code: ErrorCode, sqlstate: Option<&str>) -> String {
    match code {
        ErrorCode::Authentication => tr!("Could not sign in").into(),
        ErrorCode::PermissionDenied => tr!("Permission denied").into(),
        ErrorCode::Connection => tr!("Could not reach the server").into(),
        ErrorCode::Timeout => tr!("The statement timed out").into(),
        ErrorCode::Cancelled => tr!("Execution cancelled").into(),
        ErrorCode::Validation => tr!("Invalid settings").into(),
        ErrorCode::NotFound => tr!("Not found").into(),
        ErrorCode::Protocol => tr!("Unexpected answer from the server").into(),
        ErrorCode::Database => match sqlstate.unwrap_or_default() {
            "42601" | "42000" => tr!("Syntax error").into(),
            "42P01" | "42S02" | "42704" | "3F000" | "3D000" | "42883" => {
                tr!("Object not found").into()
            }
            "42703" | "42S22" => tr!("Column not found").into(),
            "42501" => tr!("Permission denied").into(),
            "40001" | "40P01" => tr!("Transaction conflict").into(),
            "57014" => tr!("Statement cancelled by the server").into(),
            state if state.starts_with("23") => tr!("Constraint violated").into(),
            state if state.starts_with("22") => tr!("Invalid data").into(),
            state if state.starts_with("25") => tr!("Invalid transaction state").into(),
            state if state.starts_with("42") => tr!("SQL error").into(),
            state if state.starts_with("53") => tr!("The server is out of resources").into(),
            _ => tr!("Database error").into(),
        },
        _ => tr!("Operation not available").into(),
    }
}

/// A suggested action, when one exists.
pub fn action(code: ErrorCode, sqlstate: Option<&str>) -> Option<String> {
    let text = match code {
        ErrorCode::Authentication => tr!("Check the user and password of the connection."),
        ErrorCode::PermissionDenied => {
            tr!("Ask for access to the object, or use a user that has it.")
        }
        ErrorCode::Connection => tr!("Check the host, port, network and TLS mode."),
        ErrorCode::Timeout => {
            tr!("Choose a longer statement timeout in the toolbar, or narrow the SQL.")
        }
        ErrorCode::Validation => tr!("Review the connection settings."),
        ErrorCode::Database => match sqlstate.unwrap_or_default() {
            "42601" | "42000" => tr!("Check the SQL near the marked position."),
            "42P01" | "42S02" | "42704" | "3F000" | "3D000" | "42883" => {
                tr!("Check the name and the current schema or database.")
            }
            "42703" | "42S22" => tr!("Check the column name and the table it comes from."),
            "40001" | "40P01" => tr!("Run the statement again."),
            state if state.starts_with("23") => {
                tr!("Check the values against the table's constraints.")
            }
            state if state.starts_with("22") => tr!("Check the values and their types."),
            _ => return None,
        },
        _ => return None,
    };
    Some(text.into())
}

/// `seconds` since the epoch as `YYYY-MM-DD HH:MM:SS`.
fn utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::ServerDiagnostic;

    fn server(sqlstate: &str, position: Option<usize>) -> DriverError {
        DriverError::from_server(
            ErrorCode::Database,
            "relation \"missing\" does not exist",
            ServerDiagnostic {
                sqlstate: Some(sqlstate.into()),
                vendor_code: None,
                position,
            },
        )
    }

    #[test]
    fn a_server_error_keeps_its_message_and_gets_a_title_and_an_action() {
        let report = ErrorReport::new(&server("42P01", Some(15)));
        assert_eq!(report.title, "Object not found");
        assert_eq!(report.message, "relation \"missing\" does not exist");
        assert!(report.action.is_some());
        assert_eq!(report.codes().as_deref(), Some("SQLSTATE 42P01"));
        assert!(
            report.summary().starts_with(
                "Object not found: relation \"missing\" does not exist (SQLSTATE 42P01) Check"
            ),
            "{}",
            report.summary()
        );
        let details = report.details("Run 3 · statement 1");
        for expected in [
            "Object not found",
            "does not exist",
            env!("CARGO_PKG_VERSION"),
            "Run 3 · statement 1",
            "SQLSTATE 42P01",
            "Position: 15",
        ] {
            assert!(details.contains(expected), "{expected} in {details}");
        }
    }

    #[test]
    fn titles_follow_the_code_and_the_sqlstate_class() {
        assert_eq!(title(ErrorCode::Authentication, None), "Could not sign in");
        assert_eq!(title(ErrorCode::Database, Some("42601")), "Syntax error");
        assert_eq!(
            title(ErrorCode::Database, Some("23505")),
            "Constraint violated"
        );
        assert_eq!(title(ErrorCode::Database, Some("22P02")), "Invalid data");
        assert_eq!(title(ErrorCode::Database, None), "Database error");
        assert_eq!(title(ErrorCode::Timeout, None), "The statement timed out");
        // Different failures never share the generic text.
        let titles: std::collections::HashSet<_> = [
            title(ErrorCode::Authentication, None),
            title(ErrorCode::Connection, None),
            title(ErrorCode::Timeout, None),
            title(ErrorCode::Database, Some("42601")),
            title(ErrorCode::Database, Some("23505")),
        ]
        .into_iter()
        .collect();
        assert_eq!(titles.len(), 5);
    }

    #[test]
    fn vendor_and_standard_codes_read_together() {
        let error = DriverError::from_server(
            ErrorCode::Database,
            "Table 'jykre.missing' doesn't exist",
            ServerDiagnostic {
                sqlstate: Some("42S02".into()),
                vendor_code: Some("1146".into()),
                position: None,
            },
        );
        let report = ErrorReport::new(&error);
        assert_eq!(report.codes().as_deref(), Some("1146 · SQLSTATE 42S02"));
        assert_eq!(report.title, "Object not found");
        assert_eq!(ErrorReport::database("x").codes(), None);
    }

    #[test]
    fn the_position_maps_to_characters_lines_and_columns() {
        let sql = "SELECT *\nFROM ação missing";
        let at = sql.find("missing").unwrap();
        let position = sql[..at].chars().count() + 1;
        let report = ErrorReport::new(&server("42P01", Some(position)));
        assert_eq!(report.offset_in(sql), Some(at));
        assert_eq!(report.line_and_column(sql), Some((2, 11)));
        // A position past the statement is ignored.
        let outside = ErrorReport::new(&server("42601", Some(500)));
        assert_eq!(outside.offset_in(sql), None);
        assert_eq!(
            ErrorReport::new(&server("42601", Some(0))).offset_in(sql),
            None
        );
        assert_eq!(
            ErrorReport::new(&server("42601", Some(sql.chars().count() + 1))).offset_in(sql),
            Some(sql.len())
        );
    }

    #[test]
    fn times_are_written_in_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00:00");
        assert_eq!(utc(1_793_707_200), "2026-11-03 12:00:00");
        assert_eq!(utc(951_825_600), "2000-02-29 12:00:00");
    }
}
