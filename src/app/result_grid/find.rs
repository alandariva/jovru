//! Search across the current page, with background scans and cancellation.

use super::editing::Editing;
use super::{ResultGrid, ResultGridEvent};
use crate::app::ui::*;
use gpui_kit::*;
use jykre::cell_format;
use jykre::grid::{self, CellMark, CellSelection, FindMode, GridKey, GridView};
use jykre::tr;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct FindState {
    /// Lowercase text of the open filter; empty when the bar is closed or
    /// the search is only highlighting.
    pub(super) filter: String,
    /// The open result search. Highlight keeps every row and marks hits;
    /// filter hides the other rows through `filter`.
    active: Option<OpenFind>,
    /// The `1/3` last reported, so an unchanged position is not sent.
    reported_position: Option<String>,
    generation: u64,
    task: Option<Task<()>>,
    cancel: Option<FindCancellation>,
    pub(super) view: Option<Arc<GridView>>,
    /// The visible row count last reported while filtering.
    reported_rows: Option<usize>,
}

/// Cancels the worker when its owner closes or supersedes the search.
struct FindCancellation(jykre_core::CancellationToken);

/// The result search while its bar is open.
struct OpenFind {
    mode: FindMode,
    /// Lower case text painted on the visible cells, including keystrokes
    /// that have not settled yet.
    needle: String,
    /// The needle `hits` and `matching_rows` were scanned for.
    scanned: String,
    hits: Vec<(usize, usize)>,
    hit_at: HashMap<(usize, usize), usize>,
    /// 0-based index into `hits`.
    index: Option<usize>,
    matching_rows: usize,
}

impl Drop for FindCancellation {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl FindState {
    pub(super) fn follow_selection(&mut self, selection: Option<CellSelection>) {
        if let Some(find) = self.active.as_mut()
            && find.mode == FindMode::Highlight
            && let Some(focus) = selection.map(|selection| selection.focus)
            && let Some(&at) = find.hit_at.get(&focus)
        {
            find.index = Some(at);
        }
    }
}

impl ResultGrid {
    pub(super) fn publish_search(&mut self, view: &GridView, cx: &mut Context<Self>) {
        // The footer counts the rows the search matches, as the grid does.
        let rows = self.shown_rows(view);
        if rows != self.find.reported_rows {
            self.find.reported_rows = rows;
            cx.emit(ResultGridEvent::RowsShown { rows });
        }
        self.publish_find(cx);
    }

    /// Paints `text` on the visible cells at once. The row count and the
    /// `1/3` wait until [`Self::settle_find`].
    pub fn set_find_needle(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(find) = self.find.active.as_mut() else {
            return;
        };
        let needle = text.to_lowercase();
        if find.needle == needle {
            return;
        }
        find.needle = needle;
        self.find.generation += 1;
        self.find.cancel = None;
        self.find.task = None;
        self.publish_find(cx);
        cx.notify();
    }

    /// Applies a settled search: filter hides the other rows, highlight
    /// keeps them, counts the matching rows and lands on an occurrence.
    pub fn settle_find(&mut self, text: &str, mode: FindMode, cx: &mut Context<Self>) {
        let needle = text.to_lowercase();
        if self.find.active.is_none() {
            self.find.active = Some(OpenFind {
                mode,
                needle: String::new(),
                scanned: String::new(),
                hits: Vec::new(),
                hit_at: HashMap::new(),
                index: None,
                matching_rows: 0,
            });
        }
        if let Some(find) = self.find.active.as_mut() {
            find.mode = mode;
            find.needle = needle.clone();
            let drop_hits = mode == FindMode::Filter || needle.is_empty();
            if drop_hits {
                find.hits.clear();
                find.hit_at.clear();
                find.index = None;
                find.matching_rows = 0;
                find.scanned = if needle.is_empty() && mode == FindMode::Highlight {
                    String::new()
                } else {
                    needle.clone()
                };
            } else {
                find.scanned = needle.clone();
            }
        }
        self.find.filter = if mode == FindMode::Filter {
            needle.clone()
        } else {
            String::new()
        };
        if needle.is_empty() {
            self.find.generation += 1;
            self.find.cancel = None;
            self.find.task = None;
            self.find.view = None;
            self.select(self.selection, cx);
        } else {
            self.rescan_find(cx);
        }
        cx.notify();
    }

    /// Closes the search: every row shows, and nothing stays marked.
    pub fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find.generation += 1;
        self.find.cancel = None;
        self.find.task = None;
        self.find.view = None;
        self.find.active = None;
        self.find.filter.clear();
        self.select(self.selection, cx);
        self.publish_find(cx);
    }

    /// The next or previous occurrence, from the list the last scan stored.
    /// `Enter`, `↓` and `Shift Enter`, `↑` in the field. `false` when
    /// highlighting has no settled occurrence to land on.
    pub fn step_find(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        let Some(find) = self.find.active.as_ref() else {
            return false;
        };
        if self.find.task.is_some()
            || find.mode != FindMode::Highlight
            || find.needle != find.scanned
        {
            return false;
        }
        let Some(index) = grid::step_index(find.hits.len(), find.index, forward) else {
            return false;
        };
        let Some(hit) = find.hits.get(index).copied() else {
            return false;
        };
        if let Some(find) = self.find.active.as_mut() {
            find.index = Some(index);
        }
        self.go_to_hit(hit, cx);
        true
    }

