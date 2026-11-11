//! The grid's context menu and keyboard placement.

use super::editing::{EditAction, Editing};
use super::{HEADER_HEIGHT, ROW_NUMBERS, ResultGrid};
use super::{Inspector, StatsScope};
use crate::app::copying::format_name;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::cell_format;
use jykre::column_layout::ColumnLayout;
use jykre::copy_as::CopyFormat;
use jykre::grid_edit;
use jykre::tr;

/// Hidden columns the menu lists by name; finding a column reaches the rest.
const HIDDEN_IN_MENU: usize = 20;

/// Whether the menu's editing items act, and why not.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EditItems {
    cell: bool,
    null: bool,
    insert: bool,
    delete: bool,
    /// Why the result cannot be edited, shown as a disabled item.
    reason: Option<String>,
}

/// The menu's editing items: only a query tab's result can be edited
/// (ADR-0039, section 1), so a report panel's or a job's grid shows them
/// disabled with the reason, as does a query tab known to be read-only.
fn edit_items(query_tab: bool, read_only: Option<String>, has_selection: bool) -> EditItems {
    let reason = if query_tab {
        read_only
    } else {
        Some(tr!("Only a query tab's result can be edited.").into())
    };
    let can = reason.is_none();
    EditItems {
        cell: can && has_selection,
        null: can && has_selection,
        insert: can,
        delete: can && has_selection,
        reason,
    }
}

/// Whether the grid's cells and headers carry tooltips: not while its menu
/// is open, since GPUI draws a tooltip over every layer, the menu included.
pub(super) fn tooltips_shown(menu_open: bool) -> bool {
    !menu_open
}

