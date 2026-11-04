//! The result grid: its own entity, embedded in the workspace with
//! `.cached(...)` ([ADR-0012](../../docs/adr/0012-result-grid-as-a-cached-entity.md)).
//!
//! Hover, scroll, selection, drags and copies notify only this entity; the
//! workspace learns what it shows (the selected count, the selection's
//! statistics, notices) through [`ResultGridEvent`]. What the grid derives
//! from a result lives in [`jykre::grid`], [`jykre::stats`] and
//! [`jykre::copy_as`].
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarAxis};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::cell_format::{self, ValueFamily};
use jykre::copy_as::{self, Block, CopyFormat, SqlStyle};
use jykre::grid::{
    self, CellSelection, ColumnSort, GridCache, GridKey, GridView, Jump, selection_block,
};
use jykre::result::ResultData;
use jykre::result_tabs::ResultId;
use jykre::stats::{self, ColumnSummary, Sample, SelectionSummary};
use jykre::tr;
use jykre_core::SqlDialect;
use std::collections::HashMap;
use std::time::Duration;
use std::{ops::Range, rc::Rc};

/// What the grid reports to the view that embeds it.
pub enum ResultGridEvent {
    /// The number of selected visible cells changed.
    SelectionChanged { cells: usize },
    /// How many rows the open filter shows, or `None` without a filter.
    RowsShown { rows: Option<usize> },
    /// Feedback for the notice bar, such as the outcome of a copy.
    Notice(Feedback),
    /// The statistics of the selected cells changed.
    Summary(Summary),
    /// The records view was turned on or off.
    RecordsShown,
}

/// The statistics of the selected cells, for the footer.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Summary {
    /// Fewer than two cells are selected.
    #[default]
    None,
    /// A large selection is being read; the footer says so.
    Computing,
    Ready(SelectionSummary),
}

/// How copies as SQL write names and strings for the tab's connection.
#[derive(Clone)]
pub struct GridSql {
    pub dialect: Option<SqlDialect>,
    /// Quotes a name when the dialect needs it.
    pub quote_name: super::sql_editor::QuoteName,
    /// The table the tab was opened for, quoted, for `INSERT`s.
    pub table: Option<String>,
}

impl Default for GridSql {
    fn default() -> Self {
        Self {
            dialect: None,
            quote_name: Rc::new(|name: &str| {
                let plain = name
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
                    && name
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_');
                if plain {
                    name.to_owned()
                } else {
                    format!("\"{}\"", name.replace('"', "\"\""))
                }
            }),
            table: None,
        }
    }
}

/// Where the user left a result tab: its page, selection, scroll and the
/// column widths they set.
struct Place {
    page: usize,
    selection: Option<CellSelection>,
    scroll: Point<Pixels>,
    widths: HashMap<usize, f32>,
}

/// A mouse gesture in progress over the grid.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gesture {
    /// Dragging from the anchor cell; `rows` when it started on a row
    /// number, so whole rows are selected.
    Selecting { rows: bool },
    /// Dragging the right edge of a column's header.
    Resizing {
        column: usize,
        start_x: f32,
        start_width: f32,
    },
}

/// What the side panel shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inspector {
    /// The full value of the inspected cell.
    Value,
    /// The statistics of the inspected column.
    Column,
}

/// A column's statistics, for the rows the filter shows.
struct ColumnStats {
    /// The result version, the filter and the column they were read for.
    key: (u64, String, usize),
    /// `None` while they are being computed.
    summary: Option<ColumnSummary>,
}

/// Height of a data row, in pixels.
const ROW_HEIGHT: f32 = 34.;
/// Height of the header and width of the row numbers, in pixels.
const HEADER_HEIGHT: f32 = 34.;
const ROW_NUMBERS: f32 = 35.;
/// Selections up to this many cells are summarized at once; larger ones are
/// read a chunk per frame and summarized off the UI thread.
const SYNC_CELLS: usize = 20_000;
/// Cells read per frame while summarizing a large selection or column.
const CHUNK_CELLS: usize = 40_000;
/// The value viewer shows up to this many bytes; Copy copies all of it.
const VIEWER_BYTES: usize = 1024 * 1024;
/// Rows the records view shows side by side.
const MAX_RECORDS: usize = 50;
/// Width of the side panel and of a record's column, in pixels.
const PANEL_WIDTH: f32 = 380.;
const RECORD_WIDTH: f32 = 280.;
const FIELD_WIDTH: f32 = 220.;

pub struct ResultGrid {
    result: Rc<ResultData>,
    /// The result tab and page on screen, when it is a result tab's.
    shown: Option<(ResultId, usize)>,
    /// Where each other result tab was left, so switching back to it
    /// restores its selection and scroll instead of inheriting this one's.
    places: HashMap<ResultId, Place>,
    /// Bumped with every new result, so the cached view is rebuilt.
    version: u64,
    /// The demo result has fixed column proportions.
    sales: bool,
    /// Lowercase text of the open filter; empty when the bar is closed.
    filter: String,
    /// The column the rows are sorted by, chosen in the header.
    sort: Option<ColumnSort>,
    /// The visible row count last reported while filtering.
    reported_rows: Option<usize>,
    numbers_pt_br: bool,
    /// Position in the whole result of this page's first row, for the row
    /// numbers.
    first_row: usize,
    cache: GridCache,
    selection: Option<CellSelection>,
    /// The selected count last reported, so unchanged counts are not sent.
    reported: usize,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// The horizontal pan of the columns; the rows scroll with `scroll`.
    columns_scroll: ScrollHandle,
    /// Widths the user dragged or fitted, by column, over the measured ones.
    widths: HashMap<usize, f32>,
    gesture: Option<Gesture>,
    /// Rows and columns a drag past an edge moves per step.
    edge: (isize, isize),
    autoscrolling: bool,
    autoscroll: Option<Task<()>>,
    /// The column a right click last landed on, for the header's menu.
    menu_column: Option<usize>,
    inspector: Option<Inspector>,
    /// The cell the value viewer shows, as (data row, column): the focused
    /// cell, or the one double-clicked in the records view.
    inspected: Option<(usize, usize)>,
    value_input: Option<Entity<TextareaState>>,
    /// What `value_input` holds: (result version, data row, column).
    value_shown: Option<(u64, usize, usize)>,
    column_stats: Option<ColumnStats>,
    column_task: Option<Task<()>>,
    /// Bumped with every selection, so a summary of an older one is dropped.
    summary_generation: u64,
    summary_task: Option<Task<()>>,
    reported_summary: Summary,
    /// The selected rows are shown as records, side by side.
    records: bool,
    records_scroll: UniformListScrollHandle,
    records_pan: ScrollHandle,
    sql: GridSql,
}

impl EventEmitter<ResultGridEvent> for ResultGrid {}

impl ResultGrid {
    pub(super) fn benchmark_begin(&mut self, bottom: bool, cx: &mut Context<Self>) {
        if bottom {
            self.select_all(cx);
        } else {
            self.select(Some(CellSelection::cell(0, 0)), cx);
        }
    }

    pub(super) fn benchmark_step(&mut self, phase: &str, tick: usize, cx: &mut Context<Self>) {
        let direction = (tick / 3) % 4;
        let (mut rows, mut cols) = [(1, 0), (0, 1), (-1, 0), (0, -1)][direction];
        if phase == "selection_bottom" {
            rows = -rows;
            cols = -cols;
        }
        if phase == "scroll" {
            let position = (tick * 5) % self.result.row_count().max(1);
            self.scroll
                .scroll_to_item(position, ScrollStrategy::Nearest);
            cx.notify();
        } else if phase != "idle" {
            self.move_selection(rows, cols, true, cx);
        }
    }

