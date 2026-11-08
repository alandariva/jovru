//! What a query tab derives from its editor's text: where each statement
//! starts, the statements as their gutter status compares them, the line
//! count and the `:name` parameters. Reading them grows with the script, so
//! the tab reads them off the UI thread after an edit and applies the
//! outline only while it still describes the editor's text
//! ([Performance](../docs/standards/performance.md#the-editor-with-a-large-sql-file)).
//!
//! The text itself is kept in step with the editor's edits, so a keystroke
//! costs what it changes, not a copy of the whole script.
use crate::parameters;
use crate::sql::{self, SqlDialect};

/// What the tab shows and keeps for one version of its text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outline {
    /// The version of the text it was read from ([`Revisions`]).
    pub revision: u64,
    /// Each statement with layout collapsed, as statuses compare them
    /// ([`sql::normalized_statements`]).
    pub statements: Vec<String>,
    /// The zero-based line on which each statement starts.
    pub statement_lines: Vec<usize>,
    pub line_count: usize,
    /// The `:name` parameters, once each, in the order they first appear.
    pub parameters: Vec<String>,
}

impl Outline {
    /// Reads `sql`, version `revision` of the text, by the rules of
    /// `dialect`. Runs off the UI thread for anything but a short text.
    pub fn read(revision: u64, sql: &str, dialect: Option<SqlDialect>) -> Self {
        Self {
            revision,
            statements: sql::normalized_statements(sql, dialect),
            statement_lines: sql::statement_lines(sql, dialect),
            line_count: sql.split('\n').count(),
            // Most SQL holds no colon at all: nothing to read.
            parameters: if sql.contains(':') {
                parameters::names(sql, dialect)
            } else {
                Vec::new()
            },
        }
    }
}

/// The version of a tab's text and dialect. Every change takes a new
/// revision; an outline read from an older one is stale and never applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Revisions {
    current: u64,
    /// The revision whose outline the tab shows.
    applied: u64,
}

impl Revisions {
    /// The text or the dialect changed: returns the new revision.
    pub fn bump(&mut self) -> u64 {
        self.current += 1;
        self.current
    }

    pub fn current(&self) -> u64 {
        self.current
    }

    /// The outline shown is older than the text.
    pub fn is_stale(&self) -> bool {
        self.applied != self.current
    }

    /// Whether an outline read from `revision` may be applied: only one read
    /// from the current text, once. Records it as applied when it may.
    pub fn accept(&mut self, revision: u64) -> bool {
        if revision != self.current || revision == self.applied {
            return false;
        }
        self.applied = revision;
        true
    }
}

/// Above this size a tab's editor does not soft-wrap: the editor component
/// measures every line of the text with the text shaper when it wraps,
/// about 1.2–1.4 ms per KiB in one frame, so a long script would freeze the
/// window when it opens ([Large `.sql` files in the editor](../docs/plans/editor-large-files.md)).
pub const SOFT_WRAP_OFF_ABOVE: usize = 256 * 1024;
/// Wrapping comes back only once the text is this short again, so typing
/// around [`SOFT_WRAP_OFF_ABOVE`] does not wrap the whole text over and
/// over.
pub const SOFT_WRAP_ON_AT_MOST: usize = 192 * 1024;

/// Whether the editor soft-wraps a text of `len` bytes, given whether it
/// wraps now.
pub fn soft_wrap(len: usize, wrapping: bool) -> bool {
    if wrapping {
        len <= SOFT_WRAP_OFF_ABOVE
    } else {
        len <= SOFT_WRAP_ON_AT_MOST
    }
}

