//! The highlighted row of the command palette, moved by the keyboard.

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
