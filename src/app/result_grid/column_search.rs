//! The column picker, its match list and focus.

use super::ResultGrid;
use crate::app::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::cell_format;
use jykre::column_search::{self, ColumnMatch};
use jykre::grid::CellSelection;
use jykre::tr;
use std::{cell::Cell, ops::Range, rc::Rc};

#[derive(Default)]
pub(super) struct ColumnSearchState {
    /// The grid area's size at the last layout, for the search card.
    area: Rc<Cell<(f32, f32)>>,
    /// Kept after closing so reopening can select the previous text.
    dialog: Option<ColumnFind>,
}

/// The dialog that finds a column by name, and what it found.
struct ColumnFind {
    input: Entity<InputState>,
    /// The text the matches are for.
    text: String,
    /// Computed when the text, the layout or the result changes, never per frame.
    matches: Vec<ColumnMatch>,
    /// The column `Enter` chooses.
    highlighted: usize,
    scroll: UniformListScrollHandle,
    /// The dialog is on screen. Closing it keeps the text for the next open
    /// and stops filtering the records view.
    open: bool,
    /// Keeps Tab inside the dialog.
    focus: FocusHandle,
    _events: Subscription,
}

impl ColumnSearchState {
    pub(super) fn active_matches(&self) -> Option<&[ColumnMatch]> {
        let dialog = self.dialog.as_ref()?;
        (dialog.open && !dialog.text.trim().is_empty()).then_some(dialog.matches.as_slice())
    }
}

/// Matches the list shows at once before it scrolls, and the height of each.
const MATCH_ROWS: usize = 10;

const MATCH_HEIGHT: f32 = 30.;

/// Header, field and footer of the column search dialog. The list is what
/// gets shorter when the grid is too small to show all of them.
const COLUMN_SEARCH_CHROME: f32 = 136.;

/// Where the column search card sits in a grid of `width` by `height`:
/// `(card width, top offset, max height)`. `None` until the grid has been
/// laid out.
///
/// The width is `min(560, width - 32)`. The card starts 48 px down when the
/// grid can still show the header, the field, the footer and one match; a
/// shorter grid gives that offset back to the card.
fn column_search_frame(width: f32, height: f32) -> Option<(f32, f32, f32)> {
    if width <= 32. || height <= 32. {
        return None;
    }
    let card_width = (width - 32.).min(560.);
    let room = (height - 16.).max(0.);
    let with_row = COLUMN_SEARCH_CHROME + MATCH_HEIGHT;
    let top = if room >= 48. + with_row {
        48.
    } else if room > COLUMN_SEARCH_CHROME {
        (room - with_row).clamp(0., 48.)
    } else {
        0.
    };
    Some((card_width, top, (room - top).max(0.)))
}

impl ResultGrid {
    pub(super) fn measure_column_search_area(&self, cx: &Context<Self>) -> impl IntoElement {
        let area = self.column_search.area.clone();
        let area_weak = cx.weak_entity();
        let search_open = self
            .column_search
            .dialog
            .as_ref()
            .is_some_and(|find| find.open);
        // The card's width and offset follow the grid's size, known after
        // layout. A size other than the one this frame was built for builds
        // the next frame again, once, while the dialog is open.
        canvas(
            move |bounds, _, cx| {
                let next = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                let previous = area.get();
                if (next.0 - previous.0).abs() > 0.5 || (next.1 - previous.1).abs() > 0.5 {
                    area.set(next);
                    if search_open {
                        cx.defer(move |cx| {
                            let _ = area_weak.update(cx, |_, cx| cx.notify());
                        });
                    }
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0()
    }

    /// Opens the dialog that finds a column by name (`Ctrl Shift F` or the
    /// toolbar's button). Opening it again selects the text it already holds.
    pub fn open_column_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.column_search.dialog.is_none() {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Find a column by name…")));
            let events = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
                if let InputEvent::Change = event {
                    this.search_columns(cx);
                }
            });
            self.column_search.dialog = Some(ColumnFind {
                input,
                text: String::new(),
                matches: Vec::new(),
                highlighted: 0,
                scroll: UniformListScrollHandle::new(),
                open: true,
                focus: cx.focus_handle(),
                _events: events,
            });
        } else if let Some(find) = &mut self.column_search.dialog {
            find.open = true;
        }
        if let Some(find) = &self.column_search.dialog {
            find.input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        }
        // An empty text lists every column; a later change searches again.
        self.search_columns(cx);
    }

