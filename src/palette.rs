//! The rows of the command palette: how many it lists and the highlighted
//! one, moved by the keyboard.

/// Rows the palette lists while its text is empty: a short start, not
/// every table of every connection.
pub const EMPTY_LIMIT: usize = 10;

/// Rows the palette lists for a search; typing more narrows it.
pub const LIMIT: usize = 50;

/// The rows the palette lists for `text`.
pub fn limit(text: &str) -> usize {
    if text.trim().is_empty() {
        EMPTY_LIMIT
    } else {
        LIMIT
    }
}

/// Keeps the first rows `text` allows; `true` when some were left out.
pub fn truncate<T>(items: &mut Vec<T>, text: &str) -> bool {
    let limit = limit(text);
    let more = items.len() > limit;
    items.truncate(limit);
    more
}

/// Where `text` holds each of `needles`, ignoring case as the searches do,
/// as byte ranges in order and never overlapping, for the palette to mark
/// what made a row match. Empty needles mark nothing.
pub fn highlights(text: &str, needles: &[&str]) -> Vec<std::ops::Range<usize>> {
    let mut found: Vec<std::ops::Range<usize>> = Vec::new();
    for needle in needles {
        let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
        if needle.is_empty() {
            continue;
        }
        let mut starts = text.char_indices();
        while let Some((start, _)) = starts.next() {
            // Compares char by char in lower case, so the range stays on the
            // original text's char boundaries.
            let mut wanted = needle.iter();
            let mut end = None;
            let mut pending = Vec::new();
            for (at, ch) in text[start..].char_indices() {
                pending.extend(ch.to_lowercase());
                let mut matched = true;
                while let Some(&lower) = pending.first() {
                    match wanted.next() {
                        Some(&want) if want == lower => {
                            pending.remove(0);
                        }
                        _ => {
                            matched = false;
                            break;
                        }
                    }
                }
                if !matched {
                    break;
                }
                if wanted.len() == 0 {
                    end = Some(start + at + ch.len_utf8());
                    break;
                }
            }
            if let Some(end) = end {
                found.push(start..end);
                // The next match starts after this one.
                while starts.clone().next().is_some_and(|(at, _)| at < end) {
                    starts.next();
                }
            }
        }
    }
    found.sort_by_key(|range| range.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for range in found {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// The row after moving `delta` rows from `current` in a list of `len`,
/// wrapping at both ends as other palettes do.
pub fn step(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as isize;
    ((current.min(len as usize - 1) as isize + delta).rem_euclid(len)) as usize
}

/// The row to highlight when the list changes: the same item when it is
/// still there, so new search results do not move the choice, otherwise
/// the first.
pub fn keep<T: PartialEq>(previous: Option<&T>, items: &[T]) -> usize {
    previous
        .and_then(|previous| items.iter().position(|item| item == previous))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_are_marked_ignoring_case_on_char_boundaries() {
        assert_eq!(
            highlights("Orders and ORDER_items", &["order"]),
            vec![0..5, 11..16]
        );
        assert_eq!(highlights("Ação · ação", &["AÇÃO"]), vec![0..6, 10..16]);
        assert_eq!(highlights("aaa", &["aa"]), vec![0..2]);
        // Several needles merge where they touch or overlap.
        assert_eq!(
            highlights("#sales #salary", &["#sal", "sales"]),
            vec![0..6, 7..11]
        );
        assert!(highlights("orders", &["", "x"]).is_empty());
    }

    #[test]
    fn an_empty_text_lists_a_short_start_and_a_search_more() {
        let mut items: Vec<usize> = (0..300).collect();
        assert!(truncate(&mut items, "  "));
        assert_eq!(items.len(), EMPTY_LIMIT);
        let mut items: Vec<usize> = (0..300).collect();
        assert!(truncate(&mut items, "ord"));
        assert_eq!(items.len(), LIMIT);
        let mut items: Vec<usize> = (0..LIMIT).collect();
        assert!(!truncate(&mut items, "ord"));
        assert_eq!(items.len(), LIMIT);
    }

    #[test]
    fn arrows_wrap_around_the_list() {
        assert_eq!(step(0, 3, 1), 1);
        assert_eq!(step(2, 3, 1), 0);
        assert_eq!(step(0, 3, -1), 2);
        assert_eq!(step(1, 3, -1), 0);
        // A position past a shorter list starts from its last row.
        assert_eq!(step(9, 3, 1), 0);
        assert_eq!(step(4, 0, 1), 0);
    }

    #[test]
    fn the_choice_survives_new_results_that_still_hold_it() {
        let items = ["orders", "customers", "products"];
        assert_eq!(keep(Some(&"customers"), &items), 1);
        assert_eq!(keep(Some(&"missing"), &items), 0);
        assert_eq!(keep(None, &items), 0);
    }
}
