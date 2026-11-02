use crate::tr;
use gpui_kit::component::{
    ActiveTheme, IconName, Sizable,
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{prelude::FluentBuilder as _, *};

use super::TabId;

#[derive(Clone, PartialEq)]
pub(super) struct PickerItem {
    pub tab: TabId,
    pub title: SharedString,
    pub group: SharedString,
    pub color: Hsla,
}

pub(super) enum PickerEvent {
    Selected(TabId),
    Cancelled,
}

/// Normalize common Portuguese accents without adding a search dependency.
fn searchable(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|character| !matches!(character, '\u{0300}'..='\u{036f}'))
        .map(|character| match character {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            _ => character,
        })
        .collect()
}

fn matching_items(items: &[PickerItem], query: &str) -> Vec<usize> {
    let query = searchable(query);
    let terms: Vec<_> = query.split_whitespace().collect();
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let text = searchable(&format!("{} {}", item.title, item.group));
            terms
                .iter()
                .all(|term| text.contains(term))
                .then_some(index)
        })
        .collect()
}

pub(super) struct TabPicker {
    pub input: Entity<InputState>,
    items: Vec<PickerItem>,
    matches: Vec<usize>,
    query: String,
    cursor: usize,
    active: Option<TabId>,
    scroll: ScrollHandle,
    _subscription: Subscription,
}

impl EventEmitter<PickerEvent> for TabPicker {}

impl TabPicker {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Search tab or group…")));
        let subscription = cx.subscribe(&input, |this, input, event, cx| match event {
            InputEvent::Change => {
                this.query = input.read(cx).value().to_string();
                this.rebuild(None, cx);
            }
            InputEvent::PressEnter { .. } => this.confirm(cx),
            _ => {}
        });
        Self {
            input,
            items: Vec::new(),
            matches: Vec::new(),
            query: String::new(),
            cursor: 0,
            active: None,
            scroll: ScrollHandle::new(),
            _subscription: subscription,
        }
    }

    pub fn reset(
        &mut self,
        items: Vec<PickerItem>,
        active: Option<TabId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.items = items;
        self.active = active;
        self.query.clear();
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.rebuild(active, cx);
    }

    pub fn sync_items(
        &mut self,
        items: Vec<PickerItem>,
        active: Option<TabId>,
        cx: &mut Context<Self>,
    ) {
        if items == self.items && active == self.active {
            return;
        }
        let highlighted = self
            .matches
            .get(self.cursor)
            .map(|index| self.items[*index].tab);
        self.items = items;
        self.active = active;
        self.rebuild(highlighted, cx);
    }

    fn rebuild(&mut self, highlighted: Option<TabId>, cx: &mut Context<Self>) {
        self.matches = matching_items(&self.items, &self.query);
        self.cursor = self
            .matches
            .iter()
            .position(|index| Some(self.items[*index].tab) == highlighted)
            .unwrap_or(0);
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.scroll.scroll_to_item(self.cursor);
        cx.notify();
    }

    fn confirm(&self, cx: &mut Context<Self>) {
        if let Some(index) = self.matches.get(self.cursor) {
            cx.emit(PickerEvent::Selected(self.items[*index].tab));
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => cx.emit(PickerEvent::Cancelled),
            "up" | "down" => {
                if !self.matches.is_empty() {
                    self.cursor = if event.keystroke.key == "down" {
                        (self.cursor + 1) % self.matches.len()
                    } else {
                        (self.cursor + self.matches.len() - 1) % self.matches.len()
                    };
                    self.scroll.scroll_to_item(self.cursor);
                    cx.notify();
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        window.prevent_default();
    }
}

impl Render for TabPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .matches
            .iter()
            .enumerate()
            .map(|(position, index)| {
                let item = &self.items[*index];
                let tab = item.tab;
                let highlighted = self.cursor == position;
                div()
                    .id(("tab-result", tab.0))
                    .role(Role::ListBoxOption)
                    .aria_label(format!("{}, {}", item.title, item.group))
                    .aria_selected(highlighted)
                    .h(px(48.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .rounded_md()
                    .cursor_pointer()
                    .when(highlighted, |this| this.bg(cx.theme().accent))
                    .hover(|style| style.bg(cx.theme().accent))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(PickerEvent::Selected(tab))))
                    .child(
                        div()
                            .w(px(3.))
                            .h(px(28.))
                            .flex_none()
                            .rounded_full()
                            .bg(item.color),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().truncate().text_sm().child(item.title.clone()))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(item.group.clone()),
                            ),
                    )
                    .when(self.active == Some(tab), |this| this.child(IconName::Check))
            })
            .collect();
        div()
            .id("tab-picker")
            .w(px(340.).min(window.viewport_size().width - px(48.)))
            .flex()
            .flex_col()
            .gap_2()
            .text_color(cx.theme().foreground)
            .capture_key_down(cx.listener(Self::key_down))
            .child(
                Input::new(&self.input)
                    .small()
                    .prefix(IconName::Search)
                    .cleanable(true)
                    .aria_label(tr!("Search tabs"))
                    .w_full(),
            )
            .child(
                div()
                    .id("tab-results")
                    .role(Role::ListBox)
                    .aria_label(tr!("Open tabs"))
                    .max_h(px(300.).min((window.viewport_size().height - px(160.)).max(px(96.))))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(rows)
                    .when(self.matches.is_empty(), |this| {
                        this.p_4()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("No tabs found"))
                    }),
            )
            .child(
                div()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .pt_2()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(tr!("↑ ↓ navigate · Enter select · Esc close")),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{PickerItem, TabId, matching_items, rgb, searchable};

    fn items() -> Vec<PickerItem> {
        [
            ("Café sales", "PostgreSQL · Naïve prod"),
            ("Customers", "MySQL · Local"),
            ("Monthly sales", "MySQL · Local"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (title, group))| PickerItem {
            tab: TabId(index as u64 + 1),
            title: title.into(),
            group: group.into(),
            color: rgb(0x477de6).into(),
        })
        .collect()
    }

    #[test]
    fn searches_titles_and_groups_ignoring_case_accents_and_spacing() {
        let items = items();
        assert_eq!(matching_items(&items, "  CAFE   naive "), vec![0]);
        assert_eq!(matching_items(&items, "sales"), vec![0, 2]);
        assert_eq!(matching_items(&items, "mysql sales"), vec![2]);
        assert_eq!(matching_items(&items, "MySQL"), vec![1, 2]);
        assert_eq!(matching_items(&items, " \t "), vec![0, 1, 2]);
        assert!(matching_items(&items, "missing").is_empty());
        assert!(matching_items(&[], "sales").is_empty());
        assert_eq!(searchable("nai\u{0308}ve"), "naive");
    }

    #[test]
    fn results_keep_tab_ids_when_items_move_or_close() {
        let mut items = items();
        items.swap(0, 2);
        let result_ids: Vec<_> = matching_items(&items, "sales")
            .iter()
            .map(|index| items[*index].tab)
            .collect();
        assert_eq!(result_ids, vec![TabId(3), TabId(1)]);
        items.remove(0);
        assert_eq!(
            matching_items(&items, "sales")
                .iter()
                .map(|index| items[*index].tab)
                .collect::<Vec<_>>(),
            vec![TabId(1)]
        );
    }
}
