//! What the user chose to introspect for a connection, the namespace tree
//! they choose it from, and the progress of reading it. No window needed.
//!
//! A connection introspects nothing until the user chooses. The choice is a
//! set of rules over namespaces (databases, catalogs, schemas): everything,
//! a namespace with all that is created inside it later, or one namespace of
//! the last level.
use crate::drivers::{MetadataObject, ObjectKind, ObjectPath};
use crate::tr;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetadataScope {
    /// Everything the connection can see, including namespaces created later.
    pub all: bool,
    /// Chosen namespaces. One above the last namespace level (a database
    /// with schemas, a catalog) also takes in what is created inside it later.
    pub include: Vec<ObjectPath>,
}

impl MetadataScope {
    pub fn everything() -> Self {
        Self {
            all: true,
            include: Vec::new(),
        }
    }

    /// Nothing was chosen yet, so nothing is introspected.
    pub fn is_pending(&self) -> bool {
        !self.all && self.include.is_empty()
    }

    /// Whether `path` is inside what was chosen.
    pub fn covers(&self, path: &ObjectPath) -> bool {
        self.all || self.include.iter().any(|rule| path.is_within(rule))
    }

    /// Sorted, without duplicates or rules inside other rules.
    pub fn normalized(mut self) -> Self {
        if self.all {
            self.include.clear();
            return self;
        }
        self.include.sort_by(|a, b| names(a).cmp(names(b)));
        self.include.dedup();
        let rules = self.include.clone();
        self.include.retain(|path| {
            !rules
                .iter()
                .any(|rule| rule != path && path.is_within(rule))
        });
        self
    }

    /// The rules of `self` that `previous` did not cover: what has to be
    /// read after the choice changed.
    pub fn added_since(&self, previous: &Self) -> Self {
        if self.all {
            return if previous.all {
                Self::default()
            } else {
                Self::everything()
            };
        }
        Self {
            all: false,
            include: self
                .include
                .iter()
                .filter(|path| !previous.covers(path))
                .cloned()
                .collect(),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({"all": self.all, "include": self.include})
    }

    /// Reads a stored choice; `None` when the value is not one.
    pub fn from_value(value: &Value) -> Option<Self> {
        let all = value.get("all")?.as_bool()?;
        let include = value
            .get("include")?
            .as_array()?
            .iter()
            .map(|path| serde_json::from_value::<ObjectPath>(path.clone()).ok())
            .collect::<Option<Vec<_>>>()?;
        if include.iter().any(|path| path.0.is_empty()) {
            return None;
        }
        Some(Self { all, include }.normalized())
    }
}

fn names(path: &ObjectPath) -> impl Iterator<Item = &str> + Clone {
    path.0.iter().map(|part| part.name.as_str())
}

/// The state of a namespace's child list in the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listing {
    Loading,
    Loaded,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    On,
    /// Something inside is chosen, not all of it.
    Partial,
    Off,
}

/// One line of the tree, in display order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeRow {
    pub path: ObjectPath,
    pub name: String,
    /// Zero for the top level.
    pub depth: usize,
    pub check: Check,
    /// Whether clicking the box changes the choice now: not while choosing
    /// everything, and not while a parent's list is still loading.
    pub enabled: bool,
    /// A namespace that holds further namespaces (a database with schemas,
    /// a catalog).
    pub expandable: bool,
    pub expanded: bool,
    pub listing: Option<Listing>,
    /// Chosen, but the server's list no longer has it.
    pub missing: bool,
    /// Chosen because it is the connection's default, not by the user.
    pub suggested: bool,
}

