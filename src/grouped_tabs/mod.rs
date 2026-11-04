//! A continuous, grouped tab strip with arbitrary GPUI views as content.
//!
//! Create an `Entity<GroupedTabs>`, add groups and views through its API, and
//! subscribe to [`GroupedTabsEvent::NewTabRequested`] to supply new content.
//! Content remains alive while its tab is open, including across group moves.
use crate::tr;

mod model;
mod picker;

use std::fmt;

use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable,
    button::{Button, ButtonVariants},
    popover::Popover,
    scroll::ScrollableMask,
};
use gpui_kit::{base::Tab as BaseTab, prelude::FluentBuilder as _, *};

use model::{Group, Model};
use picker::{PickerEvent, PickerItem, TabPicker};

const HEADER_HEIGHT: f32 = 24.;
const TAB_HEIGHT: f32 = 38.;
const TAB_WIDTH: f32 = 178.;
const MIN_GROUP_WIDTH: f32 = 142.;
const GROUP_DIVIDER_WIDTH: f32 = 1.;

/// Stable group identity within one [`GroupedTabs`] instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabGroupId(u64);

/// Stable tab identity within one [`GroupedTabs`] instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabId(u64);

/// What a tab's content is doing, shown before its title. The host decides
/// what each state means for its content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TabStatus {
    /// Nothing to show.
    #[default]
    Idle,
    /// Work in progress: a spinner.
    Running,
    /// The last work succeeded.
    Succeeded,
    /// The last work failed.
    Failed,
    /// The last work was stopped before it ended.
    Cancelled,
}

impl TabStatus {
    /// Its name for assistive technologies; empty when idle.
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "",
            Self::Running => tr!("running"),
            Self::Succeeded => tr!("succeeded"),
            Self::Failed => tr!("failed"),
            Self::Cancelled => tr!("cancelled"),
        }
    }
}

/// Invalid IDs leave the component unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabsError {
    GroupNotFound,
    TabNotFound,
}

impl fmt::Display for TabsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::GroupNotFound => "tab group does not exist",
            Self::TabNotFound => "tab does not exist",
        })
    }
}

impl std::error::Error for TabsError {}

/// Mutations are committed before their corresponding events are emitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupedTabsEvent {
    /// The host should create content and call [`GroupedTabs::add_tab`].
    NewTabRequested(TabGroupId),
    SelectionChanged(Option<TabId>),
    TabClosed(TabId),
    /// With [`GroupedTabs::set_confirm_changes`], the user asked to close a
    /// tab; the host confirms and calls [`GroupedTabs::close_tab`].
    CloseRequested(TabId),
    /// With [`GroupedTabs::set_confirm_changes`], the user dropped a tab in
    /// another place; the host confirms and calls [`GroupedTabs::move_tab`].
    MoveRequested {
        tab: TabId,
        group: TabGroupId,
        index: usize,
    },
    TabMoved {
        tab: TabId,
        from: TabGroupId,
        to: TabGroupId,
        index: usize,
    },
}

struct Content {
    view: AnyView,
    focus: FocusHandle,
}

#[derive(Clone)]
struct TabDrag {
    owner: EntityId,
    tab: TabId,
    title: SharedString,
    color: Hsla,
}

impl TabDrag {
    fn belongs_to(&self, owner: EntityId) -> bool {
        self.owner == owner
    }
}

struct DragPreview {
    title: SharedString,
    color: Hsla,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(TAB_WIDTH))
            .h(px(TAB_HEIGHT))
            .flex()
            .items_center()
            .px_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_sm()
            .border_1()
            .border_color(self.color)
            .rounded_md()
            .shadow_md()
            .child(div().truncate().child(self.title.clone()))
    }
}

/// The icon before a tab's title for its status; none while idle.
fn status_icon(status: TabStatus, tab: TabId, cx: &App) -> Option<AnyElement> {
    let theme = cx.theme();
    let icon =
        |name: IconName, color: Hsla| Icon::new(name).size(px(14.)).flex_none().text_color(color);
    Some(match status {
        TabStatus::Idle => return None,
        TabStatus::Running => icon(IconName::LoaderCircle, theme.foreground)
            .with_animation(
                ("tab-status-running", tab.0),
                Animation::new(std::time::Duration::from_millis(900)).repeat(),
                |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
            )
            .into_any_element(),
        TabStatus::Succeeded => icon(IconName::CircleCheck, theme.success).into_any_element(),
        TabStatus::Failed => icon(IconName::CircleX, theme.danger).into_any_element(),
        TabStatus::Cancelled => icon(IconName::Ban, theme.muted_foreground).into_any_element(),
    })
}

