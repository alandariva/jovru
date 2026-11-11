//! The explorer's search: one field above the connections. What is typed in
//! it, or on the tree while the tree has the keyboard, marks the nodes
//! whose name holds the text (Highlight) or leaves only them (Filter),
//! selects the first one, and the arrows then move between the matches. It
//! searches the nodes shown (expanded levels, whose children are all read
//! in the background), in the order shown, without reading the cache;
//! with **Everything** on, the whole local cache is searched instead and
//! its matches show with their ancestors. While typing on the tree the
//! keyboard stays there, so the selection moves while the text is kept.
//! The rules live in `jykre::explorer` (`quick_target`, `quick_step`,
//! `filter_rows`).
use super::connections::OpenTable;
use super::*;
use jykre::explorer::QuickMode;
use std::sync::Arc;

/// The key context of the explorer tree, for its shortcuts.
pub(super) const EXPLORER_CONTEXT: &str = "Explorer";

/// The search field's text as applied to the tree, and how many shown
/// nodes hold it (counted when the text or the rows change).
#[derive(Clone, Debug, Default)]
pub(super) struct QuickSearch {
    pub text: String,
    pub matches: usize,
    /// Which of the matches the selection is on, counted from 1 in the
    /// order shown; `None` while the selected line is not a match.
    pub position: Option<usize>,
}

impl AppView {
    /// The nodes the explorer shows, without the quick search's filter, in
    /// the order shown, with their connection.
    pub(super) fn shown_nodes(&self, cx: &App) -> Vec<(usize, Arc<MetadataObject>)> {
        let searching = self.cache_searching(cx);
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

    /// Whether the field searches the whole local cache now: **Everything**
    /// is on and something is typed. The tree then shows the matches with
    /// their ancestors instead of what is expanded.
    pub(super) fn cache_searching(&self, cx: &App) -> bool {
        self.explorer_everything && !self.search.read(cx).value().is_empty()
    }

    /// A key while the keyboard is inside the explorer tree, before the
    /// focused node sees it: typing edits the search field's text from
    /// here. Returns whether the search used the key.
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
        let mut value = self.search.read(cx).value().to_string();
        match keystroke.key.as_str() {
            "escape" if plain && !value.is_empty() => self.clear_search(window, cx),
            "backspace" if plain && !value.is_empty() => {
                value.pop();
                self.set_search_text(value, window, cx);
            }
            // Up and Down are the explorer's keymap actions, which step
            // through the matches while something is searched.
            "enter" if plain && self.explorer_quick.is_some() => self.activate_selected(window, cx),
            // Left, Right, Home, End, F5 and the rest go to the node.
            _ => match typed {
                // Typing a name starts the search; Space still activates
                // the node while nothing is typed.
                Some(text) if !(value.is_empty() && text.trim().is_empty()) => {
                    value.push_str(&text);
                    self.set_search_text(value, window, cx);
                }
                _ => return false,
            },
        }
        true
    }

