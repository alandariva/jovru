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
