//! Which result columns the grid shows, in which order, and which stay
//! frozen at its left edge, computed without a window.
//!
//! A [`ColumnLayout`] maps the grid's display positions to result columns.
//! It belongs to one result tab and lives in memory only. Copying,
//! exporting, the records view and the statistics read the visible columns
//! in display order through it; the values, their types and NULLs are never
//! touched ([Results](../docs/reference/results.md#grid)).
use std::ops::Range;

/// The columns of a result as the grid lays them out: every column in
/// display order, which are hidden, and how many lead the order frozen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnLayout {
    /// Every result column in display order, hidden ones included.
    order: Vec<usize>,
    /// By result column.
    hidden: Vec<bool>,
    /// The first `frozen_order` columns of `order` are frozen.
    frozen_order: usize,
    /// The columns shown, in display order (derived).
    visible: Vec<usize>,
    /// Display position of each result column; `None` when hidden (derived).
    positions: Vec<Option<usize>>,
    /// Frozen columns among `visible`, which lead it (derived).
    frozen_visible: usize,
}

impl Default for ColumnLayout {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ColumnLayout {
    /// Every one of `columns` shown in the result's order, none frozen.
    pub fn new(columns: usize) -> Self {
        let mut layout = Self {
            order: (0..columns).collect(),
            hidden: vec![false; columns],
            frozen_order: 0,
            visible: Vec::new(),
            positions: Vec::new(),
            frozen_visible: 0,
        };
        layout.rebuild();
        layout
    }

    /// The result's number of columns, hidden ones included.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The result columns shown, in display order.
    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    /// The result column at display `position`.
    pub fn column(&self, position: usize) -> Option<usize> {
        self.visible.get(position).copied()
    }

    /// The display position of result `column`; `None` when it is hidden or
    /// past the result's columns.
    pub fn position(&self, column: usize) -> Option<usize> {
        self.positions.get(column).copied().flatten()
    }

    /// How many shown columns are frozen: display positions `0..frozen()`.
    pub fn frozen(&self) -> usize {
        self.frozen_visible
    }

    pub fn is_frozen(&self, column: usize) -> bool {
        self.order[..self.frozen_order].contains(&column)
    }

    pub fn is_hidden(&self, column: usize) -> bool {
        self.hidden.get(column).copied().unwrap_or(false)
    }

    /// How many columns are hidden.
    pub fn hidden(&self) -> usize {
        self.order.len() - self.visible.len()
    }

    /// The hidden columns, in display order.
    pub fn hidden_columns(&self) -> impl Iterator<Item = usize> + '_ {
        self.order
            .iter()
            .copied()
            .filter(|column| self.is_hidden(*column))
    }

    /// Whether the layout is the result's own: every column shown, in its
    /// order, none frozen.
    pub fn is_identity(&self) -> bool {
        self.frozen_order == 0
            && self.visible.len() == self.order.len()
            && self
                .order
                .iter()
                .enumerate()
                .all(|(at, column)| at == *column)
    }

    /// Hides `column`. The last column shown stays, so the grid never
    /// shows nothing; `false` when nothing changed.
    pub fn hide(&mut self, column: usize) -> bool {
        if self.position(column).is_none() || self.visible.len() <= 1 {
            return false;
        }
        self.hidden[column] = true;
        self.rebuild();
        true
    }

    /// Shows `column` again where it was, frozen if it was.
    pub fn show(&mut self, column: usize) -> bool {
        if !self.is_hidden(column) {
            return false;
        }
        self.hidden[column] = false;
        self.rebuild();
        true
    }

    /// Shows every hidden column; `false` when none was.
    pub fn show_all(&mut self) -> bool {
        if self.hidden() == 0 {
            return false;
        }
        self.hidden.fill(false);
        self.rebuild();
        true
    }

    /// Freezes `column` at the left edge, after the columns frozen before it.
    pub fn freeze(&mut self, column: usize) -> bool {
        let Some(at) = self.order.iter().position(|each| *each == column) else {
            return false;
        };
        if at < self.frozen_order {
            return false;
        }
        self.order.remove(at);
        self.order.insert(self.frozen_order, column);
        self.frozen_order += 1;
        self.rebuild();
        true
    }

