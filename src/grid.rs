//! What the result grid shows, computed without a window.
//!
//! [`GridView`] is the grid's derived data for one result, filter, sort and
//! number format: the rows in display order, column widths and value
//! families. [`GridCache`] rebuilds it only when one of those inputs changes,
//! so hover, scroll and selection frames never pay for it. Selections address
//! data rows, so filtering or sorting does not change what is selected.
use crate::cell_format::{self, ValueFamily};
use crate::decimal::compare_numbers;
use crate::result::ResultData;
use std::{cell::RefCell, ops::RangeInclusive, sync::Arc};

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
    /// The column the rows are sorted by; `None` keeps the server's order.
    pub sort: Option<ColumnSort>,
    pub numbers_pt_br: bool,
}

/// Rows sorted by the values of one column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColumnSort {
    pub column: usize,
    pub descending: bool,
}

impl ColumnSort {
    /// The sort after a click on the header of `column`: ascending first,
    /// then descending, then the server's order again. Another column
    /// starts ascending.
    pub fn next(current: Option<Self>, column: usize) -> Option<Self> {
        match current {
            Some(sort) if sort.column == column && !sort.descending => Some(Self {
                column,
                descending: true,
            }),
            Some(sort) if sort.column == column => None,
            _ => Some(Self {
                column,
                descending: false,
            }),
        }
    }
}

/// Orders two values of a column by its family. NULLs always come last,
/// whichever the direction, as in the databases' default `NULLS LAST` for
/// ascending order.
fn compare_values(
    left: Option<&str>,
    right: Option<&str>,
    family: ValueFamily,
    descending: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => {
            let order = if family == ValueFamily::Number {
                compare_numbers(left, right)
            } else {
                left.cmp(right)
            };
            if descending { order.reverse() } else { order }
        }
    }
}

