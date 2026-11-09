//! Declared outputs (specification, section 9.3.2; ADR-0035): what a chain
//! may read from a step, by name and kind, with the exact text the database
//! or the program gave and NULL apart from the text `NULL`.
//!
//! Values are held in memory only, except for jobs that opt in to ADR-0035.
//! They never reach a log, a span or an error message.
use super::definition::{OutputSpec, RowRule, SqlOutput};
use crate::result::ResultData;
use jykre_core::{ParameterKind, is_parameter_value};

/// The most one value may take in memory.
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
/// The most one step's outputs may take in memory.
pub const MAX_STEP_BYTES: usize = 256 * 1024;

/// One output's value. `value` is `None` for NULL.
#[derive(Clone, PartialEq, Eq)]
pub struct Output {
    pub name: String,
    pub kind: ParameterKind,
    pub value: Option<String>,
}

impl std::fmt::Debug for Output {
    /// Never the value: outputs are data.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("null", &self.value.is_none())
            .finish()
    }
}

/// Why a step's outputs could not be taken (cause `Output`). Names only,
/// never values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputProblem {
    /// The result had no rows and the rule is `exactly_one`.
    NoRow,
    /// The result had several rows and the rule does not take the first.
    SeveralRows,
    /// The row the outputs read was returned but not kept: it alone was
    /// over the memory ceiling of a result.
    RowNotKept,
    /// An output names a column the result does not have.
    MissingColumn { name: String, column: u32 },
    /// A declared output is absent.
    Missing { name: String },
    /// An output that was not declared.
    Undeclared { name: String },
    /// A value is not text of its kind.
    WrongKind { name: String, kind: ParameterKind },
    /// A value over [`MAX_VALUE_BYTES`].
    ValueTooLarge { name: String },
    /// The outputs together over [`MAX_STEP_BYTES`].
    TooLarge,
}

