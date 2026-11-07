//! The explorer tree without a window: the pages read from the cache, which
//! nodes are expanded, the rows that shows and where the arrow keys go, as
//! in other desktop trees.
use crate::drivers::{MetadataObject, ObjectKind, ObjectPath};
use crate::tr;
use jykre_storage::{ChildPage, PageCursor, RefreshStatus};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// One line of the explorer tree, with an element ID that stays the same
/// while the line shows, whatever moves around it.
#[derive(Clone, Debug, PartialEq)]
pub enum TreeRow {
    /// A node, indented by the length of its path.
    Node {
        object: Arc<MetadataObject>,
        id: u64,
    },
    /// "Load more" after the children read so far of a node.
    More { parent: ObjectPath, id: u64 },
}

impl TreeRow {
    pub fn id(&self) -> u64 {
        match self {
            Self::Node { id, .. } | Self::More { id, .. } => *id,
        }
    }

    pub fn object(&self) -> Option<&MetadataObject> {
        match self {
            Self::Node { object, .. } => Some(object),
            Self::More { .. } => None,
        }
    }
}

/// A stable ID from the structural path, so names holding a separator can
/// never collide (`a/b` + `c` against `a` + `b/c`).
fn row_id(path: &ObjectPath, more: bool) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    more.hash(&mut hasher);
    hasher.finish()
}

/// A connection's explorer tree: the pages of children read from the local
/// cache, which nodes are expanded and the rows that shows. The rows are
/// rebuilt when one of those changes, never while drawing, and cost what is
/// expanded, not what was read.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    children: HashMap<ObjectPath, Vec<Arc<MetadataObject>>>,
    next: HashMap<ObjectPath, PageCursor>,
    expanded: HashSet<ObjectPath>,
    loading: HashSet<ObjectPath>,
    version: i64,
    rows: Vec<TreeRow>,
    /// Bumped by every change of `rows`.
    revision: u64,
}

impl Tree {
    /// Replaces what was read with `pages`, all from cache `version`. The
    /// expanded nodes stay expanded; those not read again are reported by
    /// [`Tree::missing`].
    pub fn replace(&mut self, version: i64, pages: Vec<(ObjectPath, ChildPage)>) {
        self.children.clear();
        self.next.clear();
        self.loading.clear();
        self.version = version;
        for (parent, page) in pages {
            self.insert(parent, page);
        }
        self.rebuild();
    }

    /// Adds the next page of `parent`'s children. Returns `false`, keeping
    /// the tree as it was, when the page comes from another cache version:
    /// a refresh published in between and the tree must be read again.
    pub fn append(&mut self, version: i64, parent: ObjectPath, page: ChildPage) -> bool {
        self.loading.remove(&parent);
        if version != self.version {
            return false;
        }
        self.insert(parent, page);
        self.rebuild();
        true
    }

    fn insert(&mut self, parent: ObjectPath, page: ChildPage) {
        let children = self.children.entry(parent.clone()).or_default();
        let known: HashSet<ObjectPath> = children.iter().map(|child| child.path.clone()).collect();
        children.extend(
            page.objects
                .into_iter()
                .filter(|object| !known.contains(&object.path))
                .map(Arc::new),
        );
        match page.next {
            Some(next) => self.next.insert(parent, next),
            None => self.next.remove(&parent),
        };
    }

