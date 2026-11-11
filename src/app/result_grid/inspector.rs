//! The value viewer and statistics for the selection, page and columns.

use super::selection::Gesture;
use super::table::value_color;
use super::{HEADER_HEIGHT, ResultGrid, ResultGridEvent};
use crate::app::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::cell_format::{self, ValueFamily};
use jykre::grid::{self, selection_block};
use jykre::grid_edit::EditSession;
use jykre::stats::{self, BlockSummary, ColumnSummary, Sample, SelectionSummary};
use jykre::tr;
use std::sync::Arc;
use std::time::Duration;

pub(super) struct InspectorState {
    pub(super) panel: Option<Inspector>,
    scope: StatsScope,
    /// The selection's statistics and the summary generation they are for.
    selection_block: Option<(u64, BlockSummary)>,
    /// Whether `selection_block` holds each column's numbers and distinct
    /// values, which only the panel shows.
    selection_detail: bool,
    result_stats: Option<ResultStats>,
    result_task: Option<Task<()>>,
    /// The cell the value viewer shows, as (data row, column): the focused
    /// cell, or the one double-clicked in the records view.
    pub(super) cell: Option<(usize, usize)>,
    value_input: Option<Entity<TextareaState>>,
    /// What `value_input` holds: (result version, edits version, data row,
    /// column).
    value_shown: Option<(u64, u64, usize, usize)>,
    column_stats: Option<ColumnStats>,
    column_task: Option<Task<()>>,
    /// Bumped with every selection, so a summary of an older one is dropped.
    summary_generation: u64,
    summary_task: Option<Task<()>>,
    reported_summary: Summary,
}

impl Default for InspectorState {
    fn default() -> Self {
        Self {
            panel: None,
            scope: StatsScope::Result,
            selection_block: None,
            selection_detail: false,
            result_stats: None,
            result_task: None,
            cell: None,
            value_input: None,
            value_shown: None,
            column_stats: None,
            column_task: None,
            summary_generation: 0,
            summary_task: None,
            reported_summary: Summary::None,
        }
    }
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

/// Selections up to this many cells are summarized at once; larger ones are
/// read a chunk per frame and summarized off the UI thread.
const SYNC_CELLS: usize = 20_000;

/// Cells read per frame while summarizing a large selection or column.
const CHUNK_CELLS: usize = 40_000;

/// The value viewer shows up to this many bytes; Copy copies all of it.
const VIEWER_BYTES: usize = 1024 * 1024;

/// Width of the side panel and of a record's column, in pixels.
const PANEL_WIDTH: f32 = 380.;

/// Counts the cells of `rows` × `columns` that are not NULL and copies the
/// values of the `numeric` columns into `sample`.
/// Columns the statistics of a block list; the others say how many more.
const STATS_COLUMNS: usize = 100;

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

/// A statistic: its name, then its value in the values' font.
fn stat_line(name: &'static str, value: String) -> Div {
    row().justify_between().gap_3().child(label(name)).child(
        div()
            .min_w_0()
            .truncate()
            .font_family(fonts().mono.clone())
            .text_size(px(13.))
            .text_color(rgb(results_colors().text))
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
                .text_color(rgb(results_colors().text))
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
        .bg(rgb(results_colors().chart_track))
        .child(
            div()
                .h_full()
                .w(relative(share.clamp(0., 1.)))
                .rounded(px(2.))
                .bg(rgb(results_colors().chart_fill)),
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
                results_colors().amber,
            ))
            .child(label(tr!("Calculating…"))),
    )
}

