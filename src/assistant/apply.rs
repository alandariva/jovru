//! Putting a suggestion into the editor. Nothing here runs SQL.
use std::ops::Range;

/// Where `statement` is in the editor text: at `origin` when it is still
/// there unchanged, else its only occurrence. `None` when it moved and
/// appears more than once, or was edited.
pub fn locate_statement(
    editor: &str,
    statement: &str,
    origin: Option<usize>,
) -> Option<Range<usize>> {
    if statement.is_empty() {
        return None;
    }
    let start = origin
        .filter(|start| editor.get(*start..*start + statement.len()) == Some(statement))
        .or_else(|| {
            let mut found = editor.match_indices(statement).map(|(at, _)| at);
            let first = found.next()?;
            found.next().is_none().then_some(first)
        })?;
    Some(start..start + statement.len())
}

/// The suggestion as it should replace `original`: trimmed, and ending in
/// `;` exactly when the original did, so the script around it keeps its
/// separators and never gets `;;`. Line endings follow the original's.
pub fn fit_replacement(original: &str, suggestion: &str) -> String {
    let mut text = suggestion
        .trim()
        .trim_end_matches(';')
        .trim_end()
        .to_owned();
    if original.trim_end().ends_with(';') {
        text.push(';');
    }
    if original.contains("\r\n") {
        text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_is_found_where_it_ran_or_where_it_moved_once() {
        let editor = "SELECT 1;\nSELECT bad;\nSELECT 2;";
        assert_eq!(
            locate_statement(editor, "SELECT bad;", Some(10)),
            Some(10..21)
        );
        // Text was typed above it: found by its only occurrence.
        let moved = format!("-- note\n{editor}");
        assert_eq!(
            locate_statement(&moved, "SELECT bad;", Some(10)),
            Some(18..29)
        );
        // Edited, or present twice: not found.
        assert_eq!(
            locate_statement("SELECT good;", "SELECT bad;", Some(0)),
            None
        );
        assert_eq!(
            locate_statement("SELECT bad; SELECT bad;", "SELECT bad;", Some(3)),
            None
        );
        assert_eq!(
            locate_statement("SELECT bad; SELECT bad;", "SELECT bad;", Some(12)),
            Some(12..23)
        );
        assert_eq!(locate_statement("x", "", None), None);
    }

    #[test]
    fn a_replacement_keeps_the_original_terminator_and_line_endings() {
        assert_eq!(
            fit_replacement("SELECT bad;", "  SELECT good;;\n"),
            "SELECT good;"
        );
        assert_eq!(fit_replacement("SELECT bad", "SELECT good;"), "SELECT good");
        assert_eq!(
            fit_replacement("SELECT bad\r\nFROM t;", "SELECT a\nFROM t"),
            "SELECT a\r\nFROM t;"
        );
    }
}
