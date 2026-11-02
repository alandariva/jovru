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
    Column,
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

    pub fn name_of(&self, kind: ObjectKind) -> Option<&str> {
        self.0
            .iter()
            .find(|part| part.kind == kind)
            .map(|part| part.name.as_str())
    }

    /// Validate a contiguous prefix of the driver's hierarchy. Views occupy
    /// the same level as tables; names remain opaque structural identifiers.
    pub fn validate_scope(&self, hierarchy: &[ObjectKind]) -> crate::DriverResult<()> {
        let levels: Vec<_> = hierarchy
            .iter()
            .copied()
            .filter(|kind| *kind != ObjectKind::View)
            .collect();
        if self.0.len() > levels.len()
            || self.0.iter().zip(levels).any(|(part, expected)| {
                part.name.is_empty()
                    || part.name.chars().any(char::is_control)
                    || (part.kind != expected
                        && !(part.kind == ObjectKind::View && expected == ObjectKind::Table))
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataObject {
    pub kind: ObjectKind,
    pub path: ObjectPath,
    pub name: String,
    /// One-based column position, when available.
    pub ordinal: Option<u32>,
    pub data_type: Option<String>,
    pub nullable: Option<bool>,
    pub comment: Option<String>,
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
