//! Dispatch for shortcuts and Escape within the grid.

use super::editing::EditAction;
use super::inspector::Inspector;
use super::{ResultGrid, ResultGridEvent};
use gpui_kit::*;
use jykre::grid::Jump;

/// What a key of the grid does ([`ResultGrid::grid_key`]).
#[derive(Clone, Copy, Debug)]
enum KeyAction {
    Copy {
        names: bool,
    },
    SelectAll,
    Move {
        rows: isize,
        cols: isize,
        extend: bool,
    },
    Jump {
        jump: Jump,
        extend: bool,
    },
    Page {
        down: bool,
        extend: bool,
    },
    Inspect,
    Edit(EditAction),
    Write,
    Discard,
}

/// What a grid key does while a cell's field is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WhileEditing {
    /// Nothing: the field has the keyboard. Its Enter commits the edit
    /// itself, and an arrow at the end of its text stays in it.
    Nothing,
    /// Keep what was typed, then ask to write.
    CommitAndWrite,
    /// Drop what was typed, then ask to discard.
    DropAndDiscard,
}

/// The single-line field lets Enter, and an arrow at the end of its text,
/// through to the grid. Those keys must neither open the value panel on the
/// value read nor move the selection under the field.
fn while_editing(key: KeyAction) -> WhileEditing {
    match key {
        KeyAction::Write => WhileEditing::CommitAndWrite,
        KeyAction::Discard => WhileEditing::DropAndDiscard,
        _ => WhileEditing::Nothing,
    }
}

/// The grid's actions on the table: each one is a key of the keymap.
pub(super) fn grid_keys(grid: Stateful<Div>, cx: &mut Context<ResultGrid>) -> Stateful<Div> {
    use crate::app::shortcuts::*;
    fn on<A: Action>(
        grid: Stateful<Div>,
        key: KeyAction,
        cx: &mut Context<ResultGrid>,
    ) -> Stateful<Div> {
        grid.on_action(cx.listener(move |this, _: &A, window, cx| this.grid_key(key, window, cx)))
    }
    let moved = |rows, cols, extend| KeyAction::Move { rows, cols, extend };
    let jumped = |jump, extend| KeyAction::Jump { jump, extend };
    let grid = on::<GridCopy>(grid, KeyAction::Copy { names: false }, cx);
    let grid = on::<GridCopyWithNames>(grid, KeyAction::Copy { names: true }, cx);
    let grid = on::<GridSelectAll>(grid, KeyAction::SelectAll, cx);
    let grid = on::<GridUp>(grid, moved(-1, 0, false), cx);
    let grid = on::<GridDown>(grid, moved(1, 0, false), cx);
    let grid = on::<GridLeft>(grid, moved(0, -1, false), cx);
    let grid = on::<GridRight>(grid, moved(0, 1, false), cx);
    let grid = on::<GridExtendUp>(grid, moved(-1, 0, true), cx);
    let grid = on::<GridExtendDown>(grid, moved(1, 0, true), cx);
    let grid = on::<GridExtendLeft>(grid, moved(0, -1, true), cx);
    let grid = on::<GridExtendRight>(grid, moved(0, 1, true), cx);
    let grid = on::<GridRowStart>(grid, jumped(Jump::RowStart, false), cx);
    let grid = on::<GridRowEnd>(grid, jumped(Jump::RowEnd, false), cx);
    let grid = on::<GridExtendRowStart>(grid, jumped(Jump::RowStart, true), cx);
    let grid = on::<GridExtendRowEnd>(grid, jumped(Jump::RowEnd, true), cx);
    let grid = on::<GridFirstCell>(grid, jumped(Jump::First, false), cx);
    let grid = on::<GridLastCell>(grid, jumped(Jump::Last, false), cx);
    let grid = on::<GridExtendFirstCell>(grid, jumped(Jump::First, true), cx);
    let grid = on::<GridExtendLastCell>(grid, jumped(Jump::Last, true), cx);
    let grid = on::<GridPageUp>(
        grid,
        KeyAction::Page {
            down: false,
            extend: false,
        },
        cx,
    );
    let grid = on::<GridPageDown>(
        grid,
        KeyAction::Page {
            down: true,
            extend: false,
        },
        cx,
    );
    let grid = on::<GridExtendPageUp>(
        grid,
        KeyAction::Page {
            down: false,
            extend: true,
        },
        cx,
    );
    let grid = on::<GridExtendPageDown>(
        grid,
        KeyAction::Page {
            down: true,
            extend: true,
        },
        cx,
    );
    let grid = on::<GridEditCell>(grid, KeyAction::Edit(EditAction::Cell), cx);
    let grid = on::<GridSetNull>(grid, KeyAction::Edit(EditAction::Null), cx);
    let grid = on::<GridInsertRow>(grid, KeyAction::Edit(EditAction::Insert), cx);
    let grid = on::<GridDeleteRows>(grid, KeyAction::Edit(EditAction::Delete), cx);
    let grid = on::<GridWriteEdits>(grid, KeyAction::Write, cx);
    let grid = on::<GridDiscardEdits>(grid, KeyAction::Discard, cx);
    on::<GridInspect>(grid, KeyAction::Inspect, cx)
}