    /// The search follows edits, number formatting, a new page, sort or layout.
    /// Keystrokes still waiting keep the list they will replace.
    pub(super) fn refresh_find(&mut self, cx: &mut Context<Self>) {
        let settled = self
            .find
            .active
            .as_ref()
            .is_some_and(|find| !find.needle.is_empty() && find.needle == find.scanned);
        if settled {
            self.rescan_find(cx);
        }
    }

    /// Rebuilds the search off the UI thread and publishes only the latest scan.
    fn rescan_find(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.active.as_ref() else {
            return;
        };
        if find.needle.is_empty() {
            return;
        }
        let needle = find.needle.clone();
        let mode = find.mode;
        self.find.generation += 1;
        let generation = self.find.generation;
        let result = self.result.clone();
        let session = self.shown.and_then(|(id, _)| match self.editing(id) {
            Some(Editing::Session(session)) => Some(session.clone()),
            _ => None,
        });
        let key = self.view_key();
        let layout = self.layout.clone();
        let first_row = self.first_row;
        let focus = self.selection.map(|selection| selection.focus);
        let cancel = jykre_core::CancellationToken::new();
        self.find.cancel = Some(FindCancellation(cancel.clone()));
        self.find.task = Some(cx.spawn(async move |this, cx| {
            let (view, scan, hit_at) = cx
                .background_executor()
                .spawn(async move {
                    let mut view = GridView::new(
                        &result,
                        GridKey {
                            filter: String::new(),
                            ..key.clone()
                        },
                    );
                    let scan = grid::scan_find_cells(
                        grid::FindCells {
                            result: &result,
                            session: session.as_deref(),
                            first_row,
                            cancel: Some(&cancel),
                        },
                        &view.rows,
                        layout.visible(),
                        &view.families,
                        key.numbers_pt_br,
                        &needle,
                        focus,
                    );
                    if mode == FindMode::Filter {
                        view.rows.clone_from(&scan.matching);
                    }
                    view.key = key;
                    let hit_at: HashMap<_, _> = scan
                        .hits
                        .iter()
                        .enumerate()
                        .map(|(i, hit)| (*hit, i))
                        .collect();
                    (Arc::new(view), scan, hit_at)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.find.generation != generation {
                    return;
                }
                this.find.task = None;
                this.find.cancel = None;
                if !scan.complete {
                    if let Some(find) = this.find.active.as_mut() {
                        find.hits.clear();
                        find.hit_at.clear();
                        find.index = None;
                        find.scanned.clear();
                    }
                    this.publish_find(cx);
                    cx.emit(ResultGridEvent::Notice(Feedback::new(
                        Tone::Warning,
                        tr!("Result search timed out. Try a narrower result."),
                    )));
                    cx.notify();
                    return;
                }
                this.find.view = Some(view);
                if let Some(find) = this.find.active.as_mut() {
                    find.matching_rows = scan.rows;
                    find.hits = scan.hits;
                    find.hit_at = hit_at;
                    find.index = scan.index;
                    find.scanned = find.needle.clone();
                }
                if mode == FindMode::Highlight {
                    let hit = this
                        .find
                        .active
                        .as_ref()
                        .and_then(|find| find.index.and_then(|i| find.hits.get(i).copied()));
                    if let Some(hit) = hit {
                        this.go_to_hit(hit, cx);
                    } else {
                        this.select(this.selection, cx);
                    }
                } else {
                    this.select(this.selection, cx);
                }
                cx.notify();
            });
        }));
        self.publish_find(cx);
        cx.notify();
    }

    fn go_to_hit(&mut self, hit: (usize, usize), cx: &mut Context<Self>) {
        let view = self.view();
        let row_position = view.rows.iter().position(|candidate| *candidate == hit.0);
        let display = self.layout.position(hit.1);
        self.select(Some(CellSelection::cell(hit.0, hit.1)), cx);
        if let Some(display) = display {
            let view = self.view();
            self.reveal(row_position, display, &view);
        }
    }

    /// `1/3` while highlight has settled and the focus is that occurrence.
    fn position_label(&self) -> Option<String> {
        let find = self.find.active.as_ref()?;
        if self.find.task.is_some()
            || find.mode != FindMode::Highlight
            || find.needle.is_empty()
            || find.needle != find.scanned
        {
            return None;
        }
        let index = find.index?;
        let hit = *find.hits.get(index)?;
        if self.selection.map(|selection| selection.focus) != Some(hit) {
            return None;
        }
        Some(format!(
            "{}/{}",
            cell_format::count(index + 1),
            cell_format::count(find.hits.len())
        ))
    }

    fn publish_find(&mut self, cx: &mut Context<Self>) {
        let label = self.position_label();
        if label != self.find.reported_position {
            self.find.reported_position = label.clone();
            cx.emit(ResultGridEvent::FindPosition(label));
        }
    }

    /// Rows the footer counts while the search is open.
    fn shown_rows(&self, view: &GridView) -> Option<usize> {
        if !self.find.filter.is_empty() {
            return Some(view.rows.len());
        }
        self.find.active.as_ref().and_then(|find| {
            (find.mode == FindMode::Highlight
                && !find.scanned.is_empty()
                && find.needle == find.scanned)
                .then_some(find.matching_rows)
        })
    }

    /// The mark for a cell, from the text painted now.
    pub(super) fn cell_mark(&self, displayed: &str) -> CellMark {
        match self.find.active.as_ref() {
            Some(find) if !find.needle.is_empty() => grid::cell_mark(displayed, &find.needle),
            _ => CellMark::None,
        }
    }
}