impl ResultGrid {
    /// Summarizes the selection for the footer and the statistics panel:
    /// at once for a small one, off the UI thread for a large one, reading
    /// the shared page in place. Each column's numbers and distinct values
    /// are summarized only while the panel shows the selection. A drag is
    /// summarized when it ends.
    pub(super) fn summarize(&mut self, cx: &mut Context<Self>) {
        self.inspector.summary_generation += 1;
        self.inspector.summary_task = None;
        if matches!(self.gesture, Some(Gesture::Selecting { .. })) {
            return;
        }
        let view = self.view();
        let Some((rows, cols)) = self
            .selection
            .and_then(|selection| selection_block(selection, &view.rows, &self.layout))
        else {
            self.inspector.selection_block = None;
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
            self.inspector.selection_block = None;
            self.report_summary(Summary::None, cx);
            return;
        }
        let detail = self.shows_stats(StatsScope::Selection);
        self.inspector.selection_detail = detail;
        let generation = self.inspector.summary_generation;
        // The details cost about twice as much, so fewer cells are read at
        // once for them.
        let sync = if detail { SYNC_CELLS / 4 } else { SYNC_CELLS };
        if cells <= sync {
            let block = stats::block(&self.result, &data_rows, &columns, detail);
            self.report_summary(Summary::Ready(block.selection()), cx);
            self.inspector.selection_block = Some((generation, block));
            return;
        }
        self.report_summary(Summary::Computing, cx);
        // The task shares the page; if it holds the last handle, the page is
        // freed off the UI thread.
        let result = self.result.clone();
        self.inspector.summary_task = Some(cx.spawn(async move |this, cx| {
            let block = cx
                .background_executor()
                .spawn(async move { stats::block(&result, &data_rows, &columns, detail) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.inspector.summary_generation == generation {
                    this.report_summary(Summary::Ready(block.selection()), cx);
                    this.inspector.selection_block = Some((generation, block));
                    // The footer learns it from the event; the grid draws it
                    // only in the panel's selection statistics, so a large
                    // selection does not draw the table again for nothing.
                    if this.shows_stats(StatsScope::Selection) {
                        cx.notify();
                    }
                }
            });
        }));
    }

    /// Whether the side panel shows the statistics of `scope`.
    pub(super) fn shows_stats(&self, scope: StatsScope) -> bool {
        self.inspector.panel == Some(Inspector::Statistics) && self.inspector.scope == scope
    }

