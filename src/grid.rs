//! What the result grid shows, computed without a window.
//!
//! [`GridView`] is the grid's derived data for one result, filter, sort and
//! number format: the rows in display order, column widths and value
//! families. [`GridCache`] rebuilds it only when one of those inputs changes,
//! so hover, scroll and selection frames never pay for it. Selections address
//! data rows and result columns, so filtering, sorting or laying out the
//! columns ([`ColumnLayout`]) does not change what is selected.
use crate::cell_format::{self, ValueFamily};
use crate::column_layout::{ColumnLayout, ColumnWidths};
use crate::decimal::compare_numbers;
use crate::result::ResultData;
use std::collections::HashMap;
use std::rc::Rc;
use std::{cell::RefCell, ops::RangeInclusive, sync::Arc};

/// A rectangular block of result cells, from the cell clicked first (anchor)
/// to the one reached with Shift (focus). Rows are indices into the result
/// data and columns into the result's columns, so sorting or filtering the
/// view, or moving its columns, does not change what is selected; the block
/// spans the display positions between its ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSelection {
    /// `(data row, column)` of the cell clicked first.
    pub anchor: (usize, usize),
    /// `(data row, column)` of the cell reached last.
    pub focus: (usize, usize),
}

/// Position of each selected cell in the displayed grid: rows as positions in
/// `visible`, columns as display positions of `layout` (its
/// [`ColumnLayout::column`] gives the result column). `None` when the
/// selection is hidden: an end's row is filtered out or its column hidden.
pub fn selection_block(
    selection: CellSelection,
    visible: &[usize],
    layout: &ColumnLayout,
) -> Option<(RangeInclusive<usize>, RangeInclusive<usize>)> {
    let anchor = visible.iter().position(|row| *row == selection.anchor.0)?;
    let focus = visible.iter().position(|row| *row == selection.focus.0)?;
    let left = layout.position(selection.anchor.1)?;
    let right = layout.position(selection.focus.1)?;
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
    /// The version of the pending edits drawn over the result
    /// ([`crate::grid_edit`]), so a change to them is drawn.
    pub edits: u64,
    /// Rows the edits insert: they follow the result's rows, as data rows
    /// `row_count..row_count + inserted`, unfiltered and unsorted.
    pub inserted: usize,
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
        let families: Vec<_> = result
            .columns()
            .iter()
            .map(|column| cell_format::family(&column.data_type))
            .collect();
        // The filter matches the text the cell draws (a boolean `t` is
        // "true"), not the stored characters.
        let mut rows: Vec<_> = (0..result.row_count())
            .filter(|row| {
                key.filter.is_empty()
                    || result.row(*row).enumerate().any(|(column, cell)| {
                        shown_matches(
                            cell,
                            families.get(column).copied().unwrap_or(ValueFamily::Text),
                            key.numbers_pt_br,
                            &key.filter,
                        )
                    })
            })
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
        // Inserted rows have no values to filter or sort by: they stay last,
        // in the order they were added.
        rows.extend(result.row_count()..result.row_count() + key.inserted);
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

/// Whether the result search marks matches or shows only the rows that match.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FindMode {
    /// Every row stays. Matching text is marked, and the footer counts the
    /// rows that contain the text.
    #[default]
    Highlight,
    /// Only the rows that contain the text stay.
    Filter,
}

/// How a cell draws a search hit on the text it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellMark {
    None,
    /// Byte ranges in the displayed text.
    Ranges(Vec<std::ops::Range<usize>>),
    /// The displayed text matches, and its characters do not line up with a
    /// range (a case fold that splits one character into several).
    Whole,
}

/// Whether the text a cell paints contains `needle`. `needle` is already
/// lower case. An empty needle matches nothing. NULL is the word NULL, a
/// boolean is `true` or `false`, and a pt-BR number is the grouped text.
pub fn shown_matches(
    value: Option<&str>,
    family: ValueFamily,
    numbers_pt_br: bool,
    needle: &str,
) -> bool {
    if needle.is_empty() {
        return false;
    }
    if redraws(value, family, numbers_pt_br) {
        return cell_format::shown(value, family, numbers_pt_br)
            .to_lowercase()
            .contains(needle);
    }
    value.is_some_and(|value| value.to_lowercase().contains(needle))
}

