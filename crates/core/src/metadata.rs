use serde::{Deserialize, Serialize};

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectKind {
    Database,
    Catalog,
    Schema,
    Table,
    View,
    /// A view whose rows are stored and refreshed on demand.
    #[serde(rename = "materialized_view")]
    MaterializedView,
    Column,
    /// A foreign key of a table, with what it references
    /// ([`MetadataObject::references`]).
    #[serde(rename = "foreign_key")]
    ForeignKey,
    /// A partition of a partitioned table, listed under the table it
    /// belongs to rather than as a relation of its own.
    Partition,
}

impl ObjectKind {
    /// Tables, views and materialized views: what a query reads from.
    pub fn is_relation(self) -> bool {
        matches!(self, Self::Table | Self::View | Self::MaterializedView)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PathSegment {
    pub kind: ObjectKind,
    pub name: String,
}

/// Structural identity: names are never split on dots, slashes or other separators.
/// This is not SQL; identifier quoting belongs to the selected driver.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectPath(pub Vec<PathSegment>);

impl ObjectPath {
    /// Whether this path is the scope itself or one of its descendants.
    pub fn is_within(&self, scope: &Self) -> bool {
        self.0.starts_with(&scope.0)
    }

    /// The name of the relation on this path (table, view or materialized
    /// view), if it reaches one.
    pub fn relation(&self) -> Option<&str> {
        self.0
            .iter()
            .find(|part| part.kind.is_relation())
            .map(|part| part.name.as_str())
    }

    /// The path of the relation a query reads for this node: the
    /// namespaces and the relation itself, without columns or keys. A
    /// partition is read as a table of its own, in its root table's place.
    /// `None` when the path does not reach a relation.
    pub fn relation_path(&self) -> Option<Self> {
        let at = self.0.iter().position(|part| part.kind.is_relation())?;
        let mut segments = self.0[..=at].to_vec();
        if let Some(partition) = self
            .0
            .iter()
            .find(|part| part.kind == ObjectKind::Partition)
        {
            segments[at] = PathSegment {
                kind: ObjectKind::Table,
                name: partition.name.clone(),
            };
        }
        Some(Self(segments))
    }

    pub fn name_of(&self, kind: ObjectKind) -> Option<&str> {
        self.0
            .iter()
            .find(|part| part.kind == kind)
            .map(|part| part.name.as_str())
    }

    /// Validate a contiguous prefix of the driver's hierarchy. Views and
    /// materialized views occupy the same level as tables; names remain
    /// opaque structural identifiers.
    pub fn validate_scope(&self, hierarchy: &[ObjectKind]) -> crate::DriverResult<()> {
        let levels: Vec<_> = hierarchy
            .iter()
            .copied()
            .filter(|kind| !matches!(kind, ObjectKind::View | ObjectKind::MaterializedView))
            .collect();
        if self.0.len() > levels.len()
            || self.0.iter().zip(levels).any(|(part, expected)| {
                part.name.is_empty()
                    || part.name.chars().any(char::is_control)
                    || (part.kind != expected
                        && !(part.kind.is_relation() && expected == ObjectKind::Table))
            })
        {
            return Err(crate::DriverError::validation("Invalid metadata scope"));
        }
        Ok(())
    }

    pub fn child(&self, kind: ObjectKind, name: impl Into<String>) -> Self {
        let mut segments = self.0.clone();
        segments.push(PathSegment {
            kind,
            name: name.into(),
        });
        Self(segments)
    }

    /// Root has no parent; a top-level object has the empty root as its parent.
    pub fn parent(&self) -> Option<Self> {
        self.0
            .len()
            .checked_sub(1)
            .map(|length| Self(self.0[..length].to_vec()))
    }
}

/// What a foreign key points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeyTarget {
    /// The referenced relation, as the driver addresses it.
    pub relation: ObjectPath,
    /// The key's columns, in key order.
    pub columns: Vec<String>,
    /// The referenced columns, in the same order.
    pub referenced: Vec<String>,
}

/// The fields added after the first cache version are optional and left out
/// when empty, so a cache written before them still reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataObject {
    pub kind: ObjectKind,
    pub path: ObjectPath,
    pub name: String,
    /// One-based column position, when available.
    pub ordinal: Option<u32>,
    /// The type as the database writes it, with length, precision and
    /// array or enum names (`character varying(80)`, `integer[]`).
    pub data_type: Option<String>,
    pub nullable: Option<bool>,
    pub comment: Option<String>,
    /// A column's default, as the database reports it (an expression or a
    /// literal, depending on the database).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
    /// One-based position of a column in its table's primary key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<u32>,
    /// What a foreign key references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<ForeignKeyTarget>,
    /// Where the object is stored when the tree shows it under another
    /// node, such as a partition kept in another schema than its root
    /// table. Queries name this path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_at: Option<ObjectPath>,
}

impl MetadataObject {
    pub fn new(kind: ObjectKind, parent: &ObjectPath, name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            kind,
            path: parent.child(kind, name.clone()),
            name,
            ordinal: None,
            data_type: None,
            nullable: None,
            comment: None,
            default_value: None,
            primary_key: None,
            references: None,
            stored_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntrospectionWarning {
    pub path: ObjectPath,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntrospectionReport {
    pub warnings: Vec<IntrospectionWarning>,
}

/// How much of a scope an introspection reads.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntrospectionLevel {
    /// Namespaces and relations only, without columns, keys or partitions:
    /// what the explorer can show before the rest arrives.
    Names,
    /// Everything a refresh reads.
    Full,
}

/// What an inspection lists beyond the refreshed tree.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectedKind {
    Index,
    PrimaryKey,
    Unique,
    Check,
    ForeignKey,
    Exclusion,
    Trigger,
    Function,
    Procedure,
    Sequence,
    Type,
    /// A column's default the refresh does not read.
    Default,
}

/// One thing an inspection found: its kind, its name and what the database
/// says about it (columns, a signature with its parameters, enum labels or a
/// definition), as the database writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectionItem {
    pub kind: InspectedKind,
    pub name: String,
    pub detail: String,
}

/// What a relation or a namespace has beyond the tree a refresh reads
/// (indexes, constraints, triggers, routines, sequences, types), read only
/// when the user asks and never cached.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inspection {
    pub items: Vec<InspectionItem>,
}