/// A slot is a boundary in the destination's current, pre-removal tab list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DropTarget {
    group: TabGroupId,
    slot: usize,
}

fn group_width<C>(group: &Group<C>) -> f32 {
    (group.tabs.len() as f32 * TAB_WIDTH).max(MIN_GROUP_WIDTH) + GROUP_DIVIDER_WIDTH
}

fn target_in_strip<C>(model: &Model<C>, mut x: f32, header: bool) -> Option<DropTarget> {
    if x < 0. {
        return None;
    }
    for group in &model.groups {
        let width = group_width(group);
        if x < width {
            let slot = if header {
                group.tabs.len()
            } else {
                ((x / TAB_WIDTH + 0.5).floor() as usize).min(group.tabs.len())
            };
            return Some(DropTarget {
                group: group.id,
                slot,
            });
        }
        x -= width;
    }
    None
}

fn final_drop_index(source: (TabGroupId, usize), target: DropTarget) -> usize {
    target.slot - usize::from(source.0 == target.group && source.1 < target.slot)
}

/// Stateful tab container. Groups are supplied by the host; tabs own their views.
pub struct GroupedTabs {
    model: Model<Content>,
    scroll: ScrollHandle,
    bar_focus: FocusHandle,
    reveal: Option<TabId>,
    metrics: (Pixels, Pixels, Pixels),
    drop_target: Option<DropTarget>,
    dragging: Option<TabId>,
    picker: Option<Entity<TabPicker>>,
    picker_open: bool,
    picker_subscription: Option<Subscription>,
    /// Closing and moving tabs by hand ask the host first.
    confirm_changes: bool,
}

impl EventEmitter<GroupedTabsEvent> for GroupedTabs {}

#[cfg(test)]
mod tests;