/// Values whose painted text is not their stored characters, ignoring case.
fn redraws(value: Option<&str>, family: ValueFamily, numbers_pt_br: bool) -> bool {
    let Some(value) = value else {
        return true;
    };
    if value.is_empty() || value.contains(['\n', '\r']) || value.chars().all(char::is_whitespace) {
        return true;
    }
    match family {
        ValueFamily::Number if numbers_pt_br => true,
        ValueFamily::Boolean => {
            matches!(value.to_ascii_lowercase().as_str(), "t" | "1" | "f" | "0")
        }
        ValueFamily::Binary => value.starts_with("\\x"),
        _ => false,
    }
}

/// The mark for the text a cell paints. `needle` is already lower case.
pub fn cell_mark(displayed: &str, needle: &str) -> CellMark {
    if needle.is_empty() || !displayed.to_lowercase().contains(needle) {
        return CellMark::None;
    }
    let ranges = crate::palette::highlights(displayed, &[needle]);
    if ranges.is_empty() {
        CellMark::Whole
    } else {
        CellMark::Ranges(ranges)
    }
}

/// What a settled search found: matching rows, the visible cells that match
/// in display order, and which of those cells is current.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindScan {
    /// Rows that contain the needle in any column, hidden ones included.
    pub rows: usize,
    /// Data rows containing a match, in display order.
    pub matching: Vec<usize>,
    /// False when cancelled or the scan exceeded its time limit.
    pub complete: bool,
    /// Visible matching cells, left to right and top to bottom:
    /// `(data row, result column)`.
    pub hits: Vec<(usize, usize)>,
    /// The hit the focus is on, or the next one after it, wrapping.
    /// `None` when nothing matches.
    pub index: Option<usize>,
}

/// Scans `rows` (display order, data-row indices) for `needle`. `columns` are
/// the visible result columns, in display order: only those are occurrences.
/// A row still counts when the match is in a hidden column. The match is the
/// text the cell paints. `needle` is already lower case.
pub fn scan_find(
    result: &ResultData,
    rows: &[usize],
    columns: &[usize],
    families: &[ValueFamily],
    numbers_pt_br: bool,
    needle: &str,
    focus: Option<(usize, usize)>,
) -> FindScan {
    scan_find_cells(
        FindCells {
            result,
            session: None,
            first_row: 0,
            cancel: None,
        },
        rows,
        columns,
        families,
        numbers_pt_br,
        needle,
        focus,
    )
}

/// The same source used by the grid when painting edited and inserted cells.
#[derive(Clone, Copy)]
pub struct FindCells<'a> {
    pub result: &'a ResultData,
    pub session: Option<&'a crate::grid_edit::EditSession>,
    pub first_row: usize,
    pub cancel: Option<&'a crate::drivers::CancellationToken>,
}

impl FindCells<'_> {
    fn matches(
        self,
        row: usize,
        column: usize,
        family: ValueFamily,
        numbers_pt_br: bool,
        needle: &str,
    ) -> bool {
        let overlay = self.session.and_then(|session| {
            session.cell_overlay(self.first_row, self.result.row_count(), row, column)
        });
        if row >= self.result.row_count() && overlay.is_none() {
            return crate::tr!("DEFAULT").to_lowercase().contains(needle);
        }
        let value = match overlay {
            Some(overlay) => overlay.value().text(),
            None => self.result.value(row, column),
        };
        shown_matches(value, family, numbers_pt_br, needle)
    }
}

