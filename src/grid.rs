//! What the result grid shows, computed without a window.
//!
//! [`GridView`] is the grid's derived data for one result, filter, sort and
//! number format: the rows in display order, column widths and value
//! families. [`GridCache`] rebuilds it only when one of those inputs changes,
//! so hover, scroll and selection frames never pay for it. Selections address
//! data rows, so filtering or sorting does not change what is selected.
use crate::cell_format::{self, ValueFamily};
use crate::result::ResultData;
use std::{cell::RefCell, ops::RangeInclusive, rc::Rc};

/// A rectangular block of result cells, from the cell clicked first (anchor)
/// to the one reached with Shift (focus). Rows are indices into the result
/// data, so sorting or filtering the view does not change what is selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSelection {
    /// `(data row, column)` of the cell clicked first.
    pub anchor: (usize, usize),
    /// `(data row, column)` of the cell reached last.
    pub focus: (usize, usize),
}

/// Position of each selected cell in the displayed grid: rows as positions in
/// `visible`, columns as indices. `None` when the selection is hidden.
pub fn selection_block(
    selection: CellSelection,
    visible: &[usize],
) -> Option<(RangeInclusive<usize>, RangeInclusive<usize>)> {
    let anchor = visible.iter().position(|row| *row == selection.anchor.0)?;
    let focus = visible.iter().position(|row| *row == selection.focus.0)?;
    let (left, right) = (selection.anchor.1, selection.focus.1);
    Some((
        anchor.min(focus)..=anchor.max(focus),
        left.min(right)..=left.max(right),
    ))
}

/// What the grid shows for one result, filter, sort and number format. It is
/// rebuilt only when one of them changes, not on every frame, so scrolling,
/// hovering and selecting cost only the rows on screen.
pub struct GridView {
    pub key: GridKey,
    /// Data rows in display order, after the open filter and the sort.
    pub rows: Vec<usize>,
    /// Column widths in pixels, from the header and the first 200 rows.
    pub widths: Vec<f32>,
    pub families: Vec<ValueFamily>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GridKey {
    /// Changes with every new result.
    pub result: u64,
    /// Lowercase text of the open filter; empty when the bar is closed.
    pub filter: String,
    pub descending: bool,
    pub numbers_pt_br: bool,
}

impl GridView {
    pub fn new(result: &ResultData, key: GridKey) -> Self {
        // NULL matches as the text it shows, so a filter for "null" finds it.
        let mut rows: Vec<_> = (0..result.row_count())
            .filter(|row| {
                key.filter.is_empty()
                    || result
                        .row(*row)
                        .any(|cell| cell.unwrap_or("NULL").to_lowercase().contains(&key.filter))
            })
            .collect();
        if !key.descending {
            rows.reverse();
        }
        let families: Vec<_> = result
            .columns()
            .iter()
            .map(|column| cell_format::family(&column.data_type))
            .collect();
        // Widths follow the first rows only, so a long result sizes as fast
        // as a short one.
        let widths = result
            .columns()
            .iter()
            .zip(&families)
            .enumerate()
            .map(|(column, (info, family))| {
                let (name, kind) = (&info.name, &info.data_type);
                let content = (0..result.row_count().min(200))
                    .map(|row| {
                        cell_format::display(result.value(row, column), *family, key.numbers_pt_br)
                            .chars()
                            .count()
                    })
                    .max()
                    .unwrap_or(0);
                let header = name.chars().count() + 3 + kind.chars().count();
                ((content.max(header) as f32 * 8.) + 32.).clamp(110., 360.)
            })
            .collect();
        Self {
            key,
            rows,
            widths,
            families,
        }
    }
}

/// The last [`GridView`], reused while its key is unchanged.
#[derive(Default)]
pub struct GridCache(RefCell<Option<Rc<GridView>>>);

impl GridCache {
    pub fn get(&self, result: &ResultData, key: GridKey) -> Rc<GridView> {
        let mut cached = self.0.borrow_mut();
        match cached.as_ref() {
            Some(grid) if grid.key == key => grid.clone(),
            _ => {
                // Rebuilt on the UI thread once per change, never per frame.
                let _timer = crate::logging::UiTimer::start("grid view");
                cached.insert(Rc::new(GridView::new(result, key))).clone()
            }
        }
    }
}

impl CellSelection {
    /// A single cell.
    pub fn cell(row: usize, column: usize) -> Self {
        Self {
            anchor: (row, column),
            focus: (row, column),
        }
    }

    /// A click on a cell: a new single cell, or with `extend` the block from
    /// the current anchor to it.
    pub fn click(current: Option<Self>, row: usize, column: usize, extend: bool) -> Self {
        match current {
            Some(selection) if extend => Self {
                anchor: selection.anchor,
                focus: (row, column),
            },
            _ => Self::cell(row, column),
        }
    }