/// Applies one edit to `text`: the bytes `start..old_end` are replaced with
/// `inserted`. Returns `false`, changing nothing, when the edit does not fit
/// the text (out of range or not on character boundaries); the caller then
/// reads the whole text again.
pub fn apply_edit(text: &mut String, start: usize, old_end: usize, inserted: &str) -> bool {
    let fits = start <= old_end
        && old_end <= text.len()
        && text.is_char_boundary(start)
        && text.is_char_boundary(old_end);
    if fits {
        text.replace_range(start..old_end, inserted);
    }
    fits
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn an_outline_reads_statements_lines_and_parameters() {
        let sql = "SELECT 1;\n\nSELECT\n  :id;\nSELECT ':no' FROM t WHERE a = :name AND b = :id";
        let outline = Outline::read(7, sql, None);
        assert_eq!(outline.revision, 7);
        assert_eq!(outline.statement_lines, [0, 2, 4]);
        assert_eq!(outline.line_count, 5);
        assert_eq!(outline.parameters, ["id", "name"]);
        assert_eq!(
            outline.statements,
            sql::normalized_statements(sql, None),
            "statuses compare the same statements as before"
        );
        assert_eq!(Outline::read(0, "", None).line_count, 1);
    }

    #[test]
    fn the_dialect_decides_the_outline() {
        let sql = "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END;";
        assert_eq!(Outline::read(1, sql, None).statements.len(), 3);
        assert_eq!(
            Outline::read(1, sql, Some(SqlDialect::MySql))
                .statements
                .len(),
            1
        );
    }

    #[test]
    fn a_stale_outline_is_never_applied() {
        let mut revisions = Revisions::default();
        let first = revisions.bump();
        let second = revisions.bump();
        assert!(revisions.is_stale());
        // The outline of the older text arrives after a newer edit.
        assert!(!revisions.accept(first));
        assert!(revisions.is_stale());
        assert!(revisions.accept(second));
        assert!(!revisions.is_stale());
        // The same outline twice applies once.
        assert!(!revisions.accept(second));
        // A dialect change after it makes it stale again.
        let third = revisions.bump();
        assert!(!revisions.accept(second));
        assert!(revisions.accept(third));
    }

    #[test]
    fn a_large_text_does_not_soft_wrap_until_it_is_short_again() {
        // A file opens wrapped up to the threshold, and not above it.
        assert!(soft_wrap(SOFT_WRAP_OFF_ABOVE, true));
        assert!(!soft_wrap(SOFT_WRAP_OFF_ABOVE + 1, true));
        assert!(!soft_wrap(4 * 1024 * 1024, true));
        // Deleting a little below the threshold keeps it unwrapped...
        assert!(!soft_wrap(SOFT_WRAP_OFF_ABOVE - 1, false));
        assert!(!soft_wrap(SOFT_WRAP_ON_AT_MOST + 1, false));
        // ...until the text is short enough to wrap cheaply.
        assert!(soft_wrap(SOFT_WRAP_ON_AT_MOST, false));
        assert!(soft_wrap(0, true));
    }

    #[test]
    fn edits_keep_the_text_in_step() {
        let mut text = String::from("SELECT 1;\nSELECT 2;");
        assert!(apply_edit(&mut text, 7, 8, "10"));
        assert_eq!(text, "SELECT 10;\nSELECT 2;");
        assert!(apply_edit(&mut text, 10, 10, "\n"));
        assert!(apply_edit(&mut text, 0, 0, "-- é\n"));
        assert_eq!(text, "-- é\nSELECT 10;\n\nSELECT 2;");
        let len = text.len();
        assert!(apply_edit(&mut text, 0, len, ""));
        assert_eq!(text, "");
    }

    #[test]
    fn an_edit_that_does_not_fit_changes_nothing() {
        let mut text = String::from("é;");
        // Inside the two bytes of `é`, past the end, and reversed.
        assert!(!apply_edit(&mut text, 1, 1, "x"));
        assert!(!apply_edit(&mut text, 0, 9, "x"));
        assert!(!apply_edit(&mut text, 2, 1, "x"));
        assert_eq!(text, "é;");
    }

    /// Statement lines and parameters stay what reading the whole text gives
    /// after a series of edits, wherever they land.
    #[test]
    fn the_outline_after_edits_matches_the_text() {
        let mut text = String::from("SELECT 1;\nSELECT :a;\n");
        let edits: [(usize, usize, &str); 4] = [
            (9, 9, "\nSELECT :b;"),
            (0, 0, "-- ; not a statement\n"),
            (39, 40, "c"),
            (0, 21, ""),
        ];
        for (start, old_end, inserted) in edits {
            assert!(apply_edit(&mut text, start, old_end, inserted));
        }
        assert_eq!(text, "SELECT 1;\nSELECT :c;\nSELECT :a;\n");
        let outline = Outline::read(1, &text, None);
        assert_eq!(outline.statement_lines, [0, 1, 2]);
        assert_eq!(outline.parameters, ["c", "a"]);
        assert_eq!(outline.line_count, 4);
    }

    /// The edit a keystroke applies on the UI thread costs a move of the
    /// bytes after it, not a read of the script: 4 MiB in well under the
    /// frame budget even in a debug build (about 1 ms in release).
    #[test]
    fn a_keystroke_in_a_large_script_is_applied_within_budget() {
        let line = "SELECT 00000000 AS n, 'synthetic row number 00000000' AS label, 42 AS answer FROM dual;\n";
        let mut text = line.repeat(4 * 1024 * 1024 / line.len());
        let middle = text.len() / 2;
        let started = Instant::now();
        for at in 0..10 {
            assert!(apply_edit(&mut text, middle + at, middle + at, "x"));
        }
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "ten keystrokes took {:?}",
            started.elapsed()
        );
    }
}