/// The outputs of a SQL Job from the rows its result kept, by `row`.
/// The row comes from what the result kept (the row limit is at least 1), so
/// a write's completion rule is unchanged. `rows_seen` counts every row the
/// statement returned, kept or discarded.
pub fn from_result(
    outputs: &[SqlOutput],
    row: RowRule,
    data: &ResultData,
    rows_seen: u64,
) -> Result<Vec<Output>, OutputProblem> {
    if outputs.is_empty() {
        return Ok(Vec::new());
    }
    let taken = match (rows_seen.max(data.row_count() as u64), row) {
        (0, RowRule::ExactlyOne) => return Err(OutputProblem::NoRow),
        (0, RowRule::AtMostOne | RowRule::First) => None,
        (1, _) | (_, RowRule::First) => Some(0),
        (_, RowRule::ExactlyOne | RowRule::AtMostOne) => {
            return Err(OutputProblem::SeveralRows);
        }
    };
    // A row seen but not kept has no values to read: they are not empty.
    if taken.is_some_and(|row| row >= data.row_count()) {
        return Err(OutputProblem::RowNotKept);
    }
    let values = outputs
        .iter()
        .map(|output| {
            let value = match taken {
                None => None,
                Some(row) => {
                    let column = usize::try_from(output.column)
                        .ok()
                        .and_then(|column| column.checked_sub(1))
                        .filter(|column| *column < data.column_count())
                        .ok_or_else(|| OutputProblem::MissingColumn {
                            name: output.name.clone(),
                            column: output.column,
                        })?;
                    if data.is_null(row, column) {
                        None
                    } else {
                        Some(data.value(row, column).unwrap_or_default().to_owned())
                    }
                }
            };
            Ok(Output {
                name: output.name.clone(),
                kind: output.kind,
                value,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    check(values)
}

/// The outputs of a program's `complete` message against its declaration:
/// every declared output present (a JSON `null` is NULL) and nothing else.
pub fn from_message(
    declared: &[OutputSpec],
    received: Vec<(String, Option<String>)>,
) -> Result<Vec<Output>, OutputProblem> {
    if let Some((name, _)) = received
        .iter()
        .find(|(name, _)| !declared.iter().any(|spec| &spec.name == name))
    {
        return Err(OutputProblem::Undeclared { name: name.clone() });
    }
    let mut received = received;
    let values = declared
        .iter()
        .map(|spec| {
            let index = received
                .iter()
                .position(|(name, _)| name == &spec.name)
                .ok_or_else(|| OutputProblem::Missing {
                    name: spec.name.clone(),
                })?;
            let (name, value) = received.swap_remove(index);
            Ok(Output {
                name,
                kind: spec.kind,
                value,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    check(values)
}

/// Each value of its kind and within the size limits.
fn check(values: Vec<Output>) -> Result<Vec<Output>, OutputProblem> {
    let mut total = 0usize;
    for output in &values {
        let Some(value) = &output.value else {
            continue;
        };
        if value.len() > MAX_VALUE_BYTES {
            return Err(OutputProblem::ValueTooLarge {
                name: output.name.clone(),
            });
        }
        if !is_parameter_value(output.kind, value) {
            return Err(OutputProblem::WrongKind {
                name: output.name.clone(),
                kind: output.kind,
            });
        }
        total += value.len();
    }
    if total > MAX_STEP_BYTES {
        return Err(OutputProblem::TooLarge);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::ResultColumn;

    fn outputs() -> Vec<SqlOutput> {
        vec![
            SqlOutput {
                name: "id".into(),
                kind: ParameterKind::Number,
                column: 1,
            },
            // The second column of the same name, by position.
            SqlOutput {
                name: "other_id".into(),
                kind: ParameterKind::Text,
                column: 2,
            },
        ]
    }

    fn data(rows: &[[Option<&str>; 2]]) -> ResultData {
        ResultData::from_rows(
            vec![
                ResultColumn::new("id", "int"),
                ResultColumn::new("id", "text"),
            ],
            rows.iter().map(|row| row.iter().copied()),
        )
    }

    fn values(outputs: &[Output]) -> Vec<Option<&str>> {
        outputs
            .iter()
            .map(|output| output.value.as_deref())
            .collect()
    }

    #[test]
    fn each_row_rule_with_zero_one_and_several_rows() {
        let none = data(&[]);
        let one = data(&[[Some("12.50"), Some("NULL")]]);
        let two = data(&[[Some("1"), None], [Some("2"), Some("b")]]);
        let take =
            |rule, data: &ResultData| from_result(&outputs(), rule, data, data.row_count() as u64);

        assert_eq!(take(RowRule::ExactlyOne, &none), Err(OutputProblem::NoRow));
        // A row the statement returned that was over the memory ceiling, so
        // the result kept none: its values are unknown, never empty text.
        for rule in [RowRule::ExactlyOne, RowRule::AtMostOne, RowRule::First] {
            assert_eq!(
                from_result(&outputs(), rule, &none, 1),
                Err(OutputProblem::RowNotKept),
                "{rule:?}"
            );
        }
        assert_eq!(
            from_result(&outputs(), RowRule::First, &none, 5),
            Err(OutputProblem::RowNotKept)
        );
        assert_eq!(
            values(&take(RowRule::ExactlyOne, &one).unwrap()),
            vec![Some("12.50"), Some("NULL")]
        );
        assert_eq!(
            take(RowRule::ExactlyOne, &two),
            Err(OutputProblem::SeveralRows)
        );

        assert_eq!(
            values(&take(RowRule::AtMostOne, &none).unwrap()),
            vec![None, None]
        );
        assert_eq!(
            take(RowRule::AtMostOne, &two),
            Err(OutputProblem::SeveralRows)
        );

        assert_eq!(
            values(&take(RowRule::First, &none).unwrap()),
            vec![None, None]
        );
        // NULL stays apart from the text `NULL`.
        assert_eq!(
            values(&take(RowRule::First, &two).unwrap()),
            vec![Some("1"), None]
        );
    }

    #[test]
    fn rows_read_and_discarded_count_as_several() {
        let kept = data(&[[Some("1"), Some("a")]]);
        assert_eq!(
            from_result(&outputs(), RowRule::ExactlyOne, &kept, 5),
            Err(OutputProblem::SeveralRows)
        );
    }

    #[test]
    fn kinds_columns_and_sizes_are_checked() {
        let wrong = data(&[[Some("twelve"), Some("a")]]);
        assert_eq!(
            from_result(&outputs(), RowRule::First, &wrong, 1),
            Err(OutputProblem::WrongKind {
                name: "id".into(),
                kind: ParameterKind::Number
            })
        );
        let mut beyond = outputs();
        beyond[1].column = 3;
        assert_eq!(
            from_result(&beyond, RowRule::First, &data(&[[Some("1"), None]]), 1),
            Err(OutputProblem::MissingColumn {
                name: "other_id".into(),
                column: 3
            })
        );
        let large = "x".repeat(MAX_VALUE_BYTES + 1);
        assert_eq!(
            from_result(
                &outputs(),
                RowRule::First,
                &data(&[[Some("1"), Some(&large)]]),
                1
            ),
            Err(OutputProblem::ValueTooLarge {
                name: "other_id".into()
            })
        );
        let declared: Vec<OutputSpec> = (0..5)
            .map(|index| OutputSpec {
                name: format!("v{index}"),
                kind: ParameterKind::Text,
            })
            .collect();
        let almost = "y".repeat(MAX_VALUE_BYTES);
        let received = declared
            .iter()
            .map(|spec| (spec.name.clone(), Some(almost.clone())))
            .collect();
        assert_eq!(
            from_message(&declared, received),
            Err(OutputProblem::TooLarge)
        );
    }

    #[test]
    fn a_message_must_carry_every_declared_output_and_nothing_else() {
        let declared = vec![
            OutputSpec {
                name: "rows".into(),
                kind: ParameterKind::Number,
            },
            OutputSpec {
                name: "file".into(),
                kind: ParameterKind::Text,
            },
        ];
        let ok = from_message(
            &declared,
            vec![("file".into(), None), ("rows".into(), Some("3".into()))],
        )
        .unwrap();
        assert_eq!(values(&ok), vec![Some("3"), None]);
        assert_eq!(
            from_message(&declared, vec![("rows".into(), Some("3".into()))]),
            Err(OutputProblem::Missing {
                name: "file".into()
            })
        );
        assert_eq!(
            from_message(
                &declared,
                vec![
                    ("rows".into(), Some("3".into())),
                    ("file".into(), None),
                    ("x".into(), None)
                ]
            ),
            Err(OutputProblem::Undeclared { name: "x".into() })
        );
        // Debug never shows a value.
        assert!(!format!("{:?}", ok[0]).contains('3'));
    }
}