impl ResultGrid {
    /// `Esc` closes what is on top first: a copy, the panel, the records
    /// view, then the selection. It stays a key of the grid, not an action:
    /// the window's `Dismiss` sees it first and lets it through.
    pub(super) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.key != "escape" || keystroke.modifiers.modified() {
            return;
        }
        if self.edits.editor.is_some() {
            // What was typed is dropped; the cell keeps its value.
            self.finish_cell_edit(false, window, cx);
        } else if self.copy_job.is_some() {
            self.cancel_copy(cx);
        } else if self.inspector.panel.is_some() {
            self.set_inspector(None, cx);
        } else if self.records {
            self.set_records(false, cx);
        } else if self.selection.is_some() {
            self.select(None, cx);
        } else {
            return;
        }
        window.prevent_default();
        cx.stop_propagation();
    }

    /// A key of the grid's keymap ([`crate::app::shortcuts`]), on the table that
    /// has the keyboard. `Enter` with no cell to show lets the key through.
    fn grid_key(&mut self, key: KeyAction, window: &mut Window, cx: &mut Context<Self>) {
        if self.edits.editor.is_some() {
            // The cell's field has the keyboard. A key reaches the grid
            // only when the field lets it through: Enter, which the field
            // commits itself (`InputEvent::PressEnter`), or an arrow at the
            // end of its text. Neither may open the panel or move the
            // selection under the field.
            match while_editing(key) {
                WhileEditing::Nothing => {}
                WhileEditing::CommitAndWrite => {
                    self.finish_cell_edit(true, window, cx);
                    cx.emit(ResultGridEvent::WriteRequested);
                }
                WhileEditing::DropAndDiscard => {
                    self.finish_cell_edit(false, window, cx);
                    cx.emit(ResultGridEvent::DiscardRequested);
                }
            }
            return;
        }
        match key {
            KeyAction::Copy { names } => self.copy_selection(names, window, cx),
            KeyAction::SelectAll => self.select_all(cx),
            KeyAction::Move { rows, cols, extend } => self.move_selection(rows, cols, extend, cx),
            KeyAction::Jump { jump, extend } => self.jump_selection(jump, extend, cx),
            KeyAction::Page { down, extend } => {
                let rows = self.page_rows();
                let rows = if down { rows } else { -rows };
                self.jump_selection(Jump::Rows(rows), extend, cx)
            }
            KeyAction::Inspect if self.inspector.cell.is_some() => {
                self.open_inspector(Inspector::Value, cx)
            }
            KeyAction::Inspect => cx.propagate(),
            KeyAction::Edit(action) => self.edit(action, window, cx),
            KeyAction::Write => cx.emit(ResultGridEvent::WriteRequested),
            KeyAction::Discard => cx.emit(ResultGridEvent::DiscardRequested),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EditAction, Jump, KeyAction, WhileEditing, while_editing};

    #[test]
    fn keys_the_cell_field_lets_through_never_act_on_the_grid() {
        crate::app::source_text();
        // Enter (Inspect by default) and the arrows at the end of the text
        // reach the grid only through the field; none of them may open the
        // value panel or move the selection under it.
        for key in [
            KeyAction::Inspect,
            KeyAction::Move {
                rows: 1,
                cols: 0,
                extend: false,
            },
            KeyAction::Move {
                rows: 0,
                cols: -1,
                extend: true,
            },
            KeyAction::Jump {
                jump: Jump::RowEnd,
                extend: false,
            },
            KeyAction::Page {
                down: true,
                extend: false,
            },
            KeyAction::SelectAll,
            KeyAction::Copy { names: false },
            KeyAction::Edit(EditAction::Cell),
            KeyAction::Edit(EditAction::Null),
            KeyAction::Edit(EditAction::Insert),
            KeyAction::Edit(EditAction::Delete),
        ] {
            assert_eq!(while_editing(key), WhileEditing::Nothing, "{key:?}");
        }
        // Writing keeps what was typed; discarding drops it.
        assert_eq!(
            while_editing(KeyAction::Write),
            WhileEditing::CommitAndWrite
        );
        assert_eq!(
            while_editing(KeyAction::Discard),
            WhileEditing::DropAndDiscard
        );
    }
}