/// Scans the painted values, including pending and written edits.
pub fn scan_find_cells(
    cells: FindCells<'_>,
    rows: &[usize],
    columns: &[usize],
    families: &[ValueFamily],
    numbers_pt_br: bool,
    needle: &str,
    focus: Option<(usize, usize)>,
) -> FindScan {
    if needle.is_empty() {
        return FindScan {
            rows: 0,
            matching: Vec::new(),
            complete: true,
            hits: Vec::new(),
            index: None,
        };
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut complete = true;
    let mut matching = Vec::new();
    let mut hits = Vec::new();
    let mut exact = None;
    let mut after = None;
    let mut passed = false;
    for &row in rows {
        if cells.cancel.is_some_and(|cancel| cancel.is_cancelled())
            || std::time::Instant::now() >= deadline
        {
            complete = false;
            break;
        }
        if (0..cells.result.column_count()).any(|column| {
            cells.matches(
                row,
                column,
                families.get(column).copied().unwrap_or(ValueFamily::Text),
                numbers_pt_br,
                needle,
            )
        }) {
            matching.push(row);
        }
        // A focus in a hidden column is already passed: the next visible hit
        // of this row, or of a later row, is the current one.
        if focus.is_some_and(|(focus_row, focus_column)| {
            focus_row == row && !columns.contains(&focus_column)
        }) {
            passed = true;
        }
        for &column in columns {
            let here = focus == Some((row, column));
            if cells.matches(
                row,
                column,
                families.get(column).copied().unwrap_or(ValueFamily::Text),
                numbers_pt_br,
                needle,
            ) {
                let at = hits.len();
                if here {
                    exact = Some(at);
                } else if passed && after.is_none() {
                    after = Some(at);
                }
                hits.push((row, column));
            }
            if here {
                passed = true;
            }
        }
    }
    let index = if hits.is_empty() {
        None
    } else {
        exact.or(after).or(Some(0))
    };
    FindScan {
        complete,
        rows: matching.len(),
        matching,
        hits,
        index,
    }
}

/// The occurrence after `index` (before it when `forward` is false), wrapping.
/// With no current occurrence, the first or the last. `None` when there is
/// nothing to land on.
pub fn step_index(len: usize, index: Option<usize>, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match index {
        None => {
            if forward {
                0
            } else {
                len - 1
            }
        }
        Some(index) => {
            if forward {
                (index + 1) % len
            } else {
                (index + len - 1) % len
            }
        }
    })
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

/// The last [`ColumnWidths`], reused while the grid view, the column
/// layout and the widths the user set are unchanged: a frame reads them and
/// never computes them for every column of the result.
#[derive(Default)]
pub struct WidthsCache(RefCell<Option<CachedWidths>>);

struct CachedWidths {
    view: GridKey,
    layout: u64,
    set: HashMap<usize, f32>,
    widths: Rc<ColumnWidths>,
}

impl WidthsCache {
    /// The widths of `view`'s columns, with `set` (the widths the user set,
    /// by result column) over the measured ones, laid out by `layout`.
    /// `layout_version` changes with every change of `layout`.
    pub fn get(
        &self,
        view: &GridView,
        layout: &ColumnLayout,
        layout_version: u64,
        set: &HashMap<usize, f32>,
    ) -> Rc<ColumnWidths> {
        let mut cached = self.0.borrow_mut();
        if let Some(cached) = cached.as_ref()
            && cached.view == view.key
            && cached.layout == layout_version
            && cached.set == *set
        {
            return cached.widths.clone();
        }
        let by_column = view
            .widths
            .iter()
            .enumerate()
            .map(|(column, width)| set.get(&column).copied().unwrap_or(*width))
            .collect();
        let widths = Rc::new(ColumnWidths::new(by_column, layout, MIN_COLUMN_WIDTH));
        *cached = Some(CachedWidths {
            view: view.key.clone(),
            layout: layout_version,
            set: set.clone(),
            widths: widths.clone(),
        });
        widths
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

    /// A click on a row number: the whole row as shown, or with `extend`
    /// every row from the anchor's row to it.
    pub fn click_row(
        current: Option<Self>,
        row: usize,
        layout: &ColumnLayout,
        extend: bool,
    ) -> Self {
        let first = layout.column(0).unwrap_or(0);
        let last = layout.visible().last().copied().unwrap_or(first);
        match current {
            Some(selection) if extend => Self {
                anchor: (selection.anchor.0, first),
                focus: (row, last),
            },
            _ => Self {
                anchor: (row, first),
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

    /// Every visible cell, from the first displayed row and column to the
    /// last.
    pub fn all(visible: &[usize], layout: &ColumnLayout) -> Option<Self> {
        let (first, last) = (visible.first()?, visible.last()?);
        let (left, right) = (layout.visible().first()?, layout.visible().last()?);
        Some(Self {
            anchor: (*first, *left),
            focus: (*last, *right),
        })
    }

    /// Arrow-key navigation over the displayed rows and columns; with
    /// `extend` the block grows from the anchor. Returns the new selection
    /// and the display position of its focus (row, column), to scroll it
    /// into view.
    pub fn moved(
        current: Option<Self>,
        visible: &[usize],
        layout: &ColumnLayout,
        rows: isize,
        cols: isize,
        extend: bool,
    ) -> Option<(Self, (usize, usize))> {
        let columns = layout.visible().len();
        if visible.is_empty() || columns == 0 {
            return None;
        }
        let (position, col) = current
            .and_then(|selection| {
                let position = visible.iter().position(|row| *row == selection.focus.0)?;
                Some((position, layout.position(selection.focus.1)?))
            })
            .unwrap_or((0, 0));
        let position = position.saturating_add_signed(rows).min(visible.len() - 1);
        let col = col.saturating_add_signed(cols).min(columns - 1);
        let row = *visible.get(position)?;
        let column = layout.column(col)?;
        Some((Self::click(current, row, column, extend), (position, col)))
    }

    /// Navigation by more than one cell: `Home` and `End` along the row,
    /// `Ctrl Home` and `Ctrl End` to the first and last cell of the grid,
    /// `Page Up` and `Page Down` by a screen of rows. With `extend` the
    /// block grows from the anchor, as with the arrows.
    pub fn jumped(
        current: Option<Self>,
        visible: &[usize],
        layout: &ColumnLayout,
        jump: Jump,
        extend: bool,
    ) -> Option<(Self, (usize, usize))> {
        let rows = visible.len() as isize;
        let cols = layout.visible().len() as isize;
        let (down, right) = match jump {
            Jump::RowStart => (0, -cols),
            Jump::RowEnd => (0, cols),
            Jump::First => (-rows, -cols),
            Jump::Last => (rows, cols),
            Jump::Rows(delta) => (delta, 0),
        };
        Self::moved(current, visible, layout, down, right, extend)
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
pub fn selected_rows(
    selection: Option<CellSelection>,
    visible: &[usize],
    layout: &ColumnLayout,
) -> Vec<usize> {
    selection
        .and_then(|selection| selection_block(selection, visible, layout))
        .map(|(rows, _)| {
            rows.filter_map(|position| visible.get(position).copied())
                .collect()
        })
        .unwrap_or_default()
}

/// Copies above this size ask for confirmation first
/// ([Results](../docs/reference/results.md#copying)).
pub const COPY_CONFIRM_BYTES: usize = 50 * 1024 * 1024;

/// Number of cells in the visible part of a selection.
pub fn selected_cells(
    selection: Option<CellSelection>,
    visible: &[usize],
    layout: &ColumnLayout,
) -> usize {
    selection
        .and_then(|selection| selection_block(selection, visible, layout))
        .map_or(0, |(rows, cols)| rows.count() * cols.count())
}

/// The result columns at the display positions `positions` of `layout`, in
/// display order.
pub fn block_columns(layout: &ColumnLayout, positions: RangeInclusive<usize>) -> Vec<usize> {
    positions
        .filter_map(|position| layout.column(position))
        .collect()
}

/// The selected block as tab-separated text (a single cell as its exact
/// value), optionally with the column names first, and its number of cells.
/// Its columns are the shown ones, in display order. `None` when nothing
/// visible is selected.
pub fn copy_selection(
    result: &ResultData,
    visible: &[usize],
    layout: &ColumnLayout,
    selection: Option<CellSelection>,
    headers: bool,
) -> Option<(String, usize)> {
    let (rows, cols) = selection_block(selection?, visible, layout)?;
    let cols = block_columns(layout, cols);
    let names: Vec<&str> = cols
        .iter()
        .filter_map(|col| result.columns().get(*col))
        .map(|column| column.name.as_str())
        .collect();
    let data: Vec<Vec<Option<&str>>> = rows
        .filter_map(|position| visible.get(position))
        .map(|row| cols.iter().map(|col| result.value(*row, *col)).collect())
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
        let layout = ColumnLayout::new(3);
        assert_eq!(
            selection_block(selection, &visible, &layout),
            Some((1..=4, 0..=2))
        );
        // A filter that hides an end of the block hides the selection.
        assert_eq!(selection_block(selection, &[5, 4, 3], &layout), None);
    }

    #[test]
    fn a_selection_spans_the_display_positions_between_its_columns() {
        let selection = CellSelection {
            anchor: (0, 3),
            focus: (1, 1),
        };
        let mut layout = ColumnLayout::new(5);
        // Shown as 3, 0, 1, 2, 4: columns 3 and 1 sit at positions 0 and 2.
        layout.freeze(3);
        assert_eq!(
            selection_block(selection, &[0, 1], &layout),
            Some((0..=1, 0..=2))
        );
        assert_eq!(block_columns(&layout, 0..=2), [3, 0, 1]);
        assert_eq!(selected_cells(Some(selection), &[0, 1], &layout), 6);
        // Hiding a column between the ends narrows the block; hiding an end
        // hides it until the column is shown again.
        layout.hide(0);
        assert_eq!(selected_cells(Some(selection), &[0, 1], &layout), 4);
        layout.hide(3);
        assert_eq!(selection_block(selection, &[0, 1], &layout), None);
        layout.show(3);
        assert_eq!(selected_cells(Some(selection), &[0, 1], &layout), 4);
    }
    #[test]
    fn a_selection_survives_filters_that_keep_its_ends() {
        let result = event_log(12);
        // Rows 3 and 7 are "error"; the block covers data rows 3..=7.
        let selection = Some(CellSelection {
            anchor: (3, 1),
            focus: (7, 1),
        });
        let layout = ColumnLayout::new(result.column_count());
        let all = GridView::new(&result, key(0, "", None));
        assert_eq!(selected_cells(selection, &all.rows, &layout), 5);
        let errors = GridView::new(&result, key(0, "error", None));
        assert_eq!(errors.rows, [3, 7, 11]);
        assert_eq!(selected_cells(selection, &errors.rows, &layout), 2);
        // Hiding an end hides the block, which comes back with the rows.
        let info = GridView::new(&result, key(0, "info", None));
        assert_eq!(selected_cells(selection, &info.rows, &layout), 0);
        assert_eq!(selected_cells(selection, &all.rows, &layout), 5);
    }

    #[test]
    fn the_filter_matches_the_text_on_screen() {
        let columns = vec![ResultColumn::new("note", "text")];
        let result = ResultData::from_rows(
            columns,
            [Some("NULL"), None, Some("nullable"), Some("other")]
                .into_iter()
                .map(std::iter::once),
        );
        let rows = |filter: &str| GridView::new(&result, key(0, filter, None)).rows;
        // NULL is drawn as NULL, so it matches the same text as the word.
        assert_eq!(rows("null"), [0, 1, 2]);
        assert_eq!(rows("ull"), [0, 1, 2]);
        assert_eq!(rows(""), [0, 1, 2, 3]);

        let flags = ResultData::from_rows(
            vec![ResultColumn::new("on", "boolean")],
            [
                Some("t"),
                Some("1"),
                Some("TRUE"),
                Some("f"),
                Some("0"),
                Some("no"),
            ]
            .into_iter()
            .map(std::iter::once),
        );
        let flags = |filter: &str| GridView::new(&flags, key(0, filter, None)).rows;
        // t and 1 are drawn as true; the stored "1" is not on screen.
        assert_eq!(flags("true"), [0, 1, 2]);
        assert_eq!(flags("false"), [3, 4]);
        assert!(flags("1").is_empty());

        let amounts = ResultData::from_rows(
            vec![ResultColumn::new("amount", "numeric")],
            [Some("1234.5")].into_iter().map(std::iter::once),
        );
        let formatted = |filter: &str| {
            GridView::new(
                &amounts,
                GridKey {
                    numbers_pt_br: true,
                    ..key(0, filter, None)
                },
            )
            .rows
        };
        assert_eq!(formatted("1.234"), [0]);
        assert!(formatted("1234").is_empty());
    }

    #[test]
    fn a_search_marks_the_text_on_screen_and_counts_visible_cells() {
        let result = ResultData::from_rows(
            vec![
                ResultColumn::new("a", "text"),
                ResultColumn::new("b", "text"),
            ],
            [
                [Some("Alpha"), Some("beta")],
                [Some("nope"), None],
                [Some("alpha"), Some("ALPHA")],
                [Some("zzz"), Some("zzz")],
            ],
        );
        let rows: Vec<usize> = (0..result.row_count()).collect();
        let columns = [0, 1];
        let families = [ValueFamily::Text, ValueFamily::Text];
        // "alp" hits Alpha, alpha and ALPHA: two rows, three visible cells.
        // The NULL is drawn as NULL, which does not hold "alp".
        let scan = scan_find(
            &result,
            &rows,
            &columns,
            &families,
            false,
            "alp",
            Some((1, 0)),
        );
        assert_eq!(scan.rows, 2);
        assert_eq!(scan.hits, [(0, 0), (2, 0), (2, 1)]);
        // The focus is between the first and the later hits, so the current
        // one is the next.
        assert_eq!(scan.index, Some(1));
        assert_eq!(
            scan_find(
                &result,
                &rows,
                &columns,
                &families,
                false,
                "alp",
                Some((2, 1))
            )
            .index,
            Some(2)
        );
        // Past the last hit, the search wraps to the first.
        assert_eq!(
            scan_find(
                &result,
                &rows,
                &columns,
                &families,
                false,
                "alp",
                Some((3, 0))
            )
            .index,
            Some(0)
        );
        // A hidden column still makes its row count, and is not an occurrence.
        let hidden = scan_find(&result, &rows, &[1], &families, false, "alp", None);
        assert_eq!(hidden.rows, 2);
        assert_eq!(hidden.hits, [(2, 1)]);
        // A boolean stored as t is an occurrence of the word on screen.
        let flags = ResultData::from_rows(
            vec![
                ResultColumn::new("on", "boolean"),
                ResultColumn::new("note", "text"),
            ],
            [[Some("t"), Some("no")], [Some("f"), Some("true story")]],
        );
        let flag_rows: Vec<usize> = (0..flags.row_count()).collect();
        let flag_scan = scan_find(
            &flags,
            &flag_rows,
            &[0, 1],
            &[ValueFamily::Boolean, ValueFamily::Text],
            false,
            "true",
            None,
        );
        assert_eq!(flag_scan.rows, 2);
        assert_eq!(flag_scan.hits, [(0, 0), (1, 1)]);
        assert!(matches!(
            cell_mark("Alpha", "alp"),
            CellMark::Ranges(ranges) if !ranges.is_empty()
        ));
        assert!(matches!(cell_mark("NULL", "null"), CellMark::Ranges(_)));
        assert!(matches!(cell_mark("true", "true"), CellMark::Ranges(_)));
        assert_eq!(cell_mark("1.234,50", "1234"), CellMark::None);
        assert_eq!(
            scan_find(&result, &rows, &columns, &families, false, "", None).hits,
            []
        );
        assert_eq!(step_index(3, Some(2), true), Some(0));
        assert_eq!(step_index(3, Some(0), false), Some(2));
        assert_eq!(step_index(0, None, true), None);
    }

    #[test]
    fn search_follows_edits_inserts_written_values_and_number_format() {
        use crate::drivers::write::{EditableColumn, EditableTable, KeyKind, WriteMode};
        use crate::drivers::{ObjectKind, ObjectPath};
        use crate::grid_edit::{EditSession, EditValue};
        let result = ResultData::from_rows(
            vec![
                ResultColumn::new("id", "integer"),
                ResultColumn::new("flag", "boolean"),
                ResultColumn::new("amount", "numeric"),
            ],
            [[Some("1"), Some("f"), Some("1234")]],
        );
        let mut session = EditSession::new(
            EditableTable {
                table: ObjectPath::default().child(ObjectKind::Table, "items"),
                writes: WriteMode::Direct,
                columns: ["id", "flag", "amount"]
                    .map(|name| {
                        Some(EditableColumn {
                            name: name.into(),
                            declared_type: "text".into(),
                            nullable: true,
                            writable: true,
                            has_default: true,
                            binary: false,
                            can_key: true,
                        })
                    })
                    .to_vec(),
                key: vec![0],
                key_kind: KeyKind::PrimaryKey,
                side_effects: vec![],
            },
            vec![Some(false); 3],
        );
        session
            .set(
                50,
                &|col| result.value(0, col).map(str::to_owned),
                1,
                EditValue::Text("t".into()),
            )
            .unwrap();
        let inserted = session.insert_row();
        session
            .set_inserted(inserted, 1, EditValue::Text("true".into()))
            .unwrap();
        let scan = |session: &EditSession, needle: &str, pt_br| {
            scan_find_cells(
                FindCells {
                    result: &result,
                    session: Some(session),
                    first_row: 50,
                    cancel: None,
                },
                &[0, 1],
                &[0, 1, 2],
                &[
                    ValueFamily::Number,
                    ValueFamily::Boolean,
                    ValueFamily::Number,
                ],
                pt_br,
                needle,
                None,
            )
        };
        let found = scan(&session, "true", false);
        assert!(found.complete);
        assert_eq!(found.matching, [0, 1]);
        assert_eq!(found.hits, [(0, 1), (1, 1)]);
        assert_eq!(scan(&session, "false", false).rows, 0);
        assert_eq!(scan(&session, "default", false).matching, [1]);
        assert_eq!(scan(&session, "1234", false).matching, [0]);
        assert_eq!(scan(&session, "1234", true).rows, 0);
        assert_eq!(scan(&session, "1.234", true).hits, [(0, 2)]);
        session.written();
        assert_eq!(scan(&session, "true", false).hits, found.hits);
        let cancel = crate::drivers::CancellationToken::new();
        cancel.cancel();
        assert!(
            !scan_find_cells(
                FindCells {
                    result: &result,
                    session: None,
                    first_row: 0,
                    cancel: Some(&cancel)
                },
                &[0],
                &[1],
                &[ValueFamily::Number, ValueFamily::Boolean],
                false,
                "true",
                None
            )
            .complete
        );
    }

    #[test]
    fn a_large_search_completes_within_budget() {
        let result = event_log(100_000);
        let view = GridView::new(&result, key(0, "", None));
        let started = std::time::Instant::now();
        let scan = scan_find(
            &result,
            &view.rows,
            &[0, 1, 2, 3, 4],
            &view.families,
            false,
            "event",
            None,
        );
        let elapsed = started.elapsed();
        assert!(scan.complete);
        assert_eq!(scan.rows, 100_000);
        assert_eq!(scan.hits.len(), 100_000);
        assert!(elapsed < std::time::Duration::from_secs(10), "{elapsed:?}");
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
            edits: 0,
            inserted: 0,
        }
    }

    #[test]
    fn widths_are_computed_again_only_when_what_they_follow_changes() {
        let result = event_log(10);
        let view = GridView::new(&result, key(1, "", None));
        let mut layout = ColumnLayout::new(5);
        let mut set = HashMap::new();
        let cache = WidthsCache::default();
        let first = cache.get(&view, &layout, 0, &set);
        assert_eq!(first.by_column(), view.widths);
        // The same view, layout and widths: the same widths, not a copy.
        assert!(Rc::ptr_eq(&first, &cache.get(&view, &layout, 0, &set)));
        // A width the user set takes the place of the measured one.
        set.insert(2, 300.);
        let resized = cache.get(&view, &layout, 0, &set);
        assert!(!Rc::ptr_eq(&first, &resized));
        assert_eq!(resized.by_column()[2], 300.);
        assert_eq!(resized.by_column()[1], view.widths[1]);
        // A change of the layout lays them out again.
        layout.freeze(2);
        let frozen = cache.get(&view, &layout, 1, &set);
        assert_eq!((frozen.frozen(), frozen.display()[0]), (1, 300.));
        assert!(Rc::ptr_eq(&frozen, &cache.get(&view, &layout, 1, &set)));
        // So does another view: a filter, a sort, another result.
        let filtered = GridView::new(&result, key(1, "info", None));
        assert!(!Rc::ptr_eq(
            &frozen,
            &cache.get(&filtered, &layout, 1, &set)
        ));
    }

    #[test]
    fn inserted_rows_follow_the_result_whatever_its_filter_and_sort() {
        let result = table(&[vec!["b".into(), "1".into()], vec!["a".into(), "2".into()]]);
        let view = GridView::new(
            &result,
            GridKey {
                inserted: 2,
                edits: 3,
                ..key(1, "a", Some((0, false)))
            },
        );
        assert_eq!(view.rows, [1, 2, 3]);
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
        let layout = ColumnLayout::new(3);
        let start = Some(CellSelection::cell(3, 1));
        let jump = |jump, extend| {
            CellSelection::jumped(start, &visible, &layout, jump, extend).expect("a cell")
        };
        assert_eq!(
            jump(Jump::RowStart, false),
            (CellSelection::cell(3, 0), (1, 0))
        );
        assert_eq!(
            jump(Jump::RowEnd, false),
            (CellSelection::cell(3, 2), (1, 2))
        );
        assert_eq!(
            jump(Jump::First, false),
            (CellSelection::cell(4, 0), (0, 0))
        );
        assert_eq!(jump(Jump::Last, false), (CellSelection::cell(0, 2), (4, 2)));
        assert_eq!(
            jump(Jump::Rows(2), false),
            (CellSelection::cell(1, 1), (3, 1))
        );
        assert_eq!(
            jump(Jump::Rows(-20), false),
            (CellSelection::cell(4, 1), (0, 1))
        );
        // Shift extends the block from the anchor.
        let (block, _) = jump(Jump::Last, true);
        assert_eq!((block.anchor, block.focus), ((3, 1), (0, 2)));
        assert_eq!(
            CellSelection::jumped(None, &[], &layout, Jump::Last, false),
            None
        );
    }

    #[test]
    fn keys_move_along_the_columns_as_they_are_shown() {
        let visible = [0, 1];
        let mut layout = ColumnLayout::new(4);
        layout.move_onto(3, 0);
        layout.hide(1);
        // Shown as 3, 0, 2.
        let start = Some(CellSelection::cell(0, 3));
        let right = CellSelection::moved(start, &visible, &layout, 0, 1, false);
        assert_eq!(right, Some((CellSelection::cell(0, 0), (0, 1))));
        let end = CellSelection::jumped(start, &visible, &layout, Jump::RowEnd, false);
        assert_eq!(end, Some((CellSelection::cell(0, 2), (0, 2))));
        let all = CellSelection::all(&visible, &layout).expect("cells");
        assert_eq!((all.anchor, all.focus), ((0, 3), (1, 2)));
        let row = CellSelection::click_row(None, 1, &layout, false);
        assert_eq!((row.anchor, row.focus), ((1, 3), (1, 2)));
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
        let layout = ColumnLayout::new(4);
        let row = CellSelection::click_row(Some(block), 9, &layout, false);
        assert_eq!((row.anchor, row.focus), ((9, 0), (9, 3)));
        let rows = CellSelection::click_row(Some(block), 9, &layout, true);
        assert_eq!((rows.anchor, rows.focus), ((3, 0), (9, 3)));
    }

    #[test]
    fn select_all_and_arrows_follow_the_displayed_rows() {
        let visible = [4, 2, 0];
        let layout = ColumnLayout::new(3);
        let all = CellSelection::all(&visible, &layout).expect("cells");
        assert_eq!((all.anchor, all.focus), ((4, 0), (0, 2)));
        assert_eq!(selected_cells(Some(all), &visible, &layout), 9);
        assert_eq!(CellSelection::all(&[], &layout), None);
        assert_eq!(CellSelection::all(&visible, &ColumnLayout::new(0)), None);
        // Without a selection, arrows start at the first displayed cell.
        let (down, position) =
            CellSelection::moved(None, &visible, &layout, 1, 0, true).expect("a cell");
        assert_eq!((down, position), (CellSelection::cell(2, 0), (1, 0)));
        // Shift grows the block; moves stop at the edges.
        let (grown, position) =
            CellSelection::moved(Some(down), &visible, &layout, 5, 9, true).expect("a cell");
        assert_eq!(
            (grown.anchor, grown.focus, position),
            ((2, 0), (0, 2), (2, 2))
        );
        assert_eq!(CellSelection::moved(None, &[], &layout, 1, 0, false), None);
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
        let mut layout = ColumnLayout::new(2);
        let block = CellSelection {
            anchor: (1, 0),
            focus: (0, 1),
        };
        assert_eq!(
            copy_selection(&result, &visible, &layout, Some(block), true),
            Some(("id\tnote\n2\t\n1\t\"a\tb\"".to_owned(), 4))
        );
        // A single cell copies its exact value.
        let cell = CellSelection::cell(0, 1);
        assert_eq!(
            copy_selection(&result, &visible, &layout, Some(cell), false),
            Some(("a\tb".to_owned(), 1))
        );
        assert_eq!(
            copy_selection(&result, &visible, &layout, None, false),
            None
        );
        // A selection whose rows are hidden copies nothing.
        assert_eq!(
            copy_selection(&result, &[1], &layout, Some(block), false),
            None
        );
        // Columns copy in the order they are shown.
        layout.move_onto(1, 0);
        assert_eq!(
            copy_selection(&result, &visible, &layout, Some(block), true),
            Some(("note\tid\n\t2\n\"a\tb\"\t1".to_owned(), 4))
        );
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
        assert_eq!(
            selected_cells(Some(column), &visible, &ColumnLayout::new(4)),
            3
        );
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
        let layout = ColumnLayout::new(2);
        assert_eq!(selected_rows(Some(block), &visible, &layout), [2, 0, 1]);
        assert!(selected_rows(None, &visible, &layout).is_empty());
    }
}