    /// A click on a row number: the whole row, or with `extend` every row
    /// from the anchor's row to it.
    pub fn click_row(current: Option<Self>, row: usize, columns: usize, extend: bool) -> Self {
        let last = columns.saturating_sub(1);
        match current {
            Some(selection) if extend => Self {
                anchor: (selection.anchor.0, 0),
                focus: (row, last),
            },
            _ => Self {
                anchor: (row, 0),
                focus: (row, last),
            },
        }
    }

    /// Every visible cell, from the first displayed row to the last.
    pub fn all(visible: &[usize], columns: usize) -> Option<Self> {
        let (first, last) = (visible.first()?, visible.last()?);
        (columns > 0).then(|| Self {
            anchor: (*first, 0),
            focus: (*last, columns - 1),
        })
    }

    /// Arrow-key navigation over the displayed rows; with `extend` the block
    /// grows from the anchor. Returns the new selection and the display
    /// position of its focus, to scroll it into view.
    pub fn moved(
        current: Option<Self>,
        visible: &[usize],
        columns: usize,
        rows: isize,
        cols: isize,
        extend: bool,
    ) -> Option<(Self, usize)> {
        if visible.is_empty() || columns == 0 {
            return None;
        }
        let (position, col) = current
            .and_then(|selection| {
                let position = visible.iter().position(|row| *row == selection.focus.0)?;
                Some((position, selection.focus.1))
            })
            .unwrap_or((0, 0));
        let position = position.saturating_add_signed(rows).min(visible.len() - 1);
        let col = col.saturating_add_signed(cols).min(columns - 1);
        let row = *visible.get(position)?;
        Some((Self::click(current, row, col, extend), position))
    }
}

/// Number of cells in the visible part of a selection.
pub fn selected_cells(selection: Option<CellSelection>, visible: &[usize]) -> usize {
    selection
        .and_then(|selection| selection_block(selection, visible))
        .map_or(0, |(rows, cols)| rows.count() * cols.count())
}

/// The selected block as tab-separated text (a single cell as its exact
/// value), optionally with the column names first, and its number of cells.
/// `None` when nothing visible is selected.
pub fn copy_selection(
    result: &ResultData,
    visible: &[usize],
    selection: Option<CellSelection>,
    headers: bool,
) -> Option<(String, usize)> {
    let (rows, cols) = selection_block(selection?, visible)?;
    let names: Vec<&str> = cols
        .clone()
        .filter_map(|col| result.columns().get(col))
        .map(|column| column.name.as_str())
        .collect();
    let data: Vec<Vec<Option<&str>>> = rows
        .filter_map(|position| visible.get(position))
        .map(|row| cols.clone().map(|col| result.value(*row, col)).collect())
        .collect();
    let count = data.len() * names.len();
    let text = cell_format::copy_cells(headers.then_some(names.as_slice()), &data);
    Some((text, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ResultColumn;
    use std::time::{Duration, Instant};

    #[test]
    fn selection_follows_the_displayed_order_and_hides_with_its_rows() {
        let selection = CellSelection {
            anchor: (4, 2),
            focus: (1, 0),
        };
        // Rows shown in reverse order: data rows 4..1 sit at positions 1..=4.
        let visible = [5, 4, 3, 2, 1, 0];
        assert_eq!(selection_block(selection, &visible), Some((1..=4, 0..=2)));
        // A filter that hides an end of the block hides the selection.
        assert_eq!(selection_block(selection, &[5, 4, 3]), None);
    }
    /// Shaped like the `sales.event_log` seed, without a database.
    fn event_log_rows(rows: usize) -> Vec<Vec<String>> {
        (0..rows)
            .map(|row| {
                vec![
                    row.to_string(),
                    ["debug", "info", "warn", "error"][row % 4].into(),
                    format!("Synthetic event {row}"),
                    format!("2026-01-01 00:{:02}:00+00", row % 60),
                    format!("{{\"seq\": {row}}}"),
                ]
            })
            .collect()
    }

    fn table(rows: &[Vec<String>]) -> ResultData {
        let columns = [
            ("id", "int8"),
            ("level", "text"),
            ("message", "text"),
            ("occurred_at", "timestamptz"),
            ("payload", "jsonb"),
        ]
        .into_iter()
        .map(|(name, kind)| ResultColumn::new(name, kind))
        .collect();
        ResultData::from_rows(
            columns,
            rows.iter()
                .map(|row| row.iter().map(|cell| Some(cell.as_str()))),
        )
    }

    fn event_log(rows: usize) -> ResultData {
        table(&event_log_rows(rows))
    }

    fn key(result: u64, filter: &str, descending: bool) -> GridKey {
        GridKey {
            result,
            filter: filter.into(),
            descending,
            numbers_pt_br: false,
        }
    }

    #[test]
    fn grid_view_filters_sorts_and_sizes_columns_from_the_first_rows() {
        let mut rows = event_log_rows(300);
        rows[1][1] = "ERROR".into();
        let result = table(&rows);
        let grid = GridView::new(&result, key(0, "", true));
        assert_eq!(grid.rows, (0..300).collect::<Vec<_>>());
        assert_eq!(
            grid.families,
            [
                ValueFamily::Number,
                ValueFamily::Text,
                ValueFamily::Text,
                ValueFamily::Temporal,
                ValueFamily::Text,
            ]
        );
        // The filter ignores case; ascending order reverses the rows.
        let grid = GridView::new(&result, key(0, "error", false));
        assert_eq!(grid.rows.len(), 76);
        assert_eq!(grid.rows.first(), Some(&299));
        assert_eq!(grid.rows.last(), Some(&1));
        // Only the first 200 rows size a column, so the cost does not grow
        // with the result.
        let width = GridView::new(&result, key(0, "", true)).widths[2];
        rows[250][2] = "x".repeat(80);
        assert_eq!(
            GridView::new(&table(&rows), key(0, "", true)).widths[2],
            width
        );
        rows[10][2] = "x".repeat(80);
        assert!(GridView::new(&table(&rows), key(0, "", true)).widths[2] > width);
    }

    #[test]
    fn grid_view_is_rebuilt_only_when_its_inputs_change() {
        let result = event_log(10);
        let cache = GridCache::default();
        let first = cache.get(&result, key(1, "", true));
        // Hover, scroll and selection frames change none of the inputs.
        assert!(Rc::ptr_eq(&first, &cache.get(&result, key(1, "", true))));
        for changed in [
            key(2, "", true),
            key(1, "info", true),
            key(1, "", false),
            GridKey {
                numbers_pt_br: true,
                ..key(1, "", true)
            },
        ] {
            let rebuilt = cache.get(&result, changed.clone());
            assert!(!Rc::ptr_eq(&first, &rebuilt));
            assert_eq!(rebuilt.key, changed);
        }
    }

    #[test]
    fn a_large_result_builds_its_grid_view_within_budget() {
        // A hundred times the default row limit, filtered and reversed: the
        // slowest build, paid once per change rather than once per frame.
        let result = event_log(100_000);
        let cache = GridCache::default();
        let start = Instant::now();
        let grid = cache.get(&result, key(1, "error", false));
        let elapsed = start.elapsed();
        assert_eq!(grid.rows.len(), 25_000);
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn clicks_extend_from_the_anchor_and_row_numbers_select_whole_rows() {
        let first = CellSelection::click(None, 3, 1, true);
        assert_eq!(first, CellSelection::cell(3, 1));
        let block = CellSelection::click(Some(first), 5, 0, true);
        assert_eq!(block.anchor, (3, 1));
        assert_eq!(block.focus, (5, 0));
        assert_eq!(
            CellSelection::click(Some(block), 7, 2, false),
            CellSelection::cell(7, 2)
        );
        let row = CellSelection::click_row(Some(block), 9, 4, false);
        assert_eq!((row.anchor, row.focus), ((9, 0), (9, 3)));
        let rows = CellSelection::click_row(Some(block), 9, 4, true);
        assert_eq!((rows.anchor, rows.focus), ((3, 0), (9, 3)));
    }

    #[test]
    fn select_all_and_arrows_follow_the_displayed_rows() {
        let visible = [4, 2, 0];
        let all = CellSelection::all(&visible, 3).expect("cells");
        assert_eq!((all.anchor, all.focus), ((4, 0), (0, 2)));
        assert_eq!(selected_cells(Some(all), &visible), 9);
        assert_eq!(CellSelection::all(&[], 3), None);
        assert_eq!(CellSelection::all(&visible, 0), None);
        // Without a selection, arrows start at the first displayed cell.
        let (down, position) = CellSelection::moved(None, &visible, 3, 1, 0, true).expect("a cell");
        assert_eq!((down, position), (CellSelection::cell(2, 0), 1));
        // Shift grows the block; moves stop at the edges.
        let (grown, position) =
            CellSelection::moved(Some(down), &visible, 3, 5, 9, true).expect("a cell");
        assert_eq!((grown.anchor, grown.focus, position), ((2, 0), (0, 2), 2));
        assert_eq!(CellSelection::moved(None, &[], 3, 1, 0, false), None);
    }

    #[test]
    fn copies_the_visible_block_with_or_without_headers() {
        let result = ResultData::from_rows(
            vec![
                ResultColumn::new("id", "int8"),
                ResultColumn::new("note", "text"),
            ],
            [vec![Some("1"), Some("a\tb")], vec![Some("2"), None]],
        );
        let visible = [1, 0];
        let block = CellSelection {
            anchor: (1, 0),
            focus: (0, 1),
        };
        assert_eq!(
            copy_selection(&result, &visible, Some(block), true),
            Some(("id\tnote\n2\t\n1\t\"a\tb\"".to_owned(), 4))
        );
        // A single cell copies its exact value.
        let cell = CellSelection::cell(0, 1);
        assert_eq!(
            copy_selection(&result, &visible, Some(cell), false),
            Some(("a\tb".to_owned(), 1))
        );
        assert_eq!(copy_selection(&result, &visible, None, false), None);
        // A selection whose rows are hidden copies nothing.
        assert_eq!(copy_selection(&result, &[1], Some(block), false), None);
    }
}