/// The namespaces the user chooses from, loaded level by level, and the
/// choice being made. Children come from discovery and from the stored
/// choice, so a saved choice shows before anything is loaded.
#[derive(Clone, Debug)]
pub struct ScopeTree {
    levels: Vec<ObjectKind>,
    /// Known child names of each namespace (the root included), sorted.
    children: HashMap<ObjectPath, BTreeSet<String>>,
    /// Names the server listed, for namespaces whose list was loaded.
    found: HashMap<ObjectPath, HashSet<String>>,
    listing: HashMap<ObjectPath, Listing>,
    expanded: HashSet<ObjectPath>,
    selection: MetadataScope,
    /// The connection's default namespaces, chosen as they show up in a
    /// list while the user has chosen nothing yet.
    suggested: Vec<ObjectPath>,
    /// The user changed the choice, so suggestions no longer apply.
    touched: bool,
}

impl ScopeTree {
    /// `levels` are the driver's namespace levels
    /// ([`DriverCapabilities::namespace_levels`](crate::drivers::DriverCapabilities::namespace_levels)).
    pub fn new(levels: Vec<ObjectKind>, selection: MetadataScope) -> Self {
        let mut tree = Self {
            levels,
            children: HashMap::new(),
            found: HashMap::new(),
            listing: HashMap::new(),
            expanded: HashSet::new(),
            selection: selection.normalized(),
            suggested: Vec::new(),
            touched: false,
        };
        for rule in tree.selection.include.clone() {
            tree.know(&rule);
            // A chosen schema inside a catalog shows with its catalog open.
            let mut parent = rule.parent();
            while let Some(path) = parent.filter(|path| !path.0.is_empty()) {
                parent = path.parent();
                tree.expanded.insert(path);
            }
        }
        tree
    }

    fn know(&mut self, path: &ObjectPath) {
        for length in 1..=path.0.len() {
            let parent = ObjectPath(path.0[..length - 1].to_vec());
            self.children
                .entry(parent)
                .or_default()
                .insert(path.0[length - 1].name.clone());
        }
    }

    /// Offers `paths` (the driver's
    /// [`default_namespaces`](crate::drivers::Driver::default_namespaces)) as
    /// the choice, when nothing was chosen yet. Each is chosen once a list
    /// shows it; the user sees it marked as the default before saving.
    pub fn suggest(&mut self, paths: Vec<ObjectPath>) {
        if self.touched || !self.selection.is_pending() {
            return;
        }
        self.suggested = paths;
        let listed: Vec<_> = self
            .listing
            .iter()
            .filter(|(_, listing)| **listing == Listing::Loaded)
            .map(|(parent, _)| parent.clone())
            .collect();
        for parent in listed {
            self.apply_suggestions(&parent);
        }
    }

    fn apply_suggestions(&mut self, parent: &ObjectPath) {
        if self.touched || self.selection.all {
            return;
        }
        let found = self.found.get(parent);
        let chosen: Vec<_> =
            self.suggested
                .iter()
                .filter(|path| {
                    path.parent().as_ref() == Some(parent)
                        && path.0.last().is_some_and(|part| {
                            found.is_some_and(|found| found.contains(&part.name))
                        })
                        && !self.selection.covers(path)
                })
                .cloned()
                .collect();
        self.selection.include.extend(chosen);
    }

    pub fn selection(&self) -> MetadataScope {
        self.selection.clone().normalized()
    }

    /// Whether the root list was loaded, so the whole tree can be shown.
    pub fn loaded(&self) -> bool {
        self.listing.get(&ObjectPath::default()) == Some(&Listing::Loaded)
    }

    pub fn listing(&self, parent: &ObjectPath) -> Option<&Listing> {
        self.listing.get(parent)
    }

    pub fn begin_listing(&mut self, parent: &ObjectPath) {
        self.listing.insert(parent.clone(), Listing::Loading);
    }

    /// Records what the server listed under `parent`. A namespace the
    /// choice names but the server no longer has stays, marked missing.
    pub fn listed(&mut self, parent: &ObjectPath, objects: Vec<MetadataObject>) {
        let names: HashSet<String> = objects.into_iter().map(|object| object.name).collect();
        let entry = self.children.entry(parent.clone()).or_default();
        entry.extend(names.iter().cloned());
        self.found.insert(parent.clone(), names);
        self.listing.insert(parent.clone(), Listing::Loaded);
        self.apply_suggestions(parent);
    }

