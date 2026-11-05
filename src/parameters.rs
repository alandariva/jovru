//! `:name` parameters shared by reports and SQL tabs: which ones a SQL text
//! holds, how a typed value is checked, and the requests that carry them
//! (ADR-0023, ADR-0026). Values never go into the SQL text; each request
//! carries them with the place of their markers, and the driver binds them.
use crate::sql::{self, SqlDialect};
use jykre_core::{ExecutionOptions, ParameterKind, QueryParameter, QueryRequest};

/// Every kind, in the order forms offer them.
pub const KINDS: [ParameterKind; 5] = [
    ParameterKind::Text,
    ParameterKind::Number,
    ParameterKind::Boolean,
    ParameterKind::Date,
    ParameterKind::Timestamp,
];

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterProblem {
    /// A required parameter has no value.
    Missing,
    /// The value does not have the shape of the parameter's kind.
    Invalid,
    /// The parameter offers a list (a report's), and the value is not one
    /// of its choices.
    NotAChoice,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterError {
    pub name: String,
    pub problem: ParameterProblem,
}

/// The value bound for what a field holds: trimmed (except text), lowercased
/// for booleans, `None` (NULL) when empty and not `required`.
pub fn normalize(
    kind: ParameterKind,
    typed: &str,
    required: bool,
) -> Result<Option<String>, ParameterProblem> {
    let typed = match kind {
        ParameterKind::Text => typed.to_owned(),
        ParameterKind::Boolean => typed.trim().to_ascii_lowercase(),
        _ => typed.trim().to_owned(),
    };
    if typed.is_empty() {
        return if required {
            Err(ParameterProblem::Missing)
        } else {
            Ok(None)
        };
    }
    if jykre_core::is_parameter_value(kind, &typed) {
        Ok(Some(typed))
    } else {
        Err(ParameterProblem::Invalid)
    }
}

/// One request per statement, each carrying the parameters its statement
/// uses with their markers in that statement's own text. `lookup` gives the
/// kind and value bound for a name, or why there is none. Blank statements
/// are skipped. Every problem is reported once per name, in the order the
/// markers appear, so each field can show its own.
pub fn bind<'a>(
    statements: impl IntoIterator<Item = &'a str>,
    dialect: Option<SqlDialect>,
    options: &ExecutionOptions,
    mut lookup: impl FnMut(&str) -> Result<(ParameterKind, Option<String>), ParameterProblem>,
) -> Result<Vec<QueryRequest>, Vec<ParameterError>> {
    let mut requests = Vec::new();
    let mut errors: Vec<ParameterError> = Vec::new();
    for text in statements {
        if text.trim().is_empty() {
            continue;
        }
        requests.push(bind_one(text, dialect, options, &mut errors, &mut lookup));
    }
    if errors.is_empty() {
        Ok(requests)
    } else {
        Err(errors)
    }
}

/// The request for one statement; each name not in `errors` yet is looked
/// up once, and its problem added there.
fn bind_one(
    text: &str,
    dialect: Option<SqlDialect>,
    options: &ExecutionOptions,
    errors: &mut Vec<ParameterError>,
    lookup: &mut impl FnMut(&str) -> Result<(ParameterKind, Option<String>), ParameterProblem>,
) -> QueryRequest {
    let mut parameters: Vec<QueryParameter> = Vec::new();
    for marker in sql::named_markers(text, dialect) {
        if let Some(parameter) = parameters.iter_mut().find(|p| p.name == marker.name) {
            parameter.markers.push(marker.range);
            continue;
        }
        if errors.iter().any(|error| error.name == marker.name) {
            continue;
        }
        match lookup(&marker.name) {
            Ok((kind, value)) => parameters.push(QueryParameter {
                name: marker.name,
                kind,
                value,
                markers: vec![marker.range],
            }),
            Err(problem) => errors.push(ParameterError {
                name: marker.name,
                problem,
            }),
        }
    }
    QueryRequest {
        parameters,
        ..QueryRequest::new(text, options.clone())
    }
}

/// The stored name of a kind, as reports and tabs write it.
pub fn kind_str(kind: ParameterKind) -> &'static str {
    match kind {
        ParameterKind::Text => "text",
        ParameterKind::Number => "number",
        ParameterKind::Boolean => "boolean",
        ParameterKind::Date => "date",
        ParameterKind::Timestamp => "timestamp",
    }
}

