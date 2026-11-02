use gpui_kit::{Hsla, SharedString};

use super::{TabGroupId, TabId, TabsError};

pub(super) struct Tab<C> {
    pub id: TabId,
    pub title: SharedString,
    pub content: C,
}

pub(super) struct Group<C> {
    pub id: TabGroupId,
    pub name: SharedString,
    pub color: Hsla,
    pub tabs: Vec<Tab<C>>,
    pub last_selected: Option<TabId>,
}

/// Layout and selection are independent of GPUI windows and content types.
pub(super) struct Model<C> {
    pub groups: Vec<Group<C>>,
    pub active: Option<TabId>,
    next_group: u64,
    next_tab: u64,
}

impl<C> Default for Model<C> {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            active: None,
            next_group: 0,
            next_tab: 0,
        }
    }
}

impl<C> Model<C> {
    pub fn add_group(&mut self, name: SharedString, color: Hsla) -> TabGroupId {
        self.next_group += 1;
        let id = TabGroupId(self.next_group);
        self.groups.push(Group {
            id,
            name,
            color,
            tabs: Vec::new(),
            last_selected: None,
        });
        id
    }

    pub fn group(&self, id: TabGroupId) -> Option<&Group<C>> {
        self.groups.iter().find(|group| group.id == id)
    }

    pub fn location(&self, id: TabId) -> Option<(usize, usize)> {
        self.groups
            .iter()
            .enumerate()
            .find_map(|(group_ix, group)| {
                group
                    .tabs
                    .iter()
                    .position(|tab| tab.id == id)
                    .map(|tab_ix| (group_ix, tab_ix))
            })
    }

    pub fn tab(&self, id: TabId) -> Option<&Tab<C>> {
        let (group, tab) = self.location(id)?;
        Some(&self.groups[group].tabs[tab])
    }

    pub fn add_tab(
        &mut self,
        group: TabGroupId,
        title: SharedString,
        content: C,
    ) -> Result<TabId, TabsError> {
        let group = self
            .groups
            .iter_mut()
            .find(|candidate| candidate.id == group)
            .ok_or(TabsError::GroupNotFound)?;
        self.next_tab += 1;
        let id = TabId(self.next_tab);
        group.tabs.push(Tab { id, title, content });
        self.select(id)?;
        Ok(id)
    }

    pub fn select(&mut self, id: TabId) -> Result<(), TabsError> {
        let (group, _) = self.location(id).ok_or(TabsError::TabNotFound)?;
        self.active = Some(id);
        self.groups[group].last_selected = Some(id);
        Ok(())
    }

    pub fn group_selection(&self, id: TabGroupId) -> Result<Option<TabId>, TabsError> {
        let group = self.group(id).ok_or(TabsError::GroupNotFound)?;
        Ok(group
            .last_selected
            .or_else(|| group.tabs.first().map(|tab| tab.id)))
    }

    fn first_tab(&self) -> Option<TabId> {
        self.groups
            .iter()
            .find_map(|group| group.tabs.first().map(|tab| tab.id))
    }

    /// The next tab, or the previous one if the removed tab was last.
    fn neighbor(group: &Group<C>, removed_index: usize) -> Option<TabId> {
        group
            .tabs
            .get(removed_index.min(group.tabs.len().saturating_sub(1)))
            .map(|tab| tab.id)
    }

    pub fn close(&mut self, id: TabId) -> Result<Tab<C>, TabsError> {
        let (group_ix, tab_ix) = self.location(id).ok_or(TabsError::TabNotFound)?;
        let group = &mut self.groups[group_ix];
        let removed = group.tabs.remove(tab_ix);
        let neighbor = Self::neighbor(group, tab_ix);
        if group.last_selected == Some(id) {
            group.last_selected = neighbor;
        }
        if group.tabs.is_empty() {
            self.groups.remove(group_ix);
        }
        if self.active == Some(id) {
            self.active = neighbor.or_else(|| self.first_tab());
            if let Some(next) = self.active {
                self.select(next)?;
            }
        }
        Ok(removed)
    }

    pub fn remove_group(&mut self, id: TabGroupId) -> Result<Group<C>, TabsError> {
        let index = self
            .groups
            .iter()
            .position(|group| group.id == id)
            .ok_or(TabsError::GroupNotFound)?;
        let removed = self.groups.remove(index);
        if self.active.is_some_and(|id| self.location(id).is_none()) {
            self.active = self.first_tab();
            if let Some(next) = self.active {
                self.select(next)?;
            }
        }
        Ok(removed)
    }