    pub fn listing_failed(&mut self, parent: &ObjectPath, message: impl Into<String>) {
        self.listing
            .insert(parent.clone(), Listing::Failed(message.into()));
    }

    /// Lists that were loading are abandoned (the dialog closed or the
    /// connection changed), so they can be asked for again.
    pub fn abandon_listings(&mut self) {
        self.listing
            .retain(|_, listing| !matches!(listing, Listing::Loading));
    }

    fn expandable(&self, path: &ObjectPath) -> bool {
        path.0.len() < self.levels.len()
    }

    /// Opens or closes a namespace. Returns `true` when its children have
    /// to be listed (opened, and not loaded or loading yet).
    pub fn toggle_expanded(&mut self, path: &ObjectPath) -> bool {
        if !self.expandable(path) {
            return false;
        }
        if !self.expanded.remove(path) {
            self.expanded.insert(path.clone());
            return !matches!(
                self.listing.get(path),
                Some(Listing::Loaded | Listing::Loading)
            );
        }
        false
    }

    /// The namespaces below `parent` whose lists to read now that
    /// `parent` was listed: one that is open but not loaded (a saved choice
    /// opens its ancestors), the only one at its level (a PostgreSQL
    /// connection's database) and those on the way to a suggestion. They are
    /// opened.
    pub fn to_open(&mut self, parent: &ObjectPath) -> Vec<ObjectPath> {
        let children: Vec<_> = self
            .children
            .get(parent)
            .into_iter()
            .flatten()
            .filter_map(|name| child(parent, &self.levels, name.clone()))
            .filter(|path| self.expandable(path))
            .collect();
        let single = children.len() == 1;
        let open: Vec<_> = children
            .into_iter()
            .filter(|path| {
                single
                    || self.expanded.contains(path)
                    || !self.touched
                        && self
                            .suggested
                            .iter()
                            .any(|suggestion| suggestion != path && suggestion.is_within(path))
            })
            .collect();
        self.expanded.extend(open.iter().cloned());
        open.into_iter()
            .filter(|path| {
                !matches!(
                    self.listing.get(path),
                    Some(Listing::Loaded | Listing::Loading)
                )
            })
            .collect()
    }

    pub fn set_all(&mut self, all: bool) {
        self.touched = true;
        self.selection.all = all;
        if all {
            self.selection.include.clear();
        }
    }

    pub fn all(&self) -> bool {
        self.selection.all
    }

    pub fn check(&self, path: &ObjectPath) -> Check {
        if self.selection.covers(path) {
            Check::On
        } else if self
            .selection
            .include
            .iter()
            .any(|rule| rule.is_within(path))
        {
            Check::Partial
        } else {
            Check::Off
        }
    }

    /// Whether clicking `path` can change the choice now. Unchoosing a
    /// namespace that a broader rule chose needs the full list of every
    /// namespace between that rule and `path`, to choose the siblings.
    fn enabled(&self, path: &ObjectPath) -> bool {
        if self.selection.all {
            return false;
        }
        let Some(rule) = self
            .selection
            .include
            .iter()
            .find(|rule| *rule != path && path.is_within(rule))
        else {
            return true;
        };
        (rule.0.len()..path.0.len()).all(|length| {
            self.listing.get(&ObjectPath(path.0[..length].to_vec())) == Some(&Listing::Loaded)
        })
    }

