//! The explorer's quick search: typing while the tree has the keyboard opens
//! a small field over it, selects the first node whose name holds the text,
//! and the arrows then move between the matches. It searches only the nodes
//! shown (expanded levels), in the order shown, without reading the cache.
//! The keyboard stays on the tree, so the selection moves while the text is
//! kept. The rules live in `jykre::explorer` (`quick_target`, `quick_step`,
//! `filter_rows`).
use super::connections::OpenTable;
use super::*;
use jykre::explorer::QuickMode;
use std::sync::Arc;

/// The key context of the explorer tree, for its shortcuts.
pub(super) const EXPLORER_CONTEXT: &str = "Explorer";

/// What was typed on the tree since the quick search opened, and how many
/// shown nodes hold it (counted when the text or the rows change).
#[derive(Clone, Debug, Default)]
pub(super) struct QuickSearch {
    pub text: String,
    pub matches: usize,
}

impl AppView {
    /// The nodes the explorer shows, without the quick search's filter, in
    /// the order shown, with their connection.
    pub(super) fn shown_nodes(&self, cx: &App) -> Vec<(usize, Arc<MetadataObject>)> {
        let searching = !self.search.read(cx).value().is_empty();
        let mut nodes = Vec::new();
        for (index, connection) in self.connections.iter().enumerate() {
            if connection.input.is_none() || !(connection.expanded || searching) {
                continue;
            }
            let rows = if searching {
                connection.search_rows.as_slice()
            } else {
                connection.tree.rows()
            };
            nodes.extend(rows.iter().filter_map(|row| match row {
                TreeRow::Node { object, .. } => Some((index, object.clone())),
                TreeRow::More { .. } => None,
            }));
        }
        nodes
    }

    fn selected_position(&self, nodes: &[(usize, Arc<MetadataObject>)]) -> Option<usize> {
        let (index, path) = self.explorer_selected.as_ref()?;
        nodes
            .iter()
            .position(|(at, object)| at == index && object.path == *path)
    }