    /// `index` is the final destination index, after removing the source.
    /// Returns the previous group and index only when the layout changed.
    pub fn move_tab(
        &mut self,
        id: TabId,
        destination: TabGroupId,
        index: usize,
    ) -> Result<Option<(TabGroupId, usize)>, TabsError> {
        let destination_ix = self
            .groups
            .iter()
            .position(|group| group.id == destination)
            .ok_or(TabsError::GroupNotFound)?;
        let (source_ix, source_index) = self.location(id).ok_or(TabsError::TabNotFound)?;
        let source_group = self.groups[source_ix].id;
        let count =
            self.groups[destination_ix].tabs.len() - usize::from(source_ix == destination_ix);
        let index = index.min(count);
        if source_ix == destination_ix && source_index == index {
            self.select(id)?;
            return Ok(None);
        }
        let tab = self.groups[source_ix].tabs.remove(source_index);
        if self.groups[source_ix].last_selected == Some(id) {
            self.groups[source_ix].last_selected =
                Self::neighbor(&self.groups[source_ix], source_index);
        }
        self.groups[destination_ix].tabs.insert(index, tab);
        if self.groups[source_ix].tabs.is_empty() {
            self.groups.remove(source_ix);
        }
        self.select(id)?;
        Ok(Some((source_group, source_index)))
    }