impl GroupedTabs {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            model: Model::default(),
            scroll: ScrollHandle::new(),
            bar_focus: cx.focus_handle().tab_stop(false),
            reveal: None,
            metrics: (px(0.), px(0.), px(0.)),
            drop_target: None,
            dragging: None,
            picker: None,
            picker_open: false,
            picker_subscription: None,
            confirm_changes: false,
        }
    }

    /// Makes the close button and drag and drop emit
    /// [`GroupedTabsEvent::CloseRequested`] and
    /// [`GroupedTabsEvent::MoveRequested`] instead of acting, so the host can
    /// ask the user first.
    pub fn set_confirm_changes(&mut self, confirm: bool) {
        self.confirm_changes = confirm;
    }

    pub fn add_group(
        &mut self,
        name: impl Into<SharedString>,
        color: Hsla,
        cx: &mut Context<Self>,
    ) -> TabGroupId {
        let id = self.model.add_group(name.into(), color);
        cx.notify();
        id
    }

    /// Rename a connection group without recreating its editors.
    pub fn rename_group(
        &mut self,
        group: TabGroupId,
        name: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Result<(), TabsError> {
        let group = self
            .model
            .groups
            .iter_mut()
            .find(|candidate| candidate.id == group)
            .ok_or(TabsError::GroupNotFound)?;
        group.name = name.into();
        cx.notify();
        Ok(())
    }

    /// Recolour a connection group, as when its connection's colour changes.
    pub fn set_group_color(
        &mut self,
        group: TabGroupId,
        color: Hsla,
        cx: &mut Context<Self>,
    ) -> Result<(), TabsError> {
        let group = self
            .model
            .groups
            .iter_mut()
            .find(|candidate| candidate.id == group)
            .ok_or(TabsError::GroupNotFound)?;
        group.color = color;
        cx.notify();
        Ok(())
    }

    /// Removing a group closes its tabs and emits `TabClosed` for each one.
    pub fn remove_group(
        &mut self,
        group: TabGroupId,
        cx: &mut Context<Self>,
    ) -> Result<(), TabsError> {
        let previous = self.model.active;
        let removed = self.model.remove_group(group)?;
        for tab in removed.tabs {
            cx.emit(GroupedTabsEvent::TabClosed(tab.id));
        }
        self.finish_selection(previous, cx);
        Ok(())
    }

    /// Add and select content. Accepts either `AnyView` or an `Entity<V: Render>`.
    pub fn add_tab(
        &mut self,
        group: TabGroupId,
        title: impl Into<SharedString>,
        content: impl Into<AnyView>,
        cx: &mut Context<Self>,
    ) -> Result<TabId, TabsError> {
        let previous = self.model.active;
        let id = self.model.add_tab(
            group,
            title.into(),
            Content {
                view: content.into(),
                focus: cx.focus_handle(),
            },
        )?;
        self.finish_selection(previous, cx);
        Ok(id)
    }

    /// Shows what the tab's content is doing before its title. Redraws only
    /// when the status changed.
    pub fn set_tab_status(
        &mut self,
        tab: TabId,
        status: TabStatus,
        cx: &mut Context<Self>,
    ) -> Result<(), TabsError> {
        if self.model.set_status(tab, status)? {
            cx.notify();
        }
        Ok(())
    }

    /// What the tab shows about its content's work.
    pub fn tab_status(&self, tab: TabId) -> Option<TabStatus> {
        self.model.tab(tab).map(|tab| tab.status)
    }

    pub fn select_tab(&mut self, tab: TabId, cx: &mut Context<Self>) -> Result<(), TabsError> {
        let previous = self.model.active;
        self.model.select(tab)?;
        self.reveal = Some(tab);
        self.finish_selection(previous, cx);
        Ok(())
    }

    pub fn close_tab(&mut self, tab: TabId, cx: &mut Context<Self>) -> Result<(), TabsError> {
        let previous = self.model.active;
        self.model.close(tab)?;
        cx.emit(GroupedTabsEvent::TabClosed(tab));
        self.finish_selection(previous, cx);
        Ok(())
    }

    /// Move and select a tab without recreating its view.
    /// `index` is its final position after removal; oversized indices append.
    pub fn move_tab(
        &mut self,
        tab: TabId,
        group: TabGroupId,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Result<(), TabsError> {
        let previous = self.model.active;
        if let Some((from, _)) = self.model.move_tab(tab, group, index)? {
            let (_, actual_index) = self.model.location(tab).ok_or(TabsError::TabNotFound)?;
            cx.emit(GroupedTabsEvent::TabMoved {
                tab,
                from,
                to: group,
                index: actual_index,
            });
        }
        self.reveal = Some(tab);
        self.finish_selection(previous, cx);
        Ok(())
    }

    pub fn active_tab(&self) -> Option<TabId> {
        self.model.active
    }

    /// The group `tab` is in.
    pub fn group_of(&self, tab: TabId) -> Option<TabGroupId> {
        let (group, _) = self.model.location(tab)?;
        Some(self.model.groups[group].id)
    }

    pub fn group_ids(&self) -> impl Iterator<Item = TabGroupId> + '_ {
        self.model.groups.iter().map(|group| group.id)
    }

    pub fn tab_ids(&self) -> impl Iterator<Item = TabId> + '_ {
        self.model.tab_ids()
    }

    pub fn tab_group(&self, tab: TabId) -> Option<TabGroupId> {
        let (group, _) = self.model.location(tab)?;
        Some(self.model.groups[group].id)
    }

    pub fn tab_content(&self, tab: TabId) -> Option<&AnyView> {
        Some(&self.model.tab(tab)?.content.view)
    }

    fn finish_selection(&mut self, previous: Option<TabId>, cx: &mut Context<Self>) {
        if previous != self.model.active {
            self.reveal = self.model.active;
            cx.emit(GroupedTabsEvent::SelectionChanged(self.model.active));
        }
        if let Some(picker) = &self.picker {
            let items = self.picker_items();
            picker.update(cx, |picker, cx| {
                picker.sync_items(items, self.model.active, cx)
            });
        }
        cx.notify();
    }

    fn picker_items(&self) -> Vec<PickerItem> {
        self.model
            .groups
            .iter()
            .flat_map(|group| {
                group.tabs.iter().map(|tab| PickerItem {
                    tab: tab.id,
                    title: tab.title.clone(),
                    group: group.name.clone(),
                    color: group.color,
                })
            })
            .collect()
    }

    fn ensure_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<TabPicker> {
        if let Some(picker) = &self.picker {
            return picker.clone();
        }
        let picker = cx.new(|cx| TabPicker::new(window, cx));
        self.picker_subscription =
            Some(
                cx.subscribe_in(&picker, window, |this, _, event, window, cx| {
                    this.picker_open = false;
                    if let PickerEvent::Selected(tab) = event {
                        let _ = this.select_tab(*tab, cx);
                        this.focus_tab(*tab, window, cx);
                    }
                    cx.notify();
                }),
            );
        self.picker = Some(picker.clone());
        picker
    }

    fn picker_open_changed(&mut self, open: &bool, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_open = *open;
        if *open {
            let items = self.picker_items();
            let picker = self.ensure_picker(window, cx);
            picker.update(cx, |picker, cx| {
                picker.reset(items, self.model.active, window, cx)
            });
        }
        cx.notify();
    }

    /// Moves keyboard focus to the selected tab, for example after the host
    /// closed one from the keyboard.
    pub fn focus_active(&self, window: &mut Window, cx: &mut App) {
        if let Some(tab) = self.model.active {
            self.focus_tab(tab, window, cx);
        }
    }

    fn focus_tab(&self, tab: TabId, window: &mut Window, cx: &mut App) {
        if let Some(tab) = self.model.tab(tab) {
            window.focus(&tab.content.focus, cx);
        }
    }

    fn activate_group(&mut self, group: TabGroupId, window: &mut Window, cx: &mut Context<Self>) {
        match self.model.group_selection(group) {
            Ok(Some(tab)) => {
                let _ = self.select_tab(tab, cx);
                self.focus_tab(tab, window, cx);
            }
            Ok(None) => cx.emit(GroupedTabsEvent::NewTabRequested(group)),
            Err(_) => {}
        }
    }

    /// The user closes a tab (its close button or a middle click): the host
    /// is asked first when it confirms changes, otherwise the tab closes
    /// and the keyboard stays in the bar.
    fn close_from_user(&mut self, tab_id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_changes {
            cx.emit(GroupedTabsEvent::CloseRequested(tab_id));
            return;
        }
        let focused = self.bar_focus.contains_focused(window, cx);
        let _ = self.close_tab(tab_id, cx);
        if focused && let Some(next) = self.model.active {
            self.focus_tab(next, window, cx);
        }
    }

    fn navigate(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker_open
            || event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        let tabs: Vec<_> = self.model.tab_ids().collect();
        if tabs.is_empty() {
            return;
        }
        let current = tabs
            .iter()
            .position(|id| Some(*id) == self.model.active)
            .unwrap_or(0);
        let index = match event.keystroke.key.as_str() {
            "left" => (current + tabs.len() - 1) % tabs.len(),
            "right" => (current + 1) % tabs.len(),
            "home" => 0,
            "end" => tabs.len() - 1,
            _ => return,
        };
        let _ = self.select_tab(tabs[index], cx);
        self.focus_tab(tabs[index], window, cx);
        cx.stop_propagation();
        window.prevent_default();
    }

    fn scroll_by(&mut self, amount: f32, cx: &mut Context<Self>) {
        let offset = (self.scroll.offset().x + px(amount))
            .max(-self.scroll.max_offset().x)
            .min(px(0.));
        self.scroll.set_offset(point(offset, px(0.)));
        cx.notify();
    }

    fn update_scroll(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let width = self.scroll.bounds().size.width;
        // This callback runs before layout. A newly added tab can still have
        // the previous frame's max_offset, so derive the new extent from the
        // same fixed group widths that the strip is about to lay out.
        let strip_width: f32 = self.model.groups.iter().map(group_width).sum();
        let maximum = (px(strip_width) - width).max(px(0.));
        let mut offset = self.scroll.offset().x.max(-maximum).min(px(0.));
        if width > px(0.)
            && let Some(tab) = self.reveal.take()
            && let Some((left, right)) = self.tab_extent(tab)
        {
            if px(left) + offset < px(0.) {
                offset = -px(left);
            } else if px(right) + offset > width {
                offset = width - px(right);
            }
            offset = offset.max(-maximum).min(px(0.));
        }
        if self.scroll.offset().x != offset {
            self.scroll.set_offset(point(offset, px(0.)));
        }
        let metrics = (width, maximum, offset);
        if self.metrics != metrics {
            self.metrics = metrics;
            cx.notify();
        }
    }

    fn tab_extent(&self, id: TabId) -> Option<(f32, f32)> {
        let mut left = 0.;
        for group in &self.model.groups {
            if let Some(index) = group.tabs.iter().position(|tab| tab.id == id) {
                let start = left + index as f32 * TAB_WIDTH;
                return Some((start, start + TAB_WIDTH));
            }
            left += group_width(group);
        }
        None
    }

    fn target_at(&self, position: Point<Pixels>) -> Option<DropTarget> {
        let viewport = self.scroll.bounds();
        if !viewport.contains(&position) {
            return None;
        }
        let x = f32::from(position.x - viewport.left() - self.scroll.offset().x);
        let header = position.y < viewport.top() + px(HEADER_HEIGHT);
        target_in_strip(&self.model, x, header)
    }

    fn drag_moved(
        &mut self,
        event: &DragMoveEvent<TabDrag>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.drag(cx).belongs_to(cx.entity_id()) {
            return;
        }
        let target = self.target_at(event.event.position);
        if target != self.drop_target {
            self.drop_target = target;
            cx.notify();
        }
    }

    fn drop_tab(&mut self, drag: &TabDrag, window: &mut Window, cx: &mut Context<Self>) {
        if !drag.belongs_to(cx.entity_id()) {
            return;
        }
        if let Some(target) = self.target_at(window.mouse_position())
            && let Some((source_group, source_index)) = self.model.location(drag.tab)
        {
            let index =
                final_drop_index((self.model.groups[source_group].id, source_index), target);
            if self.confirm_changes {
                cx.emit(GroupedTabsEvent::MoveRequested {
                    tab: drag.tab,
                    group: target.group,
                    index,
                });
            } else {
                let _ = self.move_tab(drag.tab, target.group, index, cx);
                self.focus_tab(drag.tab, window, cx);
            }
        }
        self.drop_target = None;
        self.dragging = None;
        cx.notify();
    }

    fn render_group(
        &self,
        group: &Group<Content>,
        left: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = group.id;
        let owner = cx.entity_id();
        let color = group.color;
        let width = group_width(group);
        let label_width = (width - GROUP_DIVIDER_WIDTH - 44.).min(270.);
        let label_offset = (-f32::from(self.scroll.offset().x) - left)
            .max(0.)
            .min((width - GROUP_DIVIDER_WIDTH - label_width - 44.).max(0.));
        let target = self.drop_target.filter(|target| target.group == id);
        let dark = cx.theme().is_dark();
        let group_background =
            cx.theme()
                .tab_bar
                .blend(color.opacity(if dark { 0.10 } else { 0.06 }));
        let active_background =
            cx.theme()
                .tab_active
                .blend(color.opacity(if dark { 0.12 } else { 0.08 }));
        let hover_background =
            cx.theme()
                .tab_active
                .blend(color.opacity(if dark { 0.16 } else { 0.12 }));
        let divider_color = cx.theme().border.blend(cx.theme().foreground.opacity(0.10));
        let tab_separator_color = cx.theme().border.blend(cx.theme().foreground.opacity(0.06));
        let header = div()
            .h(px(HEADER_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .border_t_1()
            .border_color(color.opacity(0.65))
            .bg(group_background)
            .pl_2()
            .pr_1()
            .gap_1()
            .child(
                div()
                    .id(("group-name", id.0))
                    .role(Role::Button)
                    .aria_label(format!(
                        "{}, {} {}",
                        group.name,
                        group.tabs.len(),
                        if group.tabs.len() == 1 {
                            tr!("tab")
                        } else {
                            tr!("tabs")
                        }
                    ))
                    .focusable()
                    .relative()
                    .left(px(label_offset))
                    .w(px(label_width))
                    .flex_none()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .rounded_sm()
                    .focus_visible(|style| style.bg(cx.theme().accent))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.activate_group(id, window, cx)),
                    )
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.activate_group(id, window, cx);
                            cx.stop_propagation();
                            window.prevent_default();
                        }
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .max_w(px((label_width - 36.).max(0.)))
                            .truncate()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(group.name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(group.tabs.len().to_string()),
                    ),
            )
            .child(div().flex_1().min_w_0())
            .child(
                Button::new(("add-tab", id.0))
                    .ghost()
                    .xsmall()
                    .size(px(21.))
                    .text_color(cx.theme().muted_foreground)
                    .icon(IconName::Plus)
                    .accessibility_label(tr!("New tab in {0}", group.name))
                    .tooltip(tr!("New tab in {0}", group.name))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(GroupedTabsEvent::NewTabRequested(id));
                    })),
            );

        let tabs = group.tabs.iter().enumerate().map(|(tab_index, tab)| {
            let tab_id = tab.id;
            let selected = self.model.active == Some(tab_id);
            let show_tab_separator = !selected
                && group
                    .tabs
                    .get(tab_index + 1)
                    .is_some_and(|next| self.model.active != Some(next.id));
            let handle = tab.content.focus.clone();
            let drag = TabDrag {
                owner,
                tab: tab_id,
                title: tab.title.clone(),
                color,
            };
            let weak = cx.weak_entity();
            let hover_group: SharedString = format!("grouped-tabs-{owner}-tab-{}", tab_id.0).into();
            div()
                .id(("tab-container", tab_id.0))
                .group(hover_group.clone())
                // A middle click closes the tab, as in browsers and IDEs,
                // with the same confirmations as its close button.
                .on_mouse_up(
                    MouseButton::Middle,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_from_user(tab_id, window, cx);
                    }),
                )
                .relative()
                .w(px(TAB_WIDTH))
                .h(px(TAB_HEIGHT))
                .flex_none()
                .when(self.dragging == Some(tab_id), |this| this.opacity(0.5))
                .child(
                    BaseTab::new((tr!("tab"), tab_id.0))
                        .selected(selected)
                        .accessibility_label(match tab.status {
                            TabStatus::Idle => format!("{}, {}", tab.title, group.name),
                            status => format!("{}, {}, {}", tab.title, status.label(), group.name),
                        })
                        .track_focus(&handle)
                        .tab_index(if selected { 0 } else { -1 })
                        .tab_stop(selected)
                        .size_full()
                        .justify_start()
                        .pl_3()
                        .pr(px(32.))
                        .border_t_2()
                        .border_color(if selected { color } else { transparent_black() })
                        .bg(if selected {
                            active_background
                        } else {
                            group_background
                        })
                        .text_color(if selected {
                            cx.theme().tab_active_foreground
                        } else {
                            cx.theme().muted_foreground
                        })
                        .text_size(px(12.))
                        .font_weight(if selected {
                            FontWeight::SEMIBOLD
                        } else {
                            FontWeight::NORMAL
                        })
                        .cursor_pointer()
                        .group_hover(hover_group.clone(), |style| style.bg(hover_background))
                        .focus_visible(|style| style.border_color(color))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let _ = this.select_tab(tab_id, cx);
                            this.focus_tab(tab_id, window, cx);
                        }))
                        .on_drag(drag, move |drag, _, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.dragging = Some(drag.tab);
                                this.drop_target = None;
                                cx.notify();
                            });
                            cx.new(|_| DragPreview {
                                title: drag.title.clone(),
                                color: drag.color,
                            })
                        })
                        .gap_1p5()
                        .when_some(status_icon(tab.status, tab_id, cx), |this, icon| {
                            this.child(icon)
                        })
                        .child(div().min_w_0().truncate().child(tab.title.clone())),
                )
                .when(show_tab_separator, |this| {
                    this.child(
                        div()
                            .absolute()
                            .right_0()
                            .top(px(11.))
                            .w(px(1.))
                            .h(px(16.))
                            .bg(tab_separator_color),
                    )
                })
                // The selected tab always shows its close button and it is the
                // next Tab stop after the tab; the others show it on hover.
                .child(
                    div()
                        .absolute()
                        .right(px(4.))
                        .top(px(7.))
                        .when(!selected, |this| {
                            this.invisible()
                                .group_hover(hover_group, |style| style.visible())
                        })
                        .child(
                            Button::new(("close-tab", tab_id.0))
                                .ghost()
                                .xsmall()
                                .size(px(24.))
                                .tab_stop(selected)
                                .icon(IconName::Close)
                                .accessibility_label(tr!("Close {0}", tab.title))
                                .tooltip(tr!("Close {0}", tab.title))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.close_from_user(tab_id, window, cx);
                                })),
                        ),
                )
        });

        div()
            .id(("tab-group", id.0))
            .relative()
            .w(px(width))
            .h(px(HEADER_HEIGHT + TAB_HEIGHT))
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(divider_color)
            .bg(group_background)
            .child(header)
            .child(
                div()
                    .h(px(TAB_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .when(group.tabs.is_empty(), |this| {
                        this.justify_center()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("No tabs"))
                    })
                    .children(tabs),
            )
            .when_some(target, |this, target| {
                if group.tabs.is_empty() {
                    this.child(div().absolute().inset_0().border_2().border_color(color))
                } else {
                    this.child(
                        div()
                            .absolute()
                            .top(px(HEADER_HEIGHT))
                            .left(px((target.slot as f32 * TAB_WIDTH).min(width - 3.)))
                            .w(px(3.))
                            .h(px(TAB_HEIGHT))
                            .bg(color),
                    )
                }
            })
            .into_any_element()
    }
}

