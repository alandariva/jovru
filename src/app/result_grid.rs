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
use jykre::grid::{self, CellSelection, GridCache, GridKey, GridView, selection_block};
use jykre::result::ResultData;
use jykre::tr;
use std::{ops::Range, rc::Rc};

/// What the grid reports to the view that embeds it.
pub enum ResultGridEvent {
    /// The number of selected visible cells changed.
    SelectionChanged { cells: usize },
    /// Feedback for the notice bar, such as the outcome of a copy.
    Notice(Feedback),
}

pub struct ResultGrid {
    result: Rc<ResultData>,
    /// Bumped with every new result, so the cached view is rebuilt.
    version: u64,
    /// The demo result has fixed column proportions and a sortable column.
    sales: bool,
    /// Lowercase text of the open filter; empty when the bar is closed.
    filter: String,
    descending: bool,
    numbers_pt_br: bool,
    cache: GridCache,
    selection: Option<CellSelection>,
    /// The selected count last reported, so unchanged counts are not sent.
    reported: usize,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
}

impl EventEmitter<ResultGridEvent> for ResultGrid {}

impl ResultGrid {
    pub fn new(result: Rc<ResultData>, sales: bool, cx: &mut Context<Self>) -> Self {
        Self {
            result,
            version: 0,
            sales,
            filter: String::new(),
            descending: true,
            numbers_pt_br: false,
            cache: GridCache::default(),
            selection: None,
            reported: 0,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
        }
    }

    /// Shows another result and clears the selection. The grid keeps a
    /// shared handle; it never copies the data.
    pub fn set_result(&mut self, result: Rc<ResultData>, sales: bool, cx: &mut Context<Self>) {
        self.result = result;
        self.sales = sales;
        self.version += 1;
        self.select(None, cx);
        cx.notify();
    }

    /// Filters rows by text in any column, ignoring case. An empty filter
    /// (or a closed filter bar) shows every row.
    pub fn set_filter(&mut self, filter: &str, cx: &mut Context<Self>) {
        let filter = filter.to_lowercase();
        if self.filter != filter {
            self.filter = filter;
            self.select(None, cx);
            cx.notify();
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
            descending: self.descending,
            numbers_pt_br: self.numbers_pt_br,
        };
        self.cache.get(&self.result, key)
    }

    /// Changes the selection, notifies this grid only and reports a new
    /// selected count to the workspace.
    fn select(&mut self, selection: Option<CellSelection>, cx: &mut Context<Self>) {
        self.selection = selection;
        let cells = grid::selected_cells(selection, &self.view().rows);
        if cells != self.reported {
            self.reported = cells;
            cx.emit(ResultGridEvent::SelectionChanged { cells });
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

    /// Copies the selected block as tab-separated text (a single cell as its
    /// exact value), optionally with the column names first.
    fn copy_selection(&mut self, headers: bool, cx: &mut Context<Self>) {
        let view = self.view();
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
                    self.copy_selection(shift, cx);
                    true
                }
                "a" => {
                    self.select_all(cx);
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
                                    let tooltip = format!("{name} : {kind}");
                                    row()
                                        .id(("result-header", index))
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
                                        .when(index == 2 && self.sales, |s| {
                                            s.cursor_pointer()
                                                .focusable()
                                                .tab_index(0)
                                                .focus_visible(|s| s.bg(rgb(HOVER)))
                                        })
                                        .when(index == 2 && self.sales, |s| {
                                            s.child(icon(
                                                if self.descending {
                                                    I::ArrowDown
                                                } else {
                                                    I::ArrowUp
                                                },
                                                MINT,
                                            ))
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if index == 2 && this.sales {
                                                this.descending = !this.descending;
                                                cx.notify();
                                            }
                                        }))
                                },
                            )),
                    )
                    .child(if grid.rows.is_empty() {
                        column()
                            .p_6()
                            .gap_2()
                            .child(label(tr!("No results for this selection.")))
                            .child(label(tr!(
                                "The query or the current filter returned no rows."
                            )))
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
                     run: fn(&mut ResultGrid, &mut Context<ResultGrid>)| {
                        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            let _ = weak.update(cx, |this, cx| {
                                run(this, cx);
                                cx.notify();
                            });
                        }
                    };
                menu.item(
                    PopupMenuItem::new(tr!("Copy"))
                        .disabled(!has_selection)
                        .on_click(action(weak.clone(), |this, cx| {
                            this.copy_selection(false, cx)
                        })),
                )
                .item(
                    PopupMenuItem::new(tr!("Copy with headers"))
                        .disabled(!has_selection)
                        .on_click(action(weak.clone(), |this, cx| {
                            this.copy_selection(true, cx)
                        })),
                )
                .separator()
                .item(
                    PopupMenuItem::new(tr!("Select all"))
                        .on_click(action(weak.clone(), |this, cx| this.select_all(cx))),
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
                            .child((index + 1).to_string()),
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
                                index + 1,
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
        self.table(cx)
    }
}