    pub fn tab_ids(&self) -> impl Iterator<Item = TabId> + '_ {
        self.groups
            .iter()
            .flat_map(|group| group.tabs.iter().map(|tab| tab.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::rgb;
    use std::{cell::Cell, rc::Rc};

    fn group<C>(model: &mut Model<C>) -> TabGroupId {
        model.add_group("Group".into(), rgb(0x477de6).into())
    }

    fn add<C>(model: &mut Model<C>, group: TabGroupId, content: C) -> TabId {
        model.add_tab(group, "Tab".into(), content).unwrap()
    }

    #[test]
    fn selection_is_global_and_remembered_per_group() {
        let mut model = Model::default();
        let first = group(&mut model);
        let second = group(&mut model);
        let a = add(&mut model, first, ());
        let b = add(&mut model, first, ());
        let c = add(&mut model, second, ());
        model.select(a).unwrap();
        assert_eq!(model.active, Some(a));
        assert_eq!(model.group_selection(first), Ok(Some(a)));
        assert_eq!(model.group_selection(second), Ok(Some(c)));
        model.select(b).unwrap();
        assert_eq!(model.group_selection(first), Ok(Some(b)));
    }

    #[test]
    fn closing_active_prefers_right_then_left_then_another_group() {
        let mut model = Model::default();
        let first = group(&mut model);
        let second = group(&mut model);
        let a = add(&mut model, first, ());
        let b = add(&mut model, first, ());
        let c = add(&mut model, first, ());
        let d = add(&mut model, second, ());
        model.select(b).unwrap();
        model.close(b).unwrap();
        assert_eq!(model.active, Some(c));
        model.close(c).unwrap();
        assert_eq!(model.active, Some(a));
        model.close(a).unwrap();
        assert_eq!(model.active, Some(d));
        assert!(model.group(first).is_none());
        model.close(d).unwrap();
        assert_eq!(model.active, None);
        assert!(model.groups.is_empty());
        assert_eq!(model.group_selection(first), Err(TabsError::GroupNotFound));
    }

    #[test]
    fn closing_last_inactive_tab_removes_only_its_group() {
        let mut model = Model::default();
        let first = group(&mut model);
        let second = group(&mut model);
        let a = add(&mut model, first, ());
        let b = add(&mut model, second, ());
        model.close(a).unwrap();
        assert!(model.group(first).is_none());
        assert_eq!(model.active, Some(b));
        assert_eq!(model.group_selection(second), Ok(Some(b)));
    }

    #[test]
    fn moving_last_tab_to_an_earlier_group_removes_source() {
        let mut model = Model::default();
        let destination = group(&mut model);
        let source = group(&mut model);
        let a = add(&mut model, destination, ());
        let b = add(&mut model, source, ());
        assert_eq!(model.move_tab(b, destination, 0), Ok(Some((source, 0))));
        assert!(model.group(source).is_none());
        assert_eq!(model.tab_ids().collect::<Vec<_>>(), vec![b, a]);
        assert_eq!(model.active, Some(b));
        assert_eq!(model.group_selection(destination), Ok(Some(b)));
    }

    #[test]
    fn closing_inactive_keeps_selection_and_updates_group_memory() {
        let mut model = Model::default();
        let first = group(&mut model);
        let second = group(&mut model);
        let a = add(&mut model, first, ());
        let b = add(&mut model, first, ());
        let c = add(&mut model, second, ());
        model.close(b).unwrap();
        assert_eq!(model.active, Some(c));
        assert_eq!(model.group_selection(first), Ok(Some(a)));
    }

    #[test]
    fn reorder_uses_final_index_in_both_directions() {
        let mut model = Model::default();
        let group = group(&mut model);
        let a = add(&mut model, group, ());
        let b = add(&mut model, group, ());
        let c = add(&mut model, group, ());
        model.move_tab(a, group, 2).unwrap();
        assert_eq!(model.tab_ids().collect::<Vec<_>>(), vec![b, c, a]);
        model.move_tab(a, group, 0).unwrap();
        assert_eq!(model.tab_ids().collect::<Vec<_>>(), vec![a, b, c]);
        assert_eq!(model.move_tab(a, group, 0), Ok(None));
        assert_eq!(model.active, Some(a));
        model.move_tab(b, group, usize::MAX).unwrap();
        assert_eq!(model.tab_ids().collect::<Vec<_>>(), vec![a, c, b]);
    }

    #[test]
    fn moving_keeps_content_identity_and_updates_both_groups() {
        let mut model = Model::default();
        let first = group(&mut model);
        let second = group(&mut model);
        let content = Rc::new(Cell::new(42));
        let a = add(&mut model, first, content.clone());
        let b = add(&mut model, first, Rc::new(Cell::new(7)));
        model.select(a).unwrap();
        model.move_tab(a, second, 0).unwrap();
        assert_eq!(model.group_selection(first), Ok(Some(b)));
        assert_eq!(model.group_selection(second), Ok(Some(a)));
        assert_eq!(model.active, Some(a));
        assert!(Rc::ptr_eq(&model.tab(a).unwrap().content, &content));
        content.set(99);
        assert_eq!(model.tab(a).unwrap().content.get(), 99);
        model.move_tab(b, second, 0).unwrap();
        assert!(model.group(first).is_none());
        assert_eq!(model.tab_ids().collect::<Vec<_>>(), vec![b, a]);
    }

    #[test]
    fn removing_groups_preserves_valid_selection() {
        let mut model = Model::default();
        let first = group(&mut model);
        let second = group(&mut model);
        let a = add(&mut model, first, ());
        let b = add(&mut model, second, ());
        model.remove_group(first).unwrap();
        assert_eq!(model.active, Some(b));
        assert!(model.tab(a).is_none());
        model.remove_group(second).unwrap();
        assert_eq!(model.active, None);
        assert!(model.groups.is_empty());
        let next = group(&mut model);
        let c = add(&mut model, next, ());
        assert_ne!(next, first);
        assert_ne!(c, a);
        assert_ne!(c, b);
    }

    #[test]
    fn removing_active_group_selects_first_available_tab() {
        let mut model = Model::default();
        let empty = group(&mut model);
        let first = group(&mut model);
        let second = group(&mut model);
        let a = add(&mut model, first, ());
        add(&mut model, second, ());
        model.remove_group(second).unwrap();
        assert_eq!(model.active, Some(a));
        assert_eq!(model.group_selection(empty), Ok(None));
    }

    #[test]
    fn invalid_operations_do_not_change_layout_or_selection() {
        let mut model = Model::default();
        let group = group(&mut model);
        let a = add(&mut model, group, ());
        assert_eq!(model.select(TabId(99)), Err(TabsError::TabNotFound));
        assert!(model.close(TabId(99)).is_err());
        assert!(model.remove_group(TabGroupId(99)).is_err());
        assert_eq!(
            model.move_tab(a, TabGroupId(99), 0),
            Err(TabsError::GroupNotFound)
        );
        assert!(model.add_tab(TabGroupId(99), "Invalid".into(), ()).is_err());
        assert_eq!(model.tab_ids().collect::<Vec<_>>(), vec![a]);
        assert_eq!(model.active, Some(a));
    }
}
