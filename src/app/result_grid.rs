//! The result grid: its own entity, embedded in the workspace with
//! `.cached(...)` ([ADR-0012](../../docs/adr/0012-result-grid-as-a-cached-entity.md)).
//!
//! Hover, scroll, selection, drags and copies notify only this entity; the
//! workspace learns what it shows (the selected count, the selection's
//! statistics, notices) through [`ResultGridEvent`]. What the grid derives
//! from a result lives in [`jykre::grid`], [`jykre::stats`] and
//! [`jykre::copy_as`].
mod benchmarks;
mod column_search;
mod copying;
mod editing;
mod find;
mod inspector;
mod keyboard;
mod menu;
mod records;
mod selection;
mod table;

pub use self::{
    copying::GridSql,
    editing::Editing,
    inspector::{Inspector, StatsScope, Summary},
};

use self::column_search::ColumnSearchState;
use self::editing::EditingState;
use self::find::FindState;
use self::inspector::InspectorState;
use self::keyboard::grid_keys;
use self::selection::Gesture;
use super::ui::*;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::column_layout::{ColumnLayout, ColumnWidths};
use jykre::grid::{CellSelection, ColumnSort, GridCache, GridKey, GridView, WidthsCache};
use jykre::grid_edit::EditSession;
use jykre::result::ResultData;
use jykre::result_tabs::ResultId;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

/// What the grid reports to the view that embeds it.
pub enum ResultGridEvent {
    /// The number of selected visible cells changed.
    SelectionChanged { cells: usize },
    /// How many rows the open search counts: the rows the filter shows, or
    /// the rows a highlight matches. `None` when the bar is closed or empty.
    RowsShown { rows: Option<usize> },
    /// The highlight search's `1/3`, or `None` when it is not showing one.
    FindPosition(Option<String>),
    /// Feedback for the notice bar, such as the outcome of a copy.
    Notice(Feedback),
    /// The statistics of the selected cells changed.
    Summary(Summary),
    /// The records view was turned on or off.
    RecordsShown,
    /// The side panel opened, closed or changed tab, for the toolbar's
    /// Statistics button.
    PanelChanged,
    /// Editing started on a result tab whose table is not known yet: the
    /// workspace reads it from the session and answers with
    /// [`ResultGrid::set_editing`] (ADR-0039).
    EditRequested(ResultId),
    /// The pending edits changed.
    EditsChanged,
    /// **Write…** (or **Generate SQL…**) was asked for.
    WriteRequested,
    /// **Discard** was asked for.
    DiscardRequested,
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

/// The key context of the grid and its bars, where Shift F10 opens the
/// menu ([`super::ResultGridMenu`]).
pub const GRID_CONTEXT: &str = "ResultGrid";

/// The key context of the result search field. The input stacks its own
/// context under this one, so the field's keys read `ResultFind > Input`.
pub const FIND_CONTEXT: &str = "ResultFind";
pub const FIND_FIELD_CONTEXT: &str = "ResultFind > Input";
/// Height of a data row, in pixels.
const ROW_HEIGHT: f32 = 34.;
/// Height of the header and width of the row numbers, in pixels.
const HEADER_HEIGHT: f32 = 34.;
const ROW_NUMBERS: f32 = 35.;

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
    find: FindState,
    /// The column the rows are sorted by, chosen in the header.
    sort: Option<ColumnSort>,
    numbers_pt_br: bool,
    /// Position in the whole result of this page's first row, for the row
    /// numbers.
    first_row: usize,
    cache: GridCache,
    /// The columns' widths for the grid view, the layout and `widths`.
    widths_cache: WidthsCache,
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
    /// The grid's menu is open: no tooltip of the grid is drawn over it.
    menu_open: bool,
    /// The table's width at the last layout, which decides the columns in
    /// view.
    viewport: Rc<Cell<f32>>,
    column_search: ColumnSearchState,
    gesture: Option<Gesture>,
    /// Rows and columns a drag past an edge moves per step.
    edge: (isize, isize),
    autoscrolling: bool,
    autoscroll: Option<Task<()>>,
    /// The column a right click last landed on, for the header's menu.
    menu_column: Option<usize>,
    inspector: InspectorState,
    /// The selected rows are shown as records, side by side.
    records: bool,
    records_scroll: UniformListScrollHandle,
    records_pan: ScrollHandle,
    sql: GridSql,
    copy_job: Option<super::copying::CopyJob>,
    edits: EditingState,
}

impl EventEmitter<ResultGridEvent> for ResultGrid {}