    pub fn new(result: Rc<ResultData>, sales: bool, cx: &mut Context<Self>) -> Self {
        Self {
            result,
            shown: None,
            places: HashMap::new(),
            version: 0,
            sales,
            first_row: 0,
            filter: String::new(),
            sort: None,
            reported_rows: None,
            numbers_pt_br: false,
            cache: GridCache::default(),
            selection: None,
            reported: 0,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            columns_scroll: ScrollHandle::new(),
            widths: HashMap::new(),
            gesture: None,
            edge: (0, 0),
            autoscrolling: false,
            autoscroll: None,
            menu_column: None,
            inspector: None,
            inspected: None,
            value_input: None,
            value_shown: None,
            column_stats: None,
            column_task: None,
            summary_generation: 0,
            summary_task: None,
            reported_summary: Summary::None,
            records: false,
            records_scroll: UniformListScrollHandle::new(),
            records_pan: ScrollHandle::new(),
            sql: GridSql::default(),
        }
    }

    /// Shows one page of a result whose first row is row `first_row` (zero
    /// based) of the whole result. The grid keeps a shared handle; it never
    /// copies the data.
    ///
    /// `shown` is the result tab and page number. Leaving a tab remembers
    /// its selection, scroll and column widths, and coming back to the same
    /// page of it restores them; any other page starts at the top with
    /// nothing selected.
    pub fn set_page(
        &mut self,
        result: Rc<ResultData>,
        sales: bool,
        first_row: usize,
        shown: Option<(ResultId, usize)>,
        cx: &mut Context<Self>,
    ) {
        if Rc::ptr_eq(&self.result, &result)
            && self.first_row == first_row
            && self.sales == sales
            && self.shown == shown
        {
            return;
        }
        let same_tab = self.shown.map(|(id, _)| id) == shown.map(|(id, _)| id);
        if let Some((id, page)) = self.shown
            && !same_tab
        {
            let scroll = self.scroll.0.borrow().base_handle.offset();
            self.places.insert(
                id,
                Place {
                    page,
                    selection: self.selection,
                    scroll,
                    widths: std::mem::take(&mut self.widths),
                },
            );
        }
        let place =
            shown.and_then(|(id, page)| self.places.remove(&id).filter(|place| place.page == page));
        // A sort chosen for one result tab does not carry to another; the
        // pages of the same tab keep it, and their column widths.
        if !same_tab {
            self.sort = None;
            self.widths.clear();
        }
        self.result = result;
        self.sales = sales;
        self.first_row = first_row;
        self.shown = shown;
        self.version += 1;
        self.gesture = None;
        let (selection, scroll) = match place {
            Some(place) => {
                if !place.widths.is_empty() {
                    self.widths = place.widths;
                }
                (place.selection, place.scroll)
            }
            None => (None, Point::default()),
        };
        self.scroll.0.borrow().base_handle.set_offset(scroll);
        self.select(selection, cx);
        cx.notify();
    }

    /// Forgets where closed result tabs were left.
    pub fn retain_places(&mut self, open: &[ResultId]) {
        self.places.retain(|id, _| open.contains(id));
    }

    /// Filters rows by text in any column, ignoring case. An empty filter
    /// (or a closed filter bar) shows every row.
    pub fn set_filter(&mut self, filter: &str, cx: &mut Context<Self>) {
        let filter = filter.to_lowercase();
        if self.filter != filter {
            self.filter = filter;
            // The selection addresses data rows: it stays while its ends are
            // visible and reappears when they are again; only the count
            // reported to the footer changes.
            self.select(self.selection, cx);
        }
    }

    pub fn set_number_format(&mut self, numbers_pt_br: bool, cx: &mut Context<Self>) {
        if self.numbers_pt_br != numbers_pt_br {
            self.numbers_pt_br = numbers_pt_br;
            cx.notify();
        }
    }

    /// How copies as SQL write names, strings and the table.
    pub fn set_sql(&mut self, sql: GridSql) {
        self.sql = sql;
    }

    pub fn records(&self) -> bool {
        self.records
    }

    /// Shows the selected rows as records side by side, or the table again.
    pub fn set_records(&mut self, records: bool, cx: &mut Context<Self>) {
        if self.records != records {
            self.records = records;
            if !records {
                self.inspected = self.selection.map(|selection| selection.focus);
            }
            cx.emit(ResultGridEvent::RecordsShown);
            cx.notify();
        }
    }

    /// The grid for the current result, filter, sort and number format.
    fn view(&self) -> Rc<GridView> {
        let key = GridKey {
            result: self.version,
            filter: self.filter.clone(),
            sort: self.sort,
            numbers_pt_br: self.numbers_pt_br,
        };
        self.cache.get(&self.result, key)
    }

    /// The width of each column: the user's where they set one, otherwise
    /// the measured one.
    fn column_widths(&self, grid: &GridView) -> Vec<f32> {
        grid.widths
            .iter()
            .enumerate()
            .map(|(column, width)| self.widths.get(&column).copied().unwrap_or(*width))
            .collect()
    }

    /// Changes the selection, notifies this grid only and reports a new
    /// selected count to the workspace.
    fn select(&mut self, selection: Option<CellSelection>, cx: &mut Context<Self>) {
        self.selection = selection;
        let view = self.view();
        let cells = grid::selected_cells(selection, &view.rows);
        if cells != self.reported {
            self.reported = cells;
            cx.emit(ResultGridEvent::SelectionChanged { cells });
        }
        // The footer counts the rows the filter shows, as the grid does.
        let rows = (!self.filter.is_empty()).then_some(view.rows.len());
        if rows != self.reported_rows {
            self.reported_rows = rows;
            cx.emit(ResultGridEvent::RowsShown { rows });
        }
        // The side panel follows the focused cell; in the records view it
        // keeps the cell double-clicked there.
        if !self.records {
            self.inspected = selection.map(|selection| selection.focus);
        }
        self.summarize(cx);
        self.refresh_column_stats(cx);
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let all = CellSelection::all(&self.view().rows, self.result.column_count());
        if all.is_some() {
            self.select(all, cx);
        }
    }

    /// Arrow-key navigation; with Shift the block grows from the anchor.
    /// The focused cell scrolls into view, rows and columns.
    fn move_selection(&mut self, rows: isize, cols: isize, extend: bool, cx: &mut Context<Self>) {
        let view = self.view();
        let columns = self.result.column_count();
        if let Some((selection, position)) =
            CellSelection::moved(self.selection, &view.rows, columns, rows, cols, extend)
        {
            self.select(Some(selection), cx);
            self.reveal(position, selection.focus.1, &view);
        }
    }

    /// Home, End, Page Up, Page Down and Ctrl Home or End; with Shift the
    /// block grows from the anchor.
    fn jump_selection(&mut self, jump: Jump, extend: bool, cx: &mut Context<Self>) {
        let view = self.view();
        let columns = self.result.column_count();
        if let Some((selection, position)) =
            CellSelection::jumped(self.selection, &view.rows, columns, jump, extend)
        {
            self.select(Some(selection), cx);
            self.reveal(position, selection.focus.1, &view);
        }
    }

    /// Scrolls the cell at display `position` and `column` into view.
    fn reveal(&self, position: usize, column: usize, view: &GridView) {
        self.scroll
            .scroll_to_item(position, ScrollStrategy::Nearest);
        let bounds = self.columns_scroll.bounds();
        let pan = -f32::from(self.columns_scroll.offset().x);
        let widths = self.column_widths(view);
        if let Some(pan) = grid::pan_to_column(
            &widths,
            column,
            ROW_NUMBERS,
            f32::from(bounds.size.width),
            pan,
        ) {
            let offset = self.columns_scroll.offset();
            self.columns_scroll.set_offset(point(px(-pan), offset.y));
        }
    }

