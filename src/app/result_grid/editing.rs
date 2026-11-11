//! The cell editor and pending changes for each result tab.

use super::{ResultGrid, ResultGridEvent};
use crate::app::ui::*;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use jykre::grid::CellSelection;
use jykre::grid_edit::{self, EditSession, EditValue};
use jykre::result::ResultData;
use jykre::result_tabs::ResultId;
use jykre::tr;
use jykre_core::write;
use std::{collections::HashMap, sync::Arc};

#[derive(Default)]
pub(super) struct EditingState {
    /// Editing, by result tab: kept while the tab is open (ADR-0039).
    by_result: HashMap<ResultId, Editing>,
    /// The key that asked for a table still being read, done once it is.
    deferred_action: Option<EditAction>,
    /// The cell being typed in.
    pub(super) editor: Option<CellEditor>,
}

/// Where editing a result tab stands (ADR-0039).
pub enum Editing {
    /// Its table is being read from the session.
    Resolving,
    /// It cannot be edited, for this reason.
    ReadOnly(write::ReadOnlyReason),
    /// Its pending edits.
    Session(Arc<EditSession>),
}

/// What an editing key asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EditAction {
    Cell,
    Null,
    Insert,
    Delete,
}

/// The field a cell is typed in, over the cell.
pub(super) struct CellEditor {
    /// (data row, result column).
    pub(super) cell: (usize, usize),
    pub(super) input: Entity<InputState>,
    _events: Subscription,
}

/// Gives data row `row` of the page (an inserted row past the result's
/// rows) the value `value` in `column`.
fn set_cell(
    session: &mut EditSession,
    result: &ResultData,
    first_row: usize,
    row: usize,
    column: usize,
    value: EditValue,
) -> Result<(), grid_edit::Refused> {
    match row.checked_sub(result.row_count()) {
        Some(inserted) => session.set_inserted(inserted, column, value),
        None => {
            let read = |column: usize| result.value(row, column).map(str::to_owned);
            session.set(first_row + row, &read, column, value)
        }
    }
}

impl ResultGrid {
    pub(super) fn retain_edits(&mut self, open: &[ResultId]) {
        self.edits.by_result.retain(|id, _| open.contains(id));
    }

    /// Where editing result tab `id` stands.
    pub fn editing(&self, id: ResultId) -> Option<&Editing> {
        self.edits.by_result.get(&id)
    }

    /// The pending edits of the result tab on screen.
    pub(super) fn session(&self) -> Option<&EditSession> {
        match self.edits.by_result.get(&self.shown?.0)? {
            Editing::Session(session) => Some(session),
            _ => None,
        }
    }

    /// Changed rows waiting to be written, in the result tabs among `ids`
    /// (all of them with `None`).
    pub fn pending_edits(&self, ids: Option<&[ResultId]>) -> usize {
        self.edits
            .by_result
            .iter()
            .filter(|(id, _)| ids.is_none_or(|ids| ids.contains(id)))
            .map(|(_, editing)| match editing {
                Editing::Session(session) => session.pending(),
                _ => 0,
            })
            .sum()
    }

    /// The workspace's answer to [`ResultGridEvent::EditRequested`]: what
    /// result tab `id` can be written back to. The key that asked is done
    /// now when it is the tab on screen. `None` forgets the question (the
    /// user did not go on).
    pub fn set_editing(
        &mut self,
        id: ResultId,
        editing: Option<Editing>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let after = self.edits.deferred_action.take();
        match editing {
            None => {
                self.edits.by_result.remove(&id);
            }
            Some(editing) => {
                if let Editing::ReadOnly(reason) = &editing {
                    let text = grid_edit::read_only_text(reason);
                    cx.emit(ResultGridEvent::Notice(Feedback::new(Tone::Info, text)));
                }
                let ready = matches!(editing, Editing::Session(_));
                self.edits.by_result.insert(id, editing);
                if ready
                    && self.shown.map(|(shown, _)| shown) == Some(id)
                    && let Some(action) = after
                {
                    self.perform(action, window, cx);
                }
            }
        }
        cx.emit(ResultGridEvent::EditsChanged);
        cx.notify();
    }

    /// Changes result tab `id`'s pending edits with `change` (after a
    /// write, or a discard), and draws them.
    pub fn update_session(
        &mut self,
        id: ResultId,
        change: impl FnOnce(&mut EditSession),
        cx: &mut Context<Self>,
    ) {
        if let Some(Editing::Session(session)) = self.edits.by_result.get_mut(&id) {
            change(Arc::make_mut(session));
            self.refresh_find(cx);
            cx.emit(ResultGridEvent::EditsChanged);
            cx.notify();
        }
    }