impl ResultGrid {
    pub fn new(result: Arc<ResultData>, sales: bool, cx: &mut Context<Self>) -> Self {
        let layout = Arc::new(ColumnLayout::new(result.column_count()));
        Self {
            result,
            shown: None,
            places: HashMap::new(),
            version: 0,
            sales,
            first_row: 0,
            find: FindState::default(),
            sort: None,
            numbers_pt_br: false,
            cache: GridCache::default(),
            widths_cache: WidthsCache::default(),
            selection: None,
            reported: 0,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            columns_scroll: ScrollHandle::new(),
            widths: HashMap::new(),
            layout,
            layout_version: 0,
            menu_locked: false,
            menu_open: false,
            viewport: Rc::default(),
            column_search: ColumnSearchState::default(),
            gesture: None,
            edge: (0, 0),
            autoscrolling: false,
            autoscroll: None,
            menu_column: None,
            inspector: InspectorState::default(),
            records: false,
            records_scroll: UniformListScrollHandle::new(),
            records_pan: ScrollHandle::new(),
            sql: GridSql::default(),
            copy_job: None,
            edits: EditingState::default(),
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
        let same_columns = self.result.column_count() == result.column_count()
            && self
                .result
                .columns()
                .iter()
                .zip(result.columns())
                .all(|(old, new)| old.same_as(new));
        self.edits.editor = None;
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
        self.refresh_find(cx);
        cx.notify();
    }

    /// Forgets where closed result tabs were left.
    pub fn retain_places(&mut self, open: &[ResultId]) {
        self.places.retain(|id, _| open.contains(id));
        self.retain_edits(open);
    }

    pub fn set_number_format(&mut self, numbers_pt_br: bool, cx: &mut Context<Self>) {
        if self.numbers_pt_br != numbers_pt_br {
            self.numbers_pt_br = numbers_pt_br;
            self.refresh_find(cx);
            cx.notify();
        }
    }

    /// The grid for the current result, filter, sort and number format.
    fn view_key(&self) -> GridKey {
        let session = self.session();
        GridKey {
            result: self.version,
            filter: self.find.filter.clone(),
            sort: self.sort,
            numbers_pt_br: self.numbers_pt_br,
            edits: session.map_or(0, EditSession::version),
            inserted: session.map_or(0, EditSession::drawn_inserted),
        }
    }

    fn view(&self) -> Arc<GridView> {
        let mut key = self.view_key();
        if let Some(view) = &self.find.view
            && view.key == key
        {
            return view.clone();
        }
        // Until the background search publishes, paint the unfiltered page.
        key.filter.clear();
        self.cache.get(&self.result, key)
    }

    /// The width of each column (the user's where they set one, otherwise
    /// the measured one), by result column and in display order; computed
    /// when the view, the layout or the widths change, not per frame.
    fn column_widths(&self, grid: &GridView) -> Rc<ColumnWidths> {
        self.widths_cache
            .get(grid, &self.layout, self.layout_version, &self.widths)
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
        self.refresh_find(cx);
    }

    /// Gives the grid the keyboard.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }
}

impl Render for ResultGrid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::probe::rendered("ResultGrid");
        self.sync_value(window, cx);
        let menu_weak = cx.weak_entity();
        let main = if self.records {
            self.records_view(cx)
        } else {
            self.table(cx)
        };
        let hidden_bar = self.hidden_bar(cx);
        let column_search = self.column_search(cx);
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
            .bg(rgb(results_colors().grid_bg))
            .text_color(rgb(results_colors().value_text))
            .child(
                div()
                    .id("result-table-area")
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(main)
                    .children(self.copy_progress(cx))
                    .context_menu(move |menu, window, cx| {
                        Self::menu(menu_weak.clone(), menu, window, cx)
                    })
                    .children(self.capture_gesture(cx)),
            )
            .when_some(self.inspector.panel, |s, inspector| {
                s.child(self.inspector_panel(inspector, cx))
            });
        column()
            .id("result-grid-area")
            .key_context(GRID_CONTEXT)
            .relative()
            .size_full()
            .gap_0()
            .bg(rgb(results_colors().grid_bg))
            // From the table or the dialog, whose text it selects again.
            .on_action(cx.listener(|this, _: &super::FindColumn, window, cx| {
                this.open_column_search(window, cx)
            }))
            .child(self.measure_column_search_area(cx))
            .children(hidden_bar)
            .child(grid)
            // Last, so it is drawn over the table.
            .children(column_search)
    }
}
