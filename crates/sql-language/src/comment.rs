//! Line comments as the editor's **Ctrl+/** toggles them.
//!
//! The lines are those the selection touches, or the caret's line. When every
//! line with text already starts with `--` (after its indentation), the
//! markers are taken out; otherwise each line with text gets `-- ` at the
//! smallest indentation among them, so the block stays aligned and toggling
//! again gives the text back. `--` is a line comment in every dialect the
//! application reads, and the space after it is what MySQL needs to read it
//! as one. Blank lines are left as they are, unless every line is blank.
use std::ops::Range;

/// What every line gets when it is commented.
const MARKER: &str = "-- ";

/// What toggling line comments changes in the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToggledComments {
    /// The bytes of the text that change: the whole lines toggled, without
    /// the line break after the last one.
    pub range: Range<usize>,
    /// What replaces `range`.
    pub text: String,
    /// The selection afterwards, in byte offsets of the new text: the same
    /// text stays selected, and a caret stays on the same character.
    pub selection: Range<usize>,
}

/// One change to the text: `inserted` bytes at `at`, or `removed` bytes from
/// `at`, in offsets of the text before the change.
#[derive(Clone, Copy)]
struct Change {
    at: usize,
    inserted: usize,
    removed: usize,
}

/// Comments the lines `selection` touches (a byte range of `sql`; the
/// caret's line when it is empty) with `--`, or takes the comment out when
/// every line with text is already a line comment. A selection that ends at
/// the start of a line does not toggle that line. Offsets past the text or
/// inside a character are moved back to the nearest character boundary.
pub fn toggle_line_comments(sql: &str, selection: Range<usize>) -> ToggledComments {
    let boundary = |offset: usize| {
        let mut offset = offset.min(sql.len());
        while !sql.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    };
    let (start, end) = {
        let (a, b) = (boundary(selection.start), boundary(selection.end));
        (a.min(b), a.max(b))
    };
    let first = sql[..start].rfind('\n').map_or(0, |at| at + 1);
    // A selection of whole lines usually ends at the start of the next one,
    // which is not part of it.
    let last_at = if end > start && end > first && sql[..end].ends_with('\n') {
        end - 1
    } else {
        end
    };
    let last = sql[last_at..]
        .find('\n')
        .map_or(sql.len(), |at| last_at + at);
    let range = first..last;
    let mut lines = Vec::new();
    let mut at = first;
    for line in sql[range.clone()].split('\n') {
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        let blank = line.trim().is_empty();
        lines.push((at, indent, blank, line));
        at += line.len() + 1;
    }
    let with_text: Vec<_> = lines.iter().filter(|line| !line.2).collect();
    let with_text_is_empty = with_text.is_empty();
    let uncomment = !with_text_is_empty
        && with_text
            .iter()
            .all(|(_, indent, _, line)| line[*indent..].starts_with("--"));
    let mut changes = Vec::with_capacity(lines.len());
    if uncomment {
        for (at, indent, _, line) in with_text {
            let removed = if line[*indent..].starts_with("-- ") {
                3
            } else {
                2
            };
            changes.push(Change {
                at: at + indent,
                inserted: 0,
                removed,
            });
        }
    } else if with_text_is_empty {
        // Only blank lines: each one is commented where its spaces end.
        for (at, indent, _, _) in &lines {
            changes.push(Change {
                at: at + indent,
                inserted: MARKER.len(),
                removed: 0,
            });
        }
    } else {
        let column = with_text
            .iter()
            .map(|(_, indent, _, _)| *indent)
            .min()
            .unwrap_or(0);
        for (at, _, _, _) in with_text {
            changes.push(Change {
                at: at + column,
                inserted: MARKER.len(),
                removed: 0,
            });
        }
    }
    let mut text = String::with_capacity(range.len() + changes.len() * MARKER.len());
    let mut copied = range.start;
    for change in &changes {
        text.push_str(&sql[copied..change.at]);
        if change.inserted > 0 {
            text.push_str(MARKER);
        }
        copied = change.at + change.removed;
    }
    text.push_str(&sql[copied..range.end]);
    // A selection that starts where a marker goes takes the marker in, so a
    // block selected from the start of its first line stays whole; a caret
    // moves past it, onto the character it was on, and on a blank line goes
    // after the new marker, ready for the comment's text.
    let selection = if start == end {
        let caret = match changes.first() {
            Some(change) if with_text_is_empty => change.at + MARKER.len(),
            _ => moved(start, &changes, false),
        };
        caret..caret
    } else {
        moved(start, &changes, true)..moved(end, &changes, false)
    };
    ToggledComments {
        range,
        text,
        selection,
    }
}

