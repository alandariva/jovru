//! A report's `:name` parameters: which ones its SQL holds, and the bound
//! values for a run (ADR-0023). Values never go into the SQL text; each
//! request carries them with the place of their markers, and the driver
//! binds them.
use super::ParameterSpec;
use crate::sql::{self, SqlDialect};
use jykre_core::{ExecutionOptions, ParameterKind, QueryParameter, QueryRequest};
use std::collections::HashMap;

/// The parameter names of `sql`, once each, in the order they first appear.
pub fn names(sql: &str, dialect: Option<SqlDialect>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for marker in sql::named_markers(sql, dialect) {
        if !names.contains(&marker.name) {
            names.push(marker.name);
        }
    }
    names
}

/// The specifications for `names`, in their order: a name keeps its
/// specification, a new name gets a required text one, and a name no longer
/// in the SQL loses its own.
pub fn reconcile(specs: &[ParameterSpec], names: &[String]) -> Vec<ParameterSpec> {
    names
        .iter()
        .map(|name| {
            specs
                .iter()
                .find(|spec| &spec.name == name)
                .cloned()
                .unwrap_or_else(|| ParameterSpec::new(name.clone()))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterProblem {
    /// A required parameter has no value.
    Missing,
    /// The value does not have the shape of the parameter's kind.
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterError {
    pub name: String,
    pub problem: ParameterProblem,
}

/// The value a run binds for `spec` from what the form holds: trimmed
/// (except text), lowercased for booleans, `None` (NULL) when empty and
/// optional.
pub fn value(spec: &ParameterSpec, typed: &str) -> Result<Option<String>, ParameterProblem> {
    let typed = match spec.kind {
        ParameterKind::Text => typed.to_owned(),
        ParameterKind::Boolean => typed.trim().to_ascii_lowercase(),
        _ => typed.trim().to_owned(),
    };
    if typed.is_empty() {
        return if spec.required {
            Err(ParameterProblem::Missing)
        } else {
            Ok(None)
        };
    }
    if jykre_core::is_parameter_value(spec.kind, &typed) {
        Ok(Some(typed))
    } else {
        Err(ParameterProblem::Invalid)
    }
}

/// One request per statement of `sql`, each carrying the parameters its
/// statement uses, with their markers in that statement's text. `values`
/// holds what the form shows for each name; a name without one is empty.
/// Every invalid or missing value is reported, so each field can show its
/// own problem.
pub fn requests(
    sql: &str,
    dialect: Option<SqlDialect>,
    specs: &[ParameterSpec],
    values: &HashMap<String, String>,
    options: &ExecutionOptions,
) -> Result<Vec<QueryRequest>, Vec<ParameterError>> {
    let mut bound = HashMap::new();
    let mut errors = Vec::new();
    for spec in specs {
        let typed = values.get(&spec.name).map_or("", String::as_str);
        match value(spec, typed) {
            Ok(value) => {
                bound.insert(spec.name.as_str(), (spec.kind, value));
            }
            Err(problem) => errors.push(ParameterError {
                name: spec.name.clone(),
                problem,
            }),
        }
    }
    let mut requests = Vec::new();
    for range in sql::statements(sql, dialect) {
        let Some(text) = sql.get(range) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let mut parameters: Vec<QueryParameter> = Vec::new();
        for marker in sql::named_markers(text, dialect) {
            if let Some(parameter) = parameters.iter_mut().find(|p| p.name == marker.name) {
                parameter.markers.push(marker.range);
                continue;
            }
            match bound.get(marker.name.as_str()) {
                Some((kind, value)) => parameters.push(QueryParameter {
                    name: marker.name,
                    kind: *kind,
                    value: value.clone(),
                    markers: vec![marker.range],
                }),
                // A marker without a specification (the SQL changed and the
                // form was not updated) cannot run.
                None if !errors.iter().any(|error| error.name == marker.name) => {
                    errors.push(ParameterError {
                        name: marker.name,
                        problem: ParameterProblem::Missing,
                    });
                }
                None => {}
            }
        }
        requests.push(QueryRequest {
            parameters,
            ..QueryRequest::new(text, options.clone())
        });
    }
    if errors.is_empty() {
        Ok(requests)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POSTGRES: Option<SqlDialect> = Some(SqlDialect::Postgres);

    fn spec(name: &str, kind: ParameterKind, required: bool) -> ParameterSpec {
        ParameterSpec {
            kind,
            required,
            ..ParameterSpec::new(name)
        }
    }

    fn typed(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn names_come_once_in_order_of_first_appearance() {
        assert_eq!(names("SELECT :b, :a, ':c', :b -- :d", POSTGRES), ["b", "a"]);
    }

    #[test]
    fn reconcile_keeps_known_specs_adds_new_ones_and_drops_gone_ones() {
        let kept = spec("a", ParameterKind::Number, false);
        let reconciled = reconcile(
            &[kept.clone(), spec("gone", ParameterKind::Date, true)],
            &["new".to_owned(), "a".to_owned()],
        );
        assert_eq!(reconciled, vec![ParameterSpec::new("new"), kept]);
    }

    #[test]
    fn values_are_normalised_by_kind_and_empty_ones_are_null_or_missing() {
        let text = spec("t", ParameterKind::Text, true);
        assert_eq!(value(&text, "  padded "), Ok(Some("  padded ".into())));
        assert_eq!(value(&text, ""), Err(ParameterProblem::Missing));
        let optional = spec("n", ParameterKind::Number, false);
        assert_eq!(value(&optional, "  "), Ok(None));
        assert_eq!(value(&optional, " 1.5 "), Ok(Some("1.5".into())));
        assert_eq!(value(&optional, "1,5"), Err(ParameterProblem::Invalid));
        let flag = spec("b", ParameterKind::Boolean, true);
        assert_eq!(value(&flag, "TRUE"), Ok(Some("true".into())));
        assert_eq!(value(&flag, "sim"), Err(ParameterProblem::Invalid));
        let day = spec("d", ParameterKind::Date, true);
        assert_eq!(value(&day, "2024-02-30"), Ok(Some("2024-02-30".into())));
        assert_eq!(value(&day, "30/02/2024"), Err(ParameterProblem::Invalid));
    }

    #[test]
    fn each_statement_carries_only_its_parameters_with_local_markers() {
        let sql = "SELECT :a, :a; SELECT 1;\nSELECT :b";
        let requests = requests(
            sql,
            POSTGRES,
            &[
                spec("a", ParameterKind::Text, true),
                spec("b", ParameterKind::Number, false),
            ],
            &typed(&[("a", "x'; --")]),
            &ExecutionOptions::default(),
        )
        .unwrap();
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert_eq!(request.validate(), Ok(()), "{}", request.sql);
            assert!(!request.sql.contains("x'"));
        }
        assert_eq!(requests[0].parameters.len(), 1);
        assert_eq!(requests[0].parameters[0].markers.len(), 2);
        assert_eq!(requests[0].parameters[0].value.as_deref(), Some("x'; --"));
        assert!(requests[1].parameters.is_empty());
        assert_eq!(requests[2].parameters[0].name, "b");
        assert_eq!(requests[2].parameters[0].value, None);
        assert_eq!(
            requests[2].with_references(|_, _| "$1".into()).trim(),
            "SELECT $1"
        );
    }

    #[test]
    fn every_problem_is_reported_including_markers_without_a_spec() {
        let errors = requests(
            "SELECT :a, :b, :c, :c",
            POSTGRES,
            &[
                spec("a", ParameterKind::Text, true),
                spec("b", ParameterKind::Date, true),
            ],
            &typed(&[("b", "tomorrow")]),
            &ExecutionOptions::default(),
        )
        .unwrap_err();
        assert_eq!(
            errors,
            vec![
                ParameterError {
                    name: "a".into(),
                    problem: ParameterProblem::Missing
                },
                ParameterError {
                    name: "b".into(),
                    problem: ParameterProblem::Invalid
                },
                ParameterError {
                    name: "c".into(),
                    problem: ParameterProblem::Missing
                },
            ]
        );
    }
}
