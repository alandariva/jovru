//! Where the SQL is in an answer: fenced blocks tagged as SQL, or untagged.
use std::ops::Range;

/// The text of one block, without its fences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlBlock {
    pub range: Range<usize>,
    /// `false` while the closing fence has not arrived; such a block shows
    /// no action until it is complete.
    pub closed: bool,
}

const TAGS: [&str; 9] = [
    "",
    "sql",
    "postgres",
    "postgresql",
    "mysql",
    "sqlite",
    "databricks",
    "sparksql",
    "spark",
];

/// Every SQL block of `text`, in order. A fence is three or more backticks
/// at the start of a line; blocks tagged with another language are skipped.
pub fn sql_blocks(text: &str) -> Vec<SqlBlock> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, usize, bool)> = None; // body start, fence length, is SQL
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let trimmed = line.trim_start_matches([' ', '\t']);
        let fence = trimmed.bytes().take_while(|byte| *byte == b'`').count();
        match open {
            None if fence >= 3 => {
                let tag = trimmed[fence..].trim().to_ascii_lowercase();
                open = Some((at, fence, TAGS.contains(&tag.as_str())));
            }
            Some((body, length, sql)) if fence >= length && trimmed[fence..].trim().is_empty() => {
                if sql {
                    blocks.push(SqlBlock {
                        range: body..start,
                        closed: true,
                    });
                }
                open = None;
            }
            _ => {}
        }
    }
    if let Some((body, _, true)) = open {
        blocks.push(SqlBlock {
            range: body.min(text.len())..text.len(),
            closed: false,
        });
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bodies(text: &str) -> Vec<(&str, bool)> {
        sql_blocks(text)
            .into_iter()
            .map(|block| (&text[block.range], block.closed))
            .collect()
    }

    #[test]
    fn tagged_and_untagged_blocks_are_found_and_other_languages_skipped() {
        let text = "Try this:\n```sql\nSELECT 1;\n```\nor\n```\nSELECT 2;\n```\n```python\nprint(1)\n```\n";
        assert_eq!(bodies(text), [("SELECT 1;\n", true), ("SELECT 2;\n", true)]);
    }

    #[test]
    fn an_unclosed_block_is_reported_open() {
        assert_eq!(bodies("```SQL\nSELECT"), [("SELECT", false)]);
        assert_eq!(bodies("```sql"), [("", false)]);
    }

    #[test]
    fn longer_fences_need_as_long_a_closing_fence_and_crlf_works() {
        let text = "````sql\r\nSELECT '```';\r\n```\r\nSELECT 2;\r\n````\r\n";
        assert_eq!(
            bodies(text),
            [("SELECT '```';\r\n```\r\nSELECT 2;\r\n", true)]
        );
        assert_eq!(bodies("no code here"), []);
        assert_eq!(bodies("```mysql\nSELECT 1\n```"), [("SELECT 1\n", true)]);
    }
}
