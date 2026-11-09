//! **Go to line** (`Ctrl G` in a SQL editor): what the user typed, and
//! where in the text it puts the cursor. Nothing here reads the text: the
//! editor gives the number of lines, which its rope knows without a scan,
//! and clamps the column to the line itself
//! ([User experience](../docs/reference/ux.md#keyboard-shortcuts)).
use crate::tr;

/// A line, and optionally a column, as the user numbers them (from 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub line: usize,
    pub column: Option<usize>,
}

/// Where the cursor goes, numbered from 0 as the editor counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub line: usize,
    /// The character in the line; the editor stops at the line's end.
    pub column: usize,
    /// The line asked for is past the last one, so the cursor goes to the
    /// start of the last line, and the user is told.
    pub past_end: bool,
}

/// Reads `12`, `12:5` or `12,5` (spaces around the numbers are fine). A
/// number too long to hold is past any text, so it reads as the largest
/// one. Anything else, and a line or column 0, is not a target.
pub fn parse(text: &str) -> Option<Target> {
    let text = text.trim();
    let (line, column) = match text.split_once([':', ',']) {
        Some((line, column)) => (line, Some(column.trim())),
        None => (text, None),
    };
    let line = number(line.trim())?;
    let column = match column {
        // `12:` while the column is still being typed.
        Some("") | None => None,
        Some(column) => Some(number(column)?),
    };
    Some(Target { line, column })
}

/// A positive decimal number, saturated at `usize::MAX`.
fn number(text: &str) -> Option<usize> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value = text.bytes().fold(0usize, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(usize::from(digit - b'0'))
    });
    (value > 0).then_some(value)
}

/// Where `target` falls in a text of `lines` lines (an empty text has one).
pub fn place(target: Target, lines: usize) -> Place {
    let lines = lines.max(1);
    if target.line > lines {
        return Place {
            line: lines - 1,
            column: 0,
            past_end: true,
        };
    }
    Place {
        line: target.line - 1,
        column: target.column.map_or(0, |column| column - 1),
        past_end: false,
    }
}

/// What the prompt shows before anything is typed.
pub fn hint(lines: usize) -> String {
    tr!("Line, or line:column (1 to {0})", lines.max(1))
}

/// What the user is told when the line asked for is past the text.
pub fn past_end_notice(target: Target, lines: usize) -> String {
    tr!(
        "There is no line {0}: the SQL ends at line {1}, where the cursor is now.",
        target.line,
        lines.max(1)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(line: usize, column: Option<usize>) -> Option<Target> {
        Some(Target { line, column })
    }

    #[test]
    fn a_line_and_an_optional_column_are_read() {
        assert_eq!(parse("12"), target(12, None));
        assert_eq!(parse(" 12 "), target(12, None));
        assert_eq!(parse("12:5"), target(12, Some(5)));
        assert_eq!(parse("12,5"), target(12, Some(5)));
        assert_eq!(parse(" 12 : 5 "), target(12, Some(5)));
        assert_eq!(parse("12:"), target(12, None));
        assert_eq!(parse("007"), target(7, None));
    }

    #[test]
    fn anything_else_is_not_a_target() {
        for text in [
            "", " ", "0", "0:4", "12:0", "-3", "+3", "1.5", "a", "12:a", ":5", "1:2:3", "1 2", "١٢",
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_huge_number_reads_as_past_any_text() {
        let huge = parse("99999999999999999999999999");
        assert_eq!(huge, target(usize::MAX, None));
        let place = place(
            huge.unwrap_or(Target {
                line: 1,
                column: None,
            }),
            40,
        );
        assert!(place.past_end);
        assert_eq!(place.line, 39);
    }

    #[test]
    fn a_line_in_the_text_is_numbered_from_zero() {
        assert_eq!(
            place(
                Target {
                    line: 3,
                    column: Some(7)
                },
                10
            ),
            Place {
                line: 2,
                column: 6,
                past_end: false
            }
        );
        assert_eq!(
            place(
                Target {
                    line: 10,
                    column: None
                },
                10
            ),
            Place {
                line: 9,
                column: 0,
                past_end: false
            }
        );
        // An empty text has one line.
        assert_eq!(
            place(
                Target {
                    line: 1,
                    column: Some(4)
                },
                0
            ),
            Place {
                line: 0,
                column: 3,
                past_end: false
            }
        );
    }

    #[test]
    fn a_line_past_the_end_goes_to_the_last_one_and_says_so() {
        let asked = Target {
            line: 51,
            column: Some(3),
        };
        assert_eq!(
            place(asked, 50),
            Place {
                line: 49,
                column: 0,
                past_end: true
            }
        );
        let notice = past_end_notice(asked, 50);
        assert!(notice.contains("51") && notice.contains("50"), "{notice}");
        assert!(hint(0).contains("1 to 1"), "{}", hint(0));
    }
}
