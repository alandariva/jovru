//! Which file column fills which table column
//! ([ADR-0040](../../docs/adr/0040-importing-files-into-a-table.md#2-the-table-and-its-columns)).
//!
//! Each table column is filled from one file column, or left out so it
//! takes its default. A required column (`NOT NULL` without a default) left
//! out is refused before anything runs, and so is a generated column given a
//! value. By default columns are matched by header name, ignoring case;
//! without a header row, by position. The user changes either.
use super::ImportValue;
use crate::drivers::MetadataObject;
use crate::tr;

/// A column of the table the rows go into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetColumn {
    pub name: String,
    /// The type as the database writes it, when known.
    pub data_type: Option<String>,
    /// `NOT NULL` without a default: it must be filled.
    pub required: bool,
    /// Computed by the database (a generated column): it cannot be given a
    /// value.
    pub generated: bool,
}

impl TargetColumn {
    /// The column as the metadata cache describes it, which fills the
    /// dialog before the import asks the driver. The cache knows neither
    /// identity nor auto-increment columns nor generated ones, so a key
    /// column is not marked required (its value usually comes from the
    /// database) and nothing is marked generated; the driver's description
    /// of the table, read when the import starts, decides.
    pub fn from_cached(column: &MetadataObject) -> Self {
        Self {
            name: column.name.clone(),
            data_type: column.data_type.clone(),
            required: column.nullable == Some(false)
                && column.default_value.is_none()
                && column.primary_key.is_none(),
            generated: false,
        }
    }
}

/// How the file's columns are matched to the table's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MatchBy {
    /// By header name, ignoring case.
    #[default]
    Name,
    /// The file's columns in order, to the table's columns that can take a
    /// value, in order.
    Position,
}

impl MatchBy {
    /// Name with a header row, position without one.
    pub fn default_for(has_header: bool) -> Self {
        if has_header {
            Self::Name
        } else {
            Self::Position
        }
    }
}

/// What stops a mapping from running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// No table column takes a value from the file.
    NothingMapped,
    /// A column that must be filled is left out.
    RequiredLeftOut(String),
    /// A generated column is given a value.
    GeneratedFilled(String),
    /// A column is filled from a file column the file no longer has.
    MissingSource(String),
    /// A column chosen from the metadata is no longer in the table.
    TargetGone(String),
}

impl Problem {
    pub fn message(&self) -> String {
        match self {
            Self::NothingMapped => {
                tr!("Choose a file column for at least one of the table's columns.").to_owned()
            }
            Self::RequiredLeftOut(column) => tr!(
                "{0} cannot be empty and has no default. Choose a file column for it.",
                column
            ),
            Self::GeneratedFilled(column) => tr!(
                "{0} is computed by the database and cannot take a value. Leave it out.",
                column
            ),
            Self::MissingSource(column) => tr!(
                "The file no longer has the column chosen for {0}. Choose it again.",
                column
            ),
            Self::TargetGone(column) => tr!(
                "The table no longer has the column {0}. Refresh its metadata and choose the columns again.",
                column
            ),
        }
    }
}

/// The table's columns, each with the file column that fills it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mapping {
    targets: Vec<TargetColumn>,
    /// One per target: the file column that fills it, or `None` to leave it
    /// out.
    sources: Vec<Option<usize>>,
}

impl Mapping {
    /// The table's `targets` matched to the file's `columns` `by` name or
    /// position.
    pub fn new(targets: Vec<TargetColumn>, columns: &[String], by: MatchBy) -> Self {
        let mut mapping = Self {
            sources: vec![None; targets.len()],
            targets,
        };
        mapping.rematch(columns, by);
        mapping
    }