    /// Lets `column` scroll again, as the first column after the frozen ones.
    pub fn unfreeze(&mut self, column: usize) -> bool {
        let Some(at) = self.order.iter().position(|each| *each == column) else {
            return false;
        };
        if at >= self.frozen_order {
            return false;
        }
        self.order.remove(at);
        self.frozen_order -= 1;
        self.order.insert(self.frozen_order, column);
        self.rebuild();
        true
    }

    /// Moves `column` to the place of `target`, as a header dropped on
    /// another: `target` shifts towards where `column` was. Dropped on a
    /// frozen column it is frozen there; on a scrolling one it scrolls.
    pub fn move_onto(&mut self, column: usize, target: usize) -> bool {
        let find = |order: &[usize], wanted: usize| order.iter().position(|each| *each == wanted);
        let (Some(from), Some(to)) = (find(&self.order, column), find(&self.order, target)) else {
            return false;
        };
        if from == to {
            return false;
        }
        let target_frozen = to < self.frozen_order;
        self.order.remove(from);
        if from < self.frozen_order {
            self.frozen_order -= 1;
        }
        // Rightwards the target moved one place left with the removal, so
        // `to` is just after it; leftwards `to` is just before it.
        self.order.insert(to, column);
        if target_frozen {
            self.frozen_order += 1;
        }
        self.rebuild();
        true
    }

    /// The display range `column` can move within: the frozen columns, or
    /// the ones that scroll.
    fn group(&self, position: usize) -> Range<usize> {
        if position < self.frozen_visible {
            0..self.frozen_visible
        } else {
            self.frozen_visible..self.visible.len()
        }
    }

    /// The display position `column` reaches moving `delta` places among
    /// the shown columns of its group (frozen or scrolling), clamped at its
    /// ends; `None` when it cannot move that way.
    fn moved_position(&self, column: usize, delta: isize) -> Option<usize> {
        let position = self.position(column)?;
        let group = self.group(position);
        let target = position
            .saturating_add_signed(delta)
            .clamp(group.start, group.end.saturating_sub(1));
        (target != position).then_some(target)
    }

    /// Whether `column` can move `delta` places within its group.
    pub fn can_move(&self, column: usize, delta: isize) -> bool {
        self.moved_position(column, delta).is_some()
    }

    /// Moves `column` `delta` places among the shown columns of its group,
    /// left when negative; frozen columns stay frozen and the others stay
    /// scrolling. `isize::MIN` and `isize::MAX` reach its group's ends.
    pub fn move_by(&mut self, column: usize, delta: isize) -> bool {
        match self
            .moved_position(column, delta)
            .and_then(|target| self.column(target))
        {
            Some(target) => self.move_onto(column, target),
            None => false,
        }
    }

    /// Back to the result's own layout.
    pub fn reset(&mut self) -> bool {
        if self.is_identity() {
            return false;
        }
        *self = Self::new(self.order.len());
        true
    }

    fn rebuild(&mut self) {
        self.visible.clear();
        self.positions.clear();
        self.positions.resize(self.order.len(), None);
        self.frozen_visible = 0;
        for (at, column) in self.order.iter().enumerate() {
            if self.hidden[*column] {
                continue;
            }
            self.positions[*column] = Some(self.visible.len());
            self.visible.push(*column);
            if at < self.frozen_order {
                self.frozen_visible += 1;
            }
        }
    }
}

/// The widths the grid lays its columns out with: by result column, and
/// for the shown columns in display order with where each one starts.
/// Built when the widths or the layout change, never per frame, so finding
/// the columns in view does not grow with the result's columns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColumnWidths {
    /// By result column.
    by_column: Vec<f32>,
    /// Of the shown columns, in display order.
    display: Vec<f32>,
    /// `starts[position]` is the width of the shown columns before display
    /// `position`; the last entry is the width of them all.
    starts: Vec<f32>,
    /// Display positions `0..frozen` are frozen.
    frozen: usize,
}