/// The kind a stored name stands for.
pub fn parse_kind(text: &str) -> Option<ParameterKind> {
    KINDS.into_iter().find(|kind| kind_str(*kind) == text)
}

/// The kind a SQL tab's field binds: inferred from what it holds, or the one
/// the user chose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KindChoice {
    #[default]
    Auto,
    Fixed(ParameterKind),
}

impl KindChoice {
    /// The kind bound for `typed`.
    pub fn resolve(self, typed: &str) -> ParameterKind {
        match self {
            Self::Auto => infer_kind(typed),
            Self::Fixed(kind) => kind,
        }
    }

    /// The stored name: `auto` or the kind's own.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fixed(kind) => kind_str(kind),
        }
    }

    /// The choice a stored name stands for; anything unknown is `Auto`.
    pub fn parse(text: &str) -> Self {
        parse_kind(text).map_or(Self::Auto, Self::Fixed)
    }
}

/// The kind a typed value looks like: `true` or `false` is a boolean, an
/// exact number a number, `YYYY-MM-DD` a date, a date and time a timestamp,
/// anything else text.
pub fn infer_kind(typed: &str) -> ParameterKind {
    let trimmed = typed.trim();
    if trimmed.is_empty() {
        return ParameterKind::Text;
    }
    let lowered = trimmed.to_ascii_lowercase();
    [
        (ParameterKind::Boolean, lowered.as_str()),
        (ParameterKind::Number, trimmed),
        (ParameterKind::Date, trimmed),
        (ParameterKind::Timestamp, trimmed),
    ]
    .into_iter()
    .find(|(kind, text)| jykre_core::is_parameter_value(*kind, text))
    .map_or(ParameterKind::Text, |(kind, _)| kind)
}

/// A SQL tab's field for one `:name`: what was typed, the kind chosen, and
/// whether it binds NULL instead (keeping the text for later).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TabParameter {
    pub name: String,
    pub kind: KindChoice,
    pub text: String,
    pub null: bool,
}

impl TabParameter {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// The kind and value a run binds. An empty field is missing: NULL is
    /// chosen explicitly.
    pub fn bound(&self) -> Result<(ParameterKind, Option<String>), ParameterProblem> {
        let kind = self.kind.resolve(&self.text);
        if self.null {
            return Ok((kind, None));
        }
        normalize(kind, &self.text, true).map(|value| (kind, value))
    }
}

/// The request for one statement a SQL tab runs, binding each of its
/// markers from `fields`. Only the markers of this statement need a value.
pub fn tab_request(
    statement: &str,
    dialect: Option<SqlDialect>,
    fields: &[TabParameter],
    options: &ExecutionOptions,
) -> Result<QueryRequest, Vec<ParameterError>> {
    let mut errors = Vec::new();
    let request = bind_one(statement, dialect, options, &mut errors, &mut |name| {
        fields
            .iter()
            .find(|field| field.name == name)
            .ok_or(ParameterProblem::Missing)?
            .bound()
    });
    if errors.is_empty() {
        Ok(request)
    } else {
        Err(errors)
    }
}