    /// Matches every column again, forgetting the user's choices.
    pub fn rematch(&mut self, columns: &[String], by: MatchBy) {
        self.sources = vec![None; self.targets.len()];
        let mut used = vec![false; columns.len()];
        match by {
            MatchBy::Name => {
                // Exact names first, so `ID` and `id` in a case-sensitive
                // table each find their own; then ignoring case.
                for exact in [true, false] {
                    for (target, source) in self.targets.iter().zip(self.sources.iter_mut()) {
                        if source.is_some() || target.generated {
                            continue;
                        }
                        let found = columns.iter().enumerate().position(|(index, name)| {
                            !used.get(index).copied().unwrap_or(true)
                                && !name.is_empty()
                                && if exact {
                                    *name == target.name
                                } else {
                                    name.to_lowercase() == target.name.to_lowercase()
                                }
                        });
                        if let Some(index) = found
                            && let Some(flag) = used.get_mut(index)
                        {
                            *flag = true;
                            *source = Some(index);
                        }
                    }
                }
            }
            MatchBy::Position => {
                let mut next = 0;
                for (target, source) in self.targets.iter().zip(self.sources.iter_mut()) {
                    if target.generated {
                        continue;
                    }
                    if next < columns.len() {
                        *source = Some(next);
                        next += 1;
                    }
                }
            }
        }
    }

    pub fn targets(&self) -> &[TargetColumn] {
        &self.targets
    }

    /// The same choices against `targets`, the table as the driver
    /// describes it when the import starts: each table column keeps the
    /// file column chosen for its name. A chosen column the table no longer
    /// has is refused.
    pub fn with_targets(&self, targets: Vec<TargetColumn>) -> Result<Mapping, Problem> {
        let mut sources = vec![None; targets.len()];
        for (target, source) in self.targets.iter().zip(&self.sources) {
            let Some(source) = source else {
                continue;
            };
            let Some(slot) = targets
                .iter()
                .position(|fresh| fresh.name == target.name)
                .and_then(|index| sources.get_mut(index))
            else {
                return Err(Problem::TargetGone(target.name.clone()));
            };
            *slot = Some(*source);
        }
        Ok(Mapping { targets, sources })
    }

    /// The positions of the table columns that take a value, in table order:
    /// the columns of [`Mapping::project`]'s values.
    pub fn mapped(&self) -> Vec<usize> {
        self.sources
            .iter()
            .enumerate()
            .filter(|(_, source)| source.is_some())
            .map(|(index, _)| index)
            .collect()
    }

    /// The file column that fills the table column `target`.
    pub fn source(&self, target: usize) -> Option<usize> {
        self.sources.get(target).copied().flatten()
    }

    /// Fills the table column `target` from the file column `source`, or
    /// leaves it out with `None`.
    pub fn set(&mut self, target: usize, source: Option<usize>) {
        if let Some(slot) = self.sources.get_mut(target) {
            *slot = source;
        }
    }

    /// What stops the mapping from running against a file with `width`
    /// columns; empty when it can run.
    pub fn problems(&self, width: usize) -> Vec<Problem> {
        let mut problems = Vec::new();
        if self.sources.iter().all(Option::is_none) {
            problems.push(Problem::NothingMapped);
        }
        for (target, source) in self.targets.iter().zip(&self.sources) {
            match source {
                None if target.required => {
                    problems.push(Problem::RequiredLeftOut(target.name.clone()));
                }
                Some(_) if target.generated => {
                    problems.push(Problem::GeneratedFilled(target.name.clone()));
                }
                Some(index) if *index >= width => {
                    problems.push(Problem::MissingSource(target.name.clone()));
                }
                _ => {}
            }
        }
        problems
    }

    /// The table columns that take a value, in table order: the column list
    /// of the rows [`Mapping::project`] makes.
    pub fn columns(&self) -> Vec<&TargetColumn> {
        self.targets
            .iter()
            .zip(&self.sources)
            .filter(|(_, source)| source.is_some())
            .map(|(target, _)| target)
            .collect()
    }

    /// The values of one file row for [`Mapping::columns`], in their order.
    /// A file column that fills two table columns is copied; one past the
    /// row's end gives NULL, which [`Mapping::problems`] prevents.
    pub fn project(&self, mut values: Vec<ImportValue>) -> Vec<ImportValue> {
        let mut uses = vec![0usize; values.len()];
        for index in self.sources.iter().flatten() {
            if let Some(count) = uses.get_mut(*index) {
                *count += 1;
            }
        }
        self.sources
            .iter()
            .flatten()
            .map(|index| {
                let Some(count) = uses.get_mut(*index) else {
                    return ImportValue::Null;
                };
                *count -= 1;
                let Some(value) = values.get_mut(*index) else {
                    return ImportValue::Null;
                };
                if *count == 0 {
                    std::mem::replace(value, ImportValue::Null)
                } else {
                    value.clone()
                }
            })
            .collect()
    }
}