impl ResultGrid {
    /// Opens the grid's menu on `column` from the keyboard (Shift F10), at
    /// its header, panned into view: the menu acts on that column whatever
    /// lies under the point.
    pub(super) fn open_column_menu(
        &mut self,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(position) = self.layout.position(column) else {
            return;
        };
        let view = self.view();
        self.reveal(None, position, &view);
        let widths = self.column_widths(&view);
        let strip = self.strip(&widths);
        let width = widths.display().get(position).copied().unwrap_or_default();
        let x = if position < strip.frozen {
            ROW_NUMBERS + widths.span(0..position)
        } else {
            strip.lead + widths.span(strip.frozen..position) - strip.pan
        } + width / 2.;
        let bounds = self.columns_scroll.bounds();
        let x = x.clamp(8., (f32::from(bounds.size.width) - 8.).max(8.));
        let at = point(bounds.left() + px(x), bounds.top() + px(HEADER_HEIGHT / 2.));
        self.menu_column = Some(column);
        self.menu_locked = true;
        let weak = cx.weak_entity();
        // The menu listens for the right button over the table; the press
        // is sent after this key's dispatch ends.
        window.defer(cx, move |window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Right,
                    position: at,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Right,
                    position: at,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                }),
                cx,
            );
            let _ = weak.update(cx, |this, _| this.menu_locked = false);
        });
    }

    /// Shift F10 in the table: the menu on the focused cell's column, or
    /// the first column shown.
    pub(super) fn open_menu_from_keyboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let column = self
            .inspector
            .cell
            .map(|(_, column)| column)
            .filter(|column| self.layout.position(*column).is_some())
            .or_else(|| self.layout.column(0));
        if let Some(column) = column {
            self.open_column_menu(column, window, cx);
        }
    }

    /// The grid's context menu.
    pub(super) fn menu(
        weak: WeakEntity<Self>,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        // GPUI draws tooltips over every layer: the grid sets none while its
        // menu is open, until the menu is dismissed.
        let _ = weak.update(cx, |this, cx| {
            this.menu_open = true;
            cx.notify();
        });
        let closed = weak.clone();
        cx.subscribe_self(move |_, _: &DismissEvent, cx| {
            let _ = closed.update(cx, |this, cx| {
                this.menu_open = false;
                cx.notify();
            });
        })
        .detach();
        let (has_selection, records, inspector, has_column, column_stats, edits) = weak
            .upgrade()
            .map(|this| {
                let this = this.read(cx);
                let read_only = this.shown.and_then(|(id, _)| match this.editing(id) {
                    Some(Editing::ReadOnly(reason)) => Some(grid_edit::read_only_text(reason)),
                    _ => None,
                });
                (
                    this.selection.is_some(),
                    this.records,
                    this.inspector.panel,
                    this.stats_column().is_some(),
                    this.shows_stats(StatsScope::Column),
                    edit_items(this.shown.is_some(), read_only, this.selection.is_some()),
                )
            })
            .unwrap_or_else(|| {
                (
                    false,
                    false,
                    None,
                    false,
                    false,
                    edit_items(false, None, false),
                )
            });
        let action =
            |weak: WeakEntity<ResultGrid>,
             run: fn(&mut ResultGrid, &mut Window, &mut Context<ResultGrid>)| {
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = weak.update(cx, |this, cx| {
                        this.keyboard_back(window, cx);
                        run(this, window, cx);
                        cx.notify();
                    });
                }
            };
        let copy_as_weak = weak.clone();
        let format = crate::app::copying::options(cx).format;
        // The keys in use, named in the items.
        let copy_key = crate::app::shortcuts::label::<crate::app::shortcuts::GridCopy>(cx);
        let copy_names_key =
            crate::app::shortcuts::label::<crate::app::shortcuts::GridCopyWithNames>(cx);
        let find_key = crate::app::shortcuts::label::<crate::app::FindColumn>(cx);
        // What the column items act on: the column right-clicked, else the
        // focused one.
        let columns = weak
            .upgrade()
            .map(|this| {
                let this = this.read(cx);
                let layout = this.layout.clone();
                let column = this
                    .stats_column()
                    .filter(|column| layout.position(*column).is_some());
                let hidden: Vec<(usize, SharedString)> = layout
                    .hidden_columns()
                    .take(HIDDEN_IN_MENU)
                    .filter_map(|column| {
                        let name = this.result.columns().get(column)?.name.clone();
                        Some((column, name.into()))
                    })
                    .collect();
                (layout, column, hidden)
            })
            .unwrap_or_default();
        let (layout, column, hidden) = columns;
        let can = |delta: isize| column.is_some_and(|column| layout.can_move(column, delta));
        let frozen = column.is_some_and(|column| layout.is_frozen(column));
        let hidden_count = layout.hidden();
        let column_action =
            move |weak: WeakEntity<ResultGrid>, change: fn(&mut ColumnLayout, usize) -> bool| {
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = weak.update(cx, |this, cx| {
                        this.keyboard_back(window, cx);
                        if let Some(column) = column {
                            this.change_layout(|layout| change(layout, column), cx);
                        }
                    });
                }
            };
        let show_weak = weak.clone();
        menu.item(
            PopupMenuItem::new(tr!("Copy as {0} ({1})", format_name(format), copy_key))
                .disabled(!has_selection)
                .on_click(action(weak.clone(), |this, window, cx| {
                    this.copy_selection(false, window, cx)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Copy with headers ({0})", copy_names_key))
                .disabled(!has_selection)
                .on_click(action(weak.clone(), |this, window, cx| {
                    this.copy_selection(true, window, cx)
                })),
        )
        .submenu(tr!("Copy as"), window, cx, move |menu, _, _| {
            CopyFormat::ALL.into_iter().fold(menu, |menu, format| {
                let weak = copy_as_weak.clone();
                menu.item(
                    PopupMenuItem::new(format_name(format))
                        .disabled(!has_selection)
                        .on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.keyboard_back(window, cx);
                                this.copy_as(format, window, cx)
                            });
                        }),
                )
            })
        })
        .separator()
        .item(
            PopupMenuItem::new(tr!("Show the full value"))
                .checked(inspector == Some(Inspector::Value))
                .disabled(!has_selection)
                .on_click(action(weak.clone(), |this, _, cx| {
                    if this.inspector.panel == Some(Inspector::Value) {
                        this.set_inspector(None, cx)
                    } else {
                        this.open_inspector(Inspector::Value, cx)
                    }
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Statistics"))
                .checked(inspector == Some(Inspector::Statistics) && !column_stats)
                .on_click(action(weak.clone(), |this, _, cx| {
                    if this.shows_stats(StatsScope::Column) {
                        // From a column's statistics to the selection's or the page's.
                        this.set_inspector(None, cx);
                    }
                    this.toggle_statistics(cx)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Column statistics"))
                .checked(column_stats)
                .disabled(!has_column)
                .on_click(action(weak.clone(), |this, _, cx| {
                    this.toggle_column_statistics(cx)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Show the selected rows as records"))
                .checked(records)
                .on_click(action(weak.clone(), |this, _, cx| {
                    let records = !this.records;
                    this.set_records(records, cx)
                })),
        )
        .separator()
        .item(
            PopupMenuItem::new(tr!("Fit the column to its values"))
                .disabled(!has_column || records)
                .on_click(action(weak.clone(), |this, _, cx| {
                    if let Some(column) = this.stats_column() {
                        this.fit_column(column, cx)
                    }
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Reset column widths"))
                .disabled(records)
                .on_click(action(weak.clone(), |this, _, _| this.widths.clear())),
        )
        .separator()
        .item(
            PopupMenuItem::new(tr!("Hide the column"))
                .disabled(column.is_none() || layout.visible().len() <= 1)
                .on_click(column_action(weak.clone(), ColumnLayout::hide)),
        )
        .item(
            PopupMenuItem::new(tr!("Freeze the column at the left"))
                .checked(frozen)
                .disabled(column.is_none())
                .on_click(column_action(weak.clone(), |layout, column| {
                    if layout.is_frozen(column) {
                        layout.unfreeze(column)
                    } else {
                        layout.freeze(column)
                    }
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Move the column left"))
                .disabled(!can(-1))
                .on_click(column_action(weak.clone(), |layout, column| {
                    layout.move_by(column, -1)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Move the column right"))
                .disabled(!can(1))
                .on_click(column_action(weak.clone(), |layout, column| {
                    layout.move_by(column, 1)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Move the column to the start"))
                .disabled(!can(-1))
                .on_click(column_action(weak.clone(), |layout, column| {
                    layout.move_by(column, isize::MIN)
                })),
        )
        .item(
            PopupMenuItem::new(tr!("Move the column to the end"))
                .disabled(!can(1))
                .on_click(column_action(weak.clone(), |layout, column| {
                    layout.move_by(column, isize::MAX)
                })),
        )
        .submenu(
            tr!("Show a hidden column"),
            window,
            cx,
            move |menu, _, _| {
                let more = hidden_count.saturating_sub(hidden.len());
                let menu = if hidden.is_empty() {
                    menu.item(PopupMenuItem::new(tr!("No column is hidden")).disabled(true))
                } else {
                    menu
                };
                let menu = hidden.iter().fold(menu, |menu, (column, name)| {
                    let (weak, column) = (show_weak.clone(), *column);
                    menu.item(
                        PopupMenuItem::new(name.clone()).on_click(move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.keyboard_back(window, cx);
                                this.change_layout(|layout| layout.show(column), cx)
                            });
                        }),
                    )
                });
                if more > 0 {
                    menu.item(
                        PopupMenuItem::new(tr!(
                            "{0} more: find them by name ({1})",
                            cell_format::count(more),
                            find_key
                        ))
                        .disabled(true),
                    )
                } else {
                    menu
                }
            },
        )
        .item(
            PopupMenuItem::new(tr!(
                "Show all columns ({0} hidden)",
                cell_format::count(hidden_count)
            ))
            .disabled(hidden_count == 0)
            .on_click(action(weak.clone(), |this, _, cx| {
                this.change_layout(ColumnLayout::show_all, cx)
            })),
        )
        .item(
            PopupMenuItem::new(tr!("Reset the column layout"))
                .disabled(layout.is_identity())
                .on_click(action(weak.clone(), |this, _, cx| {
                    this.change_layout(ColumnLayout::reset, cx)
                })),
        )
        .separator()
        .when_some(edits.reason.clone(), |menu, reason| {
            menu.item(PopupMenuItem::new(reason).disabled(true))
        })
        .item(
            PopupMenuItem::new(tr!(
                "Edit the cell ({0})",
                crate::app::shortcuts::label::<crate::app::shortcuts::GridEditCell>(cx)
            ))
            .disabled(!edits.cell)
            .on_click(action(weak.clone(), |this, window, cx| {
                this.edit(EditAction::Cell, window, cx)
            })),
        )
        .item(
            PopupMenuItem::new(tr!(
                "Set to NULL ({0})",
                crate::app::shortcuts::label::<crate::app::shortcuts::GridSetNull>(cx)
            ))
            .disabled(!edits.null)
            .on_click(action(weak.clone(), |this, window, cx| {
                this.edit(EditAction::Null, window, cx)
            })),
        )
        .item(
            PopupMenuItem::new(tr!(
                "Insert a row ({0})",
                crate::app::shortcuts::label::<crate::app::shortcuts::GridInsertRow>(cx)
            ))
            .disabled(!edits.insert)
            .on_click(action(weak.clone(), |this, window, cx| {
                this.edit(EditAction::Insert, window, cx)
            })),
        )
        .item(
            PopupMenuItem::new(tr!(
                "Delete the rows ({0})",
                crate::app::shortcuts::label::<crate::app::shortcuts::GridDeleteRows>(cx)
            ))
            .disabled(!edits.delete)
            .on_click(action(weak.clone(), |this, window, cx| {
                this.edit(EditAction::Delete, window, cx)
            })),
        )
        .separator()
        .item(
            PopupMenuItem::new(tr!("Select all"))
                .on_click(action(weak.clone(), |this, _, cx| this.select_all(cx))),
        )
    }

    /// Gives the grid the keyboard back before an item of its menu acts.
    /// The menu would hand it back to what had it when the menu opened,
    /// and after Shift F10 that is the column's header, which **Hide** or
    /// a move takes away or redraws: the keyboard then went nowhere and the
    /// grid's next key (Ctrl Shift F) did nothing. An action that moves the
    /// keyboard on (the column search) still does, since it runs after.
    fn keyboard_back(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{EditItems, edit_items, tooltips_shown};

    #[test]
    fn only_a_query_tab_offers_the_editing_items() {
        crate::app::source_text();
        // A report panel's or a job's grid: disabled, and the menu says why.
        for selected in [false, true] {
            let items = edit_items(false, None, selected);
            assert!(!items.cell && !items.null && !items.insert && !items.delete);
            assert_eq!(
                items.reason.as_deref(),
                Some("Only a query tab's result can be edited.")
            );
        }
        // A query tab known to be read-only gives its own reason.
        let items = edit_items(true, Some("Computed columns only".into()), true);
        assert!(!items.cell && !items.null && !items.insert && !items.delete);
        assert_eq!(items.reason.as_deref(), Some("Computed columns only"));
        // A query tab: inserting needs no selection, the others do.
        assert_eq!(
            edit_items(true, None, false),
            EditItems {
                cell: false,
                null: false,
                insert: true,
                delete: false,
                reason: None,
            }
        );
        assert_eq!(
            edit_items(true, None, true),
            EditItems {
                cell: true,
                null: true,
                insert: true,
                delete: true,
                reason: None,
            }
        );
    }

    #[test]
    fn no_tooltip_is_drawn_while_the_menu_is_open() {
        crate::app::source_text();
        assert!(!tooltips_shown(true));
        assert!(tooltips_shown(false));
    }
}