    /// Closes the dialog, which stops filtering the records view, and gives
    /// the grid the keyboard. The text stays, so the next open selects it.
    fn close_column_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = &mut self.column_search.dialog {
            find.open = false;
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Finds the columns the field's text names, once per change of the
    /// text, the layout or the result; the highlighted match stays when it
    /// still matches.
    pub(super) fn search_columns(&mut self, cx: &mut Context<Self>) {
        let Some(find) = &mut self.column_search.dialog else {
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
        if let Some(find) = &mut self.column_search.dialog {
            find.highlighted = jykre::palette::step(find.highlighted, find.matches.len(), delta);
            find.scroll
                .scroll_to_item(find.highlighted, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    /// Goes to the highlighted column and closes the dialog.
    fn choose_column_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let column = self
            .column_search
            .dialog
            .as_ref()
            .and_then(|find| find.matches.get(find.highlighted))
            .map(|found| found.column);
        // Close first: the records view then lists every column, and
        // `go_to_column` scrolls to the chosen one in that list.
        self.close_column_search(window, cx);
        if let Some(column) = column {
            self.go_to_column(column, window, cx);
        }
    }

    /// Shows `column` when the layout hides it, scrolls the grid to it and
    /// selects it over the displayed rows (in the records view, scrolls to
    /// its line), and gives the grid the keyboard.
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

    /// The dialog that finds a column by name, drawn over the grid while it
    /// is open. `↑` and `↓` choose a column, `Enter` goes to it and closes
    /// the dialog, and `Esc` closes it. Only the match lines on screen are
    /// built.
    pub(super) fn column_search(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let find = self
            .column_search
            .dialog
            .as_ref()
            .filter(|find| find.open)?;
        let count = cell_format::count;
        let total = count(self.result.column_count());
        let status = if find.text.trim().is_empty() {
            tr!("{0} columns", total).to_string()
        } else if find.matches.is_empty() {
            tr!("No column matches.").to_string()
        } else {
            tr!("{0} of {1} columns", count(find.matches.len()), total).to_string()
        };
        let title = tr!("Find a column");
        let (area_w, area_h) = self.column_search.area.get();
        let frame = column_search_frame(area_w, area_h);
        let preferred = find.matches.len().min(MATCH_ROWS) as f32 * MATCH_HEIGHT;
        let list_h = frame.map_or(preferred, |(_, _, max_h)| {
            preferred.min((max_h - COLUMN_SEARCH_CHROME).max(0.))
        });
        let card = column()
            .id("column-search")
            .role(Role::Dialog)
            .aria_label(title)
            .when_some(frame, |card, (width, top, max_h)| {
                card.w(px(width)).mt(px(top)).max_h(px(max_h))
            })
            .when(frame.is_none(), |card| {
                card.w_full()
                    .max_w(px(560.))
                    .mt(px(48.))
                    .max_h(relative(0.92))
            })
            .min_h_0()
            .flex_shrink_1()
            .rounded(px(10.))
            .bg(rgb(results_colors().panel))
            .border_1()
            .border_color(rgb(results_colors().border))
            .shadow_lg()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "up" => this.step_column_match(-1, cx),
                    "down" => this.step_column_match(1, cx),
                    "enter" => this.choose_column_match(window, cx),
                    "escape" => this.close_column_search(window, cx),
                    _ => return,
                }
                window.prevent_default();
                cx.stop_propagation();
            }))
            .child(
                row()
                    .flex_none()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .justify_between()
                    .child(
                        div()
                            .min_w_0()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(results_colors().text))
                            .child(title),
                    )
                    .child(
                        icon_button("close-column-search", I::X, tr!("Close the column search"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_column_search(window, cx);
                            })),
                    ),
            )
            .child(
                div().flex_none().px_3().pb_2().child(
                    Input::new(&find.input)
                        .small()
                        .prefix(I::TextSearch)
                        .w_full()
                        .aria_label(tr!("Find a column by name")),
                ),
            )
            .child(
                div()
                    .id("column-matches")
                    .role(Role::ListBox)
                    .aria_label(tr!("Columns found"))
                    .flex_grow_0()
                    .flex_shrink_1()
                    .min_h_0()
                    .h(px(list_h))
                    .overflow_hidden()
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
            .child(
                row()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(results_colors().border))
                    .child(
                        div()
                            .id("find-column-status")
                            .role(Role::Status)
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .child(label(status)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .child(label(tr!("↑ ↓ choose · Enter go to · Esc close"))),
                    ),
            )
            .focus_trap("column-search-trap", &find.focus);
        Some(
            modal_backdrop("column-search-backdrop")
                .when(frame.is_none(), |backdrop| backdrop.px(px(16.)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.close_column_search(window, cx)),
                )
                .child(card),
        )
    }

    /// The matches at `range`: the name with what matched marked, its
    /// type, and whether the layout hides it (choosing it shows it).
    fn match_lines(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let Some(find) = &self.column_search.dialog else {
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
                    .when(chosen, |s| s.bg(rgb(results_colors().hover)))
                    .hover(|s| s.bg(rgb(results_colors().hover)))
                    .child(
                        div()
                            .flex_shrink(1.)
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(rgb(results_colors().text))
                            .child(marked(name, found.ranges.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(rgb(results_colors().grid_type_text))
                            .child(format!(": {kind}")),
                    )
                    .when(found.hidden, |s| {
                        s.child(badge(
                            tr!("Hidden: choosing it shows it"),
                            results_colors().amber,
                        ))
                    })
                    // A press, not a click: choosing closes the dialog and
                    // the grid takes the keyboard before the click lands.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            if let Some(find) = &mut this.column_search.dialog {
                                find.highlighted = line;
                            }
                            this.choose_column_match(window, cx);
                        }),
                    )
            })
            .collect()
    }

    /// The performance run's column search (A15): opens the dialog, as
    /// `Ctrl Shift F` does, with its text selected.
    pub(in crate::app) fn benchmark_find_open(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_column_search(window, cx);
    }

    /// Types `text` into the column search: replacing what is selected
    /// (`replace`), or at the caret. The dialog's change finds the matches,
    /// as typing does.
    pub(in crate::app) fn benchmark_find_type(
        &mut self,
        text: &str,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(find) = &self.column_search.dialog {
            let input = find.input.clone();
            let text = text.to_owned();
            input.update(cx, |input, cx| {
                if replace {
                    input.replace(text, window, cx);
                } else {
                    input.insert(text, window, cx);
                }
            });
        }
    }

    /// How many columns the search found.
    pub(in crate::app) fn benchmark_find_matches(&self) -> usize {
        self.column_search
            .dialog
            .as_ref()
            .map_or(0, |find| find.matches.len())
    }

    /// Chooses the match for `column` (the first match without one), as
    /// `↓` and `Enter` do. False when it is not among the matches.
    pub(in crate::app) fn benchmark_find_choose(
        &mut self,
        column: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(find) = &mut self.column_search.dialog else {
            return false;
        };
        let index = match column {
            Some(column) => find.matches.iter().position(|found| found.column == column),
            None => (!find.matches.is_empty()).then_some(0),
        };
        let Some(index) = index else {
            return false;
        };
        find.highlighted = index;
        self.choose_column_match(window, cx);
        true
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_column_search_card_fits_the_grid() {
        assert!(super::column_search_frame(0., 0.).is_none());
        assert!(super::column_search_frame(800., 20.).is_none());
        // Wide and tall: 560 px, 48 px down, 16 px left under the grid.
        assert_eq!(
            super::column_search_frame(800., 600.),
            Some((560., 48., 536.))
        );
        // Narrow: the 32 px gutter comes off the grid's width.
        assert_eq!(
            super::column_search_frame(300., 600.),
            Some((268., 48., 536.))
        );
        // Short: the 48 px offset shrinks so the card still has room.
        assert_eq!(
            super::column_search_frame(800., 220.),
            Some((560., 38., 166.))
        );
        let short = super::column_search_frame(800., 150.).expect("a laid out grid");
        assert!(short.1 < 48.);
        assert!(short.1 + short.2 <= 150. - 16.);
    }
}
