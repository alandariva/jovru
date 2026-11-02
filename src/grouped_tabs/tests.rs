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