    /// An editing key: done now when the tab's table is known, after the
    /// workspace read it otherwise; a read-only result says why.
    pub(super) fn edit(&mut self, action: EditAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, _)) = self.shown else {
            self.edit_notice(tr!("Only a query tab's result can be edited.").into(), cx);
            return;
        };
        match self.edits.by_result.get(&id) {
            None => {
                self.edits.deferred_action = Some(action);
                self.edits.by_result.insert(id, Editing::Resolving);
                cx.emit(ResultGridEvent::EditRequested(id));
                cx.notify();
            }
            Some(Editing::Resolving) => {
                self.edits.deferred_action = Some(action);
            }
            Some(Editing::ReadOnly(reason)) => {
                let text = grid_edit::read_only_text(reason);
                self.edit_notice(text, cx);
            }
            Some(Editing::Session(_)) => self.perform(action, window, cx),
        }
    }

    fn edit_notice(&self, text: String, cx: &mut Context<Self>) {
        cx.emit(ResultGridEvent::Notice(Feedback::new(Tone::Info, text)));
    }

    /// Changes the tab's pending edits with `change`; a refusal is said.
    fn change_edits(
        &mut self,
        change: impl FnOnce(&mut EditSession, &ResultData, usize) -> Result<(), grid_edit::Refused>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((id, _)) = self.shown else {
            return false;
        };
        let (result, first_row) = (self.result.clone(), self.first_row);
        let Some(Editing::Session(session)) = self.edits.by_result.get_mut(&id) else {
            return false;
        };
        let done = change(Arc::make_mut(session), &result, first_row);
        self.refresh_find(cx);
        cx.emit(ResultGridEvent::EditsChanged);
        cx.notify();
        match done {
            Ok(()) => true,
            Err(refused) => {
                self.edit_notice(grid_edit::refused_text(&refused), cx);
                false
            }
        }
    }

    fn perform(&mut self, action: EditAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            EditAction::Cell => self.start_cell_edit(window, cx),
            EditAction::Null => {
                let Some((rows, columns)) = self.selected_block() else {
                    self.edit_notice(tr!("Select the cells to set to NULL.").into(), cx);
                    return;
                };
                self.change_edits(
                    |session, result, first_row| {
                        for row in &rows {
                            for column in &columns {
                                set_cell(
                                    session,
                                    result,
                                    first_row,
                                    *row,
                                    *column,
                                    EditValue::Null,
                                )?;
                            }
                        }
                        Ok(())
                    },
                    cx,
                );
            }
            EditAction::Insert => {
                let mut added = None;
                self.change_edits(
                    |session, result, _| {
                        if session.needs_key() {
                            return Err(grid_edit::Refused::NeedsKey);
                        }
                        added = Some(result.row_count() + session.insert_row());
                        Ok(())
                    },
                    cx,
                );
                let first = self
                    .layout
                    .visible()
                    .iter()
                    .copied()
                    .find(|column| {
                        self.session()
                            .is_some_and(|session| session.editable_column(*column).is_ok())
                    })
                    .or_else(|| self.layout.column(0));
                if let (Some(row), Some(column)) = (added, first) {
                    self.select(Some(CellSelection::cell(row, column)), cx);
                    let view = self.view();
                    let position = view.rows.iter().position(|each| *each == row);
                    if let Some(display) = self.layout.position(column) {
                        self.reveal(position, display, &view);
                    }
                }
            }
            EditAction::Delete => {
                let Some((mut rows, _)) = self.selected_block() else {
                    self.edit_notice(tr!("Select the rows to delete.").into(), cx);
                    return;
                };
                // Inserted rows go from the last, so the others keep their
                // place.
                rows.sort_unstable_by(|a, b| b.cmp(a));
                self.change_edits(
                    |session, result, first_row| {
                        for row in &rows {
                            match row.checked_sub(result.row_count()) {
                                Some(inserted) if inserted < session.inserted() => {
                                    session.remove_inserted(inserted)
                                }
                                Some(_) => {}
                                None => {
                                    let read = |column: usize| {
                                        result.value(*row, column).map(str::to_owned)
                                    };
                                    session.toggle_delete(first_row + row, &read)?;
                                }
                            }
                        }
                        Ok(())
                    },
                    cx,
                );
                self.select(None, cx);
            }
        }
    }

    /// Opens the field over the focused cell, with its value (pending or
    /// read) selected.
    fn start_cell_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, column)) = self.selection.map(|selection| selection.focus) else {
            self.edit_notice(tr!("Select the cell to edit.").into(), cx);
            return;
        };
        let Some(session) = self.session() else {
            return;
        };
        let refused = if session.needs_key() {
            Some(grid_edit::Refused::NeedsKey)
        } else {
            session.editable_column(column).err()
        };
        if let Some(refused) = refused {
            self.edit_notice(grid_edit::refused_text(&refused), cx);
            return;
        }
        let read_rows = self.result.row_count();
        let current = match row.checked_sub(read_rows) {
            Some(inserted) => session
                .inserted_value(inserted, column)
                .and_then(|overlay| overlay.value().text().map(str::to_owned)),
            None => match session.overlay(self.first_row + row, column) {
                Some(overlay) => overlay.value().text().map(str::to_owned),
                None => self.result.value(row, column).map(str::to_owned),
            },
        };
        let input =
            cx.new(|cx| InputState::new(window, cx).default_value(current.unwrap_or_default()));
        let events = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            match event {
                InputEvent::PressEnter { .. } => this.finish_cell_edit(true, window, cx),
                // Leaving the field keeps what was typed, pending.
                InputEvent::Blur => this.finish_cell_edit(true, window, cx),
                _ => {}
            }
        });
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.edits.editor = Some(CellEditor {
            cell: (row, column),
            input,
            _events: events,
        });
        cx.notify();
    }

    /// Closes the cell's field, keeping what was typed as the cell's pending
    /// value (`keep`) or not, and gives the grid the keyboard back.
    pub(super) fn finish_cell_edit(
        &mut self,
        keep: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.edits.editor.take() else {
            return;
        };
        if keep {
            let text = editor.input.read(cx).value().to_string();
            let (row, column) = editor.cell;
            self.change_edits(
                |session, result, first_row| {
                    set_cell(
                        session,
                        result,
                        first_row,
                        row,
                        column,
                        EditValue::Text(text),
                    )
                },
                cx,
            );
        }
        drop(editor);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Starts editing the focused cell (the toolbar's **Edit**).
    pub fn edit_focused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(EditAction::Cell, window, cx);
    }

    /// Adds a row to insert (the edit bar's **Insert a row**).
    pub fn insert_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(EditAction::Insert, window, cx);
    }

    /// Marks the selected rows for deletion (the edit bar's **Delete**).
    pub fn delete_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(EditAction::Delete, window, cx);
    }

    /// Sets the selected cells to NULL (the edit bar's **Set to NULL**).
    pub fn set_null(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(EditAction::Null, window, cx);
    }

    /// One edit of the performance run's editing phase (A10), through the
    /// same methods as the editing keys and the edit bar: by turns, a value
    /// typed into a cell, a block set to NULL, a row inserted and given a
    /// value, and a row marked for deletion, spread over the result.
    pub(in crate::app) fn benchmark_edit(
        &mut self,
        tick: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.result.row_count().max(1);
        let last_column = self.result.column_count().saturating_sub(1);
        let row = (tick.wrapping_mul(7_919)) % rows;
        match tick % 4 {
            0 => {
                self.select(Some(CellSelection::cell(row, 1.min(last_column))), cx);
                self.benchmark_type_cell(&format!("edited {tick}"), window, cx);
            }
            1 => {
                let start = CellSelection::cell(row, 1.min(last_column));
                let block = CellSelection::click(
                    Some(start),
                    (row + 2).min(rows - 1),
                    2.min(last_column),
                    true,
                );
                self.select(Some(block), cx);
                self.edit(EditAction::Null, window, cx);
            }
            2 => {
                // Inserting selects the new row's first column.
                self.edit(EditAction::Insert, window, cx);
                self.benchmark_type_cell(&format!("inserted {tick}"), window, cx);
            }
            _ => {
                self.select(Some(CellSelection::cell(row, 0)), cx);
                self.edit(EditAction::Delete, window, cx);
            }
        }
    }

    /// Opens the field over the focused cell, types `text` and leaves it
    /// with Enter.
    fn benchmark_type_cell(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(EditAction::Cell, window, cx);
        if let Some(editor) = &self.edits.editor {
            let input = editor.input.clone();
            let text = text.to_owned();
            input.update(cx, |input, cx| input.set_value(text, window, cx));
            self.finish_cell_edit(true, window, cx);
        }
    }
}
