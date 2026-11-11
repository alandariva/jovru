//! Virtualized rows and columns, headers and cell painting.

use super::inspector::Inspector;
use super::menu::tooltips_shown;
use super::selection::Gesture;
use super::{HEADER_HEIGHT, ROW_HEIGHT, ROW_NUMBERS, ResultGrid};
use crate::app::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarAxis};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::cell_format::{self, ValueFamily};
use jykre::column_layout::{ColumnLayout, ColumnWidths};
use jykre::grid::{self, CellMark, CellSelection, ColumnSort, GridView, selection_block};
use jykre::grid_edit::{Overlay, RowMark};
use jykre::tr;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

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
            .bg(rgb(results_colors().surface))
            .border_1()
            .border_color(rgb(results_colors().mint))
            .text_size(px(12.))
            .text_color(rgb(results_colors().text))
            .child(self.name.clone())
    }
}

/// What the rows on screen share while they are built.
struct RowsContext {
    /// One palette snapshot for the visible batch, never retained as view state.
    colors: GridPaint,
    grid: Arc<GridView>,
    widths: Rc<ColumnWidths>,
    /// The selection's display rows and columns.
    block: Option<(
        std::ops::RangeInclusive<usize>,
        std::ops::RangeInclusive<usize>,
    )>,
    /// The focused cell, as (data row, result column).
    focus: Option<(usize, usize)>,
    flexible: bool,
}

/// GPUI stores colours as HSLA. Convert once per visible batch, rather than
/// repeating RGB-to-HSLA conversion for every border and value in every cell.
struct GridPaint {
    grid_bg: Hsla,
    grid_header: Hsla,
    grid_line: Hsla,
    grid_hover: Hsla,
    grid_selected: Hsla,
    grid_focus: Hsla,
    grid_change: Hsla,
    grid_written: Hsla,
    grid_error: Hsla,
    grid_alt: Option<Hsla>,
    match_bg: Hsla,
    faint: Hsla,
    value_text: Hsla,
    value_number: Hsla,
    value_temporal: Hsla,
    value_binary: Hsla,
    value_null: Hsla,
}

impl GridPaint {
    fn new(c: &Palette) -> Self {
        Self {
            grid_bg: rgb(c.grid_bg).into(),
            grid_header: rgb(c.grid_header).into(),
            grid_line: rgb(c.grid_line).into(),
            grid_hover: rgb(c.grid_hover).into(),
            grid_selected: rgb(c.grid_selected).into(),
            grid_focus: rgb(c.grid_focus).into(),
            grid_change: rgb(c.grid_change).into(),
            grid_written: rgb(c.grid_written).into(),
            grid_error: rgb(c.grid_error).into(),
            grid_alt: (c.grid_alt != c.grid_bg).then(|| rgb(c.grid_alt).into()),
            match_bg: rgba(c.match_bg).into(),
            faint: rgb(c.faint).into(),
            value_text: rgb(c.value_text).into(),
            value_number: rgb(c.value_number).into(),
            value_temporal: rgb(c.value_temporal).into(),
            value_binary: rgb(c.value_binary).into(),
            value_null: rgb(c.value_null).into(),
        }
    }

    fn value_color(&self, raw: Option<&str>, family: ValueFamily) -> Hsla {
        if raw.is_none() {
            self.value_null
        } else {
            match family {
                ValueFamily::Number => self.value_number,
                ValueFamily::Temporal => self.value_temporal,
                ValueFamily::Binary => self.value_binary,
                _ => self.value_text,
            }
        }
    }
}

/// Paints its child as one layer of the scene: everything the child draws
/// shares one place in the drawing order, found once, instead of each quad
/// finding its own among everything drawn before it. A row of the grid
/// draws four border strips per cell, its backgrounds and its values; as a
/// layer, finding their order costs one look-up per row, not one per
/// primitive. Inside a layer, backgrounds and borders are drawn before
/// text, as they already are within a cell, whose text never overlaps its
/// border; a nested layer (each line of text, the frozen part of a row)
/// is drawn over what it covers, as before.
struct Layer(AnyElement);

impl Layer {
    /// `child` as one layer when `layered`, as it is otherwise.
    fn of(child: impl IntoElement, layered: bool) -> AnyElement {
        if layered {
            Self(child.into_any_element()).into_any_element()
        } else {
            child.into_any_element()
        }
    }
}

