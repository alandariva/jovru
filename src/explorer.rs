//! The explorer tree without a window: which nodes are visible and where
//! the arrow keys go, as in other desktop trees.
use crate::drivers::{MetadataObject, ObjectKind, ObjectPath};
use std::collections::HashSet;

/// The nodes the tree shows, in order. With a search, a node shows when its
/// path or one of its descendants matches; otherwise a node shows unless an
/// ancestor is collapsed.
pub fn visible<'a>(
    source: &'a [MetadataObject],
    collapsed: &HashSet<ObjectPath>,
    query: &str,
) -> Vec<&'a MetadataObject> {
    source
        .iter()
        .filter(|object| {
            if !query.is_empty() {
                return object
                    .path
                    .0
                    .iter()
                    .any(|part| part.name.to_lowercase().contains(query))
                    || source.iter().any(|child| {
                        child.path.is_within(&object.path)
                            && child.name.to_lowercase().contains(query)
                    });
            }
            !collapsed
                .iter()
                .any(|path| path != &object.path && object.path.is_within(path))
        })
        .collect()
}

/// A node that has children to show or hide (everything but a column).
pub fn expandable(object: &MetadataObject) -> bool {
    object.kind != ObjectKind::Column
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeKey {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
}

impl TreeKey {
    /// The key for a keystroke name, if it moves in the tree.
    pub fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "home" => Self::Home,
            "end" => Self::End,
            _ => return None,
        })
    }
}

/// What an arrow key does from the node at a position in [`visible`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeMove {
    /// Move focus to the node at this position.
    Focus(usize),
    /// Show the children of the current node.
    Expand,
    /// Hide the children of the current node.
    Collapse,
    /// Nothing to do (the first or last node, a leaf).
    Stay,
}

/// Up and Down move between visible nodes, Home and End to the ends. Right
/// expands a collapsed node, or goes to its first child; Left collapses an
/// expanded node, or goes to its parent.
pub fn step(
    nodes: &[&MetadataObject],
    collapsed: &HashSet<ObjectPath>,
    current: usize,
    key: TreeKey,
) -> TreeMove {
    let Some(node) = nodes.get(current) else {
        return TreeMove::Stay;
    };
    let open = expandable(node) && !collapsed.contains(&node.path);
    match key {
        TreeKey::Up if current > 0 => TreeMove::Focus(current - 1),
        TreeKey::Down if current + 1 < nodes.len() => TreeMove::Focus(current + 1),
        TreeKey::Home if current > 0 => TreeMove::Focus(0),
        TreeKey::End if current + 1 < nodes.len() => TreeMove::Focus(nodes.len() - 1),
        TreeKey::Right if expandable(node) && !open => TreeMove::Expand,
        TreeKey::Right => match nodes.get(current + 1) {
            Some(child) if child.path.is_within(&node.path) => TreeMove::Focus(current + 1),
            _ => TreeMove::Stay,
        },
        TreeKey::Left if open => TreeMove::Collapse,
        TreeKey::Left => {
            let parent = ObjectPath(node.path.0[..node.path.0.len().saturating_sub(1)].to_vec());
            nodes[..current]
                .iter()
                .rposition(|candidate| candidate.path == parent)
                .map_or(TreeMove::Stay, TreeMove::Focus)
        }
        _ => TreeMove::Stay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::PathSegment;

    fn object(parts: &[(ObjectKind, &str)]) -> MetadataObject {
        let parent = ObjectPath(
            parts[..parts.len() - 1]
                .iter()
                .map(|(kind, name)| PathSegment {
                    kind: *kind,
                    name: (*name).into(),
                })
                .collect(),
        );
        let (kind, name) = parts[parts.len() - 1];
        MetadataObject::new(kind, &parent, name)
    }

    fn tree() -> Vec<MetadataObject> {
        use ObjectKind::*;
        vec![
            object(&[(Schema, "sales")]),
            object(&[(Schema, "sales"), (Table, "orders")]),
            object(&[(Schema, "sales"), (Table, "orders"), (Column, "id")]),
            object(&[(Schema, "sales"), (Table, "orders"), (Column, "total")]),
            object(&[(Schema, "sales"), (View, "summary")]),
            object(&[(Schema, "catalog")]),
        ]
    }

    #[test]
    fn collapsed_nodes_hide_their_descendants_and_a_search_shows_matches() {
        let source = tree();
        let names = |nodes: Vec<&MetadataObject>| -> Vec<String> {
            nodes.into_iter().map(|node| node.name.clone()).collect()
        };
        let none = HashSet::new();
        assert_eq!(visible(&source, &none, "").len(), 6);
        let orders: HashSet<_> = [source[1].path.clone()].into();
        assert_eq!(
            names(visible(&source, &orders, "")),
            ["sales", "orders", "summary", "catalog"]
        );
        // A search ignores collapsing and keeps the ancestors of a match.
        assert_eq!(
            names(visible(&source, &orders, "total")),
            ["sales", "orders", "total"]
        );
    }

    #[test]
    fn arrows_move_expand_and_collapse_like_a_desktop_tree() {
        let source = tree();
        let collapsed: HashSet<_> = [source[1].path.clone()].into();
        let nodes = visible(&source, &collapsed, "");
        // sales, orders (collapsed), summary, catalog
        assert_eq!(
            step(&nodes, &collapsed, 0, TreeKey::Down),
            TreeMove::Focus(1)
        );
        assert_eq!(step(&nodes, &collapsed, 0, TreeKey::Up), TreeMove::Stay);
        assert_eq!(step(&nodes, &collapsed, 3, TreeKey::Down), TreeMove::Stay);
        assert_eq!(
            step(&nodes, &collapsed, 2, TreeKey::Home),
            TreeMove::Focus(0)
        );
        assert_eq!(
            step(&nodes, &collapsed, 0, TreeKey::End),
            TreeMove::Focus(3)
        );
        // Right on a collapsed table expands it; on an open schema it goes in.
        assert_eq!(
            step(&nodes, &collapsed, 1, TreeKey::Right),
            TreeMove::Expand
        );
        assert_eq!(
            step(&nodes, &collapsed, 0, TreeKey::Right),
            TreeMove::Focus(1)
        );
        // Left on an open schema collapses it; on a collapsed child it goes up.
        assert_eq!(
            step(&nodes, &collapsed, 0, TreeKey::Left),
            TreeMove::Collapse
        );
        assert_eq!(
            step(&nodes, &collapsed, 1, TreeKey::Left),
            TreeMove::Focus(0)
        );
        // A column is a leaf: Right stays, Left goes to its table.
        let open = HashSet::new();
        let all = visible(&source, &open, "");
        assert_eq!(step(&all, &open, 2, TreeKey::Right), TreeMove::Stay);
        assert_eq!(step(&all, &open, 3, TreeKey::Left), TreeMove::Focus(1));
        assert_eq!(TreeKey::from_key("down"), Some(TreeKey::Down));
        assert_eq!(TreeKey::from_key("enter"), None);
    }
}