    /// Puts `text` in the search field, as typing on the tree does.
    fn set_search_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self.search_text_changed(window, cx);
    }

    /// Empties the search field: the tree shows every node again and the
    /// node found keeps the selection.
    pub(super) fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_search_text(String::new(), window, cx);
    }

    /// The search field's text changed, typed in it or on the tree. A long
    /// tree is searched after a pause in the typing, not on every key.
    pub(super) fn search_text_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_changed(cx);
        self.quick_revision += 1;
        let revision = self.quick_revision;
        if self.search.read(cx).value().is_empty() || self.explorer_entries.len() <= QUICK_AT_ONCE {
            self.apply_quick(window, cx);
            return;
        }
        cx.spawn_in(window, async move |weak, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                if this.quick_revision == revision {
                    this.apply_quick(window, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Applies the field's text to the tree.
    fn apply_quick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.search.read(cx).value().to_string();
        self.explorer_quick = (!text.is_empty()).then_some(QuickSearch {
            text,
            matches: 0,
            position: None,
        });
        self.quick_search_changed(window, cx);
    }

    /// Counts again which match the selection is on, after the selection,
    /// the text or the rows changed; never while drawing.
    pub(super) fn update_quick_position(&mut self, cx: &App) {
        let Some(text) = self.explorer_quick.as_ref().map(|quick| quick.text.clone()) else {
            return;
        };
        let mut position = None;
        if let Some((index, path)) = self.explorer_selected.as_ref() {
            let mut before = 0;
            for (at, object) in self.shown_nodes(cx) {
                let matches = jykre::explorer::quick_matches(&object.name, &text);
                if at == *index && object.path == *path {
                    position = matches.then_some(before + 1);
                    break;
                }
                before += usize::from(matches);
            }
        }
        if let Some(quick) = self.explorer_quick.as_mut() {
            quick.position = position;
        }
    }

    /// Follows a new text or new rows without moving the keyboard: the
    /// selection stays on a node that still matches, or goes to the next
    /// match in the order shown, which is scrolled in. Returns that node.
    pub(super) fn quick_retarget(&mut self, cx: &mut Context<Self>) -> Option<(usize, ObjectPath)> {
        let text = self.explorer_quick.as_ref()?.text.clone();
        let nodes = self.shown_nodes(cx);
        let names: Vec<&str> = nodes
            .iter()
            .map(|(_, object)| object.name.as_str())
            .collect();
        let current = self.selected_position(&nodes);
        // The entries follow the text first, so the match can be scrolled to.
        self.sync_explorer(cx);
        let at = jykre::explorer::quick_target(&names, current, &text)?;
        let target = (nodes[at].0, nodes[at].1.path.clone());
        self.explorer_selected = Some(target.clone());
        self.reveal_explorer_node(target.0, &target.1);
        self.update_quick_position(cx);
        Some(target)
    }

    /// Follows a new text or mode. The keyboard moves only when the tree
    /// has it: typing in the field keeps it in the field.
    fn quick_search_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let in_tree = self.explorer_focus.contains_focused(window, cx);
        let target = if self.explorer_quick.is_some() {
            self.quick_retarget(cx)
        } else {
            // Nothing typed: every node shows again and the selection stays.
            self.sync_explorer(cx);
            self.explorer_selected
                .clone()
                .filter(|(index, path)| self.explorer_position(*index, path).is_some())
        };
        if in_tree {
            if self.cache_searching(cx) {
                // The rows are replaced when the cache answers; the tree
                // itself keeps the keyboard meanwhile, so typing goes on.
                window.focus(&self.explorer_focus, cx);
            } else if let Some((index, path)) = target {
                self.select_node(index, &path, window, cx);
            } else {
                self.keep_explorer_keyboard(window, cx);
            }
        }
        cx.notify();
    }

    /// `↑` or `↓` during the quick search: the previous or next match,
    /// wrapping; with no text, the previous or next node.
    /// Up or Down on the tree itself: a step of the quick search while it
    /// is open; otherwise the key goes on.
    pub(super) fn quick_step_or_pass(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.explorer_quick.is_some() {
            self.quick_step(forward, window, cx);
        } else {
            cx.propagate();
        }
    }

    pub(super) fn quick_step(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    /// `Ctrl F` in the explorer, or the field's buttons: switches between
    /// marking the matches and showing only them.
    pub(super) fn set_quick_mode(
        &mut self,
        mode: QuickMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.explorer_quick_mode = mode;
        // A filter may hide the selection: it goes to a match.
        self.quick_search_changed(window, cx);
    }

    /// The field's **Everything** button: searches the whole local cache,
    /// closed nodes included, or only the nodes the tree shows.
    fn toggle_everything(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.explorer_everything = !self.explorer_everything;
        self.search_changed(cx);
        self.quick_search_changed(window, cx);
    }

    /// The text or **Everything** changed: the cache is searched again
    /// while **Everything** is on and, once that search ends, the selection
    /// from before it comes back when the node chosen meanwhile is not in
    /// the tree. What is expanded never changes with a search.
    pub(super) fn search_changed(&mut self, cx: &mut Context<Self>) {
        let shows = |this: &Self, selected: &Option<(usize, ObjectPath)>| {
            selected.as_ref().is_some_and(|(index, path)| {
                this.connections
                    .get(*index)
                    .is_some_and(|connection| connection.tree.shows(path))
            })
        };
        if self.cache_searching(cx) {
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

    /// The search above the connections: the field, how many nodes match,
    /// how the matches show (Highlight or Filter) and whether the whole
    /// cache is searched (Everything).
    pub(super) fn explorer_search_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.explorer_quick_mode;
        let everything = self.explorer_everything;
        let typed = !self.search.read(cx).value().is_empty();
        let matches = self.explorer_quick.as_ref().map(|quick| quick.matches);
        let position = self
            .explorer_quick
            .as_ref()
            .and_then(|quick| quick.position);
        let count = match matches {
            // Between a key and the search of a long tree.
            None if typed => String::new(),
            None if everything => tr!("Every object").to_string(),
            None => tr!("Nodes shown").to_string(),
            Some(0) if self.explorer_searching => tr!("Searching…").to_string(),
            Some(0) => tr!("No matches").to_string(),
            // Which match the selection is on, as "3/10".
            Some(matches) if position.is_some_and(|at| at <= matches) => {
                format!("{}/{matches}", position.unwrap_or_default())
            }
            Some(1) => tr!("1 match").to_string(),
            Some(matches) => tr!("{0} matches", matches),
        };
        let nothing = matches == Some(0) && !self.explorer_searching;
        let toggle = |id: &'static str, title: &'static str, chosen: bool| {
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
                .flex_none()
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
        };
        let input = Input::new(&self.search)
            .small()
            .prefix(I::Search)
            .w_full()
            .aria_label(tr!("Search objects in the explorer"));
        // A search of the whole cache runs off the UI thread; the field
        // says it is running. Otherwise a button empties the field.
        let input = if self.explorer_searching {
            input.suffix(
                div()
                    .id("explorer-search-pending")
                    .role(Role::Status)
                    .aria_label(tr!("Searching…"))
                    .child(spinning_icon(
                        "explorer-search-spinner",
                        I::LoaderCircle,
                        colors().amber,
                    )),
            )
        } else if typed {
            input.suffix(
                icon_button("clear-explorer-search", I::X, tr!("Clear the search"))
                    .size(px(20.))
                    .on_click(cx.listener(|this, _, window, cx| this.clear_search(window, cx))),
            )
        } else {
            input
        };
        column()
            .id("explorer-search")
            .role(Role::Search)
            .aria_label(tr!("Search in the explorer"))
            .flex_none()
            .px_3()
            .pb_2()
            .gap_1()
            // Esc clears the text, then gives the keyboard to the tree;
            // Down goes to the tree; Enter opens the match selected.
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "escape" if !this.search.read(cx).value().is_empty() => {
                        this.clear_search(window, cx)
                    }
                    "escape" | "down" => this.focus_explorer_tree(window, cx),
                    "enter" if this.explorer_quick.is_some() => {
                        this.activate_selected(window, cx)
                    }
                    _ => return,
                }
                window.prevent_default();
                cx.stop_propagation();
            }))
            .child(input)
            .child(
                row()
                    .gap_1()
                    .child(
                        div()
                            .id("explorer-search-count")
                            .role(Role::Status)
                            .aria_label(count.clone())
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(rgb(if nothing {
                                colors().amber
                            } else {
                                colors().faint
                            }))
                            .child(count),
                    )
                    .child(
                        toggle(
                            "explorer-search-highlight",
                            tr!("Highlight"),
                            mode == QuickMode::Highlight,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.set_quick_mode(QuickMode::Highlight, window, cx)
                        })),
                    )
                    .child(
                        toggle(
                            "explorer-search-filter",
                            tr!("Filter"),
                            mode == QuickMode::Filter,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.set_quick_mode(QuickMode::Filter, window, cx)
                        })),
                    )
                    .child(
                        // An icon, so the count keeps its room.
                        div()
                            .id("explorer-search-everything")
                            .role(Role::Button)
                            .aria_label(tr!("Everything"))
                            .aria_selected(everything)
                            .focusable()
                            .tab_index(0)
                            .border_1()
                            .border_color(transparent_black())
                            .focus_visible(|s| s.border_color(rgb(colors().mint)))
                            .size(px(22.))
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .cursor_pointer()
                            .when(everything, |s| s.bg(rgb(colors().hover)))
                            .hover(|s| s.bg(rgb(colors().hover)))
                            .child(icon(
                                I::FolderSearch,
                                if everything {
                                    colors().mint
                                } else {
                                    colors().faint
                                },
                            ))
                            .tooltip(|window, cx| {
                                Tooltip::new(tr!(
                                    "Search every object of the local cache, also inside closed nodes"
                                ))
                                .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_everything(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
}

/// Up to this many lines, the tree is searched on every key; a longer one
/// waits for a pause in the typing.
const QUICK_AT_ONCE: usize = 5_000;