impl IntoElement for Layer {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Layer {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.0.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.0.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_layer(bounds, |window| self.0.paint(window, cx));
    }
}

/// The demo result's column proportions.
const DEMO_FLEX: [f32; 5] = [1.5, 0.8, 1.2, 1.1, 0.65];

/// What the table lays out sideways in one frame: the frozen columns, the
/// scrolling ones in view, and where they go.
pub(super) struct Strip {
    /// Display positions `0..frozen` are frozen.
    pub(super) frozen: usize,
    /// The scrolling columns built, as display positions.
    shown: Range<usize>,
    /// Width of the scrolling columns before `shown`, in pixels.
    before: f32,
    /// Row numbers and frozen columns, in pixels.
    pub(super) lead: f32,
    /// How far the columns are panned, in pixels.
    pub(super) pan: f32,
    /// Row numbers and frozen columns stay at the left edge while the rest
    /// pans; without frozen columns the row numbers pan with the others.
    sticky: bool,
    /// The demo's columns share the width in fixed proportions.
    flexible: bool,
}

/// The text colour of a value, by its family ([Results](../../docs/reference/results.md#value-presentation)).
pub(super) fn value_color(raw: Option<&str>, family: ValueFamily, c: &Palette) -> u32 {
    if raw.is_none() {
        c.value_null
    } else if family == ValueFamily::Number {
        c.value_number
    } else if family == ValueFamily::Temporal {
        c.value_temporal
    } else if family == ValueFamily::Binary {
        c.value_binary
    } else {
        c.value_text
    }
}

/// The displayed text, with a search hit marked when `mark` names ranges.
pub(super) fn painted(displayed: String, mark: CellMark) -> AnyElement {
    match mark {
        CellMark::Ranges(ranges) => marked(displayed, ranges).into_any_element(),
        _ => displayed.into_any_element(),
    }
}

