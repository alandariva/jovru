//! Finding a result's columns by name, ignoring case and accents, computed
//! without a window ([Results](../docs/reference/results.md#grid)).
//!
//! [`find`] lists the columns whose name holds the text, the shown ones in
//! display order first, then the hidden ones, each with the byte ranges of
//! its name that matched, for the grid to mark them. It runs when the text
//! or the layout changes, never per frame.
use crate::column_layout::ColumnLayout;
use crate::result::ResultColumn;
use std::ops::Range;

/// A column whose name holds the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnMatch {
    /// The result column.
    pub column: usize,
    /// Byte ranges of its name that matched, in order, never overlapping.
    pub ranges: Vec<Range<usize>>,
    /// The column layout hides it; choosing it shows it again.
    pub hidden: bool,
}

/// The base letters of the accented Latin letters, so a search ignores
/// accents: `ç` finds `c`, `ã` finds `a`, `ß` finds `ss`. Upper case is
/// lowered before.
fn base(letter: char) -> Option<&'static str> {
    Some(match letter {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
        'ĵ' => "j",
        'ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' => "s",
        'ţ' | 'ť' | 'ŧ' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        'þ' => "th",
        _ => return None,
    })
}

/// Whether `ch` is a combining accent, which a search ignores.
fn combining(ch: char) -> bool {
    ('\u{0300}'..='\u{036f}').contains(&ch)
}

/// `text` as a search compares it: each character lowered and without its
/// accent (one or more characters, or none for a combining accent), with
/// the byte range of the character it came from. A combining accent joins
/// the range of the character before it.
fn folded(text: &str) -> Vec<(char, Range<usize>)> {
    let mut out: Vec<(char, Range<usize>)> = Vec::with_capacity(text.len());
    for (start, ch) in text.char_indices() {
        let end = start + ch.len_utf8();
        let before = out.len();
        for lower in ch.to_lowercase() {
            if combining(lower) {
                continue;
            }
            match base(lower) {
                Some(letters) => out.extend(letters.chars().map(|letter| (letter, start..end))),
                None => out.push((lower, start..end)),
            }
        }
        if out.len() == before
            && let Some((_, last)) = out.last_mut()
        {
            last.end = end;
        }
    }
    out
}

/// What a search looks for: `text` trimmed, lowered and without accents.
pub fn needle(text: &str) -> Vec<char> {
    folded(text.trim()).into_iter().map(|(ch, _)| ch).collect()
}

/// Where `name` holds `needle` (from [`needle`]), ignoring case and
/// accents, as byte ranges of `name` in order and never overlapping;
/// empty when it does not, or when `needle` is empty.
pub fn ranges(name: &str, needle: &[char]) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    let name = folded(name);
    let mut found: Vec<Range<usize>> = Vec::new();
    let mut at = 0;
    while at + needle.len() <= name.len() {
        let window = &name[at..at + needle.len()];
        if window.iter().map(|(ch, _)| ch).eq(needle.iter()) {
            let range = window[0].1.start..window[needle.len() - 1].1.end;
            match found.last_mut() {
                // `ß` holds two letters: a match may end inside it.
                Some(last) if range.start < last.end => last.end = last.end.max(range.end),
                _ => found.push(range),
            }
            at += needle.len();
        } else {
            at += 1;
        }
    }
    found
}

/// The columns of `columns` whose name holds `text`, ignoring case and
/// accents: the shown ones in display order, then the hidden ones in
/// layout order. Nothing for an empty text.
pub fn find(columns: &[ResultColumn], layout: &ColumnLayout, text: &str) -> Vec<ColumnMatch> {
    let needle = needle(text);
    if needle.is_empty() {
        return Vec::new();
    }
    let shown = layout.visible().iter().map(|column| (*column, false));
    let hidden = layout.hidden_columns().map(|column| (column, true));
    shown
        .chain(hidden)
        .filter_map(|(column, hidden)| {
            let name = &columns.get(column)?.name;
            let ranges = ranges(name, &needle);
            (!ranges.is_empty()).then_some(ColumnMatch {
                column,
                ranges,
                hidden,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn columns(names: &[&str]) -> Vec<ResultColumn> {
        names
            .iter()
            .map(|name| ResultColumn::new(*name, "text"))
            .collect()
    }

    /// Where `name` holds `text`, as (start, end) byte pairs.
    fn spans(name: &str, text: &str) -> Vec<(usize, usize)> {
        ranges(name, &needle(text))
            .into_iter()
            .map(|range| (range.start, range.end))
            .collect()
    }

    #[test]
    fn names_match_ignoring_case_and_accents() {
        assert_eq!(needle("  CAO "), ['c', 'a', 'o']);
        // "Ação" is "acao": the match covers "ção", whose first two
        // characters take two bytes each.
        assert_eq!(spans("Ação", "CAO"), [(1, 6)]);
        assert_eq!(spans("descricao_e_situacao", "cao"), [(6, 9), (17, 20)]);
        assert_eq!(spans("ORDER_ID", "id"), [(6, 8)]);
        assert!(spans("order", "x").is_empty());
        assert!(ranges("order", &[]).is_empty());
        // The text typed may carry the accents instead.
        assert_eq!(spans("situacao", "situação"), [(0, 8)]);
    }

    #[test]
    fn combining_accents_and_letters_that_spread_keep_whole_characters() {
        // "e" followed by a combining acute accent.
        assert_eq!(spans("cafe\u{301}_total", "café"), [(0, 6)]);
        assert_eq!(spans("Straße", "strasse"), [(0, 7)]);
        // A match ending inside "ß" marks all of it, once.
        assert_eq!(spans("ß", "s"), [(0, 2)]);
    }

    #[test]
    fn shown_columns_come_first_in_display_order_then_hidden_ones() {
        let columns = columns(&["id", "valid", "paid_at", "name", "Ídolo"]);
        let mut layout = ColumnLayout::new(5);
        layout.move_onto(2, 0);
        layout.hide(1);
        // Shown as paid_at, id, name, Ídolo; valid hidden.
        let found = find(&columns, &layout, "id");
        let summary: Vec<_> = found
            .iter()
            .map(|found| {
                let first = found.ranges.first().map(|range| (range.start, range.end));
                (found.column, found.hidden, found.ranges.len(), first)
            })
            .collect();
        assert_eq!(
            summary,
            [
                (2, false, 1, Some((2, 4))),
                (0, false, 1, Some((0, 2))),
                (4, false, 1, Some((0, 3))),
                (1, true, 1, Some((3, 5))),
            ]
        );
        assert!(find(&columns, &layout, "   ").is_empty());
        assert!(find(&columns, &layout, "zzz").is_empty());
    }

    #[test]
    fn a_search_over_thousands_of_columns_stays_within_budget() {
        let names: Vec<String> = (0..10_000)
            .map(|n| format!("coluna_descrição_{n}"))
            .collect();
        let columns: Vec<ResultColumn> = names
            .iter()
            .map(|name| ResultColumn::new(name.as_str(), "text"))
            .collect();
        let layout = ColumnLayout::new(columns.len());
        let started = Instant::now();
        let found = find(&columns, &layout, "descricao_99");
        // Each keystroke searches once; well under a frame in a release
        // build, generous here for debug builds.
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(found.len(), 111);
    }
}