    /// Expands a collapsed node or collapses an expanded one. Returns
    /// whether its children still have to be read.
    pub fn toggle(&mut self, path: &ObjectPath) -> bool {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.clone());
        }
        self.rebuild();
        self.needs(path)
    }

    /// Whether `path` is expanded but its children were not read, nor are
    /// being read.
    pub fn needs(&self, path: &ObjectPath) -> bool {
        self.expanded.contains(path)
            && !self.children.contains_key(path)
            && !self.loading.contains(path)
    }

    pub fn is_expanded(&self, path: &ObjectPath) -> bool {
        self.expanded.contains(path)
    }

    /// Marks the children of `path` as being read, or no longer.
    pub fn set_loading(&mut self, path: &ObjectPath, loading: bool) {
        if loading {
            self.loading.insert(path.clone());
        } else {
            self.loading.remove(path);
        }
    }

    /// The expanded nodes, in name order so the saved list is stable.
    pub fn expanded(&self) -> Vec<ObjectPath> {
        let mut paths: Vec<_> = self.expanded.iter().cloned().collect();
        paths.sort_by_cached_key(|path| serde_json::to_string(path).unwrap_or_default());
        paths
    }

    /// Expands every ancestor of `path`, so the node shows once their
    /// children are read ([`Tree::missing`]).
    pub fn reveal(&mut self, path: &ObjectPath) {
        for length in 1..path.0.len() {
            self.expanded.insert(ObjectPath(path.0[..length].to_vec()));
        }
        self.rebuild();
    }

    /// Whether `path` is one of the rows shown.
    pub fn shows(&self, path: &ObjectPath) -> bool {
        self.rows
            .iter()
            .any(|row| row.object().is_some_and(|object| object.path == *path))
    }

    /// Expands `paths`, as a previous session left them.
    pub fn set_expanded(&mut self, paths: impl IntoIterator<Item = ObjectPath>) {
        self.expanded = paths.into_iter().collect();
        self.rebuild();
    }

    /// Where the next page of `parent`'s children starts, if it has more.
    pub fn cursor(&self, parent: &ObjectPath) -> Option<&PageCursor> {
        self.next.get(parent)
    }

    pub fn version(&self) -> i64 {
        self.version
    }

    pub fn rows(&self) -> &[TreeRow] {
        &self.rows
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether nothing is cached at the root.
    pub fn is_empty(&self) -> bool {
        self.children
            .get(&ObjectPath::default())
            .is_none_or(Vec::is_empty)
    }

    /// Forgets what was read, keeping what is expanded.
    pub fn clear(&mut self) {
        self.replace(0, Vec::new());
    }

    /// The root and every shown node whose children were read, with how
    /// many: what to read again after the cache changed.
    pub fn loaded(&self) -> Vec<(ObjectPath, u32)> {
        let mut pages = Vec::new();
        let mut pending = vec![ObjectPath::default()];
        while let Some(parent) = pending.pop() {
            let Some(children) = self.children.get(&parent) else {
                continue;
            };
            pages.push((parent, children.len() as u32));
            pending.extend(
                children
                    .iter()
                    .rev()
                    .filter(|child| self.expanded.contains(&child.path))
                    .map(|child| child.path.clone()),
            );
        }
        if pages.is_empty() {
            pages.push((ObjectPath::default(), 0));
        }
        pages
    }

    /// Nodes that are expanded, inside expanded ancestors, but whose
    /// children were not read: after a restart, or after they were read
    /// again. Shallow ones first, so one read fills them all.
    pub fn missing(&self) -> Vec<ObjectPath> {
        let mut paths: Vec<_> = self
            .expanded
            .iter()
            .filter(|path| self.needs(path))
            .filter(|path| {
                (1..path.0.len()).all(|length| {
                    self.expanded
                        .contains(&ObjectPath(path.0[..length].to_vec()))
                })
            })
            .cloned()
            .collect();
        paths.sort_by_key(|path| path.0.len());
        paths
    }

    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        self.push_children(&ObjectPath::default(), &mut rows);
        self.rows = rows;
        self.revision += 1;
    }

    fn push_children(&self, parent: &ObjectPath, rows: &mut Vec<TreeRow>) {
        let Some(children) = self.children.get(parent) else {
            return;
        };
        for child in children {
            rows.push(TreeRow::Node {
                id: row_id(&child.path, false),
                object: child.clone(),
            });
            if expandable(child) && self.expanded.contains(&child.path) {
                self.push_children(&child.path, rows);
            }
        }
        if self.next.contains_key(parent) {
            rows.push(TreeRow::More {
                id: row_id(parent, true),
                parent: parent.clone(),
            });
        }
    }
}

/// The part of a list that changed from `old` to `new`, keeping their common
/// start and end: the range of `old` to replace and how many items of `new`
/// replace it. Replacing only that part keeps the scroll position and the
/// sizes measured for the rest.
pub fn changed_range<T: PartialEq>(old: &[T], new: &[T]) -> (std::ops::Range<usize>, usize) {
    let start = old
        .iter()
        .zip(new)
        .take_while(|(old, new)| old == new)
        .count();
    let end = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    (start..old.len() - end, new.len() - start - end)
}

/// The rows of a search: each match with its ancestors, and everything
/// under a namespace or relation whose name matches. `query` is lowercase.
pub fn search_rows(results: &[MetadataObject], query: &str) -> Vec<TreeRow> {
    let mut ancestors = HashSet::new();
    for object in results {
        if object.name.to_lowercase().contains(query) {
            for length in 1..object.path.0.len() {
                ancestors.insert(ObjectPath(object.path.0[..length].to_vec()));
            }
        }
    }
    results
        .iter()
        .filter(|object| {
            ancestors.contains(&object.path)
                || object
                    .path
                    .0
                    .iter()
                    .any(|part| part.name.to_lowercase().contains(query))
        })
        .map(|object| TreeRow::Node {
            id: row_id(&object.path, false),
            object: Arc::new(object.clone()),
        })
        .collect()
}

/// When each chosen namespace was last read in full, from the refresh
/// statuses, for its age badge.
pub fn namespace_ages(statuses: &[RefreshStatus]) -> HashMap<ObjectPath, SystemTime> {
    statuses
        .iter()
        .filter(|status| {
            status.scope.0.last().is_some_and(|part| {
                matches!(
                    part.kind,
                    ObjectKind::Database | ObjectKind::Catalog | ObjectKind::Schema
                )
            })
        })
        .filter_map(|status| {
            let at = parse_utc(status.last_success_at.as_deref()?)?;
            Some((status.scope.clone(), at))
        })
        .collect()
}