/// Where `offset` of the text before `changes` is after them. An offset where
/// text is inserted stays before it when `before` is set.
fn moved(offset: usize, changes: &[Change], before: bool) -> usize {
    let mut result = offset;
    for change in changes {
        if change.inserted > 0 {
            if offset > change.at || (offset == change.at && !before) {
                result += change.inserted;
            }
        } else if offset >= change.at + change.removed {
            result -= change.removed;
        } else if offset > change.at {
            result -= offset - change.at;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole text after toggling.
    fn apply(sql: &str, toggled: &ToggledComments) -> String {
        format!(
            "{}{}{}",
            &sql[..toggled.range.start],
            toggled.text,
            &sql[toggled.range.end..]
        )
    }

    fn toggle(sql: &str, selection: Range<usize>) -> (String, Range<usize>) {
        let toggled = toggle_line_comments(sql, selection);
        (apply(sql, &toggled), toggled.selection)
    }

    #[test]
    fn the_caret_comments_and_uncomments_its_line_only() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        let caret = sql.find("2;").unwrap();
        let toggled = toggle_line_comments(sql, caret..caret);
        // Only the caret's line changes, so the rest of the editor stays.
        assert_eq!(toggled.range, 10..19);
        assert_eq!(toggled.text, "-- SELECT 2;");
        let after = apply(sql, &toggled);
        assert_eq!(after, "SELECT 1;\n-- SELECT 2;\nSELECT 3;");
        // The caret stays on the same character.
        assert_eq!(toggled.selection, caret + 3..caret + 3);
        assert_eq!(&after[toggled.selection.start..][..2], "2;");
        let (back, selection) = toggle(&after, toggled.selection.clone());
        assert_eq!(back, sql);
        assert_eq!(selection, caret..caret);
    }

    #[test]
    fn a_block_is_commented_at_its_smallest_indentation_and_comes_back() {
        let sql = "SELECT\n    a,\n\n  b\nFROM t;";
        let start = sql.find("    a").unwrap();
        let end = sql.find("\nFROM").unwrap();
        let (commented, selection) = toggle(sql, start..end);
        // The blank line is left alone; the markers line up.
        assert_eq!(commented, "SELECT\n  --   a,\n\n  -- b\nFROM t;");
        assert_eq!(&commented[selection.clone()], "  --   a,\n\n  -- b");
        let (back, selection) = toggle(&commented, selection);
        assert_eq!(back, sql);
        assert_eq!(&back[selection], "    a,\n\n  b");
    }

    #[test]
    fn a_block_with_some_lines_commented_is_commented_again_whole() {
        let sql = "-- a\nb\n-- c";
        let (commented, _) = toggle(sql, 0..sql.len());
        assert_eq!(commented, "-- -- a\n-- b\n-- -- c");
        // Every line is a comment now: the outer markers come out.
        let (back, _) = toggle(&commented, 0..commented.len());
        assert_eq!(back, sql);
    }

    #[test]
    fn markers_without_a_space_come_out_too() {
        let (uncommented, selection) = toggle("--a\n  --  b", 0..11);
        assert_eq!(uncommented, "a\n   b");
        assert_eq!(selection, 0..uncommented.len());
    }

    #[test]
    fn a_selection_ending_at_the_start_of_a_line_leaves_that_line() {
        let sql = "a\nb\nc";
        let (commented, selection) = toggle(sql, 0..4);
        assert_eq!(commented, "-- a\n-- b\nc");
        // The selection grew with the markers, from the first line's start.
        assert_eq!(&commented[selection], "-- a\n-- b\n");
        // A selection from the middle of a line keeps its text.
        let (commented, selection) = toggle("a\nbcd\ne", 3..4);
        assert_eq!(commented, "a\n-- bcd\ne");
        assert_eq!(&commented[selection], "c");
    }

    #[test]
    fn blank_lines_alone_are_commented() {
        assert_eq!(toggle("", 0..0), ("-- ".to_owned(), 3..3));
        assert_eq!(toggle("a\n  \nb", 3..3), ("a\n  -- \nb".to_owned(), 7..7));
    }

    #[test]
    fn line_breaks_and_characters_are_kept() {
        let sql = "SELECT 'é'\r\nFROM t\r\n";
        let (commented, selection) = toggle(sql, 0..sql.len());
        assert_eq!(commented, "-- SELECT 'é'\r\n-- FROM t\r\n");
        let (back, _) = toggle(&commented, selection);
        assert_eq!(back, sql);
        // Offsets inside a character or past the end are moved back.
        let inside = sql.find('é').unwrap() + 1;
        let (commented, selection) = toggle(sql, inside..sql.len() + 10);
        assert_eq!(commented, "-- SELECT 'é'\r\n-- FROM t\r\n");
        assert_eq!(selection.end, commented.len());
    }

    #[test]
    fn the_caret_inside_a_marker_that_comes_out_moves_to_its_place() {
        let (uncommented, selection) = toggle("  -- x", 3..3);
        assert_eq!(uncommented, "  x");
        assert_eq!(selection, 2..2);
    }

    #[test]
    fn a_reversed_range_is_read_as_its_ends() {
        // As an editor may report a selection made backwards.
        let (head, anchor) = (3, 0);
        assert_eq!(toggle("a\nb", head..anchor), toggle("a\nb", 0..3));
    }
}