impl GridView {
    pub fn new(result: &ResultData, key: GridKey) -> Self {
        // The filter matches values, not how the grid draws them: a NULL has
        // no text, so it never matches (not even a filter for "null"), while
        // the text 'NULL' does.
        let mut rows: Vec<_> = (0..result.row_count())
            .filter(|row| {
                key.filter.is_empty()
                    || result.row(*row).any(|cell| {
                        cell.is_some_and(|cell| cell.to_lowercase().contains(&key.filter))
                    })
            })
            .collect();
        let families: Vec<_> = result
            .columns()
            .iter()
            .map(|column| cell_format::family(&column.data_type))
            .collect();
        // A stable sort: equal values keep the server's order.
        if let Some(sort) = key.sort
            && let Some(family) = families.get(sort.column).copied()
        {
            rows.sort_by(|left, right| {
                compare_values(
                    result.value(*left, sort.column),
                    result.value(*right, sort.column),
                    family,
                    sort.descending,
                )
            });
        }
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
pub struct GridCache(RefCell<Option<Arc<GridView>>>);

impl GridCache {
    pub fn get(&self, result: &ResultData, key: GridKey) -> Arc<GridView> {
        let mut cached = self.0.borrow_mut();
        match cached.as_ref() {
            Some(grid) if grid.key == key => grid.clone(),
            _ => {
                // Rebuilt on the UI thread once per change, never per frame.
                let _timer = crate::logging::UiTimer::start("grid view");
                cached.insert(Arc::new(GridView::new(result, key))).clone()
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

    /// A `Ctrl` click on a column's header: every displayed row of that
    /// column, or with `extend` every column from the anchor's to it.
    pub fn click_column(
        current: Option<Self>,
        visible: &[usize],
        column: usize,
        extend: bool,
    ) -> Option<Self> {
        let (first, last) = (*visible.first()?, *visible.last()?);
        let from = match current {
            Some(selection) if extend => selection.anchor.1,
            _ => column,
        };
        Some(Self {
            anchor: (first, from),
            focus: (last, column),
        })
    }

    /// A drag from the anchor reached the cell at `(row, column)`: the block
    /// from the anchor to it, or `None` when the focus is already there, so
    /// a pointer moving inside one cell redraws nothing.
    pub fn dragged(current: Option<Self>, row: usize, column: usize) -> Option<Self> {
        let current = current?;
        (current.focus != (row, column)).then_some(Self {
            anchor: current.anchor,
            focus: (row, column),
        })
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

    /// Navigation by more than one cell: `Home` and `End` along the row,
    /// `Ctrl Home` and `Ctrl End` to the first and last cell of the grid,
    /// `Page Up` and `Page Down` by a screen of rows. With `extend` the
    /// block grows from the anchor, as with the arrows.
    pub fn jumped(
        current: Option<Self>,
        visible: &[usize],
        columns: usize,
        jump: Jump,
        extend: bool,
    ) -> Option<(Self, usize)> {
        let rows = visible.len() as isize;
        let cols = columns as isize;
        let (down, right) = match jump {
            Jump::RowStart => (0, -cols),
            Jump::RowEnd => (0, cols),
            Jump::First => (-rows, -cols),
            Jump::Last => (rows, cols),
            Jump::Rows(delta) => (delta, 0),
        };
        Self::moved(current, visible, columns, down, right, extend)
    }
}

/// A move of [`CellSelection::jumped`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Jump {
    /// The first column of the row (`Home`).
    RowStart,
    /// The last column of the row (`End`).
    RowEnd,
    /// The first cell of the grid (`Ctrl Home`).
    First,
    /// The last cell of the grid (`Ctrl End`).
    Last,
    /// This many rows down, or up when negative (`Page Down`, `Page Up`).
    Rows(isize),
}

/// The narrowest and widest a column can be dragged or fitted to.
pub const MIN_COLUMN_WIDTH: f32 = 48.;
pub const MAX_COLUMN_WIDTH: f32 = 1600.;

/// Rows a fitted column measures, so fitting a long page stays instant.
pub const FIT_ROWS: usize = 5_000;

/// The width of a column dragged by `delta` pixels from `start`.
pub fn resized_width(start: f32, delta: f32) -> f32 {
    (start + delta).clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH)
}

/// The width that shows a column's header and the values of the first
/// [`FIT_ROWS`] displayed rows in full, measured as [`GridView`] measures
/// (8 px per character plus the cell's padding).
pub fn fit_width(
    result: &ResultData,
    visible: &[usize],
    column: usize,
    family: ValueFamily,
    numbers_pt_br: bool,
) -> f32 {
    let Some(info) = result.columns().get(column) else {
        return MIN_COLUMN_WIDTH;
    };
    let header = info.name.chars().count() + 3 + info.data_type.chars().count();
    let content = visible
        .iter()
        .take(FIT_ROWS)
        .map(|row| {
            cell_format::display(result.value(*row, column), family, numbers_pt_br)
                .chars()
                .count()
        })
        .max()
        .unwrap_or(0);
    ((content.max(header) as f32 * 8.) + 32.).clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH)
}

/// How many rows (or columns) a drag past an edge moves per step: none
/// while the pointer is between `start` and `end`, then one more for every
/// 24 px beyond, up to 10.
pub fn edge_step(position: f32, start: f32, end: f32) -> isize {
    let beyond = if position < start {
        -(start - position)
    } else if position > end {
        position - end
    } else {
        return 0;
    };
    let step = (1. + beyond.abs() / 24.).min(10.) as isize;
    if beyond < 0. { -step } else { step }
}

/// The horizontal pan that brings `column` into a viewport `viewport` px
/// wide, scrolled `pan` px, with `lead` px of row numbers before the first
/// column; `None` when the column is already in view.
pub fn pan_to_column(
    widths: &[f32],
    column: usize,
    lead: f32,
    viewport: f32,
    pan: f32,
) -> Option<f32> {
    let left = lead + widths.iter().take(column).sum::<f32>();
    let right = left + widths.get(column).copied()?;
    if left - lead < pan {
        Some(left - lead)
    } else if right > pan + viewport {
        Some((right - viewport).min(left - lead))
    } else {
        None
    }
}

/// The data rows of the selection's visible block, in display order: what
/// the records view shows side by side.
pub fn selected_rows(selection: Option<CellSelection>, visible: &[usize]) -> Vec<usize> {
    selection
        .and_then(|selection| selection_block(selection, visible))
        .map(|(rows, _)| {
            rows.filter_map(|position| visible.get(position).copied())
                .collect()
        })
        .unwrap_or_default()
}

/// Copies above this size ask for confirmation first
/// ([Results](../docs/reference/results.md#copying)).
pub const COPY_CONFIRM_BYTES: usize = 50 * 1024 * 1024;

/// About how many bytes [`copy_selection`] would produce: the text of the
/// selected cells plus a separator after each, and the headers. Quotes
/// added around special values are not counted. It reads only cell lengths,
/// so it costs far less than the copy itself.
pub fn selection_bytes(
    result: &ResultData,
    visible: &[usize],
    selection: Option<CellSelection>,
    headers: bool,
) -> usize {
    let Some((rows, cols)) = selection.and_then(|selection| selection_block(selection, visible))
    else {
        return 0;
    };
    let names = if headers {
        cols.clone()
            .filter_map(|col| result.columns().get(col))
            .map(|column| column.name.len() + 1)
            .sum()
    } else {
        0
    };
    names
        + rows
            .filter_map(|position| visible.get(position))
            .map(|row| {
                cols.clone()
                    .map(|col| result.value(*row, col).map_or(0, str::len) + 1)
                    .sum::<usize>()
            })
            .sum::<usize>()
}

/// About how many bytes [`cell_format::result_csv`] would produce for a
/// page: the headers and every value quoted, with a separator after each.
pub fn csv_bytes(result: &ResultData) -> usize {
    let cells = result.row_count() * result.column_count();
    let headers: usize = result
        .columns()
        .iter()
        .map(|column| column.name.len() + 3)
        .sum();
    headers + result.text_bytes() + cells * 3 + result.row_count()
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
    #[test]
    fn a_selection_survives_filters_that_keep_its_ends() {
        let result = event_log(12);
        // Rows 3 and 7 are "error"; the block covers data rows 3..=7.
        let selection = Some(CellSelection {
            anchor: (3, 1),
            focus: (7, 1),
        });
        let all = GridView::new(&result, key(0, "", None));
        assert_eq!(selected_cells(selection, &all.rows), 5);
        let errors = GridView::new(&result, key(0, "error", None));
        assert_eq!(errors.rows, [3, 7, 11]);
        assert_eq!(selected_cells(selection, &errors.rows), 2);
        // Hiding an end hides the block, which comes back with the rows.
        let info = GridView::new(&result, key(0, "info", None));
        assert_eq!(selected_cells(selection, &info.rows), 0);
        assert_eq!(selected_cells(selection, &all.rows), 5);
    }

    #[test]
    fn the_filter_matches_values_and_never_a_null() {
        let columns = vec![ResultColumn::new("note", "text")];
        let result = ResultData::from_rows(
            columns,
            [Some("NULL"), None, Some("nullable"), Some("other")]
                .into_iter()
                .map(std::iter::once),
        );
        let rows = |filter: &str| GridView::new(&result, key(0, filter, None)).rows;
        // The text 'NULL' and 'nullable' match; the real NULL does not.
        assert_eq!(rows("null"), [0, 2]);
        assert_eq!(rows("ull"), [0, 2]);
        assert_eq!(rows(""), [0, 1, 2, 3]);
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

    fn key(result: u64, filter: &str, sort: Option<(usize, bool)>) -> GridKey {
        GridKey {
            result,
            filter: filter.into(),
            sort: sort.map(|(column, descending)| ColumnSort { column, descending }),
            numbers_pt_br: false,
        }
    }

    #[test]
    fn grid_view_filters_sorts_and_sizes_columns_from_the_first_rows() {
        let mut rows = event_log_rows(300);
        rows[1][1] = "ERROR".into();
        let result = table(&rows);
        let grid = GridView::new(&result, key(0, "", None));
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
        // The filter ignores case; sorting by the id column descending
        // reverses the rows.
        let grid = GridView::new(&result, key(0, "error", Some((0, true))));
        assert_eq!(grid.rows.len(), 76);
        assert_eq!(grid.rows.first(), Some(&299));
        assert_eq!(grid.rows.last(), Some(&1));
        // Only the first 200 rows size a column, so the cost does not grow
        // with the result.
        let width = GridView::new(&result, key(0, "", None)).widths[2];
        rows[250][2] = "x".repeat(80);
        assert_eq!(
            GridView::new(&table(&rows), key(0, "", None)).widths[2],
            width
        );
        rows[10][2] = "x".repeat(80);
        assert!(GridView::new(&table(&rows), key(0, "", None)).widths[2] > width);
    }

    #[test]
    fn grid_view_is_rebuilt_only_when_its_inputs_change() {
        let result = event_log(10);
        let cache = GridCache::default();
        let first = cache.get(&result, key(1, "", None));
        // Hover, scroll and selection frames change none of the inputs.
        assert!(Arc::ptr_eq(&first, &cache.get(&result, key(1, "", None))));
        for changed in [
            key(2, "", None),
            key(1, "info", None),
            key(1, "", Some((0, true))),
            GridKey {
                numbers_pt_br: true,
                ..key(1, "", None)
            },
        ] {
            let rebuilt = cache.get(&result, changed.clone());
            assert!(!Arc::ptr_eq(&first, &rebuilt));
            assert_eq!(rebuilt.key, changed);
        }
    }

    #[test]
    fn a_large_result_builds_its_grid_view_within_budget() {
        // A hundred times the default row limit, filtered and sorted by text: the
        // slowest build, paid once per change rather than once per frame.
        let result = event_log(100_000);
        let cache = GridCache::default();
        let start = Instant::now();
        let grid = cache.get(&result, key(1, "error", Some((2, true))));
        let elapsed = start.elapsed();
        assert_eq!(grid.rows.len(), 25_000);
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn a_header_sorts_by_the_values_of_its_column() {
        let result = ResultData::from_rows(
            vec![
                ResultColumn::new("amount", "numeric"),
                ResultColumn::new("name", "text"),
            ],
            [
                [Some("10.5"), Some("b")],
                [None, Some("a")],
                [Some("-2"), Some("c")],
                [Some("9.75"), None],
                [Some("100"), Some("a")],
                [Some("-0.0"), Some("d")],
                [Some("12345678901234567890.1"), Some("e")],
            ],
        );
        let rows = |sort| GridView::new(&result, key(0, "", sort)).rows;
        // Exact numbers, not text order ("100" before "9.75") nor floats;
        // NULL last in both directions.
        assert_eq!(rows(Some((0, false))), [2, 5, 3, 0, 4, 6, 1]);
        assert_eq!(rows(Some((0, true))), [6, 4, 0, 3, 5, 2, 1]);
        // Text by its value; equal values keep the server's order.
        assert_eq!(rows(Some((1, false))), [1, 4, 0, 2, 5, 6, 3]);
        assert_eq!(rows(None), [0, 1, 2, 3, 4, 5, 6]);
        // A click sorts ascending, then descending, then not at all.
        let ascending = ColumnSort::next(None, 1);
        assert_eq!(
            ascending,
            Some(ColumnSort {
                column: 1,
                descending: false
            })
        );
        let descending = ColumnSort::next(ascending, 1);
        assert_eq!(descending.map(|sort| sort.descending), Some(true));
        assert_eq!(ColumnSort::next(descending, 1), None);
        assert_eq!(
            ColumnSort::next(descending, 0).map(|sort| sort.column),
            Some(0)
        );
        use std::cmp::Ordering::{Equal, Greater, Less};
        assert_eq!(compare_numbers("0", "-0"), Equal);
        assert_eq!(compare_numbers("1e5", "2"), Greater);
        // Exponent forms, as float8 prints very small and large values, sort
        // by value among the plain decimals, still without floating point.
        assert_eq!(compare_numbers("1.5e-05", "0.0001"), Less);
        assert_eq!(compare_numbers("1.5e-05", "0.000015"), Equal);
        assert_eq!(compare_numbers("1e+20", "99999999999999999999"), Greater);
        assert_eq!(compare_numbers("10", "1E1"), Equal);
        assert_eq!(compare_numbers("1.50", "1.5"), Equal);
        assert_eq!(compare_numbers("-1e-3", "-0.01"), Greater);
        assert_eq!(compare_numbers("-1e-3", "0"), Less);
        assert_eq!(compare_numbers("0.05", "0.5"), Less);
        // What is not a decimal comes after every number.
        assert_eq!(compare_numbers("NaN", "1e300"), Greater);
        assert_eq!(compare_numbers("1e", "1"), Greater);
    }

    #[test]
    fn sorting_a_large_result_stays_within_budget() {
        let result = event_log(100_000);
        let start = Instant::now();
        let grid = GridView::new(&result, key(1, "", Some((0, true))));
        let elapsed = start.elapsed();
        assert_eq!(grid.rows.first(), Some(&99_999));
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn home_end_and_pages_move_along_the_displayed_rows() {
        let visible = [4, 3, 2, 1, 0];
        let start = Some(CellSelection::cell(3, 1));
        let jump =
            |jump, extend| CellSelection::jumped(start, &visible, 3, jump, extend).expect("a cell");
        assert_eq!(jump(Jump::RowStart, false), (CellSelection::cell(3, 0), 1));
        assert_eq!(jump(Jump::RowEnd, false), (CellSelection::cell(3, 2), 1));
        assert_eq!(jump(Jump::First, false), (CellSelection::cell(4, 0), 0));
        assert_eq!(jump(Jump::Last, false), (CellSelection::cell(0, 2), 4));
        assert_eq!(jump(Jump::Rows(2), false), (CellSelection::cell(1, 1), 3));
        assert_eq!(jump(Jump::Rows(-20), false), (CellSelection::cell(4, 1), 0));
        // Shift extends the block from the anchor.
        let (block, _) = jump(Jump::Last, true);
        assert_eq!((block.anchor, block.focus), ((3, 1), (0, 2)));
        assert_eq!(CellSelection::jumped(None, &[], 3, Jump::Last, false), None);
    }

    #[test]
    fn copy_sizes_are_estimated_from_the_cell_lengths() {
        let result = ResultData::from_rows(
            vec![
                ResultColumn::new("id", "int8"),
                ResultColumn::new("note", "text"),
            ],
            [vec![Some("1"), Some("abc")], vec![Some("22"), None]],
        );
        let all = CellSelection::all(&[0, 1], 2);
        // "1\tabc\n22\t" is 9 bytes; the estimate counts a separator per cell.
        assert_eq!(selection_bytes(&result, &[0, 1], all, false), 10);
        assert_eq!(selection_bytes(&result, &[0, 1], all, true), 18);
        assert_eq!(selection_bytes(&result, &[0, 1], None, true), 0);
        let csv = cell_format::result_csv(&result).len();
        assert!(csv_bytes(&result) >= csv, "{csv}");
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

    #[test]
    fn a_drag_extends_from_the_anchor_and_ignores_moves_inside_a_cell() {
        let start = CellSelection::cell(2, 1);
        let block = CellSelection::dragged(Some(start), 5, 3).expect("a block");
        assert_eq!((block.anchor, block.focus), ((2, 1), (5, 3)));
        assert_eq!(CellSelection::dragged(Some(block), 5, 3), None);
        assert_eq!(CellSelection::dragged(None, 5, 3), None);
    }

    #[test]
    fn a_header_selects_its_column_over_the_displayed_rows() {
        let visible = [4, 2, 0];
        let column = CellSelection::click_column(None, &visible, 1, false).expect("a column");
        assert_eq!((column.anchor, column.focus), ((4, 1), (0, 1)));
        assert_eq!(selected_cells(Some(column), &visible), 3);
        let columns =
            CellSelection::click_column(Some(column), &visible, 3, true).expect("columns");
        assert_eq!((columns.anchor, columns.focus), ((4, 1), (0, 3)));
        assert_eq!(CellSelection::click_column(None, &[], 1, false), None);
    }

    #[test]
    fn columns_resize_within_bounds_and_fit_their_values() {
        assert_eq!(resized_width(120., 30.), 150.);
        assert_eq!(resized_width(120., -500.), MIN_COLUMN_WIDTH);
        assert_eq!(resized_width(120., 5_000.), MAX_COLUMN_WIDTH);
        let result = ResultData::from_rows(
            vec![ResultColumn::new("id", "int8")],
            [
                [Some("1")],
                [Some(
                    "123456789012345678901234567890123456789012345678901234567890",
                )],
            ],
        );
        // The long value fits, past the 360 px the grid starts with.
        assert_eq!(
            fit_width(&result, &[0, 1], 0, ValueFamily::Number, false),
            60. * 8. + 32.
        );
        // Only displayed rows count; the header is the minimum.
        assert_eq!(
            fit_width(&result, &[0], 0, ValueFamily::Number, false),
            9. * 8. + 32.
        );
    }

    #[test]
    fn a_drag_past_an_edge_steps_faster_the_further_it_goes() {
        assert_eq!(edge_step(50., 0., 100.), 0);
        assert_eq!(edge_step(-1., 0., 100.), -1);
        assert_eq!(edge_step(130., 0., 100.), 2);
        assert_eq!(edge_step(10_000., 0., 100.), 10);
    }

    #[test]
    fn panning_brings_a_column_into_view() {
        let widths = [100., 100., 100., 100.];
        // A 250 px viewport after 35 px of row numbers, not scrolled.
        assert_eq!(pan_to_column(&widths, 0, 35., 250., 0.), None);
        assert_eq!(pan_to_column(&widths, 2, 35., 250., 0.), Some(85.));
        assert_eq!(pan_to_column(&widths, 0, 35., 250., 85.), Some(0.));
        assert_eq!(pan_to_column(&widths, 9, 35., 250., 0.), None);
    }

    #[test]
    fn records_are_the_selected_rows_in_display_order() {
        let visible = [4, 2, 0, 1];
        let block = CellSelection {
            anchor: (2, 1),
            focus: (1, 0),
        };
        assert_eq!(selected_rows(Some(block), &visible), [2, 0, 1]);
        assert!(selected_rows(None, &visible).is_empty());
    }
}
