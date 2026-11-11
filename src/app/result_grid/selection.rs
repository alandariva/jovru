//! Cell selection, navigation and pointer gestures.

use super::{HEADER_HEIGHT, ROW_HEIGHT, ROW_NUMBERS, ResultGrid, ResultGridEvent};
use gpui_kit::*;
use jykre::grid::{self, CellSelection, GridView, Jump, selection_block};
use std::time::Duration;

/// A mouse gesture in progress over the grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Gesture {
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

impl ResultGrid {
    // While a drag is active, every pointer move in the
    // window reaches the grid, also past its edges, and a
    // release anywhere ends it.
    pub(super) fn capture_gesture(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        self.gesture?;
        let weak = cx.weak_entity();
        Some(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let moved = weak.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble {
                            let _ = moved.update(cx, |this, cx| {
                                this.pointer_moved(event.position, event.pressed_button, bounds, cx)
                            });
                        }
                    });
                    let released = weak.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                            let _ = released.update(cx, |this, cx| this.end_gesture(cx));
                        }
                    });
                },
            )
            .absolute()
            .inset_0(),
        )
    }

    /// Changes the selection, notifies this grid only and reports a new
    /// selected count to the workspace.
    pub(super) fn select(&mut self, selection: Option<CellSelection>, cx: &mut Context<Self>) {
        self.selection = selection;
        self.find.follow_selection(selection);
        let view = self.view();
        let cells = grid::selected_cells(selection, &view.rows, &self.layout);
        if cells != self.reported {
            self.reported = cells;
            cx.emit(ResultGridEvent::SelectionChanged { cells });
        }
        self.publish_search(&view, cx);
        // The side panel follows the focused cell; in the records view it
        // keeps the cell double-clicked there.
        if !self.records {
            self.inspector.cell = selection.map(|selection| selection.focus);
        }
        self.summarize(cx);
        self.refresh_column_stats(cx);
        self.refresh_result_stats(cx);
        cx.notify();
    }

    pub(super) fn select_all(&mut self, cx: &mut Context<Self>) {
        let all = CellSelection::all(&self.view().rows, &self.layout);
        if all.is_some() {
            self.select(all, cx);
        }
    }

    /// Arrow-key navigation; with Shift the block grows from the anchor.
    /// The focused cell scrolls into view, rows and columns.
    pub(super) fn move_selection(
        &mut self,
        rows: isize,
        cols: isize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
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
    pub(super) fn jump_selection(&mut self, jump: Jump, extend: bool, cx: &mut Context<Self>) {
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
    pub(super) fn reveal(&self, position: Option<usize>, column: usize, view: &GridView) {
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
        let widths = self.column_widths(view);
        let display = widths.display();
        let lead = ROW_NUMBERS + widths.span(0..frozen);
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
    pub(super) fn page_rows(&self) -> isize {
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

    /// The pointer moved over the cell at data row `row` and `column`
    /// during a drag: the block now reaches it (the whole row when the drag
    /// started on a row number).
    pub(super) fn drag_over(
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

    /// The result columns of the visible selection, for naming a key.
    /// The data rows and result columns of the visible selection.
    pub(super) fn selected_block(&self) -> Option<(Vec<usize>, Vec<usize>)> {
        let view = self.view();
        let (rows, columns) = selection_block(self.selection?, &view.rows, &self.layout)?;
        let rows = rows
            .filter_map(|position| view.rows.get(position).copied())
            .collect();
        Some((rows, grid::block_columns(&self.layout, columns)))
    }

    pub fn selected_columns(&self) -> Vec<usize> {
        self.selected_block()
            .map(|(_, columns)| columns)
            .unwrap_or_default()
    }
}
