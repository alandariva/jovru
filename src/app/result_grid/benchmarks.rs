//! Entry points for the isolated performance run.

use super::{ROW_NUMBERS, ResultGrid};
use gpui_kit::*;
use jykre::grid::CellSelection;

impl ResultGrid {
    pub(in crate::app) fn benchmark_begin(&mut self, bottom: bool, cx: &mut Context<Self>) {
        if bottom {
            self.select_all(cx);
        } else {
            self.select(Some(CellSelection::cell(0, 0)), cx);
        }
    }

    pub(in crate::app) fn benchmark_step(
        &mut self,
        phase: &str,
        tick: usize,
        cx: &mut Context<Self>,
    ) {
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
            let total = self.column_widths(&self.view()).total() + ROW_NUMBERS;
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
    pub(in crate::app) fn benchmark_freeze(&mut self, columns: usize, cx: &mut Context<Self>) {
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

    /// Hides `column`, as the header's menu does.
    pub(in crate::app) fn benchmark_hide(&mut self, column: usize, cx: &mut Context<Self>) {
        self.change_layout(|layout| layout.hide(column), cx);
    }

    /// Whether `column` is hidden.
    pub(in crate::app) fn benchmark_hidden(&self, column: usize) -> bool {
        self.layout.is_hidden(column)
    }
}