impl ColumnWidths {
    /// From each result column's width (`by_column`; a column past its end
    /// takes `missing`) and the layout.
    pub fn new(by_column: Vec<f32>, layout: &ColumnLayout, missing: f32) -> Self {
        let display: Vec<f32> = layout
            .visible()
            .iter()
            .map(|column| by_column.get(*column).copied().unwrap_or(missing))
            .collect();
        let mut starts = Vec::with_capacity(display.len() + 1);
        let mut x = 0.;
        starts.push(x);
        for width in &display {
            x += width;
            starts.push(x);
        }
        let frozen = layout.frozen().min(display.len());
        Self {
            by_column,
            display,
            starts,
            frozen,
        }
    }

    /// Each result column's width.
    pub fn by_column(&self) -> &[f32] {
        &self.by_column
    }

    /// The shown columns' widths, in display order.
    pub fn display(&self) -> &[f32] {
        &self.display
    }

    /// How many shown columns are frozen: display positions `0..frozen()`.
    pub fn frozen(&self) -> usize {
        self.frozen
    }

    /// The width of the shown columns at display `positions`.
    pub fn span(&self, positions: Range<usize>) -> f32 {
        let end = positions.end.min(self.display.len());
        let start = positions.start.min(end);
        self.starts[end] - self.starts[start]
    }

    /// The width of every shown column.
    pub fn total(&self) -> f32 {
        self.starts.last().copied().unwrap_or_default()
    }

