//! The result grid: its own entity, embedded in the workspace with
//! `.cached(...)` ([ADR-0012](../../docs/adr/0012-result-grid-as-a-cached-entity.md)).
//!
//! Hover, scroll, selection and copy notify only this entity; the workspace
//! learns what it shows (the selected count, notices) through
//! [`ResultGridEvent`]. What the grid derives from a result lives in
//! [`jykre::grid`].
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::cell_format::{self, ValueFamily};
use jykre::grid::{
    self, CellSelection, ColumnSort, GridCache, GridKey, GridView, Jump, selection_block,
};
use jykre::result::ResultData;
use jykre::result_tabs::ResultId;
use jykre::tr;
use std::collections::HashMap;
use std::{ops::Range, rc::Rc};

/// What the grid reports to the view that embeds it.
pub enum ResultGridEvent {
    /// The number of selected visible cells changed.
    SelectionChanged { cells: usize },
    /// How many rows the open filter shows, or `None` without a filter.
    RowsShown { rows: Option<usize> },
    /// Feedback for the notice bar, such as the outcome of a copy.
    Notice(Feedback),
}

/// Where the user left a result tab: its page, selection and scroll.
struct Place {
    page: usize,
    selection: Option<CellSelection>,
    scroll: Point<Pixels>,
}

/// Height of a data row, in pixels.
const ROW_HEIGHT: f32 = 34.;

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
        }
    }

    /// Shows one page of a result whose first row is row `first_row` (zero
    /// based) of the whole result. The grid keeps a shared handle; it never
    /// copies the data.
    ///
    /// `shown` is the result tab and page number. Leaving a tab remembers
    /// its selection and scroll, and coming back to the same page of it
    /// restores them; any other page starts at the top with nothing
    /// selected.
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
        if let Some((id, page)) = self.shown
            && self.shown.map(|(id, _)| id) != shown.map(|(id, _)| id)
        {
            let scroll = self.scroll.0.borrow().base_handle.offset();
            self.places.insert(
                id,
                Place {
                    page,
                    selection: self.selection,
                    scroll,
                },
            );
        }
        let place =
            shown.and_then(|(id, page)| self.places.remove(&id).filter(|place| place.page == page));
        // A sort chosen for one result tab does not carry to another; the
        // pages of the same tab keep it.
        if self.shown.map(|(id, _)| id) != shown.map(|(id, _)| id) {
            self.sort = None;
        }
        self.result = result;
        self.sales = sales;
        self.first_row = first_row;
        self.shown = shown;
        self.version += 1;
        let (selection, scroll) = place.map_or((None, Point::default()), |place| {
            (place.selection, place.scroll)
        });
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
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let all = CellSelection::all(&self.view().rows, self.result.column_count());
        if all.is_some() {
            self.select(all, cx);
        }
    }

    /// Arrow-key navigation; with Shift the block grows from the anchor.
    fn move_selection(&mut self, rows: isize, cols: isize, extend: bool, cx: &mut Context<Self>) {
        let view = self.view();
        let columns = self.result.column_count();
        if let Some((selection, position)) =
            CellSelection::moved(self.selection, &view.rows, columns, rows, cols, extend)
        {
            self.select(Some(selection), cx);
            self.scroll
                .scroll_to_item(position, ScrollStrategy::Nearest);
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
            self.scroll
                .scroll_to_item(position, ScrollStrategy::Nearest);
        }
    }

    /// The rows one screen of the grid shows, for Page Up and Page Down.
    fn page_rows(&self) -> isize {
        let height = f32::from(self.scroll.0.borrow().base_handle.bounds().size.height);
        ((height / ROW_HEIGHT).floor() as isize - 1).max(1)
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
                    let _ = grid.update(cx, |this, cx| this.copy_selection(headers, true, window, cx));
                },
            );
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
        let widths = &grid.widths;
        let grid_width = widths.iter().sum::<f32>() + 35.;
        let weak = cx.weak_entity();
        let table = div()
            .id("result-table-scroll")
            .size_full()
            .bg(rgb(GRID_BG))
            .text_color(rgb(VALUE_TEXT))
            .overflow_x_scrollbar()
            .child(
                column()
                    .w(px(grid_width))
                    .min_w_full()
                    .h_full()
                    .child(
                        row()
                            .h(px(34.))
                            .flex_none()
                            .gap_0()
                            .bg(rgb(GRID_HEADER))
                            .border_b_1()
                            .border_color(rgb(GRID_LINE))
                            .child(div().w(px(35.)).flex_none().text_center().child(label("#")))
                            .children(self.result.columns().iter().enumerate().map(
                                |(index, column)| {
                                    let name = &column.name;
                                    let kind = column.data_type.to_lowercase();
                                    let tooltip = tr!(
                                        "{0} : {1} · Click to sort this page by it",
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
                                        .w(px(widths[index]))
                                        .flex_none()
                                        .h_full()
                                        .px_3()
                                        .gap_1()
                                        .when(self.sales, |s| {
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
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(tooltip.clone()).build(window, cx)
                                        })
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
                                        // Ascending, descending, then the
                                        // server's order again; the rows of
                                        // this page are sorted by value.
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.sort = ColumnSort::next(this.sort, index);
                                            this.select(this.selection, cx);
                                        }))
                                },
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
                                .track_scroll(&self.scroll),
                            )
                            .vertical_scrollbar(&self.scroll)
                            .into_any_element()
                    }),
            );
        div()
            .id("result-grid")
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .child(table)
            .context_menu(move |menu, _, cx| {
                let has_selection = weak
                    .upgrade()
                    .is_some_and(|this| this.read(cx).selection.is_some());
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
                .separator()
                .item(
                    PopupMenuItem::new(tr!("Select all"))
                        .on_click(action(weak.clone(), |this, _, cx| this.select_all(cx))),
                )
            })
            .into_any_element()
    }

    /// The data rows at `range` of the displayed order.
    fn result_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let grid = self.view();
        let widths = &grid.widths;
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
                    .h(px(34.))
                    .flex_none()
                    .gap_0()
                    .child(
                        div()
                            .id(("row-number", index))
                            .w(px(35.))
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
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                let selection = CellSelection::click_row(
                                    this.selection,
                                    index,
                                    last_column + 1,
                                    event.modifiers().shift,
                                );
                                this.select(Some(selection), cx);
                                window.focus(&this.focus, cx);
                            }))
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
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                let selection = CellSelection::click(
                                    this.selection,
                                    index,
                                    col,
                                    event.modifiers().shift,
                                );
                                this.select(Some(selection), cx);
                                window.focus(&this.focus, cx);
                            }))
                            // A right click outside the selection selects the
                            // cell first, so the menu acts on what is under it.
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, _, window, cx| {
                                    if !selected {
                                        this.select(Some(CellSelection::cell(index, col)), cx);
                                    }
                                    window.focus(&this.focus, cx);
                                }),
                            )
                            .when(self.sales, |s| s.flex_grow([1.5, 0.8, 1.2, 1.1, 0.65][col]))
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
                            .text_color(rgb(if is_null {
                                VALUE_NULL
                            } else if family == ValueFamily::Number {
                                VALUE_NUMBER
                            } else if family == ValueFamily::Temporal {
                                VALUE_TEMPORAL
                            } else if family == ValueFamily::Binary {
                                VALUE_BINARY
                            } else {
                                VALUE_TEXT
                            }))
                            .child(div().truncate().child(displayed))
                    }))
            })
            .collect()
    }
}

impl Render for ResultGrid {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::probe::rendered("ResultGrid");
        self.table(cx)
    }
}
