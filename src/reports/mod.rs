//! Reports: saved SQL with named parameters and an optional chart
//! (ADR-0024).
//!
//! The library is one JSON value under its own `config.db` setting
//! ([Persistence](../docs/persistence.md)). It holds definitions only: never
//! a result, and never the values typed for a run, only each parameter's
//! default. Reading tolerates missing fields and keeps the ones it does not
//! know, so a version that writes the value back loses nothing another
//! version stored.
use jykre_core::{ConnectionId, ParameterKind};
use serde_json::{Map, Value, json};

pub mod chart;
pub mod export;
pub mod params;

/// The settings key the library is stored under.
pub const SETTINGS_KEY: &str = "reports";
/// The format written; a reader ignores what it does not know.
const VERSION: u64 = 1;

/// The stored value is not a report library (it is not a JSON object). The
/// application keeps it untouched instead of writing an empty library over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unreadable;

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The saved reports cannot be read")
    }
}

impl std::error::Error for Unreadable {}

/// The connection a report runs on: its identity, and its name for showing
/// and for finding it when the identity is gone or was never stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionRef {
    pub id: Option<ConnectionId>,
    pub name: String,
}

/// A `:name` parameter of a report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterSpec {
    pub name: String,
    /// What the form shows; empty shows the name.
    pub label: String,
    pub kind: ParameterKind,
    /// The value the form starts with.
    pub default: Option<String>,
    /// An empty value is refused instead of running as NULL.
    pub required: bool,
}

impl ParameterSpec {
    /// A required text parameter, as a new marker in the SQL gets.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            label: String::new(),
            kind: ParameterKind::Text,
            default: None,
            required: true,
        }
    }

    /// The label the form shows.
    pub fn shown_label(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.name
        } else {
            &self.label
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChartKind {
    Bar,
    Line,
    Pie,
}

impl ChartKind {
    pub const ALL: [ChartKind; 3] = [ChartKind::Bar, ChartKind::Line, ChartKind::Pie];

    fn as_str(self) -> &'static str {
        match self {
            ChartKind::Bar => "bar",
            ChartKind::Line => "line",
            ChartKind::Pie => "pie",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

/// A chart of the result: one column labels the points, the others give
/// their values. Columns are named, so reordering the `SELECT` keeps the
/// chart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartSpec {
    pub kind: ChartKind,
    pub label: String,
    pub values: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// `0` until the report is saved.
    pub id: u64,
    pub title: String,
    pub description: String,
    pub connection: ConnectionRef,
    pub sql: String,
    /// In the order their markers first appear in the SQL.
    pub parameters: Vec<ParameterSpec>,
    pub chart: Option<ChartSpec>,
    /// Seconds since the epoch.
    pub created_at: u64,
    pub updated_at: u64,
    extra: Map<String, Value>,
}

impl Report {
    /// An unsaved report with this SQL; its parameters follow the SQL's
    /// markers, by generic rules until the connection's dialect is known.
    pub fn draft(
        title: impl Into<String>,
        connection: ConnectionRef,
        sql: impl Into<String>,
    ) -> Self {
        let sql = sql.into();
        Self {
            title: title.into(),
            connection,
            parameters: params::reconcile(&[], &params::names(&sql, None)),
            sql,
            ..Self::default()
        }
    }

    /// Whether the report's title, description, connection or SQL contains
    /// `query`, ignoring case. An empty query matches everything.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || [
                &self.title,
                &self.description,
                &self.connection.name,
                &self.sql,
            ]
            .iter()
            .any(|text| text.to_lowercase().contains(&query))
    }

    /// The SQL's first `lines` non-empty lines, for a list row.
    pub fn preview(&self, lines: usize) -> String {
        self.sql
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(lines)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// An unsaved copy with the same definition.
    pub fn duplicate(&self, title: impl Into<String>) -> Self {
        Self {
            id: 0,
            title: title.into(),
            created_at: 0,
            updated_at: 0,
            ..self.clone()
        }
    }

    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let text = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let connection = &value["connection"];
        let parameters = value["parameters"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|parameter| {
                let name = parameter["name"].as_str()?;
                jykre_core::is_parameter_name(name).then(|| ParameterSpec {
                    name: name.to_owned(),
                    label: parameter["label"].as_str().unwrap_or_default().to_owned(),
                    kind: parameter["kind"]
                        .as_str()
                        .and_then(parse_kind)
                        .unwrap_or(ParameterKind::Text),
                    default: parameter["default"].as_str().map(str::to_owned),
                    required: parameter["required"].as_bool().unwrap_or(true),
                })
            })
            .collect();
        let chart = value["chart"].as_object().and_then(|chart| {
            Some(ChartSpec {
                kind: chart
                    .get("kind")
                    .and_then(Value::as_str)
                    .and_then(ChartKind::parse)?,
                label: chart.get("label").and_then(Value::as_str)?.to_owned(),
                values: chart
                    .get("values")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect(),
            })
        });
        let known = [
            "id",
            "title",
            "description",
            "connection",
            "sql",
            "parameters",
            "chart",
            "created_at",
            "updated_at",
        ];
        Some(Self {
            id: value["id"].as_u64().unwrap_or(0),
            title: text("title"),
            description: text("description"),
            connection: ConnectionRef {
                id: connection["id"].as_str().and_then(|id| id.parse().ok()),
                name: connection["name"].as_str().unwrap_or_default().to_owned(),
            },
            sql: text("sql"),
            parameters,
            chart,
            created_at: value["created_at"].as_u64().unwrap_or(0),
            updated_at: value["updated_at"].as_u64().unwrap_or(0),
            extra: object
                .iter()
                .filter(|(key, _)| !known.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        })
    }

    fn to_value(&self) -> Value {
        let mut object = self.extra.clone();
        let known = json!({
            "id": self.id,
            "title": self.title,
            "description": self.description,
            "connection": {
                "id": self.connection.id.map(|id| id.to_string()),
                "name": self.connection.name,
            },
            "sql": self.sql,
            "parameters": self.parameters.iter().map(|parameter| json!({
                "name": parameter.name,
                "label": parameter.label,
                "kind": kind_str(parameter.kind),
                "default": parameter.default,
                "required": parameter.required,
            })).collect::<Vec<_>>(),
            "chart": self.chart.as_ref().map(|chart| json!({
                "kind": chart.kind.as_str(),
                "label": chart.label,
                "values": chart.values,
            })),
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        });
        if let Value::Object(known) = known {
            object.extend(known);
        }
        Value::Object(object)
    }
}

fn kind_str(kind: ParameterKind) -> &'static str {
    match kind {
        ParameterKind::Text => "text",
        ParameterKind::Number => "number",
        ParameterKind::Boolean => "boolean",
        ParameterKind::Date => "date",
        ParameterKind::Timestamp => "timestamp",
    }
}