/// The chosen namespaces last read longer ago than `max_age` (as of `now`),
/// by their last complete read or, failing that, their last publication:
/// what opening the connection reads again when the user asked for it.
/// Namespaces never read are left to the regular read of the choice.
pub fn due_for_revalidation(
    statuses: &[RefreshStatus],
    scope: &crate::metadata_scope::MetadataScope,
    max_age: Duration,
    now: SystemTime,
) -> Vec<ObjectPath> {
    statuses
        .iter()
        .filter(|status| {
            status.scope.0.last().is_some_and(|part| {
                matches!(
                    part.kind,
                    ObjectKind::Database | ObjectKind::Catalog | ObjectKind::Schema
                )
            }) && scope.covers(&status.scope)
        })
        .filter(|status| {
            status
                .last_success_at
                .as_deref()
                .or(status.last_published_at.as_deref())
                .and_then(parse_utc)
                .is_some_and(|at| now.duration_since(at).unwrap_or_default() > max_age)
        })
        .map(|status| status.scope.clone())
        .collect()
}

/// Reads the cache's UTC timestamps (`2026-11-03T12:30:05.123Z`).
pub fn parse_utc(text: &str) -> Option<SystemTime> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let mut time = time.splitn(3, ':');
    let hours: i64 = time.next()?.parse().ok()?;
    let minutes: i64 = time.next()?.parse().ok()?;
    let seconds: f64 = time.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since 1970-01-01 in the proleptic Gregorian calendar.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year - era * 400;
    let of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let of_era_days = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    let days = era * 146_097 + of_era_days - 719_468;
    let total = days * 86_400 + hours * 3_600 + minutes * 60;
    let at = Duration::from_secs(u64::try_from(total).ok()?)
        .checked_add(Duration::try_from_secs_f64(seconds).ok()?)?;
    SystemTime::UNIX_EPOCH.checked_add(at)
}

/// How long ago something was read, as the age badge shows it.
pub fn format_age(elapsed: Duration) -> String {
    let minutes = elapsed.as_secs() / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if minutes < 1 {
        tr!("just now").into()
    } else if hours < 1 {
        tr!("{0} min ago", minutes)
    } else if days < 1 {
        tr!("{0} h ago", hours)
    } else if days == 1 {
        tr!("1 day ago").into()
    } else {
        tr!("{0} days ago", days)
    }
}

