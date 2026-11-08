//! The result grid: its own entity, embedded in the workspace with
//! `.cached(...)` ([ADR-0012](../../docs/adr/0012-result-grid-as-a-cached-entity.md)).
//!
//! Hover, scroll, selection, drags and copies notify only this entity; the
//! workspace learns what it shows (the selected count, the selection's
//! statistics, notices) through [`ResultGridEvent`]. What the grid derives
//! from a result lives in [`jykre::grid`], [`jykre::stats`] and
//! [`jykre::copy_as`].
use super::copying::format_name;
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarAxis};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::cell_format::{self, ValueFamily};
use jykre::column_layout::{self, ColumnLayout};
use jykre::column_search::{self, ColumnMatch};
use jykre::copy_as::{CopyFormat, CopyOptions, CopyRequest, CopySource};
use jykre::grid::{
    self, CellSelection, ColumnSort, GridCache, GridKey, GridView, Jump, selection_block,
};
use jykre::result::ResultData;
use jykre::result_tabs::ResultId;
use jykre::stats::{self, BlockSummary, ColumnSummary, Sample, SelectionSummary};
use jykre::tr;
use jykre_core::SqlDialect;
use std::collections::HashMap;
use std::time::Duration;
use std::{cell::Cell, ops::Range, rc::Rc, sync::Arc};

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
    /// The side panel opened, closed or changed tab, for the toolbar's
    /// Statistics button.
    PanelChanged,
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

/// Where the user left a result tab: its page, selection, scroll, the
/// column widths they set and how its columns are laid out.
struct Place {
    page: usize,
    selection: Option<CellSelection>,
    scroll: Point<Pixels>,
    widths: HashMap<usize, f32>,
    layout: Arc<ColumnLayout>,
}

/// A column's header being dragged to another place in the header row.
struct ColumnDrag {
    /// The grid it comes from, so another grid ignores it.
    grid: EntityId,
    column: usize,
    name: SharedString,
}

/// What follows the pointer while a header is dragged.
struct ColumnDragPreview {
    name: SharedString,
}

impl Render for ColumnDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        row()
            .h(px(28.))
            .px_3()
            .rounded(px(5.))
            .bg(rgb(colors().surface))
            .border_1()
            .border_color(rgb(colors().mint))
            .text_size(px(12.))
            .text_color(rgb(colors().text))
            .child(self.name.clone())
    }
}

/// The field that finds a column by name, and what it found.
struct ColumnFind {
    input: Entity<InputState>,
    /// The text the matches are for.
    text: String,
    /// Computed when the text or the layout changes, never per frame.
    matches: Vec<ColumnMatch>,
    /// The match `Enter` chooses.
    highlighted: usize,
    scroll: UniformListScrollHandle,
    /// The matches show while the field has the keyboard.
    focused: bool,
    _events: Subscription,
}

/// Matches the list shows at once, and the height of each.
const MATCH_ROWS: usize = 8;
const MATCH_HEIGHT: f32 = 30.;
/// Height of the bar holding the field.
const FIND_BAR_HEIGHT: f32 = 40.;

/// What the rows on screen share while they are built.
struct RowsContext {
    grid: Arc<GridView>,
    /// By result column.
    widths: Vec<f32>,
    /// The selection's display rows and columns.
    block: Option<(
        std::ops::RangeInclusive<usize>,
        std::ops::RangeInclusive<usize>,
    )>,
    /// The focused cell, as (data row, result column).
    focus: Option<(usize, usize)>,
    flexible: bool,
}

/// The demo result's column proportions.
const DEMO_FLEX: [f32; 5] = [1.5, 0.8, 1.2, 1.1, 0.65];

/// What the table lays out sideways in one frame: the frozen columns, the
/// scrolling ones in view, and where they go.
struct Strip {
    /// Display positions `0..frozen` are frozen.
    frozen: usize,
    /// The scrolling columns built, as display positions.
    shown: Range<usize>,
    /// Width of the scrolling columns before `shown`, in pixels.
    before: f32,
    /// Row numbers and frozen columns, in pixels.
    lead: f32,
    /// How far the columns are panned, in pixels.
    pan: f32,
    /// Row numbers and frozen columns stay at the left edge while the rest
    /// pans; without frozen columns the row numbers pan with the others.
    sticky: bool,
    /// The demo's columns share the width in fixed proportions.
    flexible: bool,
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
    /// Statistics of the selection, the page or a column ([`StatsScope`]).
    Statistics,
}

/// What the Statistics tab of the side panel summarizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatsScope {
    /// The selected cells.
    Selection,
    /// Every row of the page the filter shows, all columns.
    Result,
    /// One column, over the rows the filter shows.
    Column,
}

/// The statistics of the page, for the rows the filter shows.
struct ResultStats {
    /// The result version, the layout version and the filter they were read
    /// for.
    key: (u64, u64, String),
    /// `None` while they are being computed.
    summary: Option<BlockSummary>,
}

/// A column's statistics, for the rows the filter shows.
struct ColumnStats {
    /// The result version, the filter and the column they were read for.
    key: (u64, String, usize),
    /// `None` while they are being computed.
    summary: Option<ColumnSummary>,
}

/// The key context of the grid and its bars, where Shift F10 opens the
/// menu ([`super::ResultGridMenu`]).
pub const GRID_CONTEXT: &str = "ResultGrid";
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
    result: Arc<ResultData>,
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
    /// Which columns are shown, in which order and which are frozen: the
    /// result tab's, in memory only.
    layout: Arc<ColumnLayout>,
    /// Bumped with every change of `layout`, so what follows it (the page's
    /// statistics) is computed again.
    layout_version: u64,
    /// The menu Shift F10 opened acts on `menu_column`, whatever the
    /// synthesized click lands on.
    menu_locked: bool,
    /// The table's width at the last layout, which decides the columns in
    /// view.
    viewport: Rc<Cell<f32>>,
    /// The field that finds a column by name, while it is open; its text
    /// also filters the records view's lines.
    column_find: Option<ColumnFind>,
    gesture: Option<Gesture>,
    /// Rows and columns a drag past an edge moves per step.
    edge: (isize, isize),
    autoscrolling: bool,
    autoscroll: Option<Task<()>>,
    /// The column a right click last landed on, for the header's menu.
    menu_column: Option<usize>,
    inspector: Option<Inspector>,
    stats_scope: StatsScope,
    /// The selection's statistics and the summary generation they are for.
    selection_block: Option<(u64, BlockSummary)>,
    /// Whether `selection_block` holds each column's numbers and distinct
    /// values, which only the panel shows.
    selection_detail: bool,
    result_stats: Option<ResultStats>,
    result_task: Option<Task<()>>,
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
    copy_job: Option<super::copying::CopyJob>,
}

impl EventEmitter<ResultGridEvent> for ResultGrid {}

impl super::copying::CopyHost for ResultGrid {
    fn copy_job(&mut self) -> &mut Option<super::copying::CopyJob> {
        &mut self.copy_job
    }

    /// Bumped whenever another page is shown.
    fn copy_epoch(&self) -> u64 {
        self.version
    }