impl Render for GroupedTabs {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !cx.has_active_drag() {
            self.drop_target = None;
            self.dragging = None;
        }
        cx.on_next_frame(window, Self::update_scroll);
        let picker = self.ensure_picker(window, cx);
        let search_focus = picker.read(cx).input.focus_handle(cx);
        let tab_count = self.model.tab_ids().count();
        let owner = cx.entity_id();
        let can_left = self.scroll.offset().x < -px(1.);
        let can_right = self.scroll.max_offset().x + self.scroll.offset().x > px(1.);
        let mut group_left = 0.;
        let groups: Vec<_> = self
            .model
            .groups
            .iter()
            .map(|group| {
                let element = self.render_group(group, group_left, cx);
                group_left += group_width(group);
                element
            })
            .collect();
        let content = self
            .model
            .active
            .and_then(|id| self.model.tab(id))
            .map(|tab| tab.content.view.clone());
        div()
            .id("grouped-tabs")
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .min_h_0()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .id("tab-bar")
                    .role(Role::TabList)
                    .aria_label(tr!("Grouped tabs"))
                    .track_focus(&self.bar_focus)
                    .tab_index(0)
                    .tab_stop(false)
                    .h(px(HEADER_HEIGHT + TAB_HEIGHT + 1.))
                    .w_full()
                    .flex_none()
                    .flex()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().tab_bar)
                    .on_key_down(cx.listener(Self::navigate))
                    .on_drag_move(cx.listener(Self::drag_moved))
                    .can_drop(move |value, _, _| {
                        value
                            .downcast_ref::<TabDrag>()
                            .is_some_and(|drag| drag.belongs_to(owner))
                    })
                    .on_drop(cx.listener(Self::drop_tab))
                    .child(
                        div()
                            .id("groups-scroll")
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .overflow_x_scroll()
                            .overflow_y_hidden()
                            .track_scroll(&self.scroll)
                            .on_scroll(cx.listener(|_, _, _, cx| cx.notify()))
                            .children(groups)
                            .child(ScrollableMask::vertical_wheel(&self.scroll)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_between()
                            .w(px(76.))
                            .flex_none()
                            .px_1()
                            .pb_1()
                            .border_l_1()
                            .border_color(cx.theme().border)
                            .child(
                                Popover::new("tab-picker-popover")
                                    .anchor(Anchor::TopRight)
                                    .open(self.picker_open)
                                    .track_focus(&search_focus)
                                    .on_open_change(cx.listener(Self::picker_open_changed))
                                    .trigger(
                                        Button::new("search-tabs")
                                            .ghost()
                                            .xsmall()
                                            .h(px(HEADER_HEIGHT - 2.))
                                            .w(px(64.))
                                            .icon(IconName::Search)
                                            .label(tab_count.to_string())
                                            .accessibility_label(tr!(
                                                "Search tabs, {0} open",
                                                tab_count
                                            ))
                                            .tooltip(tr!("Search the {0} open tabs", tab_count)),
                                    )
                                    .content(move |_, _, _| picker.clone()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .child(
                                        Button::new("scroll-left")
                                            .ghost()
                                            .small()
                                            .icon(IconName::ChevronLeft)
                                            .accessibility_label(tr!("Scroll tabs left"))
                                            .disabled(!can_left)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.scroll_by(380., cx)
                                            })),
                                    )
                                    .child(
                                        Button::new("scroll-right")
                                            .ghost()
                                            .small()
                                            .icon(IconName::ChevronRight)
                                            .accessibility_label(tr!("Scroll tabs right"))
                                            .disabled(!can_right)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.scroll_by(-380., cx)
                                            })),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .id("tab-content")
                    .role(Role::TabPanel)
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .when_some(content.clone(), |this, content| this.child(content))
                    .when(content.is_none(), |this| {
                        this.flex()
                            .items_center()
                            .justify_center()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("No open tabs"))
                    }),
            )
    }
}