    /// Chooses or unchooses a namespace. Unchoosing one that a parent rule
    /// chose replaces that rule with the parent's other children, level by
    /// level, so only `path` leaves the choice.
    pub fn toggle(&mut self, path: &ObjectPath) {
        if !self.enabled(path) {
            return;
        }
        self.touched = true;
        match self.check(path) {
            Check::On if self.selection.include.contains(path) => {
                self.selection.include.retain(|rule| rule != path);
            }
            Check::On => {
                let Some(rule) = self
                    .selection
                    .include
                    .iter()
                    .find(|rule| path.is_within(rule))
                    .cloned()
                else {
                    return;
                };
                self.selection.include.retain(|other| *other != rule);
                let mut current = rule;
                for depth in current.0.len()..path.0.len() {
                    let next = ObjectPath(path.0[..=depth].to_vec());
                    for name in self.children.get(&current).into_iter().flatten() {
                        if let Some(sibling) = child(&current, &self.levels, name.clone())
                            && sibling != next
                        {
                            self.selection.include.push(sibling);
                        }
                    }
                    current = next;
                }
            }
            Check::Partial | Check::Off => {
                self.selection.include.retain(|rule| !rule.is_within(path));
                self.selection.include.push(path.clone());
            }
        }
        self.selection = std::mem::take(&mut self.selection).normalized();
    }

    /// The lines to show, depth first and by name. With a filter, a line
    /// shows when its name or a known descendant's matches, ignoring case.
    pub fn rows(&self, filter: &str) -> Vec<ScopeRow> {
        let filter = filter.trim().to_lowercase();
        let mut rows = Vec::new();
        self.push_rows(&ObjectPath::default(), &filter, &mut rows);
        rows
    }

    fn matches(&self, path: &ObjectPath, filter: &str) -> bool {
        path.0
            .last()
            .is_some_and(|part| part.name.to_lowercase().contains(filter))
            || self.children.get(path).is_some_and(|names| {
                names.iter().any(|name| {
                    child(path, &self.levels, name.clone())
                        .is_some_and(|child| self.matches(&child, filter))
                })
            })
    }

    fn push_rows(&self, parent: &ObjectPath, filter: &str, rows: &mut Vec<ScopeRow>) {
        for name in self.children.get(parent).into_iter().flatten() {
            let Some(path) = child(parent, &self.levels, name.clone()) else {
                continue;
            };
            if !filter.is_empty() && !self.matches(&path, filter) {
                continue;
            }
            let expandable = self.expandable(&path);
            let expanded = expandable && (self.expanded.contains(&path) || !filter.is_empty());
            rows.push(ScopeRow {
                name: name.clone(),
                depth: parent.0.len(),
                check: self.check(&path),
                enabled: self.enabled(&path),
                expandable,
                expanded,
                listing: self.listing.get(&path).cloned(),
                suggested: !self.touched
                    && self.suggested.contains(&path)
                    && self.selection.include.contains(&path),
                missing: self.selection.include.contains(&path)
                    && self
                        .found
                        .get(parent)
                        .is_some_and(|found| !found.contains(name)),
                path: path.clone(),
            });
            if expanded {
                self.push_rows(&path, filter, rows);
            }
        }
    }
}

/// The path of the namespace `name` below `parent`, at the next level.
fn child(parent: &ObjectPath, levels: &[ObjectKind], name: String) -> Option<ObjectPath> {
    levels
        .get(parent.0.len())
        .map(|kind| parent.child(*kind, name))
}

/// Where reading the chosen namespaces stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshPhase {
    /// Listing the namespaces inside broader rules, before the total is known.
    Discovering,
    Reading,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshProgress {
    pub phase: RefreshPhase,
    /// Namespaces read (or failed) so far.
    pub done: usize,
    pub total: usize,
    /// The namespace being read.
    pub current: Option<ObjectPath>,
    /// Time left, once one namespace has been read.
    pub eta: Option<Duration>,
}

impl RefreshProgress {
    pub fn discovering() -> Self {
        Self {
            phase: RefreshPhase::Discovering,
            done: 0,
            total: 0,
            current: None,
            eta: None,
        }
    }

    /// Between 0 and 1; 0 while the total is unknown.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
        }
    }
}

/// Estimates the time left from how long each namespace took. Recent ones
/// weigh more, so the estimate follows a change of pace (bigger schemas,
/// a slower server) without jumping on a single outlier.
#[derive(Clone, Debug, Default)]
pub struct EtaEstimator {
    average: Option<f64>,
}

