//! The copy menu of the result toolbar: copies the selection, chooses the
//! format `Ctrl C` copies in and holds the copy settings (the column names,
//! the CSV separator and quoting, how NULL is written)
//! ([Results](../../../docs/results.md#copying)).
use super::*;
use crate::app::copying::{self, format_name, null_name, separator_name};
use jykre::copy_as::{CopyFormat, CopyOptions, NullText};
use jykre::export::{Quoting, Separator};

/// A checked item of the copy menu that changes the options.
fn option_item(
    workspace: &WeakEntity<Workspace>,
    label: &'static str,
    checked: bool,
    change: impl Fn(&mut CopyOptions) + 'static,
) -> PopupMenuItem {
    let workspace = workspace.clone();
    PopupMenuItem::new(label)
        .checked(checked)
        .on_click(move |_, _, cx| {
            let _ = workspace.update(cx, |this, cx| this.change_copy_options(&change, cx));
        })
}

impl Workspace {
    /// Changes the copy options of every grid and saves them.
    fn change_copy_options(&mut self, change: &dyn Fn(&mut CopyOptions), cx: &mut Context<Self>) {
        let mut options = copying::options(cx);
        change(&mut options);
        let saving = copying::set_options(options, cx);
        cx.spawn(async move |workspace, cx| {
            if let Err(error) = saving.await {
                let _ = workspace.update(cx, |this, cx| {
                    this.notice = Some(Feedback::new(
                        Tone::Error,
                        tr!("Could not save the preference: {0}", error),
                    ));
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// The toolbar's copy button: its label is the format `Ctrl C` copies
    /// in, and its menu copies, chooses that format and holds the settings.
    pub(super) fn copy_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let options = copying::options(cx);
        let workspace = cx.entity().downgrade();
        let has_selection = self.selected_cells > 0;
        let grid = self.grid.downgrade();
        Button::new("copy-menu")
            .ghost()
            .small()
            .icon(I::Copy)
            .label(format_name(options.format))
            .dropdown_caret(true)
            .accessibility_label(tr!(
                "Copy menu, Ctrl+C copies as {0}",
                format_name(options.format)
            ))
            .tooltip(tr!("Copy the selection: format and settings"))
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                let options = copying::options(cx);
                let copy = |headers: bool| {
                    let grid = grid.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ =
                            grid.update(cx, |grid, cx| grid.copy_selection(headers, window, cx));
                    }
                };
                let menu = menu
                    .item(
                        PopupMenuItem::new(tr!("Copy the selection (Ctrl+C)"))
                            .disabled(!has_selection)
                            .on_click(copy(false)),
                    )
                    .item(
                        PopupMenuItem::new(tr!("Copy with column names (Ctrl+Shift+C)"))
                            .disabled(!has_selection)
                            .on_click(copy(true)),
                    )
                    .separator()
                    .label(tr!("Ctrl+C copies as"));
                let menu = CopyFormat::ALL.into_iter().fold(menu, |menu, format| {
                    menu.item(option_item(
                        &workspace,
                        format_name(format),
                        format == options.format,
                        move |options| options.format = format,
                    ))
                });
                let workspace = workspace.clone();
                menu.separator().submenu(
                    tr!("Copy settings"),
                    window,
                    cx,
                    move |menu, window, cx| {
                        let options = copying::options(cx);
                        let separators = workspace.clone();
                        let nulls = workspace.clone();
                        menu.item(option_item(
                            &workspace,
                            tr!("Column names first (text and CSV)"),
                            options.header,
                            |options| options.header = !options.header,
                        ))
                        .item(option_item(
                            &workspace,
                            tr!("Quote every CSV value"),
                            options.quoting == Quoting::All,
                            |options| {
                                options.quoting = match options.quoting {
                                    Quoting::All => Quoting::Needed,
                                    Quoting::Needed => Quoting::All,
                                }
                            },
                        ))
                        .submenu(tr!("CSV separator"), window, cx, move |menu, _, cx| {
                            let current = copying::options(cx).separator;
                            Separator::ALL.into_iter().fold(menu, |menu, separator| {
                                menu.item(option_item(
                                    &separators,
                                    separator_name(separator),
                                    separator == current,
                                    move |options| options.separator = separator,
                                ))
                            })
                        })
                        .submenu(
                            tr!("NULL in text and CSV"),
                            window,
                            cx,
                            move |menu, _, cx| {
                                let current = copying::options(cx).null;
                                [NullText::Empty, NullText::Null].into_iter().fold(
                                    menu,
                                    |menu, null| {
                                        menu.item(option_item(
                                            &nulls,
                                            null_name(null),
                                            null == current,
                                            move |options| options.null = null,
                                        ))
                                    },
                                )
                            },
                        )
                    },
                )
            })
    }
}