    /// A key while the keyboard is inside the explorer tree, before the
    /// focused node sees it. Returns whether the quick search used it.
    pub(super) fn explorer_capture_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let plain = !modifiers.modified();
        // Shift gives capitals; the other modifiers are shortcuts.
        let typing =
            !(modifiers.control || modifiers.alt || modifiers.platform || modifiers.function);
        let typed = keystroke
            .key_char
            .clone()
            .filter(|text| typing && !text.is_empty() && !text.chars().any(char::is_control));
        let Some(quick) = self.explorer_quick.as_mut() else {
            // Typing a name opens the search; Space still activates the node.
            let Some(text) = typed.filter(|text| !text.trim().is_empty()) else {
                return false;
            };
            self.explorer_quick = Some(QuickSearch { text, matches: 0 });
            self.quick_search_changed(window, cx);
            return true;
        };
        match keystroke.key.as_str() {
            "escape" => self.close_quick_search(window, cx),
            "backspace" if plain => {
                quick.text.pop();
                self.quick_search_changed(window, cx);
            }
            "up" | "down" if plain => self.quick_step(keystroke.key == "down", window, cx),
            "enter" if plain => self.activate_selected(window, cx),
            // Left, Right, Home, End, F5 and the rest go to the node.
            _ => match typed {
                Some(text) => {
                    quick.text.push_str(&text);
                    self.quick_search_changed(window, cx);
                }
                None => return false,
            },
        }
        true
    }

    /// Follows a new text: the selection stays on a node that still
    /// matches, or goes to the next match in the order shown.
    fn quick_search_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .explorer_quick
            .as_ref()
            .map(|quick| quick.text.clone())
            .unwrap_or_default();
        let nodes = self.shown_nodes(cx);
        let names: Vec<&str> = nodes
            .iter()
            .map(|(_, object)| object.name.as_str())
            .collect();
        let current = self.selected_position(&nodes);
        // The entries follow the text first, so the match can be scrolled to.
        self.sync_explorer(cx);
        match jykre::explorer::quick_target(&names, current, &text) {
            Some(at) => {
                let (index, path) = (nodes[at].0, nodes[at].1.path.clone());
                self.select_node(index, &path, window, cx);
            }
            None => self.keep_explorer_keyboard(window, cx),
        }
        cx.notify();
    }

    /// `↑` or `↓` during the quick search: the previous or next match,
    /// wrapping; with no text, the previous or next node.
    fn quick_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .explorer_quick
            .as_ref()
            .map(|quick| quick.text.clone())
            .unwrap_or_default();
        let nodes = self.shown_nodes(cx);
        let names: Vec<&str> = nodes
            .iter()
            .map(|(_, object)| object.name.as_str())
            .collect();
        let current = self.selected_position(&nodes);
        if let Some(at) = jykre::explorer::quick_step(&names, current, &text, forward) {
            let (index, path) = (nodes[at].0, nodes[at].1.path.clone());
            self.select_node(index, &path, window, cx);
        }
    }

    /// `Enter` during the quick search: opens the selected table's Data
    /// view, or expands or collapses any other node, as `Enter` on it does.
    /// The text stays, so the search can go on.
    fn activate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((index, path)) = self.explorer_selected.clone() else {
            return;
        };
        let Some(object) = self
            .shown_nodes(cx)
            .into_iter()
            .find(|(at, object)| *at == index && object.path == path)
            .map(|(_, object)| object)
        else {
            return;
        };
        if jykre::explorer::opens(&object) {
            let preview = object.stored_at.clone().unwrap_or_else(|| path.clone());
            self.open_relation(index, &path, &preview, OpenTable::Data, window, cx);
        } else if jykre::explorer::expandable(&object) {
            self.toggle_node(index, &path, cx);
        }
    }

    /// Closes the quick search; the tree shows every node again and the
    /// node found keeps the selection and the keyboard.
    pub(super) fn close_quick_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.explorer_quick.take().is_none() {
            return;
        }
        self.sync_explorer(cx);
        match self.explorer_selected.clone() {
            Some((index, path)) if self.explorer_position(index, &path).is_some() => {
                self.select_node(index, &path, window, cx)
            }
            _ => window.focus(&self.explorer_focus, cx),
        }
        cx.notify();
    }

    /// `Ctrl F` in the explorer, or the field's buttons: switches between
    /// marking the matches and showing only them, opening the search when
    /// it is closed.
    pub(super) fn set_quick_mode(
        &mut self,
        mode: QuickMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.explorer_quick_mode = mode;
        if self.explorer_quick.is_none() {
            self.explorer_quick = Some(QuickSearch::default());
        }
        // A filter may hide the selection: it goes to a match, and the
        // keyboard comes back to the tree after a click on the buttons.
        self.quick_search_changed(window, cx);
        if let Some((index, path)) = self.explorer_selected.clone()
            && self.explorer_position(index, &path).is_some()
        {
            self.select_node(index, &path, window, cx);
        } else {
            window.focus(&self.explorer_focus, cx);
        }
    }

    /// The explorer search's text changed: the cache is searched again and,
    /// once the text is cleared, the selection from before the search comes
    /// back when the node chosen meanwhile is not in the tree. What is
    /// expanded never changes with a search.
    pub(super) fn search_changed(&mut self, cx: &mut Context<Self>) {
        let shows = |this: &Self, selected: &Option<(usize, ObjectPath)>| {
            selected.as_ref().is_some_and(|(index, path)| {
                this.connections
                    .get(*index)
                    .is_some_and(|connection| connection.tree.shows(path))
            })
        };
        if !self.search.read(cx).value().is_empty() {
            if self.selection_before_search.is_none() {
                self.selection_before_search = Some(self.explorer_selected.clone());
            }
        } else if let Some(before) = self.selection_before_search.take()
            && !shows(self, &self.explorer_selected)
        {
            self.explorer_selected = if shows(self, &before) { before } else { None };
        }
        self.search_cache(false, cx);
    }

    /// Gives the keyboard to the tree: to the selected node when it shows,
    /// otherwise to the first node.
    pub(super) fn focus_explorer_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_explorer(cx);
        let target = self
            .explorer_selected
            .clone()
            .filter(|(index, path)| self.explorer_position(*index, path).is_some())
            .or_else(|| {
                self.explorer_entries.iter().find_map(|entry| match entry {
                    ExplorerEntry::Row(index, row) => {
                        row.object().map(|object| (*index, object.path.clone()))
                    }
                    ExplorerEntry::Connection(_) => None,
                })
            });
        match target {
            Some((index, path)) => self.select_node(index, &path, window, cx),
            None => window.focus(&self.explorer_focus, cx),
        }
    }

    /// When a filter hides the node that has the keyboard, the tree itself
    /// takes it, so typing goes on.
    fn keep_explorer_keyboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((index, path)) = self.explorer_selected.clone() else {
            return;
        };
        if self.explorer_position(index, &path).is_none()
            && self.node_handle(index, &path, cx).is_focused(window)
        {
            window.focus(&self.explorer_focus, cx);
        }
    }

    /// `Shift F10` in the explorer: the selected node's context menu, as a
    /// right click on it opens it.
    pub(super) fn open_node_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bounds) = self.selected_bounds.get() else {
            return;
        };
        let position = bounds.center();
        // The menu listens for the right button over its node; the press is
        // sent after this key's dispatch ends.
        window.defer(cx, move |window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Right,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Right,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                }),
                cx,
            );
        });
    }

    /// The field above the tree while the quick search is open: the text,
    /// how many nodes match, the mode and a button that closes it.
    pub(super) fn quick_search_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let quick = self.explorer_quick.as_ref()?;
        let mode = self.explorer_quick_mode;
        let count = if quick.text.is_empty() {
            tr!("Type to search the nodes shown").to_string()
        } else if quick.matches == 0 {
            tr!("No matches").to_string()
        } else if quick.matches == 1 {
            tr!("1 match").to_string()
        } else {
            tr!("{0} matches", quick.matches)
        };
        let mode_button = |id: &'static str, title: &'static str, this_mode: QuickMode| {
            let chosen = mode == this_mode;
            div()
                .id(id)
                .role(Role::Button)
                .aria_label(title)
                .aria_selected(chosen)
                .focusable()
                .tab_index(0)
                .border_1()
                .border_color(transparent_black())
                .focus_visible(|s| s.border_color(rgb(colors().mint)))
                .px(px(6.))
                .h(px(22.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(rgb(if chosen {
                    colors().text
                } else {
                    colors().faint
                }))
                .when(chosen, |s| s.bg(rgb(colors().hover)))
                .hover(|s| s.text_color(rgb(colors().text)))
                .child(title)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_quick_mode(this_mode, window, cx)
                }))
        };
        Some(
            column()
                .id("quick-search")
                .role(Role::Search)
                .aria_label(tr!("Quick search in the explorer"))
                .flex_none()
                .mx_2()
                .mb_1()
                .p_1()
                .gap_1()
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(colors().mint))
                .bg(rgb(colors().surface))
                .child(
                    row()
                        .gap_2()
                        .px_1()
                        .child(icon(I::Search, colors().mint))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(14.))
                                .child(quick.text.clone()),
                        )
                        .child(
                            icon_button("close-quick-search", I::X, tr!("Close the search"))
                                .size(px(22.))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_quick_search(window, cx)
                                })),
                        ),
                )
                .child(
                    row()
                        .gap_1()
                        .px_1()
                        .child(
                            div()
                                .id("quick-search-count")
                                .role(Role::Status)
                                .aria_label(count.clone())
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(if quick.matches == 0 && !quick.text.is_empty() {
                                    colors().amber
                                } else {
                                    colors().faint
                                }))
                                .child(count),
                        )
                        .child(mode_button(
                            "quick-search-highlight",
                            tr!("Highlight"),
                            QuickMode::Highlight,
                        ))
                        .child(mode_button(
                            "quick-search-filter",
                            tr!("Filter"),
                            QuickMode::Filter,
                        )),
                )
                .into_any_element(),
        )
    }
}