    /// The rows one screen of the grid shows, for Page Up and Page Down.
    fn page_rows(&self) -> isize {
        let height = f32::from(self.scroll.0.borrow().base_handle.bounds().size.height);
        ((height / ROW_HEIGHT).floor() as isize - 1).max(1)
    }

    /// The pointer moved anywhere in the window while a gesture is active:
    /// resizes the column, or scrolls when a selection is dragged past an
    /// edge of the table (`bounds`).
    fn pointer_moved(
        &mut self,
        position: Point<Pixels>,
        pressed: Option<MouseButton>,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // A button released outside the window ends the gesture at the next
        // move.
        if pressed != Some(MouseButton::Left) {
            self.end_gesture(cx);
            return;
        }
        match self.gesture {
            Some(Gesture::Resizing {
                column,
                start_x,
                start_width,
            }) => {
                let width = grid::resized_width(start_width, f32::from(position.x) - start_x);
                if self.widths.get(&column) != Some(&width) {
                    self.widths.insert(column, width);
                    cx.notify();
                }
            }
            Some(Gesture::Selecting { rows }) => {
                let (x, y) = (f32::from(position.x), f32::from(position.y));
                let top = f32::from(bounds.top()) + HEADER_HEIGHT;
                let left = f32::from(bounds.left()) + ROW_NUMBERS;
                self.edge = (
                    grid::edge_step(y, top, f32::from(bounds.bottom())),
                    if rows {
                        0
                    } else {
                        grid::edge_step(x, left, f32::from(bounds.right())).signum()
                    },
                );
                if self.edge != (0, 0) && !self.autoscrolling {
                    self.start_autoscroll(cx);
                }
            }
            None => {}
        }
    }