impl ResultGrid {
    /// What the table lays out sideways this frame, from the columns'
    /// widths, the pan and the viewport.
    pub(super) fn strip(&self, widths: &ColumnWidths) -> Strip {
        let frozen = widths.frozen();
        let sticky = frozen > 0;
        let lead = ROW_NUMBERS + widths.span(0..frozen);
        let pan = (-f32::from(self.columns_scroll.offset().x)).max(0.);
        let viewport = self.viewport.get();
        // The demo's columns grow to fill the grid, so all of them are built.
        let flexible = self.sales && self.layout.is_identity() && self.widths.is_empty();
        let (shown, before) = if flexible {
            (frozen..widths.display().len(), 0.)
        } else {
            widths.shown(lead, pan, viewport)
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

    /// Fits a column to its header and the values of the displayed rows.
    pub(super) fn fit_column(&mut self, column: usize, cx: &mut Context<Self>) {
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

    pub(super) fn table(&self, cx: &mut Context<Self>) -> AnyElement {
        let grid = self.view();
        let widths = self.column_widths(&grid);
        let strip = self.strip(&widths);
        let grid_width = widths.total() + ROW_NUMBERS;
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
            .bg(rgb(results_colors().grid_header))
            .border_b_1()
            .border_color(rgb(results_colors().grid_line));
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
                    .by_column()
                    .get(column)
                    .copied()
                    .unwrap_or(grid::MIN_COLUMN_WIDTH);
                header = header.child(self.header(column, position, width, strip.flexible, cx));
            }
        }
        if strip.sticky {
            let mut frozen = self
                .frozen_part(&strip)
                .bg(rgb(results_colors().grid_header))
                .child(corner());
            for position in 0..strip.frozen {
                if let Some(column) = visible.get(position).copied() {
                    let width = widths
                        .by_column()
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
                    .child(if grid.rows.is_empty() && !self.find.filter.is_empty() {
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
            .border_color(rgb(results_colors().grid_line))
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
        let c = results_colors();
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
            .border_color(rgb(c.grid_line))
            .text_size(px(14.))
            .text_color(rgb(c.grid_header_text))
            .cursor_pointer()
            .focusable()
            .tab_index(0)
            .focus_visible(|s| s.bg(rgb(c.hover)))
            .hover(|s| s.bg(rgb(c.grid_hover)))
            // GPUI draws a tooltip over every layer, so none is set while the
            // grid's menu is open.
            .when(tooltips_shown(self.menu_open), |s| {
                s.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
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
                    .text_color(rgb(c.grid_type_text))
                    .child(format!(": {kind}")),
            )
            .when_some(sorted, |s, descending| {
                s.child(icon(
                    if descending {
                        I::ArrowDown
                    } else {
                        I::ArrowUp
                    },
                    c.mint,
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
            .on_action(cx.listener(move |this, _: &crate::app::ResultGridMenu, window, cx| {
                this.open_column_menu(index, window, cx)
            }))
            // Dropped on another header, the column takes its place.
            .on_drag(drag, |drag, _, _, cx| {
                cx.new(|_| ColumnDragPreview {
                    name: drag.name.clone(),
                })
            })
            .drag_over::<ColumnDrag>(|style, _, _, _| {
                let c = results_colors();
                style
                    .bg(rgb(c.hover))
                    .border_l_2()
                    .border_color(rgb(c.mint))
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
                this.refresh_find(cx);
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
                    .hover(|s| s.bg(rgb(c.grid_focus)))
                    .when(
                        self.gesture.is_some_and(|gesture| {
                            matches!(gesture, Gesture::Resizing { column, .. } if column == index)
                        }),
                        |s| s.bg(rgb(c.grid_focus)),
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

    /// The data rows at `range` of the displayed order.
    fn result_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let grid = self.view();
        let widths = self.column_widths(&grid);
        let strip = self.strip(&widths);
        let block = self
            .selection
            .and_then(|selection| selection_block(selection, &grid.rows, &self.layout));
        let rows = RowsContext {
            colors: GridPaint::new(&results_colors()),
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
                // The field a cell is typed in draws its caret over its
                // text, so the row holding it is not one layer.
                let editing = self
                    .edits
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.cell.0 == index);
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
                let number = self.row_number(index, row_selected, &rows.colors, cx);
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
                        .bg(rows.colors.grid_bg)
                        .children(number.take());
                    for display in 0..strip.frozen {
                        if let Some(column) = self.layout.column(display) {
                            frozen = frozen
                                .child(self.cell(&rows, position, index, display, column, cx));
                        }
                    }
                    // Its own layer, over the values of the columns that
                    // pan under it.
                    line = line.child(Layer::of(frozen, !editing));
                }
                Layer::of(line, !editing)
            })
            .collect()
    }

    /// The number of data row `index`: pressing it selects the row (Shift
    /// extends) and dragging over other numbers selects them all.
    fn row_number(
        &self,
        index: usize,
        selected: bool,
        c: &GridPaint,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(("row-number", index))
            .w(px(ROW_NUMBERS))
            .flex_none()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .bg(if selected {
                c.grid_selected
            } else {
                c.grid_header
            })
            .hover(|s| s.bg(c.grid_hover))
            .border_b_1()
            .border_color(c.grid_line)
            .text_center()
            .text_size(px(12.))
            .text_color(c.faint)
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
            .map(|s| {
                // An inserted row has no number yet; an edited row's number
                // takes the colour of its edit.
                let inserted = index.checked_sub(self.result.row_count());
                let mark = self
                    .session()
                    .map_or(RowMark::Unchanged, |session| match inserted {
                        Some(inserted) => session.inserted_mark(inserted),
                        None => session.row_mark(self.first_row + index),
                    });
                let color = match mark {
                    RowMark::Unchanged => None,
                    RowMark::Changed => Some(c.grid_change),
                    RowMark::Deleted | RowMark::WrittenDeleted | RowMark::Failed => {
                        Some(c.grid_error)
                    }
                    RowMark::Written => Some(c.grid_written),
                };
                s.when_some(color, |s, color| s.text_color(color))
                    .child(match inserted {
                        Some(_) => "+".to_owned(),
                        None => (self.first_row + index + 1).to_string(),
                    })
            })
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
        let c = &rows.colors;
        let family = rows
            .grid
            .families
            .get(col)
            .copied()
            .unwrap_or(ValueFamily::Text);
        let width = rows
            .widths
            .by_column()
            .get(col)
            .copied()
            .unwrap_or(grid::MIN_COLUMN_WIDTH);
        // Pending and written edits are drawn over the values read; an
        // inserted row follows the result's rows.
        let session = self.session();
        let inserted = index.checked_sub(self.result.row_count());
        let (overlay, mark) = match (session, inserted) {
            (Some(session), Some(inserted)) => (
                session.inserted_value(inserted, col),
                session.inserted_mark(inserted),
            ),
            (Some(session), None) => (
                session.overlay(self.first_row + index, col),
                session.row_mark(self.first_row + index),
            ),
            (None, _) => (None, RowMark::Unchanged),
        };
        let raw = match overlay {
            Some(overlay) => overlay.value().text(),
            None => self.result.value(index, col),
        };
        // A column an inserted row leaves out takes the table's default.
        let default = inserted.is_some() && overlay.is_none();
        let editor = self
            .edits
            .editor
            .as_ref()
            .filter(|editor| editor.cell == (index, col))
            .map(|editor| editor.input.clone());
        // A cell is one line: line breaks show as ↵ and the full value stays
        // in the tooltip and in copies.
        let displayed = if default {
            tr!("DEFAULT").to_owned()
        } else {
            cell_format::shown(raw, family, self.numbers_pt_br)
        };
        let found = self.cell_mark(&displayed);
        let whole = matches!(found, CellMark::Whole) && editor.is_none();
        let is_null = raw.is_none();
        let deleted = matches!(mark, RowMark::Deleted | RowMark::WrittenDeleted);
        let edge = match (overlay, mark) {
            (_, RowMark::Failed) => Some(c.grid_error),
            (Some(Overlay::Pending(_)), _) => Some(c.grid_change),
            (Some(Overlay::Written(_)), _) => Some(c.grid_written),
            _ => None,
        };
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
            .border_color(c.grid_line)
            .when_some(c.grid_alt.filter(|_| position % 2 == 1), |s, color| {
                s.bg(color)
            })
            .when(!selected, |s| s.hover(|s| s.bg(c.grid_hover)))
            .when(whole && !selected, |s| s.bg(c.match_bg))
            .when(selected, |s| s.bg(c.grid_selected))
            .when_some(edge, |s, edge| s.border_color(edge))
            .when(focused, |s| s.border_color(c.grid_focus))
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
            .when(is_null || default, |s| s.italic())
            .when(family == ValueFamily::Boolean, |s| s.justify_center())
            .when_some(
                tooltip.filter(|_| tooltips_shown(self.menu_open)),
                |s, tooltip| {
                    s.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                },
            )
            .when(family == ValueFamily::Number, |s| s.justify_end())
            .text_color(if deleted {
                c.grid_error
            } else if default {
                c.faint
            } else {
                c.value_color(raw, family)
            })
            .when(deleted, |s| s.line_through())
            .map(|s| match editor {
                Some(input) => s.px_0().child(Input::new(&input).small().w_full()),
                None => s.child(div().truncate().child(painted(displayed, found))),
            })
    }

    /// How many columns are hidden, while any is, with a way to show them
    /// all: always in view above the table or the records.
    pub(super) fn hidden_bar(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let hidden = self.layout.hidden();
        (hidden > 0).then(|| {
            row()
                .id("hidden-columns")
                .role(Role::Status)
                .flex_none()
                .h(px(30.))
                .px_3()
                .gap_2()
                .bg(rgb(results_colors().panel))
                .border_b_1()
                .border_color(rgb(results_colors().grid_line))
                .child(icon(I::EyeOff, results_colors().muted))
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
}

#[cfg(test)]
mod tests {
    use super::{GridPaint, ValueFamily, value_color};

    #[test]
    fn grid_paint_preserves_value_roles_alpha_and_optional_stripes() {
        crate::app::source_text();
        let mut palette = jykre::theme::builtin_dark().clone();
        palette.value_number = 0x123456;
        palette.value_null = 0xabcdef;
        palette.match_bg = 0x12345678;
        let paint = GridPaint::new(&palette);
        assert!(paint.grid_alt.is_none());
        assert_eq!(paint.match_bg, gpui_kit::rgba(palette.match_bg).into());
        for family in [
            ValueFamily::Text,
            ValueFamily::Number,
            ValueFamily::Temporal,
            ValueFamily::Binary,
            ValueFamily::Boolean,
        ] {
            for raw in [None, Some("fixture")] {
                assert_eq!(
                    paint.value_color(raw, family),
                    gpui_kit::rgb(value_color(raw, family, &palette)).into()
                );
            }
        }
        palette.grid_alt = 0x2468ac;
        let paint = GridPaint::new(&palette);
        assert_eq!(paint.grid_alt, Some(gpui_kit::rgb(palette.grid_alt).into()));
    }
}