    /// The scrolling columns (display positions from [`Self::frozen`] on)
    /// that a viewport `viewport` px wide shows when panned `pan` px, the
    /// first starting `lead` px from the content's left (the row numbers
    /// and frozen columns): their range and the width of the scrolling
    /// columns before it. Only these are built, so a frame costs the
    /// columns on screen, not the result's. With frozen columns, the row
    /// numbers and frozen columns stay over the viewport's first `lead` px,
    /// so the scrolling columns wholly under them are not built either. A
    /// viewport not measured yet (zero) is taken as 4,096 px wide.
    pub fn shown(&self, lead: f32, pan: f32, viewport: f32) -> (Range<usize>, f32) {
        let viewport = if viewport > 0. { viewport } else { 4096. };
        let (frozen, count) = (self.frozen, self.display.len());
        // Without frozen columns the row numbers pan with the rest.
        let left = if frozen > 0 { pan + lead } else { pan };
        let right = pan + viewport;
        // Where an edge of a scrolling column lies in the content.
        let base = self.starts[frozen];
        let at = |edge: f32| lead + (edge - base);
        // The first column ending past `left`, and the first one starting
        // at or past `right`.
        let first =
            frozen + self.starts[frozen + 1..=count].partition_point(|end| at(*end) <= left);
        let end = frozen + self.starts[frozen..count].partition_point(|start| at(*start) < right);
        if first < end {
            (first..end, self.starts[first] - base)
        } else {
            (frozen..frozen, 0.)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_layout_is_the_results_own() {
        let layout = ColumnLayout::new(3);
        assert_eq!(layout.visible(), [0, 1, 2]);
        assert_eq!(layout.position(2), Some(2));
        assert_eq!(layout.position(3), None);
        assert_eq!((layout.frozen(), layout.hidden()), (0, 0));
        assert!(layout.is_identity());
    }

    #[test]
    fn hidden_columns_leave_the_display_and_come_back_where_they_were() {
        let mut layout = ColumnLayout::new(4);
        assert!(layout.hide(1));
        assert!(!layout.hide(1));
        assert!(layout.hide(3));
        assert_eq!(layout.visible(), [0, 2]);
        assert_eq!(layout.position(2), Some(1));
        assert_eq!(layout.position(1), None);
        assert_eq!(layout.hidden(), 2);
        assert_eq!(layout.hidden_columns().collect::<Vec<_>>(), [1, 3]);
        assert!(layout.show(1));
        assert_eq!(layout.visible(), [0, 1, 2]);
        assert!(layout.show_all());
        assert!(!layout.show_all());
        assert!(layout.is_identity());
    }

    #[test]
    fn the_last_column_shown_cannot_be_hidden() {
        let mut layout = ColumnLayout::new(2);
        assert!(layout.hide(0));
        assert!(!layout.hide(1));
        assert_eq!(layout.visible(), [1]);
    }

    #[test]
    fn frozen_columns_lead_in_the_order_they_were_frozen() {
        let mut layout = ColumnLayout::new(5);
        assert!(layout.freeze(3));
        assert!(layout.freeze(1));
        assert!(!layout.freeze(1));
        assert_eq!(layout.visible(), [3, 1, 0, 2, 4]);
        assert_eq!(layout.frozen(), 2);
        assert!(layout.is_frozen(3) && !layout.is_frozen(0));
        // Unfrozen, a column is the first that scrolls.
        assert!(layout.unfreeze(3));
        assert!(!layout.unfreeze(3));
        assert_eq!(layout.visible(), [1, 3, 0, 2, 4]);
        assert_eq!(layout.frozen(), 1);
    }

    #[test]
    fn a_hidden_frozen_column_counts_only_when_shown() {
        let mut layout = ColumnLayout::new(3);
        layout.freeze(2);
        layout.freeze(0);
        assert!(layout.hide(2));
        assert_eq!((layout.visible(), layout.frozen()), (&[0, 1][..], 1));
        assert!(layout.show(2));
        assert_eq!((layout.visible(), layout.frozen()), (&[2, 0, 1][..], 2));
    }

    #[test]
    fn a_column_dropped_on_another_takes_its_place() {
        let mut layout = ColumnLayout::new(4);
        // Rightwards: the target shifts left.
        assert!(layout.move_onto(0, 2));
        assert_eq!(layout.visible(), [1, 2, 0, 3]);
        // Leftwards: the target shifts right.
        assert!(layout.move_onto(3, 1));
        assert_eq!(layout.visible(), [3, 1, 2, 0]);
        assert!(!layout.move_onto(3, 3));
        assert!(!layout.move_onto(9, 1));
    }

    #[test]
    fn dropping_on_a_frozen_column_freezes_and_out_of_them_unfreezes() {
        let mut layout = ColumnLayout::new(4);
        layout.freeze(0);
        assert!(layout.move_onto(3, 0));
        assert_eq!((layout.visible(), layout.frozen()), (&[3, 0, 1, 2][..], 2));
        assert!(layout.move_onto(3, 2));
        assert_eq!((layout.visible(), layout.frozen()), (&[0, 1, 2, 3][..], 1));
        assert!(layout.is_frozen(0) && !layout.is_frozen(3));
    }

    #[test]
    fn moving_by_places_stays_in_its_group_and_skips_hidden_columns() {
        let mut layout = ColumnLayout::new(5);
        layout.freeze(4);
        layout.hide(1);
        // [4 | 0, 2, 3]
        assert!(layout.move_by(0, 1));
        assert_eq!(layout.visible(), [4, 2, 0, 3]);
        // The first scrolling column does not move into the frozen ones.
        assert!(!layout.can_move(2, -1));
        assert!(!layout.move_by(2, -1));
        assert!(!layout.can_move(4, 1));
        assert!(layout.move_by(2, isize::MAX));
        assert_eq!(layout.visible(), [4, 0, 3, 2]);
        assert!(layout.move_by(2, isize::MIN));
        assert_eq!(layout.visible(), [4, 2, 0, 3]);
        // The hidden column comes back where the order left it, after 4.
        layout.show(1);
        assert_eq!(layout.visible(), [4, 1, 2, 0, 3]);
        assert!(layout.reset());
        assert!(layout.is_identity());
    }

    #[test]
    fn only_the_scrolling_columns_in_the_viewport_are_built() {
        let widths = ColumnWidths::new(vec![100.; 6], &ColumnLayout::new(6), 48.);
        // No frozen columns, 35 px of row numbers, 250 px viewport.
        assert_eq!(widths.shown(35., 0., 250.), (0..3, 0.));
        // Panned 260 px: the content from 260 to 510 holds columns 2..5.
        assert_eq!(widths.shown(35., 260., 250.), (2..5, 200.));
        // A viewport not measured yet is taken as wide.
        assert_eq!(widths.shown(35., 0., 0.), (0..6, 0.));
        // Panned past the end.
        assert_eq!(widths.shown(35., 900., 250.), (0..0, 0.));
    }

    fn frozen(columns: usize, frozen: usize) -> ColumnLayout {
        let mut layout = ColumnLayout::new(columns);
        for column in 0..frozen {
            layout.freeze(column);
        }
        layout
    }

    #[test]
    fn scrolling_columns_under_the_frozen_ones_are_not_built() {
        let widths = ColumnWidths::new(vec![100.; 6], &frozen(6, 2), 48.);
        assert_eq!((widths.frozen(), widths.span(0..2)), (2, 200.));
        // Scrolling columns start after the frozen ones.
        assert_eq!(widths.shown(235., 0., 250.), (2..3, 0.));
        assert_eq!(widths.shown(235., 0., 400.), (2..4, 0.));
        // The row numbers and frozen columns stay over the first 235 px.
        // Panned 100 px, they cover the content from 100 to 335: column 2
        // (235 to 335) is wholly under them; one pixel less and it shows.
        assert_eq!(widths.shown(235., 100., 250.), (3..4, 100.));
        assert_eq!(widths.shown(235., 100., 400.), (3..5, 100.));
        assert_eq!(widths.shown(235., 99., 400.), (2..5, 0.));
        assert_eq!(widths.shown(235., 340., 250.), (5..6, 300.));
        // Nothing scrolls.
        let all = ColumnWidths::new(vec![100.; 6], &frozen(6, 6), 48.);
        assert_eq!(all.shown(635., 0., 250.), (6..6, 0.));
    }

    #[test]
    fn widths_follow_the_display_order_and_hidden_columns() {
        let mut layout = ColumnLayout::new(4);
        layout.freeze(2);
        layout.hide(1);
        // [2 | 0, 3]; column 3 has no width of its own.
        let widths = ColumnWidths::new(vec![10., 20., 30.], &layout, 48.);
        assert_eq!(widths.by_column(), [10., 20., 30.]);
        assert_eq!(widths.display(), [30., 10., 48.]);
        assert_eq!((widths.frozen(), widths.total()), (1, 88.));
        assert_eq!(widths.span(1..3), 58.);
        assert_eq!(widths.span(2..9), 48.);
        let empty = ColumnWidths::new(Vec::new(), &ColumnLayout::new(0), 48.);
        assert_eq!(
            (empty.total(), empty.shown(35., 0., 250.)),
            (0., (0..0, 0.))
        );
    }

    /// The columns in view, found one by one from the left.
    fn shown_by_walking(
        display: &[f32],
        frozen: usize,
        lead: f32,
        pan: f32,
        viewport: f32,
    ) -> (Range<usize>, f32) {
        let viewport = if viewport > 0. { viewport } else { 4096. };
        let left = if frozen > 0 { pan + lead } else { pan };
        let (right, mut x) = (pan + viewport, lead);
        let (mut first, mut before) = (None, 0.);
        let mut end = frozen.min(display.len());
        for (position, width) in display.iter().enumerate().skip(frozen) {
            if x >= right {
                break;
            }
            if x + width > left {
                if first.is_none() {
                    first = Some(position);
                    before = x - lead;
                }
                end = position + 1;
            }
            x += width;
        }
        (first.unwrap_or(end)..end, before)
    }

    #[test]
    fn the_columns_in_view_are_those_found_by_walking_the_widths() {
        // Whole widths of 48 to 300 px, in a fixed scrambled order.
        let by_column: Vec<f32> = (0..400u16)
            .map(|n| 48. + f32::from((n * 37) % 253))
            .collect();
        for frozen_columns in [0, 1, 3, 400] {
            let layout = frozen(400, frozen_columns);
            let widths = ColumnWidths::new(by_column.clone(), &layout, 48.);
            let lead = 35. + widths.span(0..widths.frozen());
            for pan in (0..71u16).map(|step| f32::from(step) * 997.) {
                for viewport in [0., 1., 250., 1480.] {
                    assert_eq!(
                        widths.shown(lead, pan, viewport),
                        shown_by_walking(widths.display(), widths.frozen(), lead, pan, viewport),
                        "{frozen_columns} frozen, panned {pan}, {viewport} px"
                    );
                }
            }
        }
    }
}