    /// While a selection is dragged past an edge, moves its focus a few
    /// rows (or a column) every 40 ms, as the arrows do, until the pointer
    /// comes back or the button is released.
    fn start_autoscroll(&mut self, cx: &mut Context<Self>) {
        self.autoscrolling = true;
        self.autoscroll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
                let going = this
                    .update(cx, |this, cx| {
                        let selecting = matches!(this.gesture, Some(Gesture::Selecting { .. }));
                        if !selecting || this.edge == (0, 0) {
                            this.autoscrolling = false;
                            return false;
                        }
                        let (rows, cols) = this.edge;
                        this.move_selection(rows, cols, true, cx);
                        true
                    })
                    .unwrap_or(false);
                if !going {
                    break;
                }
            }
        }));
    }

    fn end_gesture(&mut self, cx: &mut Context<Self>) {
        let Some(gesture) = self.gesture.take() else {
            return;
        };
        self.edge = (0, 0);
        if matches!(gesture, Gesture::Selecting { .. }) {
            // A drag is summarized once, when it ends.
            self.summarize(cx);
        }
        cx.notify();
    }

    /// Fits a column to its header and the values of the displayed rows.
    fn fit_column(&mut self, column: usize, cx: &mut Context<Self>) {
        let view = self.view();
        let family = view
            .families
            .get(column)
            .copied()
            .unwrap_or(ValueFamily::Text);
        let width = grid::fit_width(&self.result, &view.rows, column, family, self.numbers_pt_br);
        self.widths.insert(column, width);
        cx.notify();
    }

    /// Reports what the footer says about the selection: at once for a
    /// small one, after reading it a chunk per frame for a large one. A
    /// drag is summarized when it ends.
    fn summarize(&mut self, cx: &mut Context<Self>) {
        self.summary_generation += 1;
        self.summary_task = None;
        if matches!(self.gesture, Some(Gesture::Selecting { .. })) {
            return;
        }
        let view = self.view();
        let Some((rows, cols)) = self
            .selection
            .and_then(|selection| selection_block(selection, &view.rows))
        else {
            self.report_summary(Summary::None, cx);
            return;
        };
        let columns: Vec<usize> = cols.collect();
        let numeric: Vec<usize> = columns
            .iter()
            .copied()
            .filter(|column| view.families.get(*column) == Some(&ValueFamily::Number))
            .collect();
        let data_rows: Vec<usize> = view.rows[rows].to_vec();
        let cells = data_rows.len() * columns.len();
        if cells < 2 {
            self.report_summary(Summary::None, cx);
            return;
        }
        if cells <= SYNC_CELLS {
            let mut sample = Sample::default();
            let mut values = 0;
            read_cells(
                &self.result,
                &data_rows,
                &columns,
                &numeric,
                &mut sample,
                &mut values,
            );
            self.report_summary(Summary::Ready(stats::selection(&sample, values)), cx);
            return;
        }
        self.report_summary(Summary::Computing, cx);
        let generation = self.summary_generation;
        // A weak handle: the task never keeps a result alive, so a released
        // result is still freed off the UI thread.
        let result = Rc::downgrade(&self.result);
        let chunk = (CHUNK_CELLS / columns.len().max(1)).max(1);
        self.summary_task = Some(cx.spawn(async move |this, cx| {
            let mut sample = Sample::default();
            let mut values = 0;
            for rows in data_rows.chunks(chunk) {
                let Some(result) = result.upgrade() else {
                    return;
                };
                read_cells(&result, rows, &columns, &numeric, &mut sample, &mut values);
                drop(result);
                cx.background_executor()
                    .timer(Duration::from_millis(1))
                    .await;
            }
            let summary = cx
                .background_executor()
                .spawn(async move { stats::selection(&sample, values) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.summary_generation == generation {
                    this.report_summary(Summary::Ready(summary), cx);
                }
            });
        }));
    }

    fn report_summary(&mut self, summary: Summary, cx: &mut Context<Self>) {
        if self.reported_summary != summary {
            self.reported_summary = summary.clone();
            cx.emit(ResultGridEvent::Summary(summary));
        }
    }

    /// Opens the side panel on `inspector`, or closes it when it already
    /// shows that.
    fn toggle_inspector(&mut self, inspector: Inspector, cx: &mut Context<Self>) {
        self.inspector = (self.inspector != Some(inspector)).then_some(inspector);
        if self.inspector == Some(Inspector::Column) && self.menu_column.is_none() {
            self.menu_column = self.inspected.map(|(_, column)| column);
        }
        self.refresh_column_stats(cx);
        cx.notify();
    }

    fn open_inspector(&mut self, inspector: Inspector, cx: &mut Context<Self>) {
        self.inspector = Some(inspector);
        self.refresh_column_stats(cx);
        cx.notify();
    }

    /// The column whose statistics the panel shows: the focused one, or
    /// the one whose header was right-clicked.
    fn stats_column(&self) -> Option<usize> {
        self.menu_column
            .or(self.inspected.map(|(_, column)| column))
            .filter(|column| *column < self.result.column_count())
    }

    /// Reads the statistics of the panel's column for the rows the filter
    /// shows, a chunk per frame, and computes them off the UI thread.
    fn refresh_column_stats(&mut self, cx: &mut Context<Self>) {
        if self.inspector != Some(Inspector::Column) {
            return;
        }
        let Some(column) = self.stats_column() else {
            self.column_stats = None;
            return;
        };
        let key = (self.version, self.filter.clone(), column);
        if self
            .column_stats
            .as_ref()
            .is_some_and(|stats| stats.key == key)
        {
            return;
        }
        self.column_stats = Some(ColumnStats {
            key: key.clone(),
            summary: None,
        });
        let view = self.view();
        let rows = view.rows.clone();
        let family = view
            .families
            .get(column)
            .copied()
            .unwrap_or(ValueFamily::Text);
        let result = Rc::downgrade(&self.result);
        self.column_task = Some(cx.spawn(async move |this, cx| {
            let mut sample = Sample::default();
            for rows in rows.chunks(CHUNK_CELLS) {
                let Some(result) = result.upgrade() else {
                    return;
                };
                for row in rows {
                    sample.push(result.value(*row, column));
                }
                drop(result);
                cx.background_executor()
                    .timer(Duration::from_millis(1))
                    .await;
            }
            let summary = cx
                .background_executor()
                .spawn(async move { stats::column(&sample, family) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(stats) = &mut this.column_stats
                    && stats.key == key
                {
                    stats.summary = Some(summary);
                    cx.notify();
                }
            });
        }));
    }

    /// Shows the inspected cell's value in the viewer, once per cell.
    fn sync_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inspector != Some(Inspector::Value) {
            return;
        }
        let target = self
            .inspected
            .map(|(row, column)| (self.version, row, column));
        if self.value_shown == target && self.value_input.is_some() {
            return;
        }
        self.value_shown = target;
        let text = target
            .and_then(|(_, row, column)| self.result.value(row, column))
            .map(viewer_text)
            .unwrap_or_default();
        let input = self.value_input.get_or_insert_with(|| {
            cx.new(|cx| {
                let mut state = TextareaState::new(window, cx);
                state.set_readonly(true, cx);
                state
            })
        });
        input.update(cx, |state, cx| state.set_value(text, window, cx));
    }

    /// Copies the selected block as tab-separated text (a single cell as its
    /// exact value), optionally with the column names first. A copy
    /// estimated above 50 MiB asks first, unless `confirmed`.
    fn copy_selection(
        &mut self,
        headers: bool,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.view();
        let bytes = grid::selection_bytes(&self.result, &view.rows, self.selection, headers);
        if !confirmed && bytes > grid::COPY_CONFIRM_BYTES {
            self.confirm_copy(bytes, window, cx, move |this, window, cx| {
                this.copy_selection(headers, true, window, cx)
            });
            return;
        }
        let _timer = jykre::logging::UiTimer::start("copy selection");
        let notice = match grid::copy_selection(&self.result, &view.rows, self.selection, headers) {
            None => Feedback::new(Tone::Info, tr!("Select cells to copy.")),
            Some((text, count)) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                Feedback::new(
                    Tone::Success,
                    if count == 1 {
                        tr!("Cell copied.").to_string()
                    } else {
                        tr!("{0} cells copied.", count)
                    },
                )
            }
        };
        cx.emit(ResultGridEvent::Notice(notice));
    }

    /// Copies the selected block in `format`. Like any copy, one estimated
    /// above 50 MiB asks first.
    fn copy_as(
        &mut self,
        format: CopyFormat,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.view();
        let bytes = grid::selection_bytes(&self.result, &view.rows, self.selection, true);
        if !confirmed && bytes > grid::COPY_CONFIRM_BYTES {
            self.confirm_copy(bytes, window, cx, move |this, window, cx| {
                this.copy_as(format, true, window, cx)
            });
            return;
        }
        let _timer = jykre::logging::UiTimer::start("copy as");
        let notice = match Block::new(&self.result, &view.rows, self.selection, &view.families) {
            None => Feedback::new(Tone::Info, tr!("Select cells to copy.")),
            Some(block) => {
                let table = self
                    .sql
                    .table
                    .clone()
                    .unwrap_or_else(|| "table_name".to_owned());
                let style = SqlStyle {
                    dialect: self.sql.dialect,
                    quote_name: &*self.sql.quote_name,
                    table: &table,
                };
                let text = copy_as::copy_as(format, &block, &style);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                Feedback::new(Tone::Success, tr!("Copied as {0}.", format_name(format)))
            }
        };
        cx.emit(ResultGridEvent::Notice(notice));
    }

    fn confirm_copy(
        &self,
        bytes: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        copy: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let grid = cx.entity().downgrade();
        super::confirm(
            window,
            cx,
            super::Confirmation {
                title: tr!("Copy about {0} MB?", bytes / (1024 * 1024)),
                description: tr!(
                    "The selection is large: copying it can take a moment and holds all of it in the clipboard."
                )
                .to_string(),
                ok: tr!("Copy"),
                cancel: tr!("Cancel"),
                focus: super::ConfirmFocus::Dialog,
            },
            move |window, cx| {
                let _ = grid.update(cx, |this, cx| copy(this, window, cx));
            },
        );
    }

    /// Copies the whole value the viewer shows.
    fn copy_value(&mut self, cx: &mut Context<Self>) {
        let value = self
            .inspected
            .and_then(|(row, column)| self.result.value(row, column));
        let notice = match value {
            Some(value) => {
                cx.write_to_clipboard(ClipboardItem::new_string(value.to_owned()));
                Feedback::new(Tone::Success, tr!("Value copied."))
            }
            None => Feedback::new(
                Tone::Info,
                tr!("The value is NULL: there is nothing to copy."),
            ),
        };
        cx.emit(ResultGridEvent::Notice(notice));
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let shift = keystroke.modifiers.shift;
        let handled = if keystroke.modifiers.control || keystroke.modifiers.platform {
            match keystroke.key.as_str() {
                "c" => {
                    self.copy_selection(shift, false, window, cx);
                    true
                }
                "a" => {
                    self.select_all(cx);
                    true
                }
                "home" => {
                    self.jump_selection(Jump::First, shift, cx);
                    true
                }
                "end" => {
                    self.jump_selection(Jump::Last, shift, cx);
                    true
                }
                _ => false,
            }
        } else {
            match keystroke.key.as_str() {
                "up" => self.move_selection(-1, 0, shift, cx),
                "down" => self.move_selection(1, 0, shift, cx),
                "left" => self.move_selection(0, -1, shift, cx),
                "right" => self.move_selection(0, 1, shift, cx),
                "home" => self.jump_selection(Jump::RowStart, shift, cx),
                "end" => self.jump_selection(Jump::RowEnd, shift, cx),
                "pageup" => {
                    let rows = self.page_rows();
                    self.jump_selection(Jump::Rows(-rows), shift, cx)
                }
                "pagedown" => {
                    let rows = self.page_rows();
                    self.jump_selection(Jump::Rows(rows), shift, cx)
                }
                "enter" if self.inspected.is_some() => self.open_inspector(Inspector::Value, cx),
                // Esc closes what is on top first: the panel, the records
                // view, then the selection.
                "escape" if self.inspector.is_some() => {
                    self.inspector = None;
                    cx.notify();
                }
                "escape" if self.records => self.set_records(false, cx),
                "escape" if self.selection.is_some() => self.select(None, cx),
                _ => return,
            }
            true
        };
        if handled {
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    fn table(&self, cx: &mut Context<Self>) -> AnyElement {
        let grid = self.view();
        let widths = self.column_widths(&grid);
        let grid_width = widths.iter().sum::<f32>() + ROW_NUMBERS;
        let has_rows = !grid.rows.is_empty();
        // Not `overflow_x_scrollbar`: its wheel mask pans the columns with a
        // plain wheel until their end, and only then scrolls the rows. Here
        // the wheel scrolls the rows and only a horizontal delta (Shift +
        // wheel, a tilt wheel or a sideways swipe) pans the columns.
        let table = div()
            .id("result-table-scroll")
            .size_full()
            .overflow_x_scroll()
            .overflow_y_hidden()
            .restrict_scroll_to_axis()
            .track_scroll(&self.columns_scroll)
            .child(
                column()
                    .w(px(grid_width))
                    .min_w_full()
                    .h_full()
                    .child(
                        row()
                            .h(px(HEADER_HEIGHT))
                            .flex_none()
                            .gap_0()
                            .bg(rgb(GRID_HEADER))
                            .border_b_1()
                            .border_color(rgb(GRID_LINE))
                            .child(
                                div()
                                    .w(px(ROW_NUMBERS))
                                    .flex_none()
                                    .text_center()
                                    .child(label("#")),
                            )
                            .children(self.result.columns().iter().enumerate().map(
                                |(index, column)| self.header(index, column, widths[index], cx),
                            )),
                    )
                    .child(if grid.rows.is_empty() && !self.filter.is_empty() {
                        // The filter reads only the rows held for this page,
                        // so "nothing found" says so.
                        column()
                            .p_6()
                            .gap_2()
                            .child(label(tr!("No rows on this page match the filter.")))
                            .child(label(tr!(
                                "The filter searches only the rows of this page, not other pages or rows past the row limit."
                            )))
                            .into_any_element()
                    } else if grid.rows.is_empty() {
                        column()
                            .p_6()
                            .gap_2()
                            .child(label(tr!("No results for this selection.")))
                            .child(label(tr!("The query returned no rows.")))
                            .into_any_element()
                    } else {
                        // Only the rows on screen are built, so a frame costs
                        // the same for 30 rows or 100,000.
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(
                                uniform_list(
                                    "result-rows",
                                    grid.rows.len(),
                                    cx.processor(|this, range: Range<usize>, _, cx| {
                                        this.result_rows(range, cx)
                                    }),
                                )
                                .size_full()
                                .map(|mut list| {
                                    // A horizontal delta pans the columns
                                    // above, not the rows too.
                                    list.style().restrict_scroll_to_axis = Some(true);
                                    list
                                })
                                .track_scroll(&self.scroll),
                            )
                            .into_any_element()
                    }),
            );
        div()
            .size_full()
            .relative()
            .child(table)
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::new(&self.columns_scroll)
                        .id("result-table-columns-bar")
                        .axis(ScrollbarAxis::Horizontal)
                        .viewport_from_layout(),
                ),
            )
            // Outside the columns' pan, so the rows' bar stays on the visible
            // right edge instead of the right end of the widest column; below
            // the header, which does not scroll with the rows.
            .when(has_rows, |grid| {
                grid.child(
                    div()
                        .absolute()
                        .top(px(HEADER_HEIGHT))
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .child(
                            Scrollbar::new(&self.scroll)
                                .id("result-table-rows-bar")
                                .axis(ScrollbarAxis::Vertical)
                                .viewport_from_layout(),
                        ),
                )
            })
            .into_any_element()
    }

    /// A column's header: a click sorts by it, `Ctrl` + click selects it,
    /// and its right edge resizes it (a double click fits it).
    fn header(
        &self,
        index: usize,
        column: &jykre::result::ResultColumn,
        width: f32,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let name = &column.name;
        let kind = column.data_type.to_lowercase();
        let tooltip = tr!(
            "{0} : {1} · Click to sort this page by it, Ctrl + click to select it",
            name,
            kind
        );
        let sorted = self
            .sort
            .filter(|sort| sort.column == index)
            .map(|sort| sort.descending);
        row()
            .id(("result-header", index))
            .role(Role::ColumnHeader)
            .aria_label(match sorted {
                Some(false) => tr!("{0}, sorted ascending", name),
                Some(true) => tr!("{0}, sorted descending", name),
                None => tr!("{0}, not sorted", name),
            })
            .relative()
            .w(px(width))
            .flex_none()
            .h_full()
            .px_3()
            .gap_1()
            .when(self.sales && !self.widths.contains_key(&index), |s| {
                s.flex_grow([1.5, 0.8, 1.2, 1.1, 0.65][index])
            })
            .border_l_1()
            .border_color(rgb(GRID_LINE))
            .text_size(px(14.))
            .text_color(rgb(GRID_HEADER_TEXT))
            .cursor_pointer()
            .focusable()
            .tab_index(0)
            .focus_visible(|s| s.bg(rgb(HOVER)))
            .hover(|s| s.bg(rgb(GRID_HOVER)))
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .child(
                div()
                    .flex_shrink(1.)
                    .min_w_0()
                    .truncate()
                    .child(name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(rgb(GRID_TYPE_TEXT))
                    .child(format!(": {kind}")),
            )
            .when_some(sorted, |s, descending| {
                s.child(icon(
                    if descending {
                        I::ArrowDown
                    } else {
                        I::ArrowUp
                    },
                    MINT,
                ))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, _| this.menu_column = Some(index)),
            )
            // Ascending, descending, then the server's order again; the
            // rows of this page are sorted by value. With Ctrl the column
            // is selected instead (Shift extends from the anchor's).
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                let modifiers = event.modifiers();
                if modifiers.control || modifiers.platform {
                    let selection = CellSelection::click_column(
                        this.selection,
                        &this.view().rows,
                        index,
                        modifiers.shift,
                    );
                    if selection.is_some() {
                        this.select(selection, cx);
                        window.focus(&this.focus, cx);
                    }
                    return;
                }
                this.sort = ColumnSort::next(this.sort, index);
                this.select(this.selection, cx);
            }))
            .child(
                div()
                    .id(("resize-column", index))
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(px(6.))
                    .cursor_col_resize()
                    .hover(|s| s.bg(rgb(GRID_FOCUS)))
                    .when(
                        self.gesture.is_some_and(|gesture| {
                            matches!(gesture, Gesture::Resizing { column, .. } if column == index)
                        }),
                        |s| s.bg(rgb(GRID_FOCUS)),
                    )
                    // Taken here, so the header neither sorts nor selects.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            if event.click_count >= 2 {
                                this.gesture = None;
                                this.fit_column(index, cx);
                                return;
                            }
                            this.gesture = Some(Gesture::Resizing {
                                column: index,
                                start_x: f32::from(event.position.x),
                                start_width: width,
                            });
                            cx.notify();
                        }),
                    )
                    .on_click(|_, _, cx| cx.stop_propagation()),
            )
    }

    /// The data rows at `range` of the displayed order.
    fn result_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let grid = self.view();
        let widths = self.column_widths(&grid);
        let block = self
            .selection
            .and_then(|selection| selection_block(selection, &grid.rows));
        let focus = self.selection.map(|selection| selection.focus);
        let last_column = self.result.column_count().saturating_sub(1);
        range
            .filter_map(|position| Some((position, *grid.rows.get(position)?)))
            .map(|(position, index)| {
                let cells: Vec<_> = self.result.row(index).collect();
                let row_selected = block.as_ref().is_some_and(|(rows, cols)| {
                    rows.contains(&position) && *cols.start() == 0 && *cols.end() == last_column
                });
                row()
                    .id(("data-row", index))
                    .role(Role::Row)
                    .aria_label(
                        cells
                            .iter()
                            .map(|cell| cell.unwrap_or("NULL"))
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .flex_none()
                    .gap_0()
                    .child(
                        div()
                            .id(("row-number", index))
                            .w(px(ROW_NUMBERS))
                            .flex_none()
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .bg(rgb(if row_selected {
                                GRID_SELECTED
                            } else {
                                GRID_HEADER
                            }))
                            .hover(|s| s.bg(rgb(GRID_HOVER)))
                            .border_b_1()
                            .border_color(rgb(GRID_LINE))
                            .text_center()
                            .text_size(px(12.))
                            .text_color(rgb(FAINT))
                            // Pressing selects the row (Shift extends);
                            // dragging over other rows selects them all.
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    let selection = CellSelection::click_row(
                                        this.selection,
                                        index,
                                        last_column + 1,
                                        event.modifiers.shift,
                                    );
                                    this.gesture = Some(Gesture::Selecting { rows: true });
                                    this.select(Some(selection), cx);
                                    window.focus(&this.focus, cx);
                                }),
                            )
                            .on_mouse_move(cx.listener(
                                move |this, event: &MouseMoveEvent, _, cx| {
                                    this.drag_over(index, last_column, event, cx)
                                },
                            ))
                            .child((self.first_row + index + 1).to_string()),
                    )
                    .children(cells.iter().enumerate().map(|(col, raw)| {
                        let family = grid.families[col];
                        let raw = *raw;
                        // A cell is one line: line breaks show as ↵ and the
                        // full value stays in the tooltip and in copies.
                        let displayed = cell_format::display(raw, family, self.numbers_pt_br)
                            .replace("\r\n", "↵")
                            .replace(['\n', '\r'], "↵");
                        let is_null = raw.is_none();
                        // Only values that do not fit get a tooltip, so it
                        // does not cover the context menu or neighbours.
                        let tooltip = raw
                            .filter(|value| {
                                value.contains(['\n', '\r'])
                                    || displayed.chars().count() as f32 * 7.8 > widths[col] - 18.
                            })
                            .map(str::to_owned);
                        let selected = block.as_ref().is_some_and(|(rows, cols)| {
                            rows.contains(&position) && cols.contains(&col)
                        });
                        let focused = focus == Some((index, col));
                        div()
                            .id(("result-cell", col))
                            .role(Role::GridCell)
                            .aria_selected(selected)
                            .aria_label(tr!(
                                "Row {0}, {1}: {2}",
                                self.first_row + index + 1,
                                self.result
                                    .columns()
                                    .get(col)
                                    .map_or("", |column| column.name.as_str()),
                                raw.unwrap_or("NULL")
                            ))
                            .cursor_pointer()
                            .w(px(widths[col]))
                            .flex_none()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .items_center()
                            .px(px(8.))
                            .border_1()
                            .border_color(rgb(GRID_LINE))
                            .when(!selected, |s| s.hover(|s| s.bg(rgb(GRID_HOVER))))
                            .when(selected, |s| s.bg(rgb(GRID_SELECTED)))
                            .when(focused, |s| s.border_color(rgb(GRID_FOCUS)))
                            // Pressing selects the cell (Shift extends from
                            // the anchor) and starts a drag; a double click
                            // shows the full value.
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    window.focus(&this.focus, cx);
                                    if event.click_count >= 2 {
                                        this.open_inspector(Inspector::Value, cx);
                                        return;
                                    }
                                    let selection = CellSelection::click(
                                        this.selection,
                                        index,
                                        col,
                                        event.modifiers.shift,
                                    );
                                    this.gesture = Some(Gesture::Selecting { rows: false });
                                    this.select(Some(selection), cx);
                                }),
                            )
                            .on_mouse_move(cx.listener(
                                move |this, event: &MouseMoveEvent, _, cx| {
                                    this.drag_over(index, col, event, cx)
                                },
                            ))
                            // A right click outside the selection selects the
                            // cell first, so the menu acts on what is under it.
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, _, window, cx| {
                                    this.menu_column = None;
                                    if !selected {
                                        this.select(Some(CellSelection::cell(index, col)), cx);
                                    }
                                    window.focus(&this.focus, cx);
                                }),
                            )
                            .when(self.sales && !self.widths.contains_key(&col), |s| {
                                s.flex_grow([1.5, 0.8, 1.2, 1.1, 0.65][col])
                            })
                            .font_family("Cascadia Mono")
                            .text_size(px(13.))
                            .when(is_null, |s| s.italic())
                            .when(family == ValueFamily::Boolean, |s| s.justify_center())
                            .when_some(tooltip, |s, tooltip| {
                                s.tooltip(move |window, cx| {
                                    Tooltip::new(tooltip.clone()).build(window, cx)
                                })
                            })
                            .when(family == ValueFamily::Number, |s| s.justify_end())
                            .text_color(rgb(value_color(raw, family)))
                            .child(div().truncate().child(displayed))
                    }))
            })
            .collect()
    }

    /// The pointer moved over the cell at data row `row` and `column`
    /// during a drag: the block now reaches it (the whole row when the drag
    /// started on a row number).
    fn drag_over(
        &mut self,
        row: usize,
        column: usize,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(Gesture::Selecting { rows }) = self.gesture else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        self.edge = (0, 0);
        let column = if rows {
            self.result.column_count().saturating_sub(1)
        } else {
            column
        };
        if let Some(selection) = CellSelection::dragged(self.selection, row, column) {
            self.select(Some(selection), cx);
        }
    }

    /// The selected rows side by side: one line per column, one column per
    /// row, so wide rows read top to bottom.
    fn records_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let grid = self.view();
        let rows = grid::selected_rows(self.selection, &grid.rows);
        if rows.is_empty() {
            return column()
                .p_6()
                .gap_2()
                .child(label(tr!("Select one or more rows to see them as records.")))
                .child(label(tr!(
                    "Select cells, rows or a range in the table, then show them here; Esc goes back to the table."
                )))
                .into_any_element();
        }
        let total = rows.len();
        let shown: Rc<[usize]> = rows.into_iter().take(MAX_RECORDS).collect();
        let width = FIELD_WIDTH + shown.len() as f32 * RECORD_WIDTH;
        let header = row()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .gap_0()
            .bg(rgb(GRID_HEADER))
            .border_b_1()
            .border_color(rgb(GRID_LINE))
            .child(
                div()
                    .w(px(FIELD_WIDTH))
                    .flex_none()
                    .px_3()
                    .text_size(px(14.))
                    .text_color(rgb(GRID_HEADER_TEXT))
                    .child(tr!("Column")),
            )
            .children(shown.iter().map(|row| {
                div()
                    .w(px(RECORD_WIDTH))
                    .flex_none()
                    .h_full()
                    .flex()
                    .items_center()
                    .px_3()
                    .border_l_1()
                    .border_color(rgb(GRID_LINE))
                    .text_size(px(14.))
                    .text_color(rgb(GRID_HEADER_TEXT))
                    .child(tr!("Row {0}", self.first_row + row + 1))
            }));
        let records = shown.clone();
        column()
            .size_full()
            .when(total > MAX_RECORDS, |s| {
                s.child(div().px_3().py_1().flex_none().child(label(tr!(
                    "Shows the first {0} of the {1} selected rows.",
                    MAX_RECORDS,
                    cell_format::count(total)
                ))))
            })
            .child(
                div()
                    .id("records-pan")
                    .flex_1()
                    .min_h_0()
                    .overflow_x_scroll()
                    .overflow_y_hidden()
                    .restrict_scroll_to_axis()
                    .track_scroll(&self.records_pan)
                    .child(
                        column()
                            .w(px(width))
                            .min_w_full()
                            .h_full()
                            .child(header)
                            .child(
                                uniform_list(
                                    "record-fields",
                                    self.result.column_count(),
                                    cx.processor(move |this, range: Range<usize>, _, cx| {
                                        this.record_fields(range, &records, cx)
                                    }),
                                )
                                .flex_1()
                                .min_h_0()
                                .map(|mut list| {
                                    list.style().restrict_scroll_to_axis = Some(true);
                                    list
                                })
                                .track_scroll(&self.records_scroll),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The lines of the records view at `range`: one per column.
    fn record_fields(
        &self,
        range: Range<usize>,
        records: &[usize],
        cx: &mut Context<Self>,
    ) -> Vec<Stateful<Div>> {
        let grid = self.view();
        range
            .filter_map(|col| Some((col, self.result.columns().get(col)?)))
            .map(|(col, info)| {
                let family = grid.families.get(col).copied().unwrap_or(ValueFamily::Text);
                row()
                    .id(("record-field", col))
                    .role(Role::Row)
                    .h(px(ROW_HEIGHT))
                    .flex_none()
                    .gap_0()
                    .border_b_1()
                    .border_color(rgb(GRID_LINE))
                    .child(
                        row()
                            .w(px(FIELD_WIDTH))
                            .flex_none()
                            .h_full()
                            .px_3()
                            .gap_1()
                            .bg(rgb(GRID_HEADER))
                            .child(
                                div()
                                    .flex_shrink(1.)
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.))
                                    .text_color(rgb(GRID_HEADER_TEXT))
                                    .child(info.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(rgb(GRID_TYPE_TEXT))
                                    .child(format!(": {}", info.data_type.to_lowercase())),
                            ),
                    )
                    .children(records.iter().map(|row| {
                        let row = *row;
                        let raw = self.result.value(row, col);
                        let displayed = cell_format::display(raw, family, self.numbers_pt_br)
                            .replace("\r\n", "↵")
                            .replace(['\n', '\r'], "↵");
                        let inspected = self.inspected == Some((row, col));
                        div()
                            .id(("record-value", row))
                            .role(Role::GridCell)
                            .aria_label(tr!(
                                "Row {0}, {1}: {2}",
                                self.first_row + row + 1,
                                info.name.as_str(),
                                raw.unwrap_or("NULL")
                            ))
                            .w(px(RECORD_WIDTH))
                            .flex_none()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .items_center()
                            .px(px(8.))
                            .border_l_1()
                            .border_color(rgb(GRID_LINE))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(GRID_HOVER)))
                            .when(inspected, |s| s.bg(rgb(GRID_SELECTED)))
                            .font_family("Cascadia Mono")
                            .text_size(px(13.))
                            .when(raw.is_none(), |s| s.italic())
                            .when(family == ValueFamily::Number, |s| s.justify_end())
                            .text_color(rgb(value_color(raw, family)))
                            // A click picks the value the panel shows; a
                            // double click opens the panel on it.
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    window.focus(&this.focus, cx);
                                    this.inspected = Some((row, col));
                                    this.menu_column = None;
                                    if event.click_count >= 2 {
                                        this.open_inspector(Inspector::Value, cx);
                                    } else {
                                        this.refresh_column_stats(cx);
                                    }
                                    cx.notify();
                                }),
                            )
                            .child(div().truncate().child(displayed))
                    }))
            })
            .collect()
    }

    /// The side panel: the inspected cell's full value, or its column's
    /// statistics.
    fn inspector_panel(&self, inspector: Inspector, cx: &mut Context<Self>) -> Div {
        let tab = |id: &'static str, this: Inspector, title: &'static str| {
            row()
                .id(id)
                .role(Role::Tab)
                .aria_selected(inspector == this)
                .focusable()
                .tab_index(0)
                .focus_visible(|s| s.bg(rgb(HOVER)))
                .h(px(28.))
                .px_2()
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(rgb(if inspector == this { TEXT } else { MUTED }))
                .when(inspector == this, |s| s.bg(rgb(SURFACE)))
                .hover(|s| s.bg(rgb(HOVER)))
                .child(title)
                .on_click(cx.listener(move |grid, _, _, cx| grid.open_inspector(this, cx)))
        };
        column()
            .w(px(PANEL_WIDTH))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                row()
                    .h(px(HEADER_HEIGHT + 6.))
                    .flex_none()
                    .px_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        row()
                            .id("inspect-tabs")
                            .role(Role::TabList)
                            .aria_label(tr!("Inspect"))
                            .flex_1()
                            .gap_1()
                            .child(tab("inspect-value", Inspector::Value, tr!("Value")))
                            .child(tab(
                                "inspect-column",
                                Inspector::Column,
                                tr!("Column statistics"),
                            )),
                    )
                    .child(
                        icon_button("close-inspector", I::X, tr!("Close the panel"))
                            .size(px(24.))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.inspector = None;
                                window.focus(&this.focus, cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(match inspector {
                Inspector::Value => self.value_body(cx),
                Inspector::Column => self.column_body(),
            })
    }

    fn value_body(&self, cx: &mut Context<Self>) -> Div {
        let Some((data_row, col)) = self
            .inspected
            .filter(|(_, col)| *col < self.result.column_count())
        else {
            return column()
                .p_4()
                .child(label(tr!("Select a cell to see its full value.")));
        };
        let info = &self.result.columns()[col];
        let value = self.result.value(data_row, col);
        let caption = tr!(
            "{0} : {1} · row {2}",
            info.name.as_str(),
            info.data_type.to_lowercase(),
            self.first_row + data_row + 1
        );
        column()
            .flex_1()
            .min_h_0()
            .p_3()
            .gap_2()
            .child(label(caption))
            .child(match (value, &self.value_input) {
                (None, _) => div()
                    .flex_1()
                    .text_size(px(13.))
                    .italic()
                    .text_color(rgb(VALUE_NULL))
                    .child("NULL")
                    .into_any_element(),
                (Some(_), Some(input)) => div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        Textarea::new(input)
                            .h_full()
                            .readonly(true)
                            .aria_label(tr!("Full value"))
                            .font_family("Cascadia Mono")
                            .text_size(px(13.)),
                    )
                    .into_any_element(),
                (Some(_), None) => div().flex_1().into_any_element(),
            })
            .child(
                row()
                    .flex_none()
                    .justify_between()
                    .child(label(match value {
                        Some(value) if value.len() > VIEWER_BYTES => tr!(
                            "{0} characters · shows the first 1 MB; Copy copies all of it",
                            cell_format::count(value.chars().count())
                        ),
                        Some(value) => {
                            tr!("{0} characters", cell_format::count(value.chars().count()))
                        }
                        None => String::new(),
                    }))
                    .child(
                        button("copy-value", I::Copy, tr!("Copy"))
                            .on_click(cx.listener(|this, _, _, cx| this.copy_value(cx))),
                    ),
            )
    }

    fn column_body(&self) -> Div {
        let Some(col) = self.stats_column() else {
            return column()
                .p_4()
                .child(label(tr!("Select a cell to see its column's statistics.")));
        };
        let info = &self.result.columns()[col];
        let family = self
            .view()
            .families
            .get(col)
            .copied()
            .unwrap_or(ValueFamily::Text);
        let number =
            |text: &str| cell_format::display(Some(text), ValueFamily::Number, self.numbers_pt_br);
        let value = |text: &str| cell_format::display(Some(text), family, self.numbers_pt_br);
        let line = |name: &'static str, value: String| {
            row().justify_between().gap_3().child(label(name)).child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family("Cascadia Mono")
                    .text_size(px(13.))
                    .text_color(rgb(TEXT))
                    .child(value),
            )
        };
        let body = column()
            .flex_1()
            .min_h_0()
            .p_3()
            .gap_2()
            .child(label(format!(
                "{} : {}",
                info.name,
                info.data_type.to_lowercase()
            )));
        let summary = self
            .column_stats
            .as_ref()
            .filter(|stats| stats.key.2 == col)
            .and_then(|stats| stats.summary.as_ref());
        let Some(summary) = summary else {
            return body.child(
                row()
                    .id("column-stats-status")
                    .role(Role::Status)
                    .gap_2()
                    .child(spinning_icon(
                        "column-stats-spinner",
                        I::LoaderCircle,
                        AMBER,
                    ))
                    .child(label(tr!("Calculating…"))),
            );
        };
        let count = cell_format::count;
        body.child(label(if self.filter.is_empty() {
            tr!("The {0} rows of this page.", count(summary.rows))
        } else {
            tr!(
                "The {0} rows of this page the filter shows.",
                count(summary.rows)
            )
        }))
        .child(line(tr!("NULL"), count(summary.nulls)))
        .child(line(tr!("Distinct values"), count(summary.distinct)))
        .when(family == ValueFamily::Text, |s| {
            s.child(line(tr!("Empty"), count(summary.empty)))
        })
        .when_some(summary.min.as_deref(), |s, min| {
            s.child(line(tr!("Minimum"), value(min)))
        })
        .when_some(summary.max.as_deref(), |s, max| {
            s.child(line(tr!("Maximum"), value(max)))
        })
        .when_some(summary.numbers.as_ref(), |s, numbers| {
            let or_dash = |text: Option<&str>| text.map_or_else(|| "—".to_owned(), number);
            s.child(line(tr!("Sum"), or_dash(numbers.sum.as_deref())))
                .child(line(tr!("Average"), or_dash(numbers.average.as_deref())))
                .child(line(tr!("Median"), or_dash(numbers.median.as_deref())))
        })
        .when(family == ValueFamily::Text, |s| {
            s.when_some(
                summary.shortest.zip(summary.longest),
                |s, (shortest, longest)| {
                    s.child(line(
                        tr!("Length"),
                        tr!("{0} to {1} characters", count(shortest), count(longest)),
                    ))
                },
            )
        })
        .when(!summary.frequent.is_empty(), |s| {
            s.child(div().pt_2().child(label(tr!("Most frequent"))))
                .children(summary.frequent.iter().map(|(text, times)| {
                    row()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family("Cascadia Mono")
                                .text_size(px(13.))
                                .text_color(rgb(value_color(Some(text), family)))
                                .child(value(text).replace(['\n', '\r'], "↵")),
                        )
                        .child(label(count(*times)))
                }))
        })
    }

    /// The grid's context menu.
    fn menu(
        weak: WeakEntity<Self>,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let (has_selection, records, inspector, has_column) = weak
            .upgrade()
            .map(|this| {
                let this = this.read(cx);
                (
                    this.selection.is_some(),
                    this.records,
                    this.inspector,
                    this.stats_column().is_some(),
                )
            })
            .unwrap_or_default();
        let action =
            |weak: WeakEntity<ResultGrid>,
             run: fn(&mut ResultGrid, &mut Window, &mut Context<ResultGrid>)| {
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = weak.update(cx, |this, cx| {
                        run(this, window, cx);
                        cx.notify();
                    });
                }
            };
        let copy_as_weak = weak.clone();
        menu.item(
            PopupMenuItem::new(tr!("Copy"))
                .disabled(!has_selection)
                .on_click(action(weak.clone(), |this, window, cx| {
                    this.copy_selection(false, false, window, cx)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Copy with headers"))
                .disabled(!has_selection)
                .on_click(action(weak.clone(), |this, window, cx| {
                    this.copy_selection(true, false, window, cx)
                })),
        )
        .submenu(tr!("Copy as"), window, cx, move |menu, _, _| {
            CopyFormat::ALL.into_iter().fold(menu, |menu, format| {
                let weak = copy_as_weak.clone();
                menu.item(
                    PopupMenuItem::new(format_name(format))
                        .disabled(!has_selection)
                        .on_click(move |_, window, cx| {
                            let _ =
                                weak.update(cx, |this, cx| this.copy_as(format, false, window, cx));
                        }),
                )
            })
        })
        .separator()
        .item(
            PopupMenuItem::new(tr!("Show the full value"))
                .checked(inspector == Some(Inspector::Value))
                .disabled(!has_selection)
                .on_click(action(weak.clone(), |this, _, cx| {
                    this.toggle_inspector(Inspector::Value, cx)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Column statistics"))
                .checked(inspector == Some(Inspector::Column))
                .disabled(!has_column)
                .on_click(action(weak.clone(), |this, _, cx| {
                    this.toggle_inspector(Inspector::Column, cx)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Show the selected rows as records"))
                .checked(records)
                .on_click(action(weak.clone(), |this, _, cx| {
                    let records = !this.records;
                    this.set_records(records, cx)
                })),
        )
        .separator()
        .item(
            PopupMenuItem::new(tr!("Fit the column to its values"))
                .disabled(!has_column || records)
                .on_click(action(weak.clone(), |this, _, cx| {
                    if let Some(column) = this.stats_column() {
                        this.fit_column(column, cx)
                    }
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Reset column widths"))
                .disabled(records)
                .on_click(action(weak.clone(), |this, _, _| this.widths.clear())),
        )
        .separator()
        .item(
            PopupMenuItem::new(tr!("Select all"))
                .on_click(action(weak.clone(), |this, _, cx| this.select_all(cx))),
        )
    }
}

/// The text colour of a value, by its family ([Results](../../docs/results.md#value-presentation)).
fn value_color(raw: Option<&str>, family: ValueFamily) -> u32 {
    if raw.is_none() {
        VALUE_NULL
    } else if family == ValueFamily::Number {
        VALUE_NUMBER
    } else if family == ValueFamily::Temporal {
        VALUE_TEMPORAL
    } else if family == ValueFamily::Binary {
        VALUE_BINARY
    } else {
        VALUE_TEXT
    }
}

/// What **Copy as** calls a format.
fn format_name(format: CopyFormat) -> &'static str {
    match format {
        CopyFormat::InList => tr!("IN list"),
        CopyFormat::Insert => tr!("INSERT statements"),
        CopyFormat::Json => tr!("JSON"),
        CopyFormat::Markdown => tr!("Markdown table"),
        CopyFormat::ColumnNames => tr!("Column names"),
    }
}

/// The viewer's text for a value: JSON laid out, at most
/// [`VIEWER_BYTES`] of it.
fn viewer_text(value: &str) -> String {
    let text = cell_format::pretty_json(value).unwrap_or_else(|| value.to_owned());
    if text.len() <= VIEWER_BYTES {
        return text;
    }
    let mut end = VIEWER_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Counts the cells of `rows` × `columns` that are not NULL and copies the
/// values of the `numeric` columns into `sample`.
fn read_cells(
    result: &ResultData,
    rows: &[usize],
    columns: &[usize],
    numeric: &[usize],
    sample: &mut Sample,
    values: &mut usize,
) {
    for row in rows {
        for column in columns {
            if result.value(*row, *column).is_some() {
                *values += 1;
            }
        }
        for column in numeric {
            sample.push(result.value(*row, *column));
        }
    }
}

impl Render for ResultGrid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::probe::rendered("ResultGrid");
        self.sync_value(window, cx);
        let weak = cx.weak_entity();
        let menu_weak = weak.clone();
        let main = if self.records {
            self.records_view(cx)
        } else {
            self.table(cx)
        };
        let gesture = self.gesture.is_some();
        div()
            .id("result-grid")
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .flex()
            .bg(rgb(GRID_BG))
            .text_color(rgb(VALUE_TEXT))
            .child(
                div()
                    .id("result-table-area")
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(main)
                    .context_menu(move |menu, window, cx| {
                        Self::menu(menu_weak.clone(), menu, window, cx)
                    })
                    // While a drag is active, every pointer move in the
                    // window reaches the grid, also past its edges, and a
                    // release anywhere ends it.
                    .when(gesture, |s| {
                        let weak = weak.clone();
                        s.child(
                            canvas(
                                |_, _, _| (),
                                move |bounds, _, window, _| {
                                    let moved = weak.clone();
                                    window.on_mouse_event(
                                        move |event: &MouseMoveEvent, phase, _, cx| {
                                            if phase == DispatchPhase::Bubble {
                                                let _ = moved.update(cx, |this, cx| {
                                                    this.pointer_moved(
                                                        event.position,
                                                        event.pressed_button,
                                                        bounds,
                                                        cx,
                                                    )
                                                });
                                            }
                                        },
                                    );
                                    let released = weak.clone();
                                    window.on_mouse_event(
                                        move |event: &MouseUpEvent, phase, _, cx| {
                                            if phase == DispatchPhase::Bubble
                                                && event.button == MouseButton::Left
                                            {
                                                let _ = released
                                                    .update(cx, |this, cx| this.end_gesture(cx));
                                            }
                                        },
                                    );
                                },
                            )
                            .absolute()
                            .inset_0(),
                        )
                    }),
            )
            .when_some(self.inspector, |s, inspector| {
                s.child(self.inspector_panel(inspector, cx))
            })
    }
}