impl EtaEstimator {
    const WEIGHT: f64 = 0.3;

    pub fn record(&mut self, took: Duration) {
        let seconds = took.as_secs_f64();
        self.average = Some(match self.average {
            None => seconds,
            Some(average) => average + Self::WEIGHT * (seconds - average),
        });
    }

    pub fn remaining(&self, left: usize) -> Option<Duration> {
        self.average
            .map(|average| Duration::from_secs_f64((average * left as f64).max(0.0)))
    }
}

/// A rough, readable time left: seconds under a minute, then minutes, then
/// hours and minutes.
pub fn format_eta(left: Duration) -> String {
    let seconds = left.as_secs();
    if seconds < 60 {
        tr!("about {0} s left", seconds.max(1))
    } else if seconds < 3600 {
        tr!("about {0} min left", seconds.div_ceil(60))
    } else {
        tr!(
            "about {0} h {1} min left",
            seconds / 3600,
            (seconds % 3600) / 60
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ObjectKind::*;

    fn path(parts: &[(ObjectKind, &str)]) -> ObjectPath {
        parts
            .iter()
            .fold(ObjectPath::default(), |path, (kind, name)| {
                path.child(*kind, *name)
            })
    }

    fn listed(tree: &mut ScopeTree, parent: &ObjectPath, kind: ObjectKind, names: &[&str]) {
        tree.listed(
            parent,
            names
                .iter()
                .map(|name| MetadataObject::new(kind, parent, *name))
                .collect(),
        );
    }

    #[test]
    fn a_choice_round_trips_and_nothing_chosen_is_pending() {
        assert!(MetadataScope::default().is_pending());
        assert!(!MetadataScope::everything().is_pending());
        let scope = MetadataScope {
            all: false,
            include: vec![
                path(&[(Catalog, "main"), (Schema, "sales")]),
                path(&[(Catalog, "main")]),
                path(&[(Catalog, "a.b/c")]),
            ],
        };
        let read = MetadataScope::from_value(&scope.to_value()).unwrap();
        // The schema is inside the whole catalog, so only the catalog stays.
        assert_eq!(
            read.include,
            [path(&[(Catalog, "a.b/c")]), path(&[(Catalog, "main")])]
        );
        assert!(MetadataScope::from_value(&json!({"all": true})).is_none());
        assert!(MetadataScope::from_value(&json!({"all": false, "include": [[]]})).is_none());
    }

    #[test]
    fn only_rules_the_previous_choice_did_not_cover_are_read_again() {
        let sales = path(&[(Catalog, "main"), (Schema, "sales")]);
        let finance = path(&[(Catalog, "main"), (Schema, "finance")]);
        let previous = MetadataScope {
            all: false,
            include: vec![sales.clone()],
        };
        let next = MetadataScope {
            all: false,
            include: vec![sales, finance.clone()],
        };
        assert_eq!(next.added_since(&previous).include, [finance]);
        assert!(MetadataScope::everything().added_since(&previous).all);
        assert!(
            MetadataScope::everything()
                .added_since(&MetadataScope::everything())
                .is_pending()
        );
        assert!(previous.added_since(&next).is_pending());
    }

    #[test]
    fn the_tree_lists_level_by_level_and_shows_partial_choices() {
        let mut tree = ScopeTree::new(vec![Catalog, Schema], MetadataScope::default());
        let root = ObjectPath::default();
        tree.begin_listing(&root);
        listed(&mut tree, &root, Catalog, &["main", "analytics"]);
        let main = path(&[(Catalog, "main")]);
        let rows = tree.rows("");
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["analytics", "main"]
        );
        assert!(rows.iter().all(|row| row.expandable && !row.expanded));
        assert!(tree.toggle_expanded(&main));
        tree.begin_listing(&main);
        // Opening again while it loads does not ask twice.
        tree.toggle_expanded(&main);
        assert!(!tree.toggle_expanded(&main));
        listed(&mut tree, &main, Schema, &["sales", "finance"]);
        let sales = path(&[(Catalog, "main"), (Schema, "sales")]);
        tree.toggle(&sales);
        let rows = tree.rows("");
        let check = |name: &str| rows.iter().find(|row| row.name == name).unwrap().check;
        assert_eq!(check("main"), Check::Partial);
        assert_eq!(check("sales"), Check::On);
        assert_eq!(check("finance"), Check::Off);
        assert_eq!(check("analytics"), Check::Off);
        assert!(
            !rows
                .iter()
                .find(|row| row.name == "sales")
                .unwrap()
                .expandable
        );
        // Choosing the catalog replaces its schemas with the whole catalog.
        tree.toggle(&main);
        assert_eq!(tree.selection().include, std::slice::from_ref(&main));
    }

    #[test]
    fn unchoosing_inside_a_whole_namespace_keeps_its_listed_siblings() {
        let main = path(&[(Catalog, "main")]);
        let mut tree = ScopeTree::new(
            vec![Catalog, Schema],
            MetadataScope {
                all: false,
                include: vec![main.clone()],
            },
        );
        let sales = path(&[(Catalog, "main"), (Schema, "sales")]);
        // The catalog's list is not loaded yet: its siblings are unknown.
        tree.toggle(&sales);
        assert_eq!(tree.selection().include, std::slice::from_ref(&main));
        listed(&mut tree, &main, Schema, &["sales", "finance", "hr"]);
        assert!(tree.rows("").iter().all(|row| row.enabled));
        tree.toggle(&sales);
        assert_eq!(
            tree.selection().include,
            [
                path(&[(Catalog, "main"), (Schema, "finance")]),
                path(&[(Catalog, "main"), (Schema, "hr")])
            ]
        );
    }

    #[test]
    fn a_saved_choice_shows_before_loading_and_flags_what_the_server_lost() {
        let db = path(&[(Database, "jykre")]);
        let sales = db.child(Schema, "sales");
        let mut tree = ScopeTree::new(
            vec![Database, Schema],
            MetadataScope {
                all: false,
                include: vec![sales.clone()],
            },
        );
        let rows = tree.rows("");
        assert_eq!(
            rows.iter().map(|row| row.path.clone()).collect::<Vec<_>>(),
            [db.clone(), sales.clone()]
        );
        assert!(rows[0].expanded);
        listed(&mut tree, &db, Schema, &["public"]);
        let rows = tree.rows("");
        let sales_row = rows.iter().find(|row| row.path == sales).unwrap();
        assert!(sales_row.missing);
        assert!(
            !rows
                .iter()
                .find(|row| row.name == "public")
                .unwrap()
                .missing
        );
    }

    #[test]
    fn everything_disables_single_choices_and_a_filter_keeps_ancestors() {
        let mut tree = ScopeTree::new(vec![Catalog, Schema], MetadataScope::default());
        let root = ObjectPath::default();
        listed(&mut tree, &root, Catalog, &["main", "other"]);
        let main = path(&[(Catalog, "main")]);
        listed(&mut tree, &main, Schema, &["Vendas", "finance"]);
        // The filter ignores case beyond ASCII and opens the way to a match.
        let rows = tree.rows("VENDAS");
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["main", "Vendas"]
        );
        tree.set_all(true);
        assert!(tree.all());
        let rows = tree.rows("");
        assert!(
            rows.iter()
                .all(|row| row.check == Check::On && !row.enabled)
        );
        tree.toggle(&main);
        assert_eq!(tree.selection(), MetadataScope::everything());
        tree.set_all(false);
        assert!(tree.selection().is_pending());
    }

    #[test]
    fn a_single_database_opens_by_itself_and_an_open_one_is_listed() {
        let mut tree = ScopeTree::new(vec![Database, Schema], MetadataScope::default());
        let root = ObjectPath::default();
        listed(&mut tree, &root, Database, &["jykre"]);
        assert_eq!(tree.to_open(&root), [path(&[(Database, "jykre")])]);
        // Asked again, it is open and loading already.
        tree.begin_listing(&path(&[(Database, "jykre")]));
        assert!(tree.to_open(&root).is_empty());
        // A single level (MySQL databases) has nothing to open.
        let mut flat = ScopeTree::new(vec![Database], MetadataScope::default());
        listed(&mut flat, &root, Database, &["only"]);
        assert!(flat.to_open(&root).is_empty());
        // A saved schema opened its catalog; loading the root lists it.
        let sales = path(&[(Catalog, "main"), (Schema, "sales")]);
        let mut saved = ScopeTree::new(
            vec![Catalog, Schema],
            MetadataScope {
                all: false,
                include: vec![sales],
            },
        );
        listed(&mut saved, &root, Catalog, &["main", "other"]);
        assert_eq!(saved.to_open(&root), [path(&[(Catalog, "main")])]);
    }

    #[test]
    fn defaults_are_chosen_as_they_show_up_until_the_user_chooses() {
        let root = ObjectPath::default();
        let main = path(&[(Catalog, "main")]);
        let sales = main.child(Schema, "sales");
        let mut tree = ScopeTree::new(vec![Catalog, Schema], MetadataScope::default());
        tree.suggest(vec![
            sales.clone(),
            path(&[(Catalog, "gone"), (Schema, "x")]),
        ]);
        listed(&mut tree, &root, Catalog, &["main", "other"]);
        // The way to the suggestion opens; nothing is chosen until it shows.
        assert_eq!(tree.to_open(&root), std::slice::from_ref(&main));
        assert!(tree.selection().is_pending());
        listed(&mut tree, &main, Schema, &["sales", "finance"]);
        assert_eq!(tree.selection().include, std::slice::from_ref(&sales));
        let row = tree
            .rows("")
            .into_iter()
            .find(|row| row.path == sales)
            .unwrap();
        assert!(row.suggested && row.check == Check::On);
        // Once the user changes the choice, nothing comes back by itself.
        tree.toggle(&sales);
        assert!(tree.selection().is_pending());
        listed(&mut tree, &main, Schema, &["sales", "finance"]);
        assert!(tree.selection().is_pending());
        // A saved choice is never replaced by a suggestion.
        let finance = main.child(Schema, "finance");
        let mut saved = ScopeTree::new(
            vec![Catalog, Schema],
            MetadataScope {
                all: false,
                include: vec![finance.clone()],
            },
        );
        saved.suggest(vec![sales]);
        listed(&mut saved, &main, Schema, &["sales", "finance"]);
        assert_eq!(saved.selection().include, [finance]);
    }

    #[test]
    fn the_estimate_follows_recent_namespaces() {
        let mut eta = EtaEstimator::default();
        assert_eq!(eta.remaining(10), None);
        eta.record(Duration::from_secs(2));
        assert_eq!(eta.remaining(10), Some(Duration::from_secs(20)));
        eta.record(Duration::from_secs(12));
        // 2 + 0.3 * (12 - 2) = 5 s per namespace.
        assert_eq!(eta.remaining(4), Some(Duration::from_secs(20)));
        assert_eq!(eta.remaining(0), Some(Duration::ZERO));
        let progress = RefreshProgress {
            phase: RefreshPhase::Reading,
            done: 3,
            total: 4,
            current: None,
            eta: None,
        };
        assert_eq!(progress.fraction(), 0.75);
        assert_eq!(RefreshProgress::discovering().fraction(), 0.0);
        assert_eq!(format_eta(Duration::from_millis(300)), "about 1 s left");
        assert_eq!(format_eta(Duration::from_secs(61)), "about 2 min left");
        assert_eq!(
            format_eta(Duration::from_secs(3_900)),
            "about 1 h 5 min left"
        );
    }
}