/// The title of a tab opened for the object at `path`: its name after the
/// namespace that holds it (`sales.orders`), so objects of the same name in
/// other schemas give tabs that tell apart. A partition keeps its table.
pub fn qualified_title(path: &ObjectPath) -> String {
    let start = path.0.len().saturating_sub(2);
    path.0[start..]
        .iter()
        .map(|part| part.name.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// A node that has children to show or hide: namespaces and relations,
/// not columns, keys or partitions.
pub fn expandable(object: &MetadataObject) -> bool {
    !matches!(
        object.kind,
        ObjectKind::Column | ObjectKind::ForeignKey | ObjectKind::Partition
    )
}

/// A node that opens a preview query: a relation, or a partition, which
/// is read as a table of its own.
pub fn opens(object: &MetadataObject) -> bool {
    object.kind.is_relation() || object.kind == ObjectKind::Partition
}

/// What a refresh of this node reads: keys and partitions are read with
/// their table.
pub fn refresh_scope(path: &ObjectPath) -> ObjectPath {
    match path.0.last().map(|part| part.kind) {
        Some(ObjectKind::ForeignKey | ObjectKind::Partition) => path.parent().unwrap_or_default(),
        _ => path.clone(),
    }
}

/// The short text after a node's name: a column's type, a relation's kind
/// when it is not a table, what a foreign key references.
pub fn detail(object: &MetadataObject) -> Option<String> {
    match object.kind {
        ObjectKind::Column => object.data_type.clone(),
        ObjectKind::View => Some(tr!("view").into()),
        ObjectKind::MaterializedView => Some(tr!("materialized view").into()),
        ObjectKind::Partition => Some(tr!("partition").into()),
        ObjectKind::ForeignKey => object.references.as_ref().map(|target| {
            format!(
                "→ {}",
                target
                    .relation
                    .0
                    .last()
                    .map(|part| part.name.as_str())
                    .unwrap_or_default()
            )
        }),
        _ => None,
    }
}

/// What a kind of object is called, for hovers and tooltips.
pub fn kind_name(kind: ObjectKind) -> String {
    match kind {
        ObjectKind::Table => tr!("table"),
        ObjectKind::View => tr!("view"),
        ObjectKind::MaterializedView => tr!("materialized view"),
        ObjectKind::Column => tr!("column"),
        ObjectKind::Schema => tr!("schema"),
        ObjectKind::Database => tr!("database"),
        ObjectKind::Catalog => tr!("catalog"),
        ObjectKind::ForeignKey => tr!("foreign key"),
        ObjectKind::Partition => tr!("partition"),
        _ => "",
    }
    .into()
}

/// What an inspected item is called in the inspection panel.
pub fn inspected_kind_name(kind: crate::drivers::InspectedKind) -> String {
    use crate::drivers::InspectedKind;
    match kind {
        InspectedKind::Index => tr!("Index"),
        InspectedKind::PrimaryKey => tr!("Primary key"),
        InspectedKind::Unique => tr!("Unique"),
        InspectedKind::Check => tr!("Check"),
        InspectedKind::ForeignKey => tr!("Foreign key"),
        InspectedKind::Exclusion => tr!("Exclusion"),
        InspectedKind::Trigger => tr!("Trigger"),
        InspectedKind::Function => tr!("Function"),
        InspectedKind::Procedure => tr!("Procedure"),
        InspectedKind::Sequence => tr!("Sequence"),
        InspectedKind::Type => tr!("Type"),
        InspectedKind::Default => tr!("Default"),
        _ => "",
    }
    .into()
}

/// Whether a node can be inspected: a relation, or a namespace of the last
/// level (its routines, sequences and types).
pub fn inspectable(object: &MetadataObject, last_level: Option<ObjectKind>) -> bool {
    object.kind.is_relation() || Some(object.kind) == last_level
}

/// Everything known about a node beyond its name, one fact per line, for
/// its tooltip. Empty when there is nothing to add.
pub fn description(object: &MetadataObject) -> String {
    let mut lines = Vec::new();
    match object.kind {
        ObjectKind::Column => {
            let mut first = object.data_type.clone().unwrap_or_default();
            if let Some(nullable) = object.nullable {
                first.push_str(if nullable { " NULL" } else { " NOT NULL" });
            }
            lines.push(first.trim().to_owned());
            if let Some(position) = object.primary_key {
                lines.push(tr!("Primary key, position {0}", position));
            }
            if let Some(default) = &object.default_value {
                lines.push(tr!("Default: {0}", default));
            }
        }
        ObjectKind::ForeignKey => {
            if let Some(target) = &object.references {
                let relation = target
                    .relation
                    .0
                    .iter()
                    .filter(|part| part.kind != ObjectKind::Database)
                    .map(|part| part.name.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                lines.push(tr!(
                    "({0}) references {1} ({2})",
                    target.columns.join(", "),
                    relation,
                    target.referenced.join(", ")
                ));
            }
        }
        _ => {}
    }
    if let Some(comment) = &object.comment {
        lines.push(comment.clone());
    }
    lines.retain(|line| !line.is_empty());
    lines.join("\n")
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

/// What an arrow key does from the node at a position in the shown nodes.
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
/// `open` tells whether a node shows its children.
pub fn step(
    nodes: &[&MetadataObject],
    open: impl Fn(&MetadataObject) -> bool,
    current: usize,
    key: TreeKey,
) -> TreeMove {
    let Some(node) = nodes.get(current) else {
        return TreeMove::Stay;
    };
    let open = expandable(node) && open(node);
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

/// How the explorer's quick search shows what it found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuickMode {
    /// Every node stays; the matches are marked.
    #[default]
    Highlight,
    /// Only the matches show, with their ancestors (and what is expanded
    /// under them), so the hierarchy stays.
    Filter,
}

impl QuickMode {
    pub fn toggled(self) -> Self {
        match self {
            Self::Highlight => Self::Filter,
            Self::Filter => Self::Highlight,
        }
    }
}

/// Whether `name` holds `text`, ignoring case as every search does. Empty
/// text matches nothing.
pub fn quick_matches(name: &str, text: &str) -> bool {
    !crate::palette::highlights(name, &[text]).is_empty()
}

/// The node a quick search selects after its text changed, among `names` in
/// the order shown: the current one while it still matches, otherwise the
/// next match after it, wrapping. `None` when nothing matches.
pub fn quick_target(names: &[&str], current: Option<usize>, text: &str) -> Option<usize> {
    if let Some(current) = current
        && names
            .get(current)
            .is_some_and(|name| quick_matches(name, text))
    {
        return Some(current);
    }
    let start = current.map_or(0, |current| current + 1);
    (0..names.len())
        .map(|offset| (start + offset) % names.len())
        .find(|&at| quick_matches(names[at], text))
}

/// `↑` or `↓` while the quick search is open: the previous or next match,
/// wrapping at both ends. With no text typed, the previous or next node, as
/// without a search.
pub fn quick_step(
    names: &[&str],
    current: Option<usize>,
    text: &str,
    forward: bool,
) -> Option<usize> {
    let len = names.len();
    if len == 0 {
        return None;
    }
    if text.is_empty() {
        return Some(match (current, forward) {
            (None, _) => 0,
            (Some(at), true) => (at + 1).min(len - 1),
            (Some(at), false) => at.saturating_sub(1),
        });
    }
    let current = current.filter(|&at| at < len);
    (1..=len)
        .map(|offset| match (current, forward) {
            (Some(at), true) => (at + offset) % len,
            (Some(at), false) => (at + len - offset % len) % len,
            (None, true) => offset - 1,
            (None, false) => len - offset,
        })
        .find(|&at| quick_matches(names[at], text))
}

/// The rows a filtering quick search leaves of `rows` (one connection's, in
/// order): each node whose name holds `text`, its ancestors and what shows
/// under it because it is expanded. "Load more" rows go. Empty text keeps
/// everything.
pub fn filter_rows(rows: &[TreeRow], text: &str) -> Vec<TreeRow> {
    if text.is_empty() {
        return rows.to_vec();
    }
    let matched: Vec<&ObjectPath> = rows
        .iter()
        .filter_map(TreeRow::object)
        .filter(|object| quick_matches(&object.name, text))
        .map(|object| &object.path)
        .collect();
    if matched.is_empty() {
        return Vec::new();
    }
    let mut ancestors = HashSet::new();
    for path in &matched {
        for length in 1..path.0.len() {
            ancestors.insert(ObjectPath(path.0[..length].to_vec()));
        }
    }
    rows.iter()
        .filter(|row| {
            row.object().is_some_and(|object| {
                ancestors.contains(&object.path)
                    || matched.iter().any(|path| object.path.is_within(path))
            })
        })
        .cloned()
        .collect()
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

    fn page(objects: &[MetadataObject]) -> ChildPage {
        ChildPage {
            objects: objects.to_vec(),
            next: None,
        }
    }

    /// The tree of [`tree`], read as the cache pages it.
    fn read(source: &[MetadataObject]) -> Tree {
        let mut tree = Tree::default();
        tree.replace(
            1,
            vec![
                (
                    ObjectPath::default(),
                    page(&[source[0].clone(), source[5].clone()]),
                ),
                (
                    source[0].path.clone(),
                    page(&[source[1].clone(), source[4].clone()]),
                ),
                (
                    source[1].path.clone(),
                    page(&[source[2].clone(), source[3].clone()]),
                ),
            ],
        );
        tree
    }

    fn names(rows: &[TreeRow]) -> Vec<String> {
        rows.iter()
            .filter_map(|row| row.object().map(|object| object.name.clone()))
            .collect()
    }

    #[test]
    fn a_tab_title_names_the_schema_of_its_object() {
        use ObjectKind::*;
        let orders = object(&[(Database, "shop"), (Schema, "sales"), (Table, "orders")]);
        assert_eq!(qualified_title(&orders.path), "sales.orders");
        let other = object(&[(Database, "shop"), (Schema, "archive"), (Table, "orders")]);
        assert_ne!(qualified_title(&other.path), qualified_title(&orders.path));
        let mysql = object(&[(Database, "shop"), (Table, "orders")]);
        assert_eq!(qualified_title(&mysql.path), "shop.orders");
        assert_eq!(qualified_title(&ObjectPath::default()), "");
    }

    #[test]
    fn rows_show_expanded_nodes_only_and_are_rebuilt_when_the_tree_changes() {
        let source = tree();
        let mut tree = read(&source);
        assert_eq!(names(tree.rows()), ["sales", "catalog"]);
        let revision = tree.revision();
        // Children already read show at once; nothing more to read.
        assert!(!tree.toggle(&source[0].path));
        assert!(tree.revision() > revision);
        assert!(!tree.toggle(&source[1].path));
        assert_eq!(
            names(tree.rows()),
            ["sales", "orders", "id", "total", "summary", "catalog"]
        );
        assert_eq!(
            tree.loaded(),
            [
                (ObjectPath::default(), 2),
                (source[0].path.clone(), 2),
                (source[1].path.clone(), 2)
            ]
        );
        // A collapsed ancestor hides an expanded descendant, which stays
        // expanded for later.
        tree.toggle(&source[0].path);
        assert_eq!(names(tree.rows()), ["sales", "catalog"]);
        assert!(tree.is_expanded(&source[1].path));
        // Rows have IDs that do not depend on joining names.
        let ids: HashSet<_> = read(&source).rows().iter().map(TreeRow::id).collect();
        assert_eq!(ids.len(), 2);
        let a = ObjectPath::default()
            .child(ObjectKind::Schema, "a/b")
            .child(ObjectKind::Table, "c");
        let b = ObjectPath::default()
            .child(ObjectKind::Schema, "a")
            .child(ObjectKind::Table, "b/c");
        assert_ne!(row_id(&a, false), row_id(&b, false));
    }

    #[test]
    fn nodes_expanded_in_a_previous_session_are_read_shallow_first() {
        let source = tree();
        let mut tree = Tree::default();
        tree.set_expanded([source[1].path.clone(), source[0].path.clone()]);
        // Only the root is read at start.
        tree.replace(
            1,
            vec![(
                ObjectPath::default(),
                page(&[source[0].clone(), source[5].clone()]),
            )],
        );
        assert_eq!(
            tree.missing(),
            [source[0].path.clone(), source[1].path.clone()]
        );
        tree.set_loading(&source[0].path, true);
        assert_eq!(tree.missing(), [source[1].path.clone()]);
        assert!(tree.append(1, source[0].path.clone(), page(&[source[1].clone()])));
        assert!(tree.append(1, source[1].path.clone(), page(&[source[2].clone()])));
        assert!(tree.missing().is_empty());
        assert_eq!(names(tree.rows()), ["sales", "orders", "id", "catalog"]);
        // Revealing a column expands its schema and table.
        let mut revealed = Tree::default();
        revealed.replace(
            1,
            vec![(
                ObjectPath::default(),
                page(&[source[0].clone(), source[5].clone()]),
            )],
        );
        revealed.reveal(&source[3].path);
        assert_eq!(
            revealed.missing(),
            [source[0].path.clone(), source[1].path.clone()]
        );
        assert!(!revealed.shows(&source[3].path));
        assert!(revealed.append(1, source[0].path.clone(), page(&[source[1].clone()])));
        assert!(revealed.append(1, source[1].path.clone(), page(&[source[3].clone()])));
        assert!(revealed.shows(&source[3].path));
        // A node under a collapsed one is not read until it shows.
        let mut tree = Tree::default();
        tree.set_expanded([source[1].path.clone()]);
        assert!(tree.missing().is_empty());
        assert_eq!(tree.expanded(), [source[1].path.clone()]);
    }

    #[test]
    fn a_page_from_another_cache_version_is_refused_and_duplicates_dropped() {
        let source = tree();
        let mut tree = read(&source);
        tree.toggle(&source[0].path);
        let before = tree.rows().to_vec();
        assert!(!tree.append(2, source[0].path.clone(), page(&[source[4].clone()])));
        assert_eq!(tree.rows(), before);
        assert!(tree.append(1, source[0].path.clone(), page(&[source[4].clone()])));
        assert_eq!(
            names(tree.rows()),
            ["sales", "orders", "summary", "catalog"]
        );
        tree.clear();
        assert!(tree.is_empty() && tree.rows().is_empty());
        assert!(tree.is_expanded(&source[0].path));
    }

    #[test]
    fn only_the_changed_part_of_a_list_is_replaced() {
        assert_eq!(changed_range(&[1, 2, 3, 4], &[1, 2, 9, 9, 3, 4]), (2..2, 2));
        assert_eq!(changed_range(&[1, 2, 3, 4], &[1, 4]), (1..3, 0));
        assert_eq!(changed_range(&[1, 2, 3], &[1, 2, 3]), (3..3, 0));
        assert_eq!(changed_range::<u8>(&[], &[1, 2]), (0..0, 2));
        assert_eq!(changed_range(&[1, 1], &[1]), (1..2, 0));
    }

    #[test]
    fn a_search_shows_matches_with_their_ancestors() {
        let source = tree();
        assert_eq!(
            names(&search_rows(&source, "total")),
            ["sales", "orders", "total"]
        );
        // Everything under a matching node shows.
        assert_eq!(
            names(&search_rows(&source, "orders")),
            ["sales", "orders", "id", "total"]
        );
    }

    #[tokio::test]
    async fn a_node_with_more_children_ends_with_load_more() {
        let dir = std::env::temp_dir().join(format!(
            "jykre-explorer-{}",
            crate::drivers::ConnectionId::new()
        ));
        let config = crate::drivers::ConnectionConfig {
            driver: crate::drivers::DriverKind::MySql,
            options: serde_json::json!({"host":"localhost","user":"fixture","sslMode":"disabled"}),
        };
        let repository = jykre_storage::MetadataRepository::open(
            &dir,
            crate::drivers::ConnectionId::new(),
            &config,
        )
        .await
        .unwrap();
        let schema = object(&[(ObjectKind::Schema, "sales")]);
        let mut objects = vec![schema.clone()];
        for index in 0..3 {
            objects.push(MetadataObject::new(
                ObjectKind::Table,
                &schema.path,
                format!("t{index}"),
            ));
        }
        let mut run = repository.begin(&ObjectPath::default()).await.unwrap();
        run.batch(objects).await.unwrap();
        run.activate(
            &crate::drivers::IntrospectionReport::default(),
            &crate::drivers::CancellationToken::new(),
        )
        .await
        .unwrap();
        drop(run);
        let root = repository
            .children(&ObjectPath::default(), None, 10)
            .await
            .unwrap();
        let first = repository.children(&schema.path, None, 2).await.unwrap();
        let mut tree = Tree::default();
        tree.replace(1, vec![(ObjectPath::default(), root)]);
        tree.toggle(&schema.path);
        assert!(tree.append(1, schema.path.clone(), first));
        assert_eq!(names(tree.rows()), ["sales", "t0", "t1"]);
        assert!(matches!(
            tree.rows().last(),
            Some(TreeRow::More { parent, .. }) if *parent == schema.path
        ));
        let rest = repository
            .children(&schema.path, tree.cursor(&schema.path), 2)
            .await
            .unwrap();
        assert!(tree.append(1, schema.path.clone(), rest));
        assert_eq!(names(tree.rows()), ["sales", "t0", "t1", "t2"]);
        assert!(tree.cursor(&schema.path).is_none());
        assert_eq!(tree.loaded()[1], (schema.path.clone(), 3));
        repository.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ages_come_from_namespace_statuses_and_read_as_relative_time() {
        let at = parse_utc("2026-11-03T12:30:05.250Z").unwrap();
        // `date -u -d 2026-11-03T12:30:05Z +%s`
        let expected = Duration::from_secs(1_793_709_005) + Duration::from_millis(250);
        assert_eq!(at.duration_since(SystemTime::UNIX_EPOCH).unwrap(), expected);
        assert_eq!(
            parse_utc("1970-01-01T00:00:00.000Z"),
            Some(SystemTime::UNIX_EPOCH)
        );
        assert!(parse_utc("2026-13-01T00:00:00Z").is_none());
        assert!(parse_utc("yesterday").is_none());
        let schema = object(&[(ObjectKind::Database, "db"), (ObjectKind::Schema, "sales")]);
        let table = object(&[(ObjectKind::Schema, "sales"), (ObjectKind::Table, "orders")]);
        let statuses = [
            RefreshStatus {
                scope: schema.path.clone(),
                last_success_at: Some("2026-11-03T12:30:05.250Z".into()),
                ..RefreshStatus::default()
            },
            RefreshStatus {
                scope: table.path.clone(),
                last_success_at: Some("2026-11-03T12:30:05.250Z".into()),
                ..RefreshStatus::default()
            },
        ];
        let ages = namespace_ages(&statuses);
        assert_eq!(ages.len(), 1);
        assert_eq!(ages.get(&schema.path), Some(&at));
        // Revalidation picks chosen namespaces read too long ago.
        let other = object(&[(ObjectKind::Database, "db"), (ObjectKind::Schema, "hr")]);
        let mut statuses = statuses.to_vec();
        statuses.push(RefreshStatus {
            scope: other.path.clone(),
            last_published_at: Some("2026-11-01T12:30:05.250Z".into()),
            ..RefreshStatus::default()
        });
        let chosen = crate::metadata_scope::MetadataScope {
            all: false,
            include: vec![schema.path.clone(), other.path.clone()],
        };
        let now = at + Duration::from_secs(3_600);
        assert_eq!(
            due_for_revalidation(&statuses, &chosen, Duration::from_secs(24 * 3_600), now),
            std::slice::from_ref(&other.path)
        );
        assert_eq!(
            due_for_revalidation(&statuses, &chosen, Duration::from_secs(60), now),
            [schema.path.clone(), other.path.clone()]
        );
        let narrower = crate::metadata_scope::MetadataScope {
            all: false,
            include: vec![schema.path.clone()],
        };
        assert_eq!(
            due_for_revalidation(&statuses, &narrower, Duration::from_secs(60), now),
            std::slice::from_ref(&schema.path)
        );
        assert_eq!(format_age(Duration::from_secs(20)), "just now");
        assert_eq!(format_age(Duration::from_secs(5 * 60)), "5 min ago");
        assert_eq!(format_age(Duration::from_secs(3 * 3_600)), "3 h ago");
        assert_eq!(format_age(Duration::from_secs(30 * 3_600)), "1 day ago");
        assert_eq!(format_age(Duration::from_secs(3 * 86_400)), "3 days ago");
    }

    #[test]
    fn keys_and_partitions_are_leaves_that_describe_themselves() {
        use crate::drivers::ForeignKeyTarget;
        let table = ObjectPath::default()
            .child(ObjectKind::Database, "db")
            .child(ObjectKind::Schema, "sales")
            .child(ObjectKind::Table, "orders");
        let mut column = MetadataObject::new(ObjectKind::Column, &table, "id");
        column.data_type = Some("integer".into());
        column.nullable = Some(false);
        column.primary_key = Some(1);
        column.default_value = Some("nextval('orders_id_seq'::regclass)".into());
        column.comment = Some("Order number".into());
        assert_eq!(detail(&column).as_deref(), Some("integer"));
        assert_eq!(
            description(&column),
            "integer NOT NULL\nPrimary key, position 1\nDefault: nextval('orders_id_seq'::regclass)\nOrder number"
        );
        let mut key = MetadataObject::new(ObjectKind::ForeignKey, &table, "orders_customer");
        let customers = ObjectPath::default()
            .child(ObjectKind::Database, "db")
            .child(ObjectKind::Schema, "crm")
            .child(ObjectKind::Table, "customers");
        key.references = Some(ForeignKeyTarget {
            relation: customers,
            columns: vec!["customer_id".into()],
            referenced: vec!["id".into()],
        });
        assert_eq!(detail(&key).as_deref(), Some("→ customers"));
        assert_eq!(
            description(&key),
            "(customer_id) references crm.customers (id)"
        );
        let partition = MetadataObject::new(ObjectKind::Partition, &table, "orders_2026");
        for leaf in [&column, &key, &partition] {
            assert!(!expandable(leaf));
            assert_eq!(
                refresh_scope(&leaf.path),
                if leaf.kind == ObjectKind::Column {
                    leaf.path.clone()
                } else {
                    table.clone()
                }
            );
        }
        assert!(opens(&partition) && !opens(&key) && !opens(&column));
        let summary = MetadataObject::new(
            ObjectKind::MaterializedView,
            &table.parent().unwrap(),
            "summary",
        );
        assert!(expandable(&summary) && opens(&summary));
        assert_eq!(detail(&summary).as_deref(), Some("materialized view"));
        assert!(description(&summary).is_empty());
    }

    #[test]
    fn arrows_move_expand_and_collapse_like_a_desktop_tree() {
        let source = tree();
        let mut tree = read(&source);
        tree.toggle(&source[0].path);
        let nodes: Vec<_> = tree.rows().iter().filter_map(TreeRow::object).collect();
        let open = |node: &MetadataObject| tree.is_expanded(&node.path);
        // sales (open), orders (collapsed), summary, catalog
        assert_eq!(step(&nodes, open, 0, TreeKey::Down), TreeMove::Focus(1));
        assert_eq!(step(&nodes, open, 0, TreeKey::Up), TreeMove::Stay);
        assert_eq!(step(&nodes, open, 3, TreeKey::Down), TreeMove::Stay);
        assert_eq!(step(&nodes, open, 2, TreeKey::Home), TreeMove::Focus(0));
        assert_eq!(step(&nodes, open, 0, TreeKey::End), TreeMove::Focus(3));
        // Right on a collapsed table expands it; on an open schema it goes in.
        assert_eq!(step(&nodes, open, 1, TreeKey::Right), TreeMove::Expand);
        assert_eq!(step(&nodes, open, 0, TreeKey::Right), TreeMove::Focus(1));
        // Left on an open schema collapses it; on a collapsed child it goes up.
        assert_eq!(step(&nodes, open, 0, TreeKey::Left), TreeMove::Collapse);
        assert_eq!(step(&nodes, open, 1, TreeKey::Left), TreeMove::Focus(0));
        // A column is a leaf: Right stays, Left goes to its table.
        let mut all = read(&source);
        all.toggle(&source[0].path);
        all.toggle(&source[1].path);
        let all_nodes: Vec<_> = all.rows().iter().filter_map(TreeRow::object).collect();
        let all_open = |node: &MetadataObject| all.is_expanded(&node.path);
        assert_eq!(
            step(&all_nodes, all_open, 2, TreeKey::Right),
            TreeMove::Stay
        );
        assert_eq!(
            step(&all_nodes, all_open, 3, TreeKey::Left),
            TreeMove::Focus(1)
        );
        assert_eq!(TreeKey::from_key("down"), Some(TreeKey::Down));
        assert_eq!(TreeKey::from_key("enter"), None);
    }

    #[test]
    fn a_quick_search_keeps_a_matching_selection_or_jumps_to_the_next_match() {
        let names = ["app", "Tables", "customers", "customer_orders", "products"];
        // Ignoring case; empty text matches nothing.
        assert!(quick_matches("Customers", "CUST"));
        assert!(!quick_matches("customers", ""));
        assert_eq!(quick_target(&names, None, "cust"), Some(2));
        // From the selection, forward, wrapping.
        assert_eq!(quick_target(&names, Some(4), "cust"), Some(2));
        assert_eq!(quick_target(&names, Some(2), "cust"), Some(2));
        assert_eq!(quick_target(&names, Some(2), "customer_"), Some(3));
        assert_eq!(quick_target(&names, Some(1), "zzz"), None);
        assert_eq!(quick_target(&[], None, "a"), None);
    }

    #[test]
    fn arrows_in_a_quick_search_cycle_through_the_matches() {
        let names = [
            "app",
            "customers",
            "products",
            "customer_orders",
            "invoices",
        ];
        assert_eq!(quick_step(&names, Some(1), "cust", true), Some(3));
        // The last match wraps to the first, and back.
        assert_eq!(quick_step(&names, Some(3), "cust", true), Some(1));
        assert_eq!(quick_step(&names, Some(1), "cust", false), Some(3));
        assert_eq!(quick_step(&names, Some(3), "cust", false), Some(1));
        // From a node that does not match, the nearest match that way.
        assert_eq!(quick_step(&names, Some(2), "cust", true), Some(3));
        assert_eq!(quick_step(&names, Some(2), "cust", false), Some(1));
        assert_eq!(quick_step(&names, None, "cust", false), Some(3));
        // One match stays; none moves nowhere.
        assert_eq!(quick_step(&names, Some(4), "invo", true), Some(4));
        assert_eq!(quick_step(&names, Some(0), "zzz", true), None);
        // No text: every node, without wrapping.
        assert_eq!(quick_step(&names, Some(0), "", true), Some(1));
        assert_eq!(quick_step(&names, Some(4), "", true), Some(4));
        assert_eq!(quick_step(&names, Some(0), "", false), Some(0));
        assert_eq!(quick_step(&[], None, "", true), None);
        assert_eq!(QuickMode::Highlight.toggled(), QuickMode::Filter);
    }

    #[test]
    fn filtering_keeps_matches_with_their_ancestors_and_what_is_open_under_them() {
        let source = tree();
        let mut tree = read(&source);
        tree.toggle(&source[0].path);
        tree.toggle(&source[1].path);
        let rows = tree.rows();
        assert_eq!(
            names(&filter_rows(rows, "TOT")),
            ["sales", "orders", "total"]
        );
        // An expanded match keeps what shows under it.
        assert_eq!(
            names(&filter_rows(rows, "orders")),
            ["sales", "orders", "id", "total"]
        );
        assert_eq!(
            names(&filter_rows(rows, "a")),
            ["sales", "orders", "id", "total", "summary", "catalog"]
        );
        assert!(filter_rows(rows, "zzz").is_empty());
        assert_eq!(filter_rows(rows, ""), rows);
        // "Load more" never stays in a filter.
        let more = vec![
            rows[0].clone(),
            TreeRow::More {
                parent: source[0].path.clone(),
                id: 7,
            },
        ];
        assert_eq!(filter_rows(&more, "sal"), &more[..1]);
    }
}