    fn copy_notice(&mut self, feedback: Feedback, cx: &mut Context<Self>) {
        cx.emit(ResultGridEvent::Notice(feedback));
    }
}

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
        } else if phase == "pan" {
            // Sideways, back and forth over the whole width.
            let widths = self.column_widths(&self.view());
            let total = self.display_widths(&widths).iter().sum::<f32>() + ROW_NUMBERS;
            let room = (total - self.viewport.get()).max(1.);
            let pan = (tick as f32 * 157.) % room;
            let offset = self.columns_scroll.offset();
            self.columns_scroll.set_offset(point(px(-pan), offset.y));
            cx.notify();
        } else if phase != "idle" {
            self.move_selection(rows, cols, true, cx);
        }
    }

    /// Freezes the first `columns` columns, for the wide run.
    pub(super) fn benchmark_freeze(&mut self, columns: usize, cx: &mut Context<Self>) {
        self.change_layout(
            |layout| {
                let mut changed = false;
                for column in 0..columns {
                    changed |= layout.freeze(column);
                }
                changed
            },
            cx,
        );
    }

    pub fn new(result: Arc<ResultData>, sales: bool, cx: &mut Context<Self>) -> Self {
        let layout = Arc::new(ColumnLayout::new(result.column_count()));
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
            layout,
            layout_version: 0,
            menu_locked: false,
            viewport: Rc::default(),
            column_find: None,
            gesture: None,
            edge: (0, 0),
            autoscrolling: false,
            autoscroll: None,
            menu_column: None,
            inspector: None,
            stats_scope: StatsScope::Result,
            selection_block: None,
            selection_detail: false,
            result_stats: None,
            result_task: None,
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
            copy_job: None,
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
        result: Arc<ResultData>,
        sales: bool,
        first_row: usize,
        shown: Option<(ResultId, usize)>,
        cx: &mut Context<Self>,
    ) {
        if Arc::ptr_eq(&self.result, &result)
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
                    layout: self.layout.clone(),
                },
            );
        }
        let place =
            shown.and_then(|(id, page)| self.places.remove(&id).filter(|place| place.page == page));
        // A sort chosen for one result tab does not carry to another; the
        // pages of the same tab keep it, their column widths and layout,
        // while the result has the same columns.
        let same_columns = self.result.columns() == result.columns();
        if !same_tab {
            self.sort = None;
            self.widths.clear();
        }
        if !same_tab || !same_columns {
            self.layout = Arc::new(ColumnLayout::new(result.column_count()));
            self.layout_version += 1;
        }
        self.copy_job = None;
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
                if place.layout.len() == self.result.column_count() {
                    self.layout = place.layout;
                    self.layout_version += 1;
                }
                (place.selection, place.scroll)
            }
            None => (None, Point::default()),
        };
        self.scroll.0.borrow().base_handle.set_offset(scroll);
        self.select(selection, cx);
        // Another result has other columns to find.
        self.search_columns(cx);
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
    fn view(&self) -> Arc<GridView> {
        let key = GridKey {
            result: self.version,
            filter: self.filter.clone(),
            sort: self.sort,
            numbers_pt_br: self.numbers_pt_br,
        };
        self.cache.get(&self.result, key)
    }

    /// The width of each column, by result column: the user's where they
    /// set one, otherwise the measured one.
    fn column_widths(&self, grid: &GridView) -> Vec<f32> {
        grid.widths
            .iter()
            .enumerate()
            .map(|(column, width)| self.widths.get(&column).copied().unwrap_or(*width))
            .collect()
    }

    /// The columns' layout, shared with what copies, exports and summarizes
    /// the result.
    pub fn layout(&self) -> Arc<ColumnLayout> {
        self.layout.clone()
    }

    /// Changes the columns' layout with `change`, when it changes anything:
    /// the selection keeps its cells (an end in a hidden column hides it
    /// until the column is shown), and the counts and statistics follow.
    fn change_layout(
        &mut self,
        change: impl FnOnce(&mut ColumnLayout) -> bool,
        cx: &mut Context<Self>,
    ) {
        if !change(Arc::make_mut(&mut self.layout)) {
            return;
        }
        self.layout_version += 1;
        self.select(self.selection, cx);
        // Whether a match is hidden, and their order, follow the layout.
        self.search_columns(cx);
    }

    /// Opens the field that finds a column by name (`Ctrl Shift F` or the
    /// toolbar's button), or gives it the keyboard again with its text
    /// selected.
    pub fn open_column_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.column_find.is_none() {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Find a column by name…")));
            let events =
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::Change => this.search_columns(cx),
                        InputEvent::PressEnter { .. } => this.choose_column_match(window, cx),
                        InputEvent::Focus | InputEvent::Blur => {
                            if let Some(find) = &mut this.column_find {
                                find.focused = matches!(event, InputEvent::Focus);
                            }
                            cx.notify();
                        }
                    }
                });
            self.column_find = Some(ColumnFind {
                input,
                text: String::new(),
                matches: Vec::new(),
                highlighted: 0,
                scroll: UniformListScrollHandle::new(),
                focused: false,
                _events: events,
            });
        }
        if let Some(find) = &self.column_find {
            find.input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        }
        cx.notify();
    }

    /// Closes the field, which stops filtering the records view, and gives
    /// the grid the keyboard.
    fn close_column_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.column_find = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Finds the columns the field's text names, once per change of the
    /// text, the layout or the result; the highlighted match stays when it
    /// still matches.
    fn search_columns(&mut self, cx: &mut Context<Self>) {
        let Some(find) = &mut self.column_find else {
            return;
        };
        let text = find.input.read(cx).value().to_string();
        let previous = find.matches.get(find.highlighted).map(|found| found.column);
        find.matches = column_search::find(self.result.columns(), &self.layout, &text);
        find.text = text;
        let columns: Vec<usize> = find.matches.iter().map(|found| found.column).collect();
        find.highlighted = jykre::palette::keep(previous.as_ref(), &columns);
        find.scroll
            .scroll_to_item(find.highlighted, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// `↑` and `↓` in the field: the match `Enter` chooses, wrapping.
    fn step_column_match(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(find) = &mut self.column_find {
            find.highlighted = jykre::palette::step(find.highlighted, find.matches.len(), delta);
            find.scroll
                .scroll_to_item(find.highlighted, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    /// Goes to the highlighted match.
    fn choose_column_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let column = self
            .column_find
            .as_ref()
            .and_then(|find| find.matches.get(find.highlighted))
            .map(|found| found.column);
        if let Some(column) = column {
            self.go_to_column(column, window, cx);
        }
    }

    /// Shows `column` when the layout hides it, scrolls the grid to it and
    /// selects it over the displayed rows (in the records view, scrolls to
    /// its line), and gives the grid the keyboard. The field stays open with
    /// its text, which keeps filtering the records view.
    fn go_to_column(&mut self, column: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.layout.is_hidden(column) {
            self.change_layout(|layout| layout.show(column), cx);
        }
        let Some(position) = self.layout.position(column) else {
            return;
        };
        if self.records {
            if let Some(line) = self.record_lines().iter().position(|each| *each == column) {
                self.records_scroll
                    .scroll_to_item(line, ScrollStrategy::Nearest);
            }
        } else {
            let view = self.view();
            let selection = CellSelection::click_column(None, &view.rows, column, false);
            if selection.is_some() {
                self.select(selection, cx);
            }
            self.reveal(None, position, &view);
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The column widths in display order.
    fn display_widths(&self, widths: &[f32]) -> Vec<f32> {
        self.layout
            .visible()
            .iter()
            .map(|column| {
                widths
                    .get(*column)
                    .copied()
                    .unwrap_or(grid::MIN_COLUMN_WIDTH)
            })
            .collect()
    }

    /// What the table lays out sideways this frame, from the columns'
    /// widths by result column, the pan and the viewport.
    fn strip(&self, widths: &[f32]) -> Strip {
        let display = self.display_widths(widths);
        let frozen = self.layout.frozen().min(display.len());
        let sticky = frozen > 0;
        let lead = ROW_NUMBERS + display[..frozen].iter().sum::<f32>();
        let pan = (-f32::from(self.columns_scroll.offset().x)).max(0.);
        let viewport = self.viewport.get();
        // The demo's columns grow to fill the grid, so all of them are built.
        let flexible = self.sales && self.layout.is_identity() && self.widths.is_empty();
        let (shown, before) = if flexible {
            (frozen..display.len(), 0.)
        } else {
            column_layout::shown_columns(&display, frozen, lead, pan, viewport)
        };
        Strip {
            frozen,
            shown,
            before,
            lead,
            pan,
            sticky,
            flexible,
        }
    }

    /// Changes the selection, notifies this grid only and reports a new
    /// selected count to the workspace.
    fn select(&mut self, selection: Option<CellSelection>, cx: &mut Context<Self>) {
        self.selection = selection;
        let view = self.view();
        let cells = grid::selected_cells(selection, &view.rows, &self.layout);
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
        self.refresh_result_stats(cx);
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let all = CellSelection::all(&self.view().rows, &self.layout);
        if all.is_some() {
            self.select(all, cx);
        }
    }

    /// Arrow-key navigation; with Shift the block grows from the anchor.
    /// The focused cell scrolls into view, rows and columns.
    fn move_selection(&mut self, rows: isize, cols: isize, extend: bool, cx: &mut Context<Self>) {
        let view = self.view();
        if let Some((selection, (position, column))) =
            CellSelection::moved(self.selection, &view.rows, &self.layout, rows, cols, extend)
        {
            self.select(Some(selection), cx);
            self.reveal(Some(position), column, &view);
        }
    }

    /// Home, End, Page Up, Page Down and Ctrl Home or End; with Shift the
    /// block grows from the anchor.
    fn jump_selection(&mut self, jump: Jump, extend: bool, cx: &mut Context<Self>) {
        let view = self.view();
        if let Some((selection, (position, column))) =
            CellSelection::jumped(self.selection, &view.rows, &self.layout, jump, extend)
        {
            self.select(Some(selection), cx);
            self.reveal(Some(position), column, &view);
        }
    }

    /// Scrolls the row at display `position` (when given) and the column at
    /// display position `column` into view. A frozen column is always in
    /// view; the others pan into the room the frozen ones leave.
    fn reveal(&self, position: Option<usize>, column: usize, view: &GridView) {
        if let Some(position) = position {
            self.scroll
                .scroll_to_item(position, ScrollStrategy::Nearest);
        }
        let frozen = self.layout.frozen();
        if column < frozen {
            return;
        }
        let bounds = self.columns_scroll.bounds();
        let pan = -f32::from(self.columns_scroll.offset().x);
        let display = self.display_widths(&self.column_widths(view));
        let lead = ROW_NUMBERS + display.iter().take(frozen).sum::<f32>();
        if let Some(pan) = grid::pan_to_column(
            display.get(frozen..).unwrap_or_default(),
            column - frozen,
            lead,
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
                // Frozen columns do not pan, so the edge is past them.
                let lead = self.strip(&self.column_widths(&self.view())).lead;
                let left = f32::from(bounds.left()) + lead;
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

    /// Summarizes the selection for the footer and the statistics panel:
    /// at once for a small one, off the UI thread for a large one, reading
    /// the shared page in place. Each column's numbers and distinct values
    /// are summarized only while the panel shows the selection. A drag is
    /// summarized when it ends.
    fn summarize(&mut self, cx: &mut Context<Self>) {
        self.summary_generation += 1;
        self.summary_task = None;
        if matches!(self.gesture, Some(Gesture::Selecting { .. })) {
            return;
        }
        let view = self.view();
        let Some((rows, cols)) = self
            .selection
            .and_then(|selection| selection_block(selection, &view.rows, &self.layout))
        else {
            self.selection_block = None;
            self.report_summary(Summary::None, cx);
            return;
        };
        // The selected columns as shown, in display order.
        let columns: Vec<(usize, ValueFamily)> = grid::block_columns(&self.layout, cols)
            .into_iter()
            .map(|column| {
                let family = view
                    .families
                    .get(column)
                    .copied()
                    .unwrap_or(ValueFamily::Text);
                (column, family)
            })
            .collect();
        let data_rows: Vec<usize> = view.rows[rows].to_vec();
        let cells = data_rows.len() * columns.len();
        if cells < 2 {
            self.selection_block = None;
            self.report_summary(Summary::None, cx);
            return;
        }
        let detail = self.shows_stats(StatsScope::Selection);
        self.selection_detail = detail;
        let generation = self.summary_generation;
        // The details cost about twice as much, so fewer cells are read at
        // once for them.
        let sync = if detail { SYNC_CELLS / 4 } else { SYNC_CELLS };
        if cells <= sync {
            let block = stats::block(&self.result, &data_rows, &columns, detail);
            self.report_summary(Summary::Ready(block.selection()), cx);
            self.selection_block = Some((generation, block));
            return;
        }
        self.report_summary(Summary::Computing, cx);
        // The task shares the page; if it holds the last handle, the page is
        // freed off the UI thread.
        let result = self.result.clone();
        self.summary_task = Some(cx.spawn(async move |this, cx| {
            let block = cx
                .background_executor()
                .spawn(async move { stats::block(&result, &data_rows, &columns, detail) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.summary_generation == generation {
                    this.report_summary(Summary::Ready(block.selection()), cx);
                    this.selection_block = Some((generation, block));
                    cx.notify();
                }
            });
        }));
    }

    /// Whether the side panel shows the statistics of `scope`.
    fn shows_stats(&self, scope: StatsScope) -> bool {
        self.inspector == Some(Inspector::Statistics) && self.stats_scope == scope
    }

    /// The statistics of every row the filter shows, all columns, while the
    /// panel shows them; computed off the UI thread once per result and
    /// filter.
    fn refresh_result_stats(&mut self, cx: &mut Context<Self>) {
        if !self.shows_stats(StatsScope::Result) {
            return;
        }
        let key = (self.version, self.layout_version, self.filter.clone());
        if self
            .result_stats
            .as_ref()
            .is_some_and(|stats| stats.key == key)
        {
            return;
        }
        self.result_stats = Some(ResultStats {
            key: key.clone(),
            summary: None,
        });
        let view = self.view();
        let rows = view.rows.clone();
        // The columns shown, in display order.
        let columns: Vec<(usize, ValueFamily)> = self
            .layout
            .visible()
            .iter()
            .copied()
            .map(|column| {
                let family = view
                    .families
                    .get(column)
                    .copied()
                    .unwrap_or(ValueFamily::Text);
                (column, family)
            })
            .collect();
        let result = self.result.clone();
        self.result_task = Some(cx.spawn(async move |this, cx| {
            let block = cx
                .background_executor()
                .spawn(async move { stats::block(&result, &rows, &columns, true) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(stats) = &mut this.result_stats
                    && stats.key == key
                {
                    stats.summary = Some(block);
                    cx.notify();
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

    /// Shows `inspector` in the side panel, or closes it with `None`, and
    /// computes what it shows.
    fn set_inspector(&mut self, inspector: Option<Inspector>, cx: &mut Context<Self>) {
        if self.inspector != inspector {
            self.inspector = inspector;
            cx.emit(ResultGridEvent::PanelChanged);
        }
        self.refresh_stats(cx);
        cx.notify();
    }

    /// Computes what the Statistics tab shows for its scope: the column's
    /// statistics, the page's, or the selection's with its distinct values.
    fn refresh_stats(&mut self, cx: &mut Context<Self>) {
        self.refresh_column_stats(cx);
        self.refresh_result_stats(cx);
        if self.shows_stats(StatsScope::Selection) && !self.selection_detail {
            self.summarize(cx);
        }
    }

    fn open_inspector(&mut self, inspector: Inspector, cx: &mut Context<Self>) {
        self.set_inspector(Some(inspector), cx);
    }

    /// Shows the statistics of `scope` in the side panel.
    fn open_statistics(&mut self, scope: StatsScope, cx: &mut Context<Self>) {
        if self.stats_scope != scope {
            self.stats_scope = scope;
            cx.emit(ResultGridEvent::PanelChanged);
        }
        self.set_inspector(Some(Inspector::Statistics), cx);
    }

    /// Whether the side panel shows statistics, for the toolbar's button.
    pub fn statistics_shown(&self) -> bool {
        self.inspector == Some(Inspector::Statistics)
    }

    /// The toolbar's Statistics button: opens the panel on the selection's
    /// statistics when two or more cells are selected, otherwise on the
    /// page's; closes it when it already shows statistics.
    pub fn toggle_statistics(&mut self, cx: &mut Context<Self>) {
        if self.statistics_shown() {
            self.set_inspector(None, cx);
            return;
        }
        let selected = grid::selected_cells(self.selection, &self.view().rows, &self.layout);
        let scope = if selected >= 2 {
            StatsScope::Selection
        } else {
            StatsScope::Result
        };
        self.open_statistics(scope, cx);
    }

    /// Shows the statistics of `column`.
    pub fn show_column_statistics(&mut self, column: usize, cx: &mut Context<Self>) {
        self.menu_column = Some(column);
        self.column_stats = None;
        self.open_statistics(StatsScope::Column, cx);
    }

    /// **Column statistics** in the menu: the column right-clicked or
    /// focused, or closes the panel when it already shows them.
    fn toggle_column_statistics(&mut self, cx: &mut Context<Self>) {
        if self.shows_stats(StatsScope::Column) {
            self.set_inspector(None, cx);
            return;
        }
        if self.menu_column.is_none() {
            self.menu_column = self.inspected.map(|(_, column)| column);
        }
        self.open_statistics(StatsScope::Column, cx);
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
        if !self.shows_stats(StatsScope::Column) {
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
        let result = Arc::downgrade(&self.result);
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

    /// Copies the selected block in the format of the copy menu
    /// (`Ctrl C`), with the column names first when `headers`
    /// (`Ctrl Shift C`) or when the menu says so. A copy estimated above
    /// 50 MiB asks first, before the worker generates the text.
    pub(super) fn copy_selection(
        &mut self,
        headers: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let options = super::copying::options(cx);
        self.copy(options.with(options.format, headers), window, cx);
    }

    /// Copies the selected block in `format` (**Copy as**), with the other
    /// choices of the copy menu.
    fn copy_as(&mut self, format: CopyFormat, window: &mut Window, cx: &mut Context<Self>) {
        let options = super::copying::options(cx);
        self.copy(options.with(format, false), window, cx);
    }

    fn copy(&mut self, options: CopyOptions, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selection else {
            cx.emit(ResultGridEvent::Notice(Feedback::new(
                Tone::Info,
                tr!("Select cells to copy."),
            )));
            return;
        };
        let request = self.copy_request(
            CopySource::Selection {
                view: self.view(),
                selection,
            },
            options,
        );
        self.start_copy(request, super::copying::CopyWhat::Selection, window, cx);
    }

    fn copy_request(&self, source: CopySource, options: CopyOptions) -> CopyRequest {
        CopyRequest {
            result: self.result.clone(),
            source,
            options,
            dialect: self.sql.dialect,
            table: self
                .sql
                .table
                .clone()
                .unwrap_or_else(|| "table_name".to_owned()),
            quoted_columns: self
                .result
                .columns()
                .iter()
                .map(|column| (self.sql.quote_name)(&column.name))
                .collect(),
            layout: self.layout.clone(),
        }
    }

    pub(super) fn copy_page_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let options = CopyOptions {
            format: CopyFormat::Csv,
            header: true,
            ..CopyOptions::default()
        };
        let request = self.copy_request(CopySource::Page, options);
        self.start_copy(request, super::copying::CopyWhat::Page, window, cx);
    }

    fn start_copy(
        &mut self,
        request: CopyRequest,
        what: super::copying::CopyWhat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        super::copying::start(self, request, what, window, cx);
    }

    fn cancel_copy(&mut self, cx: &mut Context<Self>) {
        super::copying::cancel(self, cx);
    }

    /// Copies the whole value the viewer shows, never only the part it
    /// shows, off the UI thread like any other copy.
    fn copy_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, column)) = self.inspected else {
            return;
        };
        let request = self.copy_request(CopySource::Value { row, column }, CopyOptions::default());
        self.start_copy(request, super::copying::CopyWhat::Value, window, cx);
    }

    /// `Esc` closes what is on top first: a copy, the panel, the records
    /// view, then the selection. It stays a key of the grid, not an action:
    /// the window's `Dismiss` sees it first and lets it through.
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.key != "escape" || keystroke.modifiers.modified() {
            return;
        }
        if self.copy_job.is_some() {
            self.cancel_copy(cx);
        } else if self.inspector.is_some() {
            self.set_inspector(None, cx);
        } else if self.records {
            self.set_records(false, cx);
        } else if self.selection.is_some() {
            self.select(None, cx);
        } else {
            return;
        }
        window.prevent_default();
        cx.stop_propagation();
    }

    /// A key of the grid's keymap ([`super::shortcuts`]), on the table that
    /// has the keyboard. `Enter` with no cell to show lets the key through.
    fn grid_key(&mut self, key: KeyAction, window: &mut Window, cx: &mut Context<Self>) {
        match key {
            KeyAction::Copy { names } => self.copy_selection(names, window, cx),
            KeyAction::SelectAll => self.select_all(cx),
            KeyAction::Move { rows, cols, extend } => self.move_selection(rows, cols, extend, cx),
            KeyAction::Jump { jump, extend } => self.jump_selection(jump, extend, cx),
            KeyAction::Page { down, extend } => {
                let rows = self.page_rows();
                let rows = if down { rows } else { -rows };
                self.jump_selection(Jump::Rows(rows), extend, cx)
            }
            KeyAction::Inspect if self.inspected.is_some() => {
                self.open_inspector(Inspector::Value, cx)
            }
            KeyAction::Inspect => cx.propagate(),
        }
    }

    fn table(&self, cx: &mut Context<Self>) -> AnyElement {
        let grid = self.view();
        let widths = self.column_widths(&grid);
        let strip = self.strip(&widths);
        let grid_width = self.display_widths(&widths).iter().sum::<f32>() + ROW_NUMBERS;
        let has_rows = !grid.rows.is_empty();
        let visible = self.layout.visible();
        let corner = || {
            div()
                .w(px(ROW_NUMBERS))
                .flex_none()
                .text_center()
                .child(label("#"))
        };
        let mut header = row()
            .relative()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .gap_0()
            .bg(rgb(colors().grid_header))
            .border_b_1()
            .border_color(rgb(colors().grid_line));
        // Only the columns in view are built; the ones panned past keep
        // their room, so the scrollbar and the pan stay those of the whole
        // width.
        header = if strip.sticky {
            header.child(div().w(px(strip.lead)).flex_none())
        } else {
            header.child(corner())
        };
        header = header.child(div().w(px(strip.before)).flex_none());
        for position in strip.shown.clone() {
            if let Some(column) = visible.get(position).copied() {
                let width = widths
                    .get(column)
                    .copied()
                    .unwrap_or(grid::MIN_COLUMN_WIDTH);
                header = header.child(self.header(column, position, width, strip.flexible, cx));
            }
        }
        if strip.sticky {
            let mut frozen = self
                .frozen_part(&strip)
                .bg(rgb(colors().grid_header))
                .child(corner());
            for position in 0..strip.frozen {
                if let Some(column) = visible.get(position).copied() {
                    let width = widths
                        .get(column)
                        .copied()
                        .unwrap_or(grid::MIN_COLUMN_WIDTH);
                    frozen = frozen.child(self.header(column, position, width, false, cx));
                }
            }
            header = header.child(frozen);
        }
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
                    .child(header)
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
        // The columns in view depend on the viewport's width, known after
        // layout: a width other than the one this frame was built for
        // builds the next frame again, once.
        let measured = self.viewport.clone();
        let weak = cx.weak_entity();
        let measure = canvas(
            move |bounds, _, cx| {
                let width = f32::from(bounds.size.width);
                if (width - measured.get()).abs() > 0.5 {
                    measured.set(width);
                    cx.defer(move |cx| {
                        let _ = weak.update(cx, |_, cx| cx.notify());
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        div()
            .size_full()
            .relative()
            .child(measure)
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

    /// The part of a row (or of the header) that stays at the left edge:
    /// the row numbers and the frozen columns, drawn over the columns that
    /// pan under it and taking the pointer from them.
    fn frozen_part(&self, strip: &Strip) -> Div {
        row()
            .absolute()
            .top_0()
            .left(px(strip.pan))
            .h_full()
            .gap_0()
            .border_r_2()
            .border_color(rgb(colors().grid_line))
            .block_mouse_except_scroll()
    }

    /// A column's header: a click sorts by it, `Ctrl` + click selects it,
    /// dragging it moves it, its right edge resizes it (a double click fits
    /// it) and Shift F10 opens the menu on it.
    fn header(
        &self,
        index: usize,
        position: usize,
        width: f32,
        flexible: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let Some(column) = self.result.columns().get(index) else {
            return div().id(("result-header", index));
        };
        let name = &column.name;
        let kind = column.data_type.to_lowercase();
        let tooltip = tr!(
            "{0} : {1} · Click to sort this page by it, Ctrl + click to select it, drag it to move it",
            name,
            kind
        );
        let sorted = self
            .sort
            .filter(|sort| sort.column == index)
            .map(|sort| sort.descending);
        let sort_label = match sorted {
            Some(false) => tr!("{0}, sorted ascending", name),
            Some(true) => tr!("{0}, sorted descending", name),
            None => tr!("{0}, not sorted", name),
        };
        let frozen = position < self.layout.frozen();
        let drag = ColumnDrag {
            grid: cx.entity_id(),
            column: index,
            name: name.clone().into(),
        };
        row()
            .id(("result-header", index))
            .role(Role::ColumnHeader)
            .aria_label(if frozen {
                tr!("{0}, frozen", sort_label)
            } else {
                sort_label
            })
            .relative()
            .w(px(width))
            .flex_none()
            .h_full()
            .px_3()
            .gap_1()
            .when_some(
                DEMO_FLEX
                    .get(index)
                    .copied()
                    .filter(|_| flexible && !self.widths.contains_key(&index)),
                |s, grow| s.flex_grow(grow),
            )
            .border_l_1()
            .border_color(rgb(colors().grid_line))
            .text_size(px(14.))
            .text_color(rgb(colors().grid_header_text))
            .cursor_pointer()
            .focusable()
            .tab_index(0)
            .focus_visible(|s| s.bg(rgb(colors().hover)))
            .hover(|s| s.bg(rgb(colors().grid_hover)))
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
                    .text_color(rgb(colors().grid_type_text))
                    .child(format!(": {kind}")),
            )
            .when_some(sorted, |s, descending| {
                s.child(icon(
                    if descending {
                        I::ArrowDown
                    } else {
                        I::ArrowUp
                    },
                    colors().mint,
                ))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, _| {
                    if !this.menu_locked {
                        this.menu_column = Some(index);
                    }
                }),
            )
            .on_action(cx.listener(move |this, _: &super::ResultGridMenu, window, cx| {
                this.open_column_menu(index, window, cx)
            }))
            // Dropped on another header, the column takes its place.
            .on_drag(drag, |drag, _, _, cx| {
                cx.new(|_| ColumnDragPreview {
                    name: drag.name.clone(),
                })
            })
            .drag_over::<ColumnDrag>(|style, _, _, _| {
                style
                    .bg(rgb(colors().hover))
                    .border_l_2()
                    .border_color(rgb(colors().mint))
            })
            .on_drop(cx.listener(move |this, drag: &ColumnDrag, _, cx| {
                if drag.grid == cx.entity_id() {
                    let moved = drag.column;
                    this.change_layout(|layout| layout.move_onto(moved, index), cx);
                }
            }))
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
                    .hover(|s| s.bg(rgb(colors().grid_focus)))
                    .when(
                        self.gesture.is_some_and(|gesture| {
                            matches!(gesture, Gesture::Resizing { column, .. } if column == index)
                        }),
                        |s| s.bg(rgb(colors().grid_focus)),
                    )
                    // Taken here, so the header neither sorts, selects nor
                    // moves.
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

    /// Opens the grid's menu on `column` from the keyboard (Shift F10), at
    /// its header, panned into view: the menu acts on that column whatever
    /// lies under the point.
    fn open_column_menu(&mut self, column: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(position) = self.layout.position(column) else {
            return;
        };
        let view = self.view();
        self.reveal(None, position, &view);
        let widths = self.column_widths(&view);
        let strip = self.strip(&widths);
        let display = self.display_widths(&widths);
        let width = display.get(position).copied().unwrap_or_default();
        let x = if position < strip.frozen {
            ROW_NUMBERS + display[..position].iter().sum::<f32>()
        } else {
            strip.lead + display[strip.frozen..position].iter().sum::<f32>() - strip.pan
        } + width / 2.;
        let bounds = self.columns_scroll.bounds();
        let x = x.clamp(8., (f32::from(bounds.size.width) - 8.).max(8.));
        let at = point(bounds.left() + px(x), bounds.top() + px(HEADER_HEIGHT / 2.));
        self.menu_column = Some(column);
        self.menu_locked = true;
        let weak = cx.weak_entity();
        // The menu listens for the right button over the table; the press
        // is sent after this key's dispatch ends.
        window.defer(cx, move |window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Right,
                    position: at,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Right,
                    position: at,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                }),
                cx,
            );
            let _ = weak.update(cx, |this, _| this.menu_locked = false);
        });
    }

    /// Shift F10 in the table: the menu on the focused cell's column, or
    /// the first column shown.
    fn open_menu_from_keyboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let column = self
            .inspected
            .map(|(_, column)| column)
            .filter(|column| self.layout.position(*column).is_some())
            .or_else(|| self.layout.column(0));
        if let Some(column) = column {
            self.open_column_menu(column, window, cx);
        }
    }

    /// The data rows at `range` of the displayed order.
    fn result_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let grid = self.view();
        let widths = self.column_widths(&grid);
        let strip = self.strip(&widths);
        let block = self
            .selection
            .and_then(|selection| selection_block(selection, &grid.rows, &self.layout));
        let rows = RowsContext {
            focus: self.selection.map(|selection| selection.focus),
            block,
            grid,
            widths,
            flexible: strip.flexible,
        };
        let shown = self.layout.visible().len();
        range
            .filter_map(|position| Some((position, *rows.grid.rows.get(position)?)))
            .map(|(position, index)| {
                let row_selected = rows.block.as_ref().is_some_and(|(block_rows, cols)| {
                    block_rows.contains(&position) && *cols.start() == 0 && *cols.end() + 1 == shown
                });
                let built: Vec<usize> = (0..strip.frozen).chain(strip.shown.clone()).collect();
                let mut line = row()
                    .id(("data-row", index))
                    .role(Role::Row)
                    // The values on screen, in display order.
                    .aria_label(
                        built
                            .iter()
                            .filter_map(|position| self.layout.column(*position))
                            .map(|column| self.result.value(index, column).unwrap_or("NULL"))
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                    .relative()
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .flex_none()
                    .gap_0();
                let number = self.row_number(index, row_selected, cx);
                let mut number = Some(number);
                line = if strip.sticky {
                    line.child(div().w(px(strip.lead)).flex_none())
                } else {
                    line.children(number.take())
                };
                line = line.child(div().w(px(strip.before)).flex_none());
                for display in strip.shown.clone() {
                    if let Some(column) = self.layout.column(display) {
                        line = line.child(self.cell(&rows, position, index, display, column, cx));
                    }
                }
                if strip.sticky {
                    let mut frozen = self
                        .frozen_part(&strip)
                        .bg(rgb(colors().grid_bg))
                        .children(number.take());
                    for display in 0..strip.frozen {
                        if let Some(column) = self.layout.column(display) {
                            frozen = frozen
                                .child(self.cell(&rows, position, index, display, column, cx));
                        }
                    }
                    line = line.child(frozen);
                }
                line
            })
            .collect()
    }

    /// The number of data row `index`: pressing it selects the row (Shift
    /// extends) and dragging over other numbers selects them all.
    fn row_number(&self, index: usize, selected: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id(("row-number", index))
            .w(px(ROW_NUMBERS))
            .flex_none()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .bg(rgb(if selected {
                colors().grid_selected
            } else {
                colors().grid_header
            }))
            .hover(|s| s.bg(rgb(colors().grid_hover)))
            .border_b_1()
            .border_color(rgb(colors().grid_line))
            .text_center()
            .text_size(px(12.))
            .text_color(rgb(colors().faint))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    let selection = CellSelection::click_row(
                        this.selection,
                        index,
                        &this.layout,
                        event.modifiers.shift,
                    );
                    this.gesture = Some(Gesture::Selecting { rows: true });
                    this.select(Some(selection), cx);
                    window.focus(&this.focus, cx);
                }),
            )
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                let last = this.layout.visible().last().copied().unwrap_or_default();
                this.drag_over(index, last, event, cx)
            }))
            .child((self.first_row + index + 1).to_string())
    }

    /// The cell of data row `index` (at display `position`) and result
    /// `col` (at display position `display`).
    fn cell(
        &self,
        rows: &RowsContext,
        position: usize,
        index: usize,
        display: usize,
        col: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let family = rows
            .grid
            .families
            .get(col)
            .copied()
            .unwrap_or(ValueFamily::Text);
        let width = rows
            .widths
            .get(col)
            .copied()
            .unwrap_or(grid::MIN_COLUMN_WIDTH);
        let raw = self.result.value(index, col);
        // A cell is one line: line breaks show as ↵ and the full value stays
        // in the tooltip and in copies.
        let displayed = cell_format::display(raw, family, self.numbers_pt_br)
            .replace("\r\n", "↵")
            .replace(['\n', '\r'], "↵");
        let is_null = raw.is_none();
        // Only values that do not fit get a tooltip, so it does not cover the
        // context menu or neighbours.
        let tooltip = raw
            .filter(|value| {
                value.contains(['\n', '\r']) || displayed.chars().count() as f32 * 7.8 > width - 18.
            })
            .map(str::to_owned);
        let selected = rows.block.as_ref().is_some_and(|(block_rows, cols)| {
            block_rows.contains(&position) && cols.contains(&display)
        });
        let focused = rows.focus == Some((index, col));
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
            .w(px(width))
            .flex_none()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .px(px(8.))
            .border_1()
            .border_color(rgb(colors().grid_line))
            .when(!selected, |s| s.hover(|s| s.bg(rgb(colors().grid_hover))))
            .when(selected, |s| s.bg(rgb(colors().grid_selected)))
            .when(focused, |s| s.border_color(rgb(colors().grid_focus)))
            // Pressing selects the cell (Shift extends from the anchor) and
            // starts a drag; a double click shows the full value.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    if event.click_count >= 2 {
                        this.open_inspector(Inspector::Value, cx);
                        return;
                    }
                    let selection =
                        CellSelection::click(this.selection, index, col, event.modifiers.shift);
                    this.gesture = Some(Gesture::Selecting { rows: false });
                    this.select(Some(selection), cx);
                }),
            )
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                this.drag_over(index, col, event, cx)
            }))
            // A right click outside the selection selects the cell first, so
            // the menu acts on what is under it.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    if this.menu_locked {
                        return;
                    }
                    this.menu_column = None;
                    if !selected {
                        this.select(Some(CellSelection::cell(index, col)), cx);
                    }
                    window.focus(&this.focus, cx);
                }),
            )
            .when_some(
                DEMO_FLEX
                    .get(col)
                    .copied()
                    .filter(|_| rows.flexible && !self.widths.contains_key(&col)),
                |s, grow| s.flex_grow(grow),
            )
            .font_family(fonts().mono.clone())
            .text_size(px(13.))
            .when(is_null, |s| s.italic())
            .when(family == ValueFamily::Boolean, |s| s.justify_center())
            .when_some(tooltip, |s, tooltip| {
                s.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            })
            .when(family == ValueFamily::Number, |s| s.justify_end())
            .text_color(rgb(value_color(raw, family)))
            .child(div().truncate().child(displayed))
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
            self.layout.visible().last().copied().unwrap_or(column)
        } else {
            column
        };
        if let Some(selection) = CellSelection::dragged(self.selection, row, column) {
            self.select(Some(selection), cx);
        }
    }

    /// How many columns are hidden, while any is, with a way to show them
    /// all: always in view above the table or the records.
    fn hidden_bar(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let hidden = self.layout.hidden();
        (hidden > 0).then(|| {
            row()
                .id("hidden-columns")
                .role(Role::Status)
                .flex_none()
                .h(px(30.))
                .px_3()
                .gap_2()
                .bg(rgb(colors().panel))
                .border_b_1()
                .border_color(rgb(colors().grid_line))
                .child(icon(I::EyeOff, colors().muted))
                .child(label(if hidden == 1 {
                    tr!("1 column hidden").to_string()
                } else {
                    tr!("{0} columns hidden", cell_format::count(hidden))
                }))
                .child(
                    button("show-all-columns", I::Eye, tr!("Show all columns")).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.change_layout(ColumnLayout::show_all, cx);
                            window.focus(&this.focus, cx);
                        }),
                    ),
                )
        })
    }

    /// The selected rows side by side: one line per column, one column per
    /// row, so wide rows read top to bottom.
    fn records_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let grid = self.view();
        let rows = grid::selected_rows(self.selection, &grid.rows, &self.layout);
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
            .bg(rgb(colors().grid_header))
            .border_b_1()
            .border_color(rgb(colors().grid_line))
            .child(
                div()
                    .w(px(FIELD_WIDTH))
                    .flex_none()
                    .px_3()
                    .text_size(px(14.))
                    .text_color(rgb(colors().grid_header_text))
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
                    .border_color(rgb(colors().grid_line))
                    .text_size(px(14.))
                    .text_color(rgb(colors().grid_header_text))
                    .child(tr!("Row {0}", self.first_row + row + 1))
            }));
        let records = shown.clone();
        // One line per column shown, in display order.
        let lines = self.record_lines();
        let count = lines.len();
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
                                    count,
                                    cx.processor(move |this, range: Range<usize>, _, cx| {
                                        this.record_fields(range, &records, &lines, cx)
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

    /// The columns the records view lists, one line each: the shown ones,
    /// in display order; while the field that finds a column has text, only
    /// the shown ones it finds.
    fn record_lines(&self) -> Rc<[usize]> {
        match self
            .column_find
            .as_ref()
            .filter(|find| !find.text.trim().is_empty())
        {
            Some(find) => find
                .matches
                .iter()
                .filter(|found| !found.hidden)
                .map(|found| found.column)
                .collect(),
            None => self.layout.visible().iter().copied().collect(),
        }
    }

    /// The field that finds a column by name, while it is open: the text,
    /// how many columns it finds, and a button that closes it. `↑` and `↓`
    /// choose a match, `Enter` goes to it and `Esc` closes the field.
    fn find_bar(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let find = self.column_find.as_ref()?;
        let count = cell_format::count;
        let status = if find.text.trim().is_empty() {
            tr!("Type part of a column's name.").to_string()
        } else if find.matches.is_empty() {
            tr!("No column matches.").to_string()
        } else {
            tr!(
                "{0} of {1} columns",
                count(find.matches.len()),
                count(self.result.column_count())
            )
        };
        Some(
            row()
                .id("find-column-bar")
                .flex_none()
                .h(px(FIND_BAR_HEIGHT))
                .px_3()
                .gap_2()
                .bg(rgb(colors().panel))
                .border_b_1()
                .border_color(rgb(colors().grid_line))
                .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.modifiers.modified() {
                        return;
                    }
                    match event.keystroke.key.as_str() {
                        "up" => this.step_column_match(-1, cx),
                        "down" => this.step_column_match(1, cx),
                        "escape" => this.close_column_search(window, cx),
                        _ => return,
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                }))
                .child(
                    div().w(px(320.)).flex_none().child(
                        Input::new(&find.input)
                            .small()
                            .prefix(I::TextSearch)
                            .w_full()
                            .aria_label(tr!("Find a column by name")),
                    ),
                )
                .child(
                    div()
                        .id("find-column-status")
                        .role(Role::Status)
                        .min_w_0()
                        .truncate()
                        .child(label(status)),
                )
                .child(div().flex_1())
                .child(
                    icon_button("close-column-search", I::X, tr!("Close the column search"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.close_column_search(window, cx)),
                        ),
                ),
        )
    }

    /// The matches under the field while it has the keyboard, drawn over
    /// the table; only the lines on screen are built.
    fn match_list(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let find = self
            .column_find
            .as_ref()
            .filter(|find| find.focused && !find.matches.is_empty())?;
        let height = find.matches.len().min(MATCH_ROWS) as f32 * MATCH_HEIGHT;
        Some(
            div()
                .id("column-matches")
                .role(Role::ListBox)
                .aria_label(tr!("Columns found"))
                .absolute()
                .top(px(FIND_BAR_HEIGHT))
                .left(px(12.))
                .w(px(460.))
                .h(px(height + 2.))
                .bg(rgb(colors().panel))
                .border_1()
                .border_color(rgb(colors().border))
                .rounded_md()
                .overflow_hidden()
                .block_mouse_except_scroll()
                .child(
                    uniform_list(
                        "column-match-lines",
                        find.matches.len(),
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            this.match_lines(range, cx)
                        }),
                    )
                    .size_full()
                    .track_scroll(&find.scroll),
                ),
        )
    }

    /// The matches at `range`: the name with what matched marked, its
    /// type, and whether the layout hides it (choosing it shows it).
    fn match_lines(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let Some(find) = &self.column_find else {
            return Vec::new();
        };
        range
            .filter_map(|line| Some((line, find.matches.get(line)?)))
            .map(|(line, found)| {
                let info = self.result.columns().get(found.column);
                let name = info.map(|info| info.name.clone()).unwrap_or_default();
                let kind = info
                    .map(|info| info.data_type.to_lowercase())
                    .unwrap_or_default();
                let chosen = line == find.highlighted;
                row()
                    .id(("column-match", found.column))
                    .role(Role::ListBoxOption)
                    .aria_selected(chosen)
                    .aria_label(if found.hidden {
                        tr!("{0}, hidden: choosing it shows it", name)
                    } else {
                        name.clone()
                    })
                    .h(px(MATCH_HEIGHT))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .cursor_pointer()
                    .when(chosen, |s| s.bg(rgb(colors().hover)))
                    .hover(|s| s.bg(rgb(colors().hover)))
                    .child(
                        div()
                            .flex_shrink(1.)
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(rgb(colors().text))
                            .child(marked(name, found.ranges.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(rgb(colors().grid_type_text))
                            .child(format!(": {kind}")),
                    )
                    .when(found.hidden, |s| {
                        s.child(badge(tr!("Hidden: choosing it shows it"), colors().amber))
                    })
                    // A press, not a click: the field keeps the keyboard
                    // until the grid takes it.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            if let Some(find) = &mut this.column_find {
                                find.highlighted = line;
                            }
                            this.choose_column_match(window, cx);
                        }),
                    )
            })
            .collect()
    }

    /// The lines of the records view at `range` of `lines`: one per column.
    fn record_fields(
        &self,
        range: Range<usize>,
        records: &[usize],
        lines: &[usize],
        cx: &mut Context<Self>,
    ) -> Vec<Stateful<Div>> {
        let grid = self.view();
        range
            .filter_map(|line| lines.get(line).copied())
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
                    .border_color(rgb(colors().grid_line))
                    .child(
                        row()
                            .w(px(FIELD_WIDTH))
                            .flex_none()
                            .h_full()
                            .px_3()
                            .gap_1()
                            .bg(rgb(colors().grid_header))
                            .child(
                                div()
                                    .flex_shrink(1.)
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.))
                                    .text_color(rgb(colors().grid_header_text))
                                    .child(info.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors().grid_type_text))
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
                            .border_color(rgb(colors().grid_line))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(colors().grid_hover)))
                            .when(inspected, |s| s.bg(rgb(colors().grid_selected)))
                            .font_family(fonts().mono.clone())
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
                .focus_visible(|s| s.bg(rgb(colors().hover)))
                .h(px(28.))
                .px_2()
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(rgb(if inspector == this {
                    colors().text
                } else {
                    colors().muted
                }))
                .when(inspector == this, |s| s.bg(rgb(colors().surface)))
                .hover(|s| s.bg(rgb(colors().hover)))
                .child(title)
                .on_click(cx.listener(move |grid, _, _, cx| grid.open_inspector(this, cx)))
        };
        column()
            .w(px(PANEL_WIDTH))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(rgb(colors().border))
            .bg(rgb(colors().panel))
            .child(
                row()
                    .h(px(HEADER_HEIGHT + 6.))
                    .flex_none()
                    .px_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(colors().border))
                    .child(
                        row()
                            .id("inspect-tabs")
                            .role(Role::TabList)
                            .aria_label(tr!("Inspect"))
                            .flex_1()
                            .gap_1()
                            .child(tab("inspect-value", Inspector::Value, tr!("Value")))
                            .child(tab(
                                "inspect-statistics",
                                Inspector::Statistics,
                                tr!("Statistics"),
                            )),
                    )
                    .child(
                        icon_button("close-inspector", I::X, tr!("Close the panel"))
                            .size(px(24.))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_inspector(None, cx);
                                window.focus(&this.focus, cx);
                            })),
                    ),
            )
            .child(match inspector {
                Inspector::Value => self.value_body(cx),
                Inspector::Statistics => self.statistics_body(cx),
            })
    }

    /// The Statistics tab: what it summarizes, then that summary.
    fn statistics_body(&self, cx: &mut Context<Self>) -> Div {
        let scope = |id: &'static str, this: StatsScope, title: &'static str| {
            let chosen = self.stats_scope == this;
            div()
                .id(id)
                .role(Role::Tab)
                .aria_selected(chosen)
                .focusable()
                .tab_index(0)
                .focus_visible(|s| s.border_color(rgb(colors().mint)))
                .flex_1()
                .h(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .border_1()
                .border_color(transparent_black())
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(rgb(if chosen {
                    colors().text
                } else {
                    colors().muted
                }))
                .when(chosen, |s| s.bg(rgb(colors().hover)))
                .hover(|s| s.text_color(rgb(colors().text)))
                .child(title)
                .on_click(cx.listener(move |grid, _, _, cx| grid.open_statistics(this, cx)))
        };
        column()
            .flex_1()
            .min_h_0()
            .child(
                row()
                    .id("stats-scopes")
                    .role(Role::TabList)
                    .aria_label(tr!("Statistics of"))
                    .flex_none()
                    .m_3()
                    .mb_0()
                    .p(px(2.))
                    .gap(px(2.))
                    .rounded(px(6.))
                    .bg(rgb(colors().surface))
                    .child(scope(
                        "stats-selection",
                        StatsScope::Selection,
                        tr!("Selection"),
                    ))
                    .child(scope("stats-result", StatsScope::Result, tr!("Result")))
                    .child(scope("stats-column", StatsScope::Column, tr!("Column"))),
            )
            // A block, not a flex column: what it holds keeps its height and
            // scrolls, instead of shrinking to fit.
            .child(
                div()
                    // Each scope keeps its own scroll.
                    .id(("stats-body", self.stats_scope as usize))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(match self.stats_scope {
                        StatsScope::Selection => self.selection_stats_body(cx),
                        StatsScope::Result => self.result_stats_body(cx),
                        StatsScope::Column => self.column_body(),
                    }),
            )
    }

    fn selection_stats_body(&self, cx: &mut Context<Self>) -> Div {
        let summary = self
            .selection_block
            .as_ref()
            .filter(|(generation, _)| *generation == self.summary_generation)
            .map(|(_, block)| block);
        let selected = grid::selected_cells(self.selection, &self.view().rows, &self.layout);
        if selected < 2 {
            return column()
                .p_3()
                .gap_2()
                .child(label(tr!(
                    "Select two or more cells to see their statistics."
                )))
                .child(
                    button(
                        "stats-show-result",
                        I::Table2,
                        tr!("Statistics of the result"),
                    )
                    .on_click(
                        cx.listener(|this, _, _, cx| this.open_statistics(StatsScope::Result, cx)),
                    ),
                );
        }
        let Some(summary) = summary else {
            return calculating("selection-stats-status");
        };
        let caption = tr!(
            "{0} rows × {1} columns selected.",
            cell_format::count(summary.rows),
            cell_format::count(summary.columns.len())
        );
        self.block_body(summary, caption, true, cx)
    }

    fn result_stats_body(&self, cx: &mut Context<Self>) -> Div {
        let summary = self
            .result_stats
            .as_ref()
            .filter(|stats| stats.key == (self.version, self.layout_version, self.filter.clone()))
            .and_then(|stats| stats.summary.as_ref());
        let Some(summary) = summary else {
            return calculating("result-stats-status");
        };
        let count = cell_format::count;
        let rows = if self.filter.is_empty() {
            tr!("The {0} rows of this page.", count(summary.rows))
        } else {
            tr!(
                "The {0} rows of this page the filter shows.",
                count(summary.rows)
            )
        };
        let caption = tr!("{0} {1} columns.", rows, count(summary.columns.len()));
        self.block_body(summary, caption, false, cx)
    }

    /// The statistics of a block of cells: its counts, its numbers, then one
    /// line per column that opens that column's statistics.
    /// `totals` also sums the numbers of all its columns together, which
    /// means something for cells chosen together, not for a whole page.
    fn block_body(
        &self,
        summary: &BlockSummary,
        caption: String,
        totals: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let count = cell_format::count;
        let number =
            |text: &str| cell_format::display(Some(text), ValueFamily::Number, self.numbers_pt_br);
        let or_dash = |text: Option<&str>| text.map_or_else(|| "—".to_owned(), number);
        let view = self.view();
        let families = &view.families;
        let shown = summary.columns.len().min(STATS_COLUMNS);
        column()
            .p_3()
            .gap_2()
            .child(label(caption))
            .child(split_bar(
                "block-split",
                &[
                    (summary.values, colors().chart_fill),
                    (summary.nulls, colors().chart_null),
                ],
                tr!(
                    "{0} values, {1} NULL",
                    count(summary.values),
                    count(summary.nulls)
                ),
            ))
            .child(swatch_line(
                tr!("Values"),
                count(summary.values),
                colors().chart_fill,
            ))
            .child(swatch_line(
                tr!("NULL"),
                count(summary.nulls),
                colors().chart_null,
            ))
            .when_some(summary.numbers.as_ref().filter(|_| totals), |s, numbers| {
                s.child(
                    div()
                        .pt_2()
                        .child(label(tr!("{0} numbers", count(numbers.count)))),
                )
                .child(stat_line(tr!("Sum"), or_dash(numbers.sum.as_deref())))
                .child(stat_line(
                    tr!("Average"),
                    or_dash(numbers.average.as_deref()),
                ))
                .child(stat_line(tr!("Median"), or_dash(numbers.median.as_deref())))
                .child(stat_line(tr!("Minimum"), number(&numbers.min)))
                .child(stat_line(tr!("Maximum"), number(&numbers.max)))
            })
            .child(div().pt_2().child(label(tr!("Columns"))))
            .children(summary.columns[..shown].iter().map(|entry| {
                let col = entry.column;
                let info = &self.result.columns()[col];
                let family = families.get(col).copied().unwrap_or(ValueFamily::Text);
                let mut details = Vec::new();
                match (&entry.numbers, family) {
                    (Some(numbers), ValueFamily::Number) => {
                        details.push(tr!("sum {0}", or_dash(numbers.sum.as_deref())));
                        details.push(tr!("average {0}", or_dash(numbers.average.as_deref())));
                    }
                    _ => {
                        if let Some(distinct) = entry.distinct {
                            details.push(tr!("{0} distinct", count(distinct)));
                        }
                    }
                }
                if entry.nulls > 0 {
                    details.push(tr!("{0} NULL", count(entry.nulls)));
                }
                if details.is_empty() {
                    details.push(tr!("{0} values", count(entry.values)));
                }
                row()
                    .id(("stats-column-entry", col))
                    .role(Role::Button)
                    .aria_label(tr!(
                        "Statistics of the column {0}, {1} filled",
                        info.name.as_str(),
                        percent(entry.values, entry.values + entry.nulls)
                    ))
                    .focusable()
                    .tab_index(0)
                    .focus_visible(|s| s.border_color(rgb(colors().mint)))
                    .flex_none()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(5.))
                    .border_1()
                    .border_color(transparent_black())
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(colors().hover)))
                    .child(
                        column()
                            .flex_1()
                            .min_w_0()
                            .child(
                                row()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(13.))
                                            .text_color(rgb(colors().text))
                                            .child(info.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(12.))
                                            .text_color(rgb(colors().faint))
                                            .child(info.data_type.to_lowercase()),
                                    ),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors().muted))
                                    .child(details.join(" · ")),
                            )
                            // How much of the column holds a value.
                            .child(div().pt_1().child(meter(
                                entry.values as f32 / (entry.values + entry.nulls).max(1) as f32,
                            ))),
                    )
                    .child(icon(I::ChevronRight, colors().faint))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.show_column_statistics(col, cx)),
                    )
            }))
            .when(summary.columns.len() > shown, |s| {
                s.child(label(tr!(
                    "And {0} more columns; their header's menu shows their statistics.",
                    count(summary.columns.len() - shown)
                )))
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
                    .text_color(rgb(colors().value_null))
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
                            .font_family(fonts().mono.clone())
                            .text_size(px(13.)),
                    )
                    .into_any_element(),
                (Some(_), None) => div().flex_1().into_any_element(),
            })
            .child(
                row()
                    .flex_none()
                    .gap_2()
                    .justify_between()
                    // The text wraps, so a long note (or a longer language)
                    // never pushes Copy out of the panel.
                    .child(
                        label(match value {
                            Some(value) if value.len() > VIEWER_BYTES => tr!(
                                "{0} characters · shows the first 1 MB; Copy copies all of it",
                                cell_format::count(value.chars().count())
                            ),
                            Some(value) => {
                                tr!("{0} characters", cell_format::count(value.chars().count()))
                            }
                            None => String::new(),
                        })
                        .flex_1()
                        .min_w_0(),
                    )
                    .child(div().flex_none().child(
                        button("copy-value", I::Copy, tr!("Copy")).on_click(
                            cx.listener(|this, _, window, cx| this.copy_value(window, cx)),
                        ),
                    )),
            )
    }

    /// A number column's distribution: one bar per equal slice of its range,
    /// the smallest and largest value under it, and on hover how many
    /// values a bar holds and its own smallest and largest.
    fn histogram_chart(&self, bins: &[stats::Bin]) -> Div {
        let tallest = bins.iter().map(|bin| bin.count).max().unwrap_or(0).max(1);
        let total: usize = bins.iter().map(|bin| bin.count).sum();
        let pt_br = self.numbers_pt_br;
        let number = move |text: &str| cell_format::display(Some(text), ValueFamily::Number, pt_br);
        let low = bins
            .iter()
            .find_map(|bin| bin.min.clone())
            .unwrap_or_default();
        let high = bins
            .iter()
            .rev()
            .find_map(|bin| bin.max.clone())
            .unwrap_or_default();
        column()
            .flex_none()
            .gap_1()
            .child(
                row()
                    .id("column-histogram")
                    .role(Role::Image)
                    .aria_label(tr!(
                        "Distribution of {0} numbers from {1} to {2}",
                        cell_format::count(total),
                        number(&low),
                        number(&high)
                    ))
                    .h(px(56.))
                    .items_end()
                    .gap(px(2.))
                    .border_b_1()
                    .border_color(rgb(colors().grid_line))
                    .children(bins.iter().enumerate().map(|(index, bin)| {
                        let bin = bin.clone();
                        let share = bin.count as f32 / tallest as f32;
                        div()
                            .id(("histogram-bin", index))
                            .flex_1()
                            .h_full()
                            .flex()
                            .flex_col()
                            .justify_end()
                            .rounded_t(px(4.))
                            .hover(|s| s.bg(rgb(colors().hover)))
                            .when(bin.count > 0, |s| {
                                s.child(
                                    div()
                                        .w_full()
                                        .h(relative(share))
                                        .min_h(px(2.))
                                        .rounded_t(px(4.))
                                        .bg(rgb(colors().chart_fill)),
                                )
                            })
                            // Built only when shown.
                            .tooltip(move |window, cx| {
                                let text = match (&bin.min, &bin.max) {
                                    (Some(min), Some(max)) if min == max => tr!(
                                        "{0} values of {1}",
                                        cell_format::count(bin.count),
                                        number(min)
                                    ),
                                    (Some(min), Some(max)) => tr!(
                                        "{0} values from {1} to {2}",
                                        cell_format::count(bin.count),
                                        number(min),
                                        number(max)
                                    ),
                                    _ => tr!("No values in this range").to_string(),
                                };
                                Tooltip::new(text).build(window, cx)
                            })
                    })),
            )
            .child(
                row()
                    .justify_between()
                    .gap_3()
                    .child(label(number(&low)))
                    .child(label(number(&high))),
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
        let line = stat_line;
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
            return body.child(calculating("column-stats-status").p_0());
        };
        let count = cell_format::count;
        let text = family == ValueFamily::Text;
        let filled = summary.rows - summary.nulls - if text { summary.empty } else { 0 };
        body.child(label(if self.filter.is_empty() {
            tr!("The {0} rows of this page.", count(summary.rows))
        } else {
            tr!(
                "The {0} rows of this page the filter shows.",
                count(summary.rows)
            )
        }))
        .child(split_bar(
            "column-split",
            &[
                (filled, colors().chart_fill),
                (summary.nulls, colors().chart_null),
                (if text { summary.empty } else { 0 }, colors().chart_empty),
            ],
            tr!(
                "{0} values, {1} NULL, {2} empty",
                count(filled),
                count(summary.nulls),
                count(if text { summary.empty } else { 0 })
            ),
        ))
        .child(swatch_line(
            tr!("Values"),
            count(filled),
            colors().chart_fill,
        ))
        .child(swatch_line(
            tr!("NULL"),
            count(summary.nulls),
            colors().chart_null,
        ))
        .when(text, |s| {
            s.child(swatch_line(
                tr!("Empty"),
                count(summary.empty),
                colors().chart_empty,
            ))
        })
        .child(line(tr!("Distinct values"), count(summary.distinct)))
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
        .when(summary.histogram.len() > 1, |s| {
            s.child(div().pt_2().child(label(tr!("Distribution"))))
                .child(self.histogram_chart(&summary.histogram))
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
                    // Each value's share of the rows, on one scale.
                    column()
                        .flex_none()
                        .gap_1()
                        .child(
                            row()
                                .justify_between()
                                .gap_3()
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(fonts().mono.clone())
                                        .text_size(px(13.))
                                        .text_color(rgb(value_color(Some(text), family)))
                                        .child(value(text).replace(['\n', '\r'], "↵")),
                                )
                                .child(label(tr!(
                                    "{0} · {1}",
                                    count(*times),
                                    percent(*times, summary.rows)
                                ))),
                        )
                        .child(meter(*times as f32 / summary.rows.max(1) as f32))
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
        let (has_selection, records, inspector, has_column, column_stats) = weak
            .upgrade()
            .map(|this| {
                let this = this.read(cx);
                (
                    this.selection.is_some(),
                    this.records,
                    this.inspector,
                    this.stats_column().is_some(),
                    this.shows_stats(StatsScope::Column),
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
        let format = super::copying::options(cx).format;
        // The keys in use, named in the items.
        let copy_key = super::shortcuts::label::<super::shortcuts::GridCopy>(cx);
        let copy_names_key = super::shortcuts::label::<super::shortcuts::GridCopyWithNames>(cx);
        let find_key = super::shortcuts::label::<super::FindColumn>(cx);
        // What the column items act on: the column right-clicked, else the
        // focused one.
        let columns = weak
            .upgrade()
            .map(|this| {
                let this = this.read(cx);
                let layout = this.layout.clone();
                let column = this
                    .stats_column()
                    .filter(|column| layout.position(*column).is_some());
                let hidden: Vec<(usize, SharedString)> = layout
                    .hidden_columns()
                    .take(HIDDEN_IN_MENU)
                    .filter_map(|column| {
                        let name = this.result.columns().get(column)?.name.clone();
                        Some((column, name.into()))
                    })
                    .collect();
                (layout, column, hidden)
            })
            .unwrap_or_default();
        let (layout, column, hidden) = columns;
        let can = |delta: isize| column.is_some_and(|column| layout.can_move(column, delta));
        let frozen = column.is_some_and(|column| layout.is_frozen(column));
        let hidden_count = layout.hidden();
        let column_action =
            move |weak: WeakEntity<ResultGrid>, change: fn(&mut ColumnLayout, usize) -> bool| {
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(column) = column {
                            this.change_layout(|layout| change(layout, column), cx);
                        }
                    });
                }
            };
        let show_weak = weak.clone();
        menu.item(
            PopupMenuItem::new(tr!("Copy as {0} ({1})", format_name(format), copy_key))
                .disabled(!has_selection)
                .on_click(action(weak.clone(), |this, window, cx| {
                    this.copy_selection(false, window, cx)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Copy with headers ({0})", copy_names_key))
                .disabled(!has_selection)
                .on_click(action(weak.clone(), |this, window, cx| {
                    this.copy_selection(true, window, cx)
                })),
        )
        .submenu(tr!("Copy as"), window, cx, move |menu, _, _| {
            CopyFormat::ALL.into_iter().fold(menu, |menu, format| {
                let weak = copy_as_weak.clone();
                menu.item(
                    PopupMenuItem::new(format_name(format))
                        .disabled(!has_selection)
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| this.copy_as(format, window, cx));
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
                    if this.inspector == Some(Inspector::Value) {
                        this.set_inspector(None, cx)
                    } else {
                        this.open_inspector(Inspector::Value, cx)
                    }
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Statistics"))
                .checked(inspector == Some(Inspector::Statistics) && !column_stats)
                .on_click(action(weak.clone(), |this, _, cx| {
                    if this.shows_stats(StatsScope::Column) {
                        // From a column's statistics to the selection's or the page's.
                        this.set_inspector(None, cx);
                    }
                    this.toggle_statistics(cx)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Column statistics"))
                .checked(column_stats)
                .disabled(!has_column)
                .on_click(action(weak.clone(), |this, _, cx| {
                    this.toggle_column_statistics(cx)
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
            PopupMenuItem::new(tr!("Hide the column"))
                .disabled(column.is_none() || layout.visible().len() <= 1)
                .on_click(column_action(weak.clone(), ColumnLayout::hide)),
        )
        .item(
            PopupMenuItem::new(tr!("Freeze the column at the left"))
                .checked(frozen)
                .disabled(column.is_none())
                .on_click(column_action(weak.clone(), |layout, column| {
                    if layout.is_frozen(column) {
                        layout.unfreeze(column)
                    } else {
                        layout.freeze(column)
                    }
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Move the column left"))
                .disabled(!can(-1))
                .on_click(column_action(weak.clone(), |layout, column| {
                    layout.move_by(column, -1)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Move the column right"))
                .disabled(!can(1))
                .on_click(column_action(weak.clone(), |layout, column| {
                    layout.move_by(column, 1)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Move the column to the start"))
                .disabled(!can(-1))
                .on_click(column_action(weak.clone(), |layout, column| {
                    layout.move_by(column, isize::MIN)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Move the column to the end"))
                .disabled(!can(1))
                .on_click(column_action(weak.clone(), |layout, column| {
                    layout.move_by(column, isize::MAX)
                })),
        )
        .submenu(
            tr!("Show a hidden column"),
            window,
            cx,
            move |menu, _, _| {
                let more = hidden_count.saturating_sub(hidden.len());
                let menu = if hidden.is_empty() {
                    menu.item(PopupMenuItem::new(tr!("No column is hidden")).disabled(true))
                } else {
                    menu
                };
                let menu = hidden.iter().fold(menu, |menu, (column, name)| {
                    let (weak, column) = (show_weak.clone(), *column);
                    menu.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.change_layout(|layout| layout.show(column), cx)
                        });
                    }))
                });
                if more > 0 {
                    menu.item(
                        PopupMenuItem::new(tr!(
                            "{0} more: find them by name ({1})",
                            cell_format::count(more),
                            find_key
                        ))
                        .disabled(true),
                    )
                } else {
                    menu
                }
            },
        )
        .item(
            PopupMenuItem::new(tr!(
                "Show all columns ({0} hidden)",
                cell_format::count(hidden_count)
            ))
            .disabled(hidden_count == 0)
            .on_click(action(weak.clone(), |this, _, cx| {
                this.change_layout(ColumnLayout::show_all, cx)
            })),
        )
        .item(
            PopupMenuItem::new(tr!("Reset the column layout"))
                .disabled(layout.is_identity())
                .on_click(action(weak.clone(), |this, _, cx| {
                    this.change_layout(ColumnLayout::reset, cx)
                })),
        )
        .separator()
        .item(
            PopupMenuItem::new(tr!("Select all"))
                .on_click(action(weak.clone(), |this, _, cx| this.select_all(cx))),
        )
    }
}

/// The text colour of a value, by its family ([Results](../../docs/reference/results.md#value-presentation)).
fn value_color(raw: Option<&str>, family: ValueFamily) -> u32 {
    if raw.is_none() {
        colors().value_null
    } else if family == ValueFamily::Number {
        colors().value_number
    } else if family == ValueFamily::Temporal {
        colors().value_temporal
    } else if family == ValueFamily::Binary {
        colors().value_binary
    } else {
        colors().value_text
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
/// Columns the statistics of a block list; the others say how many more.
const STATS_COLUMNS: usize = 100;
/// Hidden columns the menu lists by name; finding a column reaches the rest.
const HIDDEN_IN_MENU: usize = 20;

/// A statistic: its name, then its value in the values' font.
fn stat_line(name: &'static str, value: String) -> Div {
    row().justify_between().gap_3().child(label(name)).child(
        div()
            .min_w_0()
            .truncate()
            .font_family(fonts().mono.clone())
            .text_size(px(13.))
            .text_color(rgb(colors().text))
            .child(value),
    )
}

/// A statistic with the swatch of its part in the bar above it, which
/// stands for that bar's legend.
fn swatch_line(name: &'static str, value: String, color: u32) -> Div {
    row()
        .justify_between()
        .gap_3()
        .child(
            row()
                .gap_2()
                .child(
                    div()
                        .size(px(8.))
                        .flex_none()
                        .rounded(px(2.))
                        .bg(rgb(color)),
                )
                .child(label(name)),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_family(fonts().mono.clone())
                .text_size(px(13.))
                .text_color(rgb(colors().text))
                .child(value),
        )
}

/// A thin bar split into `parts` (count, colour): widths in proportion,
/// a 2px gap between parts, rounded ends, and nothing for a part of zero.
/// `name` says the whole split to assistive technologies.
fn split_bar(id: &'static str, parts: &[(usize, u32)], name: String) -> Stateful<Div> {
    let shown: Vec<(usize, u32)> = parts.iter().copied().filter(|(n, _)| *n > 0).collect();
    let last = shown.len().saturating_sub(1);
    row()
        .id(id)
        .role(Role::Image)
        .aria_label(name)
        .flex_none()
        .w_full()
        .h(px(8.))
        .gap(px(2.))
        .children(
            shown
                .into_iter()
                .enumerate()
                .map(move |(index, (n, color))| {
                    div()
                        .h_full()
                        // Widths share what the gaps leave, in proportion.
                        .flex_basis(px(0.))
                        .flex_grow(n as f32)
                        .min_w(px(2.))
                        .bg(rgb(color))
                        .when(index == 0, |s| s.rounded_l(px(4.)))
                        .when(index == last, |s| s.rounded_r(px(4.)))
                }),
        )
}

/// A thin meter: `share` (0 to 1) of its track filled.
fn meter(share: f32) -> Div {
    div()
        .flex_none()
        .w_full()
        .h(px(4.))
        .rounded(px(2.))
        .bg(rgb(colors().chart_track))
        .child(
            div()
                .h_full()
                .w(relative(share.clamp(0., 1.)))
                .rounded(px(2.))
                .bg(rgb(colors().chart_fill)),
        )
}

/// `part` of `whole` as a whole percentage, for the meters' text.
fn percent(part: usize, whole: usize) -> String {
    if whole == 0 {
        return "0%".to_owned();
    }
    let share = part as f64 * 100. / whole as f64;
    // Never 0% for some, nor 100% for not all.
    let shown = if part > 0 && share < 1. {
        "<1".to_owned()
    } else if part < whole && share > 99. {
        ">99".to_owned()
    } else {
        format!("{}", share.round() as u64)
    };
    format!("{shown}%")
}

/// What the panel shows while its statistics are computed.
fn calculating(id: &'static str) -> Div {
    column().p_3().child(
        row()
            .id(id)
            .role(Role::Status)
            .gap_2()
            .child(spinning_icon(
                SharedString::from(format!("{id}-spinner")),
                I::LoaderCircle,
                colors().amber,
            ))
            .child(label(tr!("Calculating…"))),
    )
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
        let hidden_bar = self.hidden_bar(cx);
        let find_bar = self.find_bar(cx);
        let match_list = self.match_list(cx);
        // The bars sit outside the grid's own keys, so typing in them or
        // pressing their buttons never moves the selection.
        let grid = div()
            .id("result-grid")
            .flex_1()
            .min_h_0()
            .w_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .map(|grid| grid_keys(grid, cx))
            .on_action(cx.listener(|this, _: &super::ResultGridMenu, window, cx| {
                this.open_menu_from_keyboard(window, cx)
            }))
            .flex()
            .bg(rgb(colors().grid_bg))
            .text_color(rgb(colors().value_text))
            .child(
                div()
                    .id("result-table-area")
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(main)
                    .when_some(
                        self.copy_job.as_ref().and_then(super::copying::status),
                        |s, status| {
                            s.child(
                                row()
                                    .id("copy-progress")
                                    .role(Role::Status)
                                    .absolute()
                                    .top_2()
                                    .right_2()
                                    .p_2()
                                    .gap_3()
                                    .bg(rgb(colors().panel))
                                    .border_1()
                                    .border_color(rgb(colors().border))
                                    .rounded_md()
                                    .child(label(status))
                                    .child(
                                        button("cancel-copy", I::X, tr!("Cancel copy")).on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.cancel_copy(cx);
                                                window.focus(&this.focus, cx);
                                            }),
                                        ),
                                    ),
                            )
                        },
                    )
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
            });
        column()
            .id("result-grid-area")
            .key_context(GRID_CONTEXT)
            .relative()
            .size_full()
            .gap_0()
            .bg(rgb(colors().grid_bg))
            // From the table, the bars or the field itself, whose text it
            // selects again.
            .on_action(cx.listener(|this, _: &super::FindColumn, window, cx| {
                this.open_column_search(window, cx)
            }))
            .children(find_bar)
            .children(hidden_bar)
            .child(grid)
            // Last, so it is drawn over the table.
            .children(match_list)
    }
}

/// What a key of the grid does ([`ResultGrid::grid_key`]).
#[derive(Clone, Copy, Debug)]
enum KeyAction {
    Copy {
        names: bool,
    },
    SelectAll,
    Move {
        rows: isize,
        cols: isize,
        extend: bool,
    },
    Jump {
        jump: Jump,
        extend: bool,
    },
    Page {
        down: bool,
        extend: bool,
    },
    Inspect,
}

/// The grid's actions on the table: each one is a key of the keymap.
fn grid_keys(grid: Stateful<Div>, cx: &mut Context<ResultGrid>) -> Stateful<Div> {
    use super::shortcuts::*;
    fn on<A: Action>(
        grid: Stateful<Div>,
        key: KeyAction,
        cx: &mut Context<ResultGrid>,
    ) -> Stateful<Div> {
        grid.on_action(cx.listener(move |this, _: &A, window, cx| this.grid_key(key, window, cx)))
    }
    let moved = |rows, cols, extend| KeyAction::Move { rows, cols, extend };
    let jumped = |jump, extend| KeyAction::Jump { jump, extend };
    let grid = on::<GridCopy>(grid, KeyAction::Copy { names: false }, cx);
    let grid = on::<GridCopyWithNames>(grid, KeyAction::Copy { names: true }, cx);
    let grid = on::<GridSelectAll>(grid, KeyAction::SelectAll, cx);
    let grid = on::<GridUp>(grid, moved(-1, 0, false), cx);
    let grid = on::<GridDown>(grid, moved(1, 0, false), cx);
    let grid = on::<GridLeft>(grid, moved(0, -1, false), cx);
    let grid = on::<GridRight>(grid, moved(0, 1, false), cx);
    let grid = on::<GridExtendUp>(grid, moved(-1, 0, true), cx);
    let grid = on::<GridExtendDown>(grid, moved(1, 0, true), cx);
    let grid = on::<GridExtendLeft>(grid, moved(0, -1, true), cx);
    let grid = on::<GridExtendRight>(grid, moved(0, 1, true), cx);
    let grid = on::<GridRowStart>(grid, jumped(Jump::RowStart, false), cx);
    let grid = on::<GridRowEnd>(grid, jumped(Jump::RowEnd, false), cx);
    let grid = on::<GridExtendRowStart>(grid, jumped(Jump::RowStart, true), cx);
    let grid = on::<GridExtendRowEnd>(grid, jumped(Jump::RowEnd, true), cx);
    let grid = on::<GridFirstCell>(grid, jumped(Jump::First, false), cx);
    let grid = on::<GridLastCell>(grid, jumped(Jump::Last, false), cx);
    let grid = on::<GridExtendFirstCell>(grid, jumped(Jump::First, true), cx);
    let grid = on::<GridExtendLastCell>(grid, jumped(Jump::Last, true), cx);
    let grid = on::<GridPageUp>(
        grid,
        KeyAction::Page {
            down: false,
            extend: false,
        },
        cx,
    );
    let grid = on::<GridPageDown>(
        grid,
        KeyAction::Page {
            down: true,
            extend: false,
        },
        cx,
    );
    let grid = on::<GridExtendPageUp>(
        grid,
        KeyAction::Page {
            down: false,
            extend: true,
        },
        cx,
    );
    let grid = on::<GridExtendPageDown>(
        grid,
        KeyAction::Page {
            down: true,
            extend: true,
        },
        cx,
    );
    on::<GridInspect>(grid, KeyAction::Inspect, cx)
}