fn parse_kind(text: &str) -> Option<ParameterKind> {
    [
        ParameterKind::Text,
        ParameterKind::Number,
        ParameterKind::Boolean,
        ParameterKind::Date,
        ParameterKind::Timestamp,
    ]
    .into_iter()
    .find(|kind| kind_str(*kind) == text)
}

/// The saved reports.
#[derive(Clone, Debug, PartialEq)]
pub struct ReportLibrary {
    pub reports: Vec<Report>,
    /// The identity the next saved report gets.
    pub next_id: u64,
    extra: Map<String, Value>,
}

impl Default for ReportLibrary {
    fn default() -> Self {
        Self {
            reports: Vec::new(),
            next_id: 1,
            extra: Map::new(),
        }
    }
}

impl ReportLibrary {
    /// Reads a stored value. Nothing stored gives an empty library; a value
    /// that is not an object is [`Unreadable`]. Entries that are not objects
    /// are skipped, and a missing or repeated identity is replaced.
    pub fn from_value(value: Option<Value>) -> Result<Self, Unreadable> {
        let object = match value {
            None | Some(Value::Null) => return Ok(Self::default()),
            Some(Value::Object(object)) => object,
            Some(_) => return Err(Unreadable),
        };
        let mut reports: Vec<Report> = object
            .get("reports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Report::from_value)
            .collect();
        let mut next_id = object
            .get("next_id")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1)
            .max(
                reports
                    .iter()
                    .map(|report| report.id.saturating_add(1))
                    .max()
                    .unwrap_or(1),
            );
        let mut seen = std::collections::HashSet::new();
        for report in &mut reports {
            if report.id == 0 || !seen.insert(report.id) {
                report.id = next_id;
                seen.insert(next_id);
                next_id += 1;
            }
        }
        let extra = object
            .into_iter()
            .filter(|(key, _)| !matches!(key.as_str(), "version" | "next_id" | "reports"))
            .collect();
        Ok(Self {
            reports,
            next_id,
            extra,
        })
    }

    pub fn to_value(&self) -> Value {
        let mut object = self.extra.clone();
        object.insert("version".into(), json!(VERSION));
        object.insert("next_id".into(), json!(self.next_id));
        object.insert(
            "reports".into(),
            Value::Array(self.reports.iter().map(Report::to_value).collect()),
        );
        Value::Object(object)
    }

    pub fn get(&self, id: u64) -> Option<&Report> {
        self.reports.iter().find(|report| report.id == id)
    }

    /// Stores `report` at `now` (seconds since the epoch): the report with
    /// its identity is replaced, keeping when it was created; a report
    /// without one (or with an identity no longer stored) is added with a
    /// new one. Returns the identity.
    pub fn save(&mut self, mut report: Report, now: u64) -> u64 {
        report.updated_at = now;
        if let Some(stored) = self
            .reports
            .iter_mut()
            .find(|stored| report.id != 0 && stored.id == report.id)
        {
            report.created_at = stored.created_at;
            *stored = report;
            return stored.id;
        }
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        report.id = id;
        report.created_at = now;
        self.reports.push(report);
        id
    }