/// [`tab_request`] for each statement, every problem reported once.
pub fn tab_requests<'a>(
    statements: impl IntoIterator<Item = &'a str>,
    dialect: Option<SqlDialect>,
    fields: &[TabParameter],
    options: &ExecutionOptions,
) -> Result<Vec<QueryRequest>, Vec<ParameterError>> {
    let mut requests = Vec::new();
    let mut errors: Vec<ParameterError> = Vec::new();
    for statement in statements {
        match tab_request(statement, dialect, fields, options) {
            Ok(request) => requests.push(request),
            Err(more) => {
                for error in more {
                    if !errors.iter().any(|known| known.name == error.name) {
                        errors.push(error);
                    }
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(requests)
    } else {
        Err(errors)
    }
}

/// The fields for `names`, in their order: a name keeps its field (or the
/// one kept from before, in `kept`), and a new name gets an empty one.
pub fn reconcile_fields(kept: &[TabParameter], names: &[String]) -> Vec<TabParameter> {
    names
        .iter()
        .map(|name| {
            kept.iter()
                .find(|field| &field.name == name)
                .cloned()
                .unwrap_or_else(|| TabParameter::new(name.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const POSTGRES: Option<SqlDialect> = Some(SqlDialect::Postgres);

    fn field(name: &str, kind: KindChoice, text: &str) -> TabParameter {
        TabParameter {
            kind,
            text: text.into(),
            ..TabParameter::new(name)
        }
    }

    #[test]
    fn kinds_are_inferred_from_the_shape_of_the_value() {
        assert_eq!(infer_kind("42"), ParameterKind::Number);
        assert_eq!(infer_kind(" -1250.50 "), ParameterKind::Number);
        assert_eq!(infer_kind("1e6"), ParameterKind::Number);
        assert_eq!(infer_kind("TRUE"), ParameterKind::Boolean);
        assert_eq!(infer_kind("2026-11-05"), ParameterKind::Date);
        assert_eq!(infer_kind("2026-11-05 10:30"), ParameterKind::Timestamp);
        assert_eq!(
            infer_kind("2026-11-05T10:30:00.5"),
            ParameterKind::Timestamp
        );
        assert_eq!(infer_kind("ACME"), ParameterKind::Text);
        assert_eq!(infer_kind("1,5"), ParameterKind::Text);
        assert_eq!(infer_kind(""), ParameterKind::Text);
    }

    #[test]
    fn kind_choices_round_trip_through_their_stored_names() {
        for choice in std::iter::once(KindChoice::Auto).chain(KINDS.map(KindChoice::Fixed)) {
            assert_eq!(KindChoice::parse(choice.as_str()), choice);
        }
        assert_eq!(KindChoice::parse("money"), KindChoice::Auto);
    }

    #[test]
    fn a_field_binds_its_value_null_or_reports_its_problem() {
        let auto = field("a", KindChoice::Auto, " 7 ");
        assert_eq!(auto.bound(), Ok((ParameterKind::Number, Some("7".into()))));
        let text = field("t", KindChoice::Fixed(ParameterKind::Text), " 007 ");
        assert_eq!(
            text.bound(),
            Ok((ParameterKind::Text, Some(" 007 ".into())))
        );
        let empty = field("e", KindChoice::Auto, "");
        assert_eq!(empty.bound(), Err(ParameterProblem::Missing));
        let null = TabParameter {
            null: true,
            ..field("n", KindChoice::Fixed(ParameterKind::Date), "kept")
        };
        assert_eq!(null.bound(), Ok((ParameterKind::Date, None)));
        let wrong = field("d", KindChoice::Fixed(ParameterKind::Date), "tomorrow");
        assert_eq!(wrong.bound(), Err(ParameterProblem::Invalid));
    }

    #[test]
    fn only_the_markers_of_the_statements_run_need_a_value() {
        let fields = [field("a", KindChoice::Auto, "x'; --")];
        let requests = tab_requests(
            ["SELECT :a, :a", "SELECT 1"],
            POSTGRES,
            &fields,
            &ExecutionOptions::default(),
        )
        .unwrap();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(request.validate(), Ok(()), "{}", request.sql);
            assert!(!request.sql.contains("x'"));
        }
        assert_eq!(requests[0].parameters[0].markers, vec![7..9, 11..13]);
        assert_eq!(requests[0].parameters[0].kind, ParameterKind::Text);
        assert!(requests[1].parameters.is_empty());

        let errors = tab_requests(
            ["SELECT :a, :b", "SELECT :c, :c"],
            POSTGRES,
            &[field("b", KindChoice::Fixed(ParameterKind::Number), "x")],
            &ExecutionOptions::default(),
        )
        .unwrap_err();
        let problems: Vec<_> = errors
            .iter()
            .map(|error| (error.name.as_str(), error.problem))
            .collect();
        assert_eq!(
            problems,
            [
                ("a", ParameterProblem::Missing),
                ("b", ParameterProblem::Invalid),
                ("c", ParameterProblem::Missing),
            ]
        );
    }

    #[test]
    fn reconciled_fields_keep_what_was_typed_by_name() {
        let kept = field("a", KindChoice::Fixed(ParameterKind::Number), "1");
        let fields = reconcile_fields(
            &[kept.clone(), field("gone", KindChoice::Auto, "x")],
            &["new".to_owned(), "a".to_owned()],
        );
        assert_eq!(fields, vec![TabParameter::new("new"), kept]);
    }

    #[test]
    fn finding_names_in_a_long_script_stays_fast() {
        // The SQL tab reads its names on each edit (debug build, generous).
        let script = "SELECT id, name FROM t WHERE id = :id AND x::int > 1;\n".repeat(10_000);
        let started = Instant::now();
        assert_eq!(names(&script, POSTGRES), ["id"]);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }
}
