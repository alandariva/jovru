use super::{
    DropTarget, EntityId, Model, TAB_WIDTH, TabDrag, TabId, final_drop_index, group_width, rgb,
    target_in_strip,
};

#[test]
fn drop_targets_distinguish_tab_halves_headers_and_empty_groups() {
    let mut model = Model::default();
    let first = model.add_group("First".into(), rgb(0x477de6).into());
    model.add_tab(first, "A".into(), ()).unwrap();
    model.add_tab(first, "B".into(), ()).unwrap();
    let empty = model.add_group("Empty".into(), rgb(0x35876d).into());
    assert_eq!(
        target_in_strip(&model, 10., false),
        Some(DropTarget {
            group: first,
            slot: 0
        })
    );
    assert_eq!(
        target_in_strip(&model, TAB_WIDTH - 10., false),
        Some(DropTarget {
            group: first,
            slot: 1
        })
    );
    assert_eq!(
        target_in_strip(&model, 2. * TAB_WIDTH - 10., false),
        Some(DropTarget {
            group: first,
            slot: 2
        })
    );
    assert_eq!(
        target_in_strip(&model, 10., true),
        Some(DropTarget {
            group: first,
            slot: 2
        })
    );
    let empty_start = group_width(&model.groups[0]);
    assert_eq!(
        target_in_strip(&model, empty_start + 10., false),
        Some(DropTarget {
            group: empty,
            slot: 0
        })
    );
    assert_eq!(target_in_strip(&model, -1., false), None);
    assert_eq!(target_in_strip(&model, 1000., false), None);
}

#[test]
fn dropping_before_and_after_adjusts_for_source_removal_once() {
    let mut model: Model<()> = Model::default();
    let first = model.add_group("First".into(), rgb(0x477de6).into());
    let other = model.add_group("Other".into(), rgb(0x35876d).into());
    assert_eq!(
        final_drop_index(
            (first, 0),
            DropTarget {
                group: first,
                slot: 3
            }
        ),
        2
    );
    assert_eq!(
        final_drop_index(
            (first, 2),
            DropTarget {
                group: first,
                slot: 0
            }
        ),
        0
    );
    assert_eq!(
        final_drop_index(
            (first, 1),
            DropTarget {
                group: first,
                slot: 2
            }
        ),
        1
    );
    assert_eq!(
        final_drop_index(
            (first, 0),
            DropTarget {
                group: other,
                slot: 2
            }
        ),
        2
    );
}

#[test]
fn foreign_instances_are_rejected_even_when_tab_ids_match() {
    let owner = EntityId::from(0x100000001);
    let other = EntityId::from(0x100000002);
    let drag = TabDrag {
        owner,
        tab: TabId(1),
        title: "A".into(),
        color: rgb(0x477de6).into(),
    };
    assert!(drag.belongs_to(owner));
    assert!(!drag.belongs_to(other));
}

#[test]
fn a_tab_status_follows_the_tab_and_reports_only_changes() {
    use super::TabStatus;
    let mut model: Model<()> = Model::default();
    let first = model.add_group("First".into(), rgb(0x477de6).into());
    let other = model.add_group("Other".into(), rgb(0x35876d).into());
    let tab = model.add_tab(first, "A".into(), ()).unwrap();
    assert_eq!(model.tab(tab).unwrap().status, TabStatus::Idle);
    assert_eq!(model.set_status(tab, TabStatus::Running), Ok(true));
    // The same status again asks for no redraw.
    assert_eq!(model.set_status(tab, TabStatus::Running), Ok(false));
    model.move_tab(tab, other, 0).unwrap();
    assert_eq!(model.tab(tab).unwrap().status, TabStatus::Running);
    assert_eq!(model.set_status(tab, TabStatus::Failed), Ok(true));
    model.close(tab).unwrap();
    assert_eq!(
        model.set_status(tab, TabStatus::Idle),
        Err(super::TabsError::TabNotFound)
    );
}

#[test]
fn a_tab_is_renamed_in_place_and_reports_only_changes() {
    let mut model: Model<()> = Model::default();
    let group = model.add_group("First".into(), rgb(0x477de6).into());
    let tab = model.add_tab(group, "Query 1".into(), ()).unwrap();
    let other = model.add_tab(group, "Query 2".into(), ()).unwrap();
    assert_eq!(model.set_title(tab, "Orders".into()), Ok(true));
    assert_eq!(model.tab(tab).unwrap().title.as_ref(), "Orders");
    // The same title again asks for no redraw, and the other tab keeps its.
    assert_eq!(model.set_title(tab, "Orders".into()), Ok(false));
    assert_eq!(model.tab(other).unwrap().title.as_ref(), "Query 2");
    assert_eq!(model.location(tab), Some((0, 0)));
    model.close(tab).unwrap();
    assert_eq!(
        model.set_title(tab, "x".into()),
        Err(super::TabsError::TabNotFound)
    );
}

#[test]
fn a_group_may_hide_its_new_tab_button_and_keeps_its_tabs() {
    let mut model: Model<()> = Model::default();
    let group = model.add_group("Report".into(), rgb(0x8bddbc).into());
    assert!(model.group(group).unwrap().new_tab);
    assert_eq!(model.set_new_tab(group, false), Ok(true));
    // The same choice again asks for no redraw.
    assert_eq!(model.set_new_tab(group, false), Ok(false));
    let tab = model.add_tab(group, "Revenue".into(), ()).unwrap();
    assert!(!model.group(group).unwrap().new_tab);
    assert_eq!(model.location(tab), Some((0, 0)));
    model.close(tab).unwrap();
    assert_eq!(
        model.set_new_tab(group, true),
        Err(super::TabsError::GroupNotFound),
        "the group went away with its last tab"
    );
}