    /// Removes the report with identity `id` and returns it.
    pub fn remove(&mut self, id: u64) -> Option<Report> {
        let index = self.reports.iter().position(|report| report.id == id)?;
        Some(self.reports.remove(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Report {
        Report {
            title: "Revenue".into(),
            description: "By month".into(),
            connection: ConnectionRef {
                id: Some(ConnectionId::new()),
                name: "warehouse".into(),
            },
            sql: "SELECT month, total\nFROM sales\nWHERE day >= :start".into(),
            parameters: vec![ParameterSpec {
                kind: ParameterKind::Date,
                default: Some("2024-01-01".into()),
                ..ParameterSpec::new("start")
            }],
            chart: Some(ChartSpec {
                kind: ChartKind::Line,
                label: "month".into(),
                values: vec!["total".into()],
            }),
            ..Report::default()
        }
    }

    #[test]
    fn a_library_survives_a_round_trip() {
        let mut library = ReportLibrary::default();
        let id = library.save(sample(), 100);
        let read = ReportLibrary::from_value(Some(library.to_value())).unwrap();
        assert_eq!(read, library);
        assert_eq!(read.get(id).unwrap().created_at, 100);
        assert_eq!(read.to_value()["version"], 1);
    }

    #[test]
    fn nothing_stored_is_empty_and_a_value_that_is_not_an_object_is_unreadable() {
        assert_eq!(
            ReportLibrary::from_value(None),
            Ok(ReportLibrary::default())
        );
        assert_eq!(
            ReportLibrary::from_value(Some(Value::Null)),
            Ok(ReportLibrary::default())
        );
        assert_eq!(ReportLibrary::from_value(Some(json!([1]))), Err(Unreadable));
        assert_eq!(ReportLibrary::from_value(Some(json!("x"))), Err(Unreadable));
    }

    #[test]
    fn unknown_fields_are_kept_and_missing_ones_take_defaults() {
        let stored = json!({
            "version": 9,
            "future": {"a": 1},
            "reports": [
                {"title": "Old", "sql": "SELECT 1", "shared_with": ["x"],
                 "connection": {"id": "not-a-uuid", "name": "db"},
                 "parameters": [{"name": "n", "kind": "money"}, {"name": "1bad"}],
                 "chart": {"kind": "radar", "label": "a", "values": ["b"]}},
                "not a report",
            ],
        });
        let library = ReportLibrary::from_value(Some(stored)).unwrap();
        assert_eq!(library.reports.len(), 1);
        let report = &library.reports[0];
        assert_eq!(report.id, 1);
        assert_eq!(
            report.connection,
            ConnectionRef {
                id: None,
                name: "db".into()
            }
        );
        assert_eq!(report.parameters, vec![ParameterSpec::new("n")]);
        assert_eq!(report.chart, None);
        let written = library.to_value();
        assert_eq!(written["future"], json!({"a": 1}));
        assert_eq!(written["reports"][0]["shared_with"], json!(["x"]));
        assert_eq!(written["version"], 1);
    }

    #[test]
    fn missing_or_repeated_identities_are_replaced() {
        let stored = json!({"next_id": 2, "reports": [
            {"id": 5, "title": "a"}, {"id": 5, "title": "b"}, {"title": "c"}
        ]});
        let library = ReportLibrary::from_value(Some(stored)).unwrap();
        let ids: Vec<_> = library.reports.iter().map(|report| report.id).collect();
        assert_eq!(ids, [5, 6, 7]);
        assert_eq!(library.next_id, 8);
    }

    #[test]
    fn saving_updates_in_place_or_adds_and_removing_returns_the_report() {
        let mut library = ReportLibrary::default();
        let id = library.save(sample(), 10);
        let mut edited = library.get(id).unwrap().clone();
        edited.title = "Revenue 2".into();
        assert_eq!(library.save(edited, 20), id);
        assert_eq!(library.reports.len(), 1);
        let stored = library.get(id).unwrap();
        assert_eq!(
            (stored.title.as_str(), stored.created_at, stored.updated_at),
            ("Revenue 2", 10, 20)
        );
        let copy = stored.duplicate("Copy");
        let copy_id = library.save(copy, 30);
        assert_ne!(copy_id, id);
        assert_eq!(library.remove(id).map(|report| report.id), Some(id));
        assert_eq!(library.remove(id), None);
        assert_eq!(library.reports.len(), 1);
    }

    #[test]
    fn matching_ignores_case_and_previews_skip_blank_lines() {
        let report = sample();
        assert!(report.matches(""));
        assert!(report.matches("REVENUE"));
        assert!(report.matches("warehouse"));
        assert!(report.matches(":start"));
        assert!(!report.matches("inventory"));
        assert_eq!(report.preview(2), "SELECT month, total FROM sales");
    }
}
