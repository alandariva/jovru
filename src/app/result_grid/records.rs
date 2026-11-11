//! The records view of selected rows.

use super::inspector::Inspector;
use super::table::{painted, value_color};
use super::{HEADER_HEIGHT, ROW_HEIGHT, ResultGrid, ResultGridEvent};
use crate::app::ui::*;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::cell_format::{self, ValueFamily};
use jykre::grid::{self, CellMark};
use jykre::tr;
use std::ops::Range;
use std::rc::Rc;

/// Rows the records view shows side by side.
const MAX_RECORDS: usize = 50;

const RECORD_WIDTH: f32 = 280.;

const FIELD_WIDTH: f32 = 220.;

impl ResultGrid {
    pub fn records(&self) -> bool {
        self.records
    }

    /// Shows the selected rows as records side by side, or the table again.
    pub fn set_records(&mut self, records: bool, cx: &mut Context<Self>) {
        if self.records != records {
            self.records = records;
            if !records {
                self.inspector.cell = self.selection.map(|selection| selection.focus);
            }
            cx.emit(ResultGridEvent::RecordsShown);
            cx.notify();
        }
    }

    /// The selected rows side by side: one line per column, one column per
    /// row, so wide rows read top to bottom.
    pub(super) fn records_view(&self, cx: &mut Context<Self>) -> AnyElement {
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
            .bg(rgb(results_colors().grid_header))
            .border_b_1()
            .border_color(rgb(results_colors().grid_line))
            .child(
                div()
                    .w(px(FIELD_WIDTH))
                    .flex_none()
                    .px_3()
                    .text_size(px(14.))
                    .text_color(rgb(results_colors().grid_header_text))
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
                    .border_color(rgb(results_colors().grid_line))
                    .text_size(px(14.))
                    .text_color(rgb(results_colors().grid_header_text))
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
    /// in display order; while the column search dialog is open with text,
    /// only the shown ones it finds.
    pub(super) fn record_lines(&self) -> Rc<[usize]> {
        match self.column_search.active_matches() {
            Some(matches) => {
                // The matches are alphabetical; the records keep display order.
                let mut found = vec![false; self.result.column_count()];
                for each in matches {
                    if let Some(slot) = found.get_mut(each.column) {
                        *slot = true;
                    }
                }
                self.layout
                    .visible()
                    .iter()
                    .copied()
                    .filter(|column| found.get(*column).copied().unwrap_or(false))
                    .collect()
            }
            None => self.layout.visible().iter().copied().collect(),
        }
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
                    .border_color(rgb(results_colors().grid_line))
                    .child(
                        row()
                            .w(px(FIELD_WIDTH))
                            .flex_none()
                            .h_full()
                            .px_3()
                            .gap_1()
                            .bg(rgb(results_colors().grid_header))
                            .child(
                                div()
                                    .flex_shrink(1.)
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.))
                                    .text_color(rgb(results_colors().grid_header_text))
                                    .child(info.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(rgb(results_colors().grid_type_text))
                                    .child(format!(": {}", info.data_type.to_lowercase())),
                            ),
                    )
                    .children(records.iter().map(|row| {
                        let row = *row;
                        let raw = self.result.value(row, col);
                        let displayed = cell_format::shown(raw, family, self.numbers_pt_br);
                        let inspected = self.inspector.cell == Some((row, col));
                        let mark = self.cell_mark(&displayed);
                        let whole = matches!(mark, CellMark::Whole);
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
                            .border_color(rgb(results_colors().grid_line))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(results_colors().grid_hover)))
                            .when(whole && !inspected, |s| {
                                s.bg(rgba(results_colors().match_bg))
                            })
                            .when(inspected, |s| s.bg(rgb(results_colors().grid_selected)))
                            .font_family(fonts().mono.clone())
                            .text_size(px(13.))
                            .when(raw.is_none(), |s| s.italic())
                            .when(family == ValueFamily::Number, |s| s.justify_end())
                            .text_color(rgb(value_color(raw, family, &results_colors())))
                            // A click picks the value the panel shows; a
                            // double click opens the panel on it.
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    window.focus(&this.focus, cx);
                                    this.inspector.cell = Some((row, col));
                                    this.menu_column = None;
                                    if event.click_count >= 2 {
                                        this.open_inspector(Inspector::Value, cx);
                                    } else {
                                        this.refresh_column_stats(cx);
                                    }
                                    cx.notify();
                                }),
                            )
                            .child(div().truncate().child(painted(displayed, mark)))
                    }))
            })
            .collect()
    }
}