    /// The statistics of every row the filter shows, all columns, while the
    /// panel shows them; computed off the UI thread once per result and
    /// filter.
    pub(super) fn refresh_result_stats(&mut self, cx: &mut Context<Self>) {
        if !self.shows_stats(StatsScope::Result) {
            return;
        }
        let key = (self.version, self.layout_version, self.find.filter.clone());
        if self
            .inspector
            .result_stats
            .as_ref()
            .is_some_and(|stats| stats.key == key)
        {
            return;
        }
        self.inspector.result_stats = Some(ResultStats {
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
        self.inspector.result_task = Some(cx.spawn(async move |this, cx| {
            let block = cx
                .background_executor()
                .spawn(async move { stats::block(&result, &rows, &columns, true) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(stats) = &mut this.inspector.result_stats
                    && stats.key == key
                {
                    stats.summary = Some(block);
                    cx.notify();
                }
            });
        }));
    }

    fn report_summary(&mut self, summary: Summary, cx: &mut Context<Self>) {
        if self.inspector.reported_summary != summary {
            self.inspector.reported_summary = summary.clone();
            cx.emit(ResultGridEvent::Summary(summary));
        }
    }

    /// Shows `inspector` in the side panel, or closes it with `None`, and
    /// computes what it shows.
    pub(super) fn set_inspector(&mut self, inspector: Option<Inspector>, cx: &mut Context<Self>) {
        if self.inspector.panel != inspector {
            self.inspector.panel = inspector;
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
        if self.shows_stats(StatsScope::Selection) && !self.inspector.selection_detail {
            self.summarize(cx);
        }
    }

    pub(super) fn open_inspector(&mut self, inspector: Inspector, cx: &mut Context<Self>) {
        self.set_inspector(Some(inspector), cx);
    }

    /// Shows the statistics of `scope` in the side panel.
    fn open_statistics(&mut self, scope: StatsScope, cx: &mut Context<Self>) {
        if self.inspector.scope != scope {
            self.inspector.scope = scope;
            cx.emit(ResultGridEvent::PanelChanged);
        }
        self.set_inspector(Some(Inspector::Statistics), cx);
    }

    /// Whether the side panel shows statistics, for the toolbar's button.
    pub fn statistics_shown(&self) -> bool {
        self.inspector.panel == Some(Inspector::Statistics)
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
        self.inspector.column_stats = None;
        self.open_statistics(StatsScope::Column, cx);
    }

    /// **Column statistics** in the menu: the column right-clicked or
    /// focused, or closes the panel when it already shows them.
    pub(super) fn toggle_column_statistics(&mut self, cx: &mut Context<Self>) {
        if self.shows_stats(StatsScope::Column) {
            self.set_inspector(None, cx);
            return;
        }
        if self.menu_column.is_none() {
            self.menu_column = self.inspector.cell.map(|(_, column)| column);
        }
        self.open_statistics(StatsScope::Column, cx);
    }

    /// The column whose statistics the panel shows: the focused one, or
    /// the one whose header was right-clicked.
    pub(super) fn stats_column(&self) -> Option<usize> {
        self.menu_column
            .or(self.inspector.cell.map(|(_, column)| column))
            .filter(|column| *column < self.result.column_count())
    }

    /// Reads the statistics of the panel's column for the rows the filter
    /// shows, a chunk per frame, and computes them off the UI thread.
    pub(super) fn refresh_column_stats(&mut self, cx: &mut Context<Self>) {
        if !self.shows_stats(StatsScope::Column) {
            return;
        }
        let Some(column) = self.stats_column() else {
            self.inspector.column_stats = None;
            return;
        };
        let key = (self.version, self.find.filter.clone(), column);
        if self
            .inspector
            .column_stats
            .as_ref()
            .is_some_and(|stats| stats.key == key)
        {
            return;
        }
        self.inspector.column_stats = Some(ColumnStats {
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
        self.inspector.column_task = Some(cx.spawn(async move |this, cx| {
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
                if let Some(stats) = &mut this.inspector.column_stats
                    && stats.key == key
                {
                    stats.summary = Some(summary);
                    cx.notify();
                }
            });
        }));
    }

    /// The text page row `row` shows in `column`: its pending or written
    /// edit, else the value read; `None` for NULL.
    fn shown_value(&self, row: usize, column: usize) -> Option<&str> {
        let overlay = self.session().and_then(|session| {
            session.cell_overlay(self.first_row, self.result.row_count(), row, column)
        });
        match overlay {
            Some(overlay) => overlay.value().text(),
            None => self.result.value(row, column),
        }
    }

    /// Shows the inspected cell's value in the viewer, once per cell and
    /// change: a pending edit's value over the one read, as the cell does.
    pub(super) fn sync_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inspector.panel != Some(Inspector::Value) {
            return;
        }
        let edits = self.session().map_or(0, EditSession::version);
        let target = self
            .inspector
            .cell
            .map(|(row, column)| (self.version, edits, row, column));
        if self.inspector.value_shown == target && self.inspector.value_input.is_some() {
            return;
        }
        self.inspector.value_shown = target;
        let text = target
            .and_then(|(_, _, row, column)| self.shown_value(row, column))
            .map(viewer_text)
            .unwrap_or_default();
        let input = self.inspector.value_input.get_or_insert_with(|| {
            cx.new(|cx| {
                let mut state = TextareaState::new(window, cx);
                state.set_readonly(true, cx);
                state
            })
        });
        input.update(cx, |state, cx| state.set_value(text, window, cx));
    }

    /// The side panel: the inspected cell's full value, or its column's
    /// statistics.
    pub(super) fn inspector_panel(&self, inspector: Inspector, cx: &mut Context<Self>) -> Div {
        let tab = |id: &'static str, this: Inspector, title: &'static str| {
            row()
                .id(id)
                .role(Role::Tab)
                .aria_selected(inspector == this)
                .focusable()
                .tab_index(0)
                .focus_visible(|s| s.bg(rgb(results_colors().hover)))
                .h(px(28.))
                .px_2()
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(rgb(if inspector == this {
                    results_colors().text
                } else {
                    results_colors().muted
                }))
                .when(inspector == this, |s| s.bg(rgb(results_colors().surface)))
                .hover(|s| s.bg(rgb(results_colors().hover)))
                .child(title)
                .on_click(cx.listener(move |grid, _, _, cx| grid.open_inspector(this, cx)))
        };
        column()
            .w(px(PANEL_WIDTH))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(rgb(results_colors().border))
            .bg(rgb(results_colors().panel))
            .child(
                row()
                    .h(px(HEADER_HEIGHT + 6.))
                    .flex_none()
                    .px_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(results_colors().border))
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
            let chosen = self.inspector.scope == this;
            div()
                .id(id)
                .role(Role::Tab)
                .aria_selected(chosen)
                .focusable()
                .tab_index(0)
                .focus_visible(|s| s.border_color(rgb(results_colors().mint)))
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
                    results_colors().text
                } else {
                    results_colors().muted
                }))
                .when(chosen, |s| s.bg(rgb(results_colors().hover)))
                .hover(|s| s.text_color(rgb(results_colors().text)))
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
                    .bg(rgb(results_colors().surface))
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
                    .id(("stats-body", self.inspector.scope as usize))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(match self.inspector.scope {
                        StatsScope::Selection => self.selection_stats_body(cx),
                        StatsScope::Result => self.result_stats_body(cx),
                        StatsScope::Column => self.column_body(),
                    }),
            )
    }

    fn selection_stats_body(&self, cx: &mut Context<Self>) -> Div {
        let summary = self
            .inspector
            .selection_block
            .as_ref()
            .filter(|(generation, _)| *generation == self.inspector.summary_generation)
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
            .inspector
            .result_stats
            .as_ref()
            .filter(|stats| {
                stats.key == (self.version, self.layout_version, self.find.filter.clone())
            })
            .and_then(|stats| stats.summary.as_ref());
        let Some(summary) = summary else {
            return calculating("result-stats-status");
        };
        let count = cell_format::count;
        let rows = if self.find.filter.is_empty() {
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
                    (summary.values, results_colors().chart_fill),
                    (summary.nulls, results_colors().chart_null),
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
                results_colors().chart_fill,
            ))
            .child(swatch_line(
                tr!("NULL"),
                count(summary.nulls),
                results_colors().chart_null,
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
                    .focus_visible(|s| s.border_color(rgb(results_colors().mint)))
                    .flex_none()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(5.))
                    .border_1()
                    .border_color(transparent_black())
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(results_colors().hover)))
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
                                            .text_color(rgb(results_colors().text))
                                            .child(info.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(12.))
                                            .text_color(rgb(results_colors().faint))
                                            .child(info.data_type.to_lowercase()),
                                    ),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(rgb(results_colors().muted))
                                    .child(details.join(" · ")),
                            )
                            // How much of the column holds a value.
                            .child(div().pt_1().child(meter(
                                entry.values as f32 / (entry.values + entry.nulls).max(1) as f32,
                            ))),
                    )
                    .child(icon(I::ChevronRight, results_colors().faint))
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
            .inspector
            .cell
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
            .child(match (value, &self.inspector.value_input) {
                (None, _) => div()
                    .flex_1()
                    .text_size(px(13.))
                    .italic()
                    .text_color(rgb(results_colors().value_null))
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
                    .border_color(rgb(results_colors().grid_line))
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
                            .hover(|s| s.bg(rgb(results_colors().hover)))
                            .when(bin.count > 0, |s| {
                                s.child(
                                    div()
                                        .w_full()
                                        .h(relative(share))
                                        .min_h(px(2.))
                                        .rounded_t(px(4.))
                                        .bg(rgb(results_colors().chart_fill)),
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
            .inspector
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
        body.child(label(if self.find.filter.is_empty() {
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
                (filled, results_colors().chart_fill),
                (summary.nulls, results_colors().chart_null),
                (
                    if text { summary.empty } else { 0 },
                    results_colors().chart_empty,
                ),
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
            results_colors().chart_fill,
        ))
        .child(swatch_line(
            tr!("NULL"),
            count(summary.nulls),
            results_colors().chart_null,
        ))
        .when(text, |s| {
            s.child(swatch_line(
                tr!("Empty"),
                count(summary.empty),
                results_colors().chart_empty,
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
                                        .text_color(rgb(value_color(
                                            Some(text),
                                            family,
                                            &results_colors(),
                                        )))
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
}
