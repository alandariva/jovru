//! Reports: panels of saved SQL with shared named parameters, each panel
//! with its own connection, database and chart (ADR-0024, ADR-0025).
//!
//! The library is one JSON value under its own `config.db` setting
//! ([Persistence](../docs/persistence.md)). It holds definitions only: never
//! a result, and never the values typed for a run, only each parameter's
//! default. Reading tolerates missing fields and keeps the ones it does not
//! know, so a version that writes the value back loses nothing another
//! version stored.
use crate::parameters::{kind_str, parse_kind};
use jykre_core::{ConnectionId, ParameterKind, SessionContext};
use serde_json::{Map, Value, json};

pub mod chart;
pub mod export;
pub mod params;
pub mod run;

/// The settings key the library is stored under.
pub const SETTINGS_KEY: &str = "reports";
/// The format written; a reader ignores what it does not know. Version 1
/// held one SQL and one chart per report, read as one panel; versions 1 and
/// 2 held one connection per report, given to each of its panels.
const VERSION: u64 = 3;
/// The most panels a report holds. Each one keeps a result and a chart, so
/// the bound keeps a report's memory and its frame small.
pub const MAX_PANELS: usize = 12;
/// The most choices a list parameter offers.
pub const MAX_CHOICES: usize = 50;

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

/// The report already holds [`MAX_PANELS`] panels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full;

/// The connection a panel runs on: its identity, and its name for showing
/// and for finding it when the identity is gone or was never stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionRef {
    pub id: Option<ConnectionId>,
    pub name: String,
}

impl ConnectionRef {
    /// Whether both name the same connection: by identity when both have
    /// one, otherwise by name.
    pub fn same(&self, other: &ConnectionRef) -> bool {
        match (self.id, other.id) {
            (Some(ours), Some(theirs)) => ours == theirs,
            _ => self.name == other.name,
        }
    }

    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self {
            id: object
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| id.parse().ok()),
            name: object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }

    fn to_value(&self) -> Value {
        json!({
            "id": self.id.map(|id| id.to_string()),
            "name": self.name,
        })
    }
}

/// A `:name` parameter of a report, shared by every panel whose SQL holds
/// its marker.
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
    /// The values the form offers in a list; empty means a free field.
    pub choices: Vec<String>,
    extra: Map<String, Value>,
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
            choices: Vec::new(),
            extra: Map::new(),
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

    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let name = object.get("name")?.as_str()?;
        if !jykre_core::is_parameter_name(name) {
            return None;
        }
        let known = ["name", "label", "kind", "default", "required", "choices"];
        Some(Self {
            name: name.to_owned(),
            label: value["label"].as_str().unwrap_or_default().to_owned(),
            kind: value["kind"]
                .as_str()
                .and_then(parse_kind)
                .unwrap_or(ParameterKind::Text),
            default: value["default"].as_str().map(str::to_owned),
            required: value["required"].as_bool().unwrap_or(true),
            choices: value["choices"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|choice| choice.as_str().map(str::to_owned))
                .take(MAX_CHOICES)
                .collect(),
            extra: unknown(object, &known),
        })
    }

    fn to_value(&self) -> Value {
        with_extra(
            &self.extra,
            json!({
                "name": self.name,
                "label": self.label,
                "kind": kind_str(self.kind),
                "default": self.default,
                "required": self.required,
                "choices": self.choices,
            }),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChartKind {
    Bar,
    Line,
    Pie,
    /// One value shown large, as an indicator.
    Number,
}

impl ChartKind {
    pub const ALL: [ChartKind; 4] = [
        ChartKind::Bar,
        ChartKind::Line,
        ChartKind::Pie,
        ChartKind::Number,
    ];

    fn as_str(self) -> &'static str {
        match self {
            ChartKind::Bar => "bar",
            ChartKind::Line => "line",
            ChartKind::Pie => "pie",
            ChartKind::Number => "number",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }

    /// Whether the kind draws one column of values only.
    pub fn single_value(self) -> bool {
        matches!(self, ChartKind::Pie | ChartKind::Number)
    }

    /// Whether the kind needs a column that labels its points.
    pub fn labelled(self) -> bool {
        self != ChartKind::Number
    }
}

/// A chart of a panel's result: one column labels the points, the others
/// give their values. Columns are named, so reordering the `SELECT` keeps
/// the chart. A [`ChartKind::Number`] has no label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChartSpec {
    pub kind: ChartKind,
    pub label: String,
    pub values: Vec<String>,
}

impl ChartSpec {
    fn from_value(value: &Value) -> Option<Self> {
        let chart = value.as_object()?;
        Some(Self {
            kind: chart
                .get("kind")
                .and_then(Value::as_str)
                .and_then(ChartKind::parse)?,
            label: chart
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            values: chart
                .get("values")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect(),
        })
    }

    fn to_value(&self) -> Value {
        json!({
            "kind": self.kind.as_str(),
            "label": self.label,
            "values": self.values,
        })
    }
}

/// A panel's identity within its report: stable while the report is edited,
/// whatever the panel's place.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PanelId(pub u64);

/// How much of the report's width a panel takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Width {
    /// Half the width: two such panels share a row.
    Half,
    #[default]
    Full,
}

impl Width {
    fn as_str(self) -> &'static str {
        match self {
            Width::Half => "half",
            Width::Full => "full",
        }
    }
}

/// Where a panel moves among the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// One part of a report: its SQL, the connection and database it runs in,
/// and how its result is shown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Panel {
    pub id: PanelId,
    /// Empty shows "Panel n".
    pub title: String,
    pub connection: ConnectionRef,
    pub sql: String,
    /// Where the panel's unqualified names resolve; the default leaves the
    /// session as the connection opens it.
    pub context: SessionContext,
    pub chart: Option<ChartSpec>,
    /// The result's table shows under the chart (always without one).
    pub table: bool,
    pub width: Width,
    extra: Map<String, Value>,
}

impl Panel {
    pub fn new(
        id: PanelId,
        connection: ConnectionRef,
        sql: impl Into<String>,
        context: SessionContext,
    ) -> Self {
        Self {
            id,
            connection,
            sql: sql.into(),
            context,
            table: true,
            ..Self::default()
        }
    }

    /// The title shown for the panel at `index` (counted from zero).
    pub fn shown_title(&self, index: usize) -> String {
        if self.title.trim().is_empty() {
            crate::tr!("Panel {0}", index + 1)
        } else {
            self.title.clone()
        }
    }

    /// Whether the result's table shows: asked for, or there is no chart.
    pub fn shows_table(&self) -> bool {
        self.table || self.chart.is_none()
    }

    /// Where the panel runs, as the application shows it: the connection,
    /// then the database when one is chosen.
    pub fn source_label(&self) -> String {
        source_label(&self.connection.name, &self.context)
    }

    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let chart = ChartSpec::from_value(&value["chart"]);
        let known = [
            "id",
            "title",
            "connection",
            "sql",
            "context",
            "chart",
            "table",
            "width",
        ];
        Some(Self {
            id: PanelId(value["id"].as_u64().unwrap_or(0)),
            title: value["title"].as_str().unwrap_or_default().to_owned(),
            connection: ConnectionRef::from_value(&value["connection"]).unwrap_or_default(),
            sql: value["sql"].as_str().unwrap_or_default().to_owned(),
            context: context_from(&value["context"]),
            table: value["table"].as_bool().unwrap_or(chart.is_none()),
            chart,
            width: match value["width"].as_str() {
                Some("half") => Width::Half,
                _ => Width::Full,
            },
            extra: unknown(object, &known),
        })
    }

    fn to_value(&self) -> Value {
        with_extra(
            &self.extra,
            json!({
                "id": self.id.0,
                "title": self.title,
                "connection": self.connection.to_value(),
                "sql": self.sql,
                "context": {
                    "catalog": self.context.catalog,
                    "schema": self.context.schema,
                },
                "chart": self.chart.as_ref().map(ChartSpec::to_value),
                "table": self.table,
                "width": self.width.as_str(),
            }),
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// `0` until the report is saved.
    pub id: u64,
    pub title: String,
    pub description: String,
    /// Shared by the panels, in the order their markers first appear.
    pub parameters: Vec<ParameterSpec>,
    /// In the order they show.
    pub panels: Vec<Panel>,
    /// Seconds since the epoch.
    pub created_at: u64,
    pub updated_at: u64,
    extra: Map<String, Value>,
}

impl Report {
    /// An unsaved report with one panel of this SQL, on `connection` in
    /// `context`; its parameters follow the SQL's markers, by generic rules
    /// until the connection's dialect is known.
    pub fn draft(
        title: impl Into<String>,
        connection: ConnectionRef,
        sql: impl Into<String>,
        context: SessionContext,
    ) -> Self {
        let sql = sql.into();
        Self {
            title: title.into(),
            parameters: params::reconcile(&[], &params::names(&sql, None)),
            panels: vec![Panel::new(PanelId(1), connection, sql, context)],
            ..Self::default()
        }
    }

    /// The connections the panels run on, as a list row names them: one or
    /// two by name, more by their number.
    pub fn connections_label(&self) -> String {
        let connections = self.connections();
        match connections.len() {
            0..=2 => connections
                .into_iter()
                .map(|connection| connection.name)
                .collect::<Vec<_>>()
                .join(", "),
            count => crate::tr!("{0} connections", count),
        }
    }

    /// The connections the panels run on, once each, in the panels' order.
    pub fn connections(&self) -> Vec<ConnectionRef> {
        let mut found: Vec<ConnectionRef> = Vec::new();
        for panel in &self.panels {
            if !found.iter().any(|known| known.same(&panel.connection)) {
                found.push(panel.connection.clone());
            }
        }
        found
    }

    /// The panel with identity `id`.
    pub fn panel(&self, id: PanelId) -> Option<&Panel> {
        self.panels.iter().find(|panel| panel.id == id)
    }

    /// The identity the next panel gets.
    fn next_panel_id(&self) -> PanelId {
        PanelId(
            self.panels
                .iter()
                .map(|panel| panel.id.0)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        )
    }

    /// Adds a panel of `sql` on `connection` in `context` at the end, with a
    /// new identity.
    pub fn add_panel(
        &mut self,
        connection: ConnectionRef,
        sql: impl Into<String>,
        context: SessionContext,
    ) -> Result<PanelId, Full> {
        if self.panels.len() >= MAX_PANELS {
            return Err(Full);
        }
        let id = self.next_panel_id();
        self.panels.push(Panel::new(id, connection, sql, context));
        Ok(id)
    }

    /// A copy of the panel `id` right after it, with a new identity.
    pub fn duplicate_panel(&mut self, id: PanelId) -> Result<Option<PanelId>, Full> {
        if self.panels.len() >= MAX_PANELS {
            return Err(Full);
        }
        let Some(index) = self.panels.iter().position(|panel| panel.id == id) else {
            return Ok(None);
        };
        let copy = Panel {
            id: self.next_panel_id(),
            ..self.panels[index].clone()
        };
        let copy_id = copy.id;
        self.panels.insert(index + 1, copy);
        Ok(Some(copy_id))
    }

    /// Moves the panel `id` one place; `false` when it is already at that
    /// end or does not exist.
    pub fn move_panel(&mut self, id: PanelId, direction: Direction) -> bool {
        let Some(index) = self.panels.iter().position(|panel| panel.id == id) else {
            return false;
        };
        let other = match direction {
            Direction::Up if index > 0 => index - 1,
            Direction::Down if index + 1 < self.panels.len() => index + 1,
            _ => return false,
        };
        self.panels.swap(index, other);
        true
    }

    /// Moves the panel `id` to `index` (counted after taking it out, as
    /// [`drop_index`] gives it); `false` when nothing moved.
    pub fn move_panel_to(&mut self, id: PanelId, index: usize) -> bool {
        let Some(from) = self.panels.iter().position(|panel| panel.id == id) else {
            return false;
        };
        let index = index.min(self.panels.len() - 1);
        if from == index {
            return false;
        }
        let panel = self.panels.remove(from);
        self.panels.insert(index, panel);
        true
    }

    /// Removes the panel `id` and returns it.
    pub fn remove_panel(&mut self, id: PanelId) -> Option<Panel> {
        let index = self.panels.iter().position(|panel| panel.id == id)?;
        Some(self.panels.remove(index))
    }

    /// The parameters follow the markers of every panel's SQL, each read by
    /// its connection's dialect (`dialect` gives it): a name keeps its
    /// specification, a new one gets a required text one, and a name no
    /// panel uses any more loses its own.
    pub fn reconcile_parameters(
        &mut self,
        dialect: impl Fn(&ConnectionRef) -> Option<jykre_core::SqlDialect>,
    ) {
        let names = params::names_all(
            self.panels
                .iter()
                .map(|panel| (panel.sql.as_str(), dialect(&panel.connection))),
        );
        self.parameters = params::reconcile(&self.parameters, &names);
    }

    /// Whether the report's title or description, or a panel's title,
    /// connection or SQL contains `query`, ignoring case. An empty query
    /// matches everything.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || [&self.title, &self.description]
                .into_iter()
                .chain(
                    self.panels
                        .iter()
                        .flat_map(|panel| [&panel.title, &panel.connection.name, &panel.sql]),
                )
                .any(|text| text.to_lowercase().contains(&query))
    }

    /// The first `lines` non-empty lines of the first panel with SQL, for a
    /// list row.
    pub fn preview(&self, lines: usize) -> String {
        self.panels
            .iter()
            .find(|panel| !panel.sql.trim().is_empty())
            .map(|panel| {
                panel
                    .sql
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .take(lines)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
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

    /// Whether `other` defines the same report: the same title, description,
    /// parameters and panels. Identity and times are not compared.
    pub fn same_definition(&self, other: &Report) -> bool {
        self.title == other.title
            && self.description == other.description
            && self.parameters == other.parameters
            && self.panels == other.panels
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
        // Versions 1 and 2: one connection for the whole report.
        let shared = ConnectionRef::from_value(&value["connection"]);
        let parameters = value["parameters"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(ParameterSpec::from_value)
            .collect();
        let mut panels: Vec<Panel> = match value["panels"].as_array() {
            Some(panels) => panels.iter().filter_map(Panel::from_value).collect(),
            // Version 1: the report's SQL and chart are its one panel.
            None => {
                let chart = ChartSpec::from_value(&value["chart"]);
                vec![Panel {
                    id: PanelId(1),
                    sql: text("sql"),
                    table: true,
                    chart,
                    ..Panel::default()
                }]
            }
        };
        if let Some(shared) = &shared {
            for panel in &mut panels {
                if panel.connection == ConnectionRef::default() {
                    panel.connection = shared.clone();
                }
            }
        }
        let mut next = panels
            .iter()
            .map(|panel| panel.id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let mut seen = std::collections::HashSet::new();
        for panel in &mut panels {
            if panel.id.0 == 0 || !seen.insert(panel.id) {
                panel.id = PanelId(next);
                seen.insert(panel.id);
                next += 1;
            }
        }
        let known = [
            "id",
            "title",
            "description",
            "connection",
            "sql",
            "parameters",
            "chart",
            "panels",
            "created_at",
            "updated_at",
        ];
        Some(Self {
            id: value["id"].as_u64().unwrap_or(0),
            title: text("title"),
            description: text("description"),
            parameters,
            panels,
            created_at: value["created_at"].as_u64().unwrap_or(0),
            updated_at: value["updated_at"].as_u64().unwrap_or(0),
            extra: unknown(object, &known),
        })
    }

    fn to_value(&self) -> Value {
        with_extra(
            &self.extra,
            json!({
                "id": self.id,
                "title": self.title,
                "description": self.description,
                "parameters": self.parameters.iter().map(ParameterSpec::to_value).collect::<Vec<_>>(),
                "panels": self.panels.iter().map(Panel::to_value).collect::<Vec<_>>(),
                "created_at": self.created_at,
                "updated_at": self.updated_at,
            }),
        )
    }
}

/// Where a panel on `connection` runs in `context`, as the application
/// shows it: the connection, then the database when one is chosen.
pub fn source_label(connection: &str, context: &SessionContext) -> String {
    if *context == SessionContext::default() {
        connection.to_owned()
    } else {
        format!("{connection} · {}", crate::ddl::context_label(context))
    }
}

/// Where a panel dragged from `from` lands when dropped on the panel at
/// `target` (or after the last one, `target == len`), counted after taking
/// it out: it takes the target's place. `None` when nothing moves.
pub fn drop_index(from: usize, target: usize, len: usize) -> Option<usize> {
    if from >= len {
        return None;
    }
    let index = if target >= len { len - 1 } else { target };
    (index != from).then_some(index)
}

/// The context a stored `{"catalog", "schema"}` names; anything else is the
/// default.
fn context_from(value: &Value) -> SessionContext {
    let level = |key: &str| {
        value[key]
            .as_str()
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
    };
    SessionContext {
        catalog: level("catalog"),
        schema: level("schema"),
    }
}

/// The entries of `object` whose keys are not in `known`.
fn unknown(object: &Map<String, Value>, known: &[&str]) -> Map<String, Value> {
    object
        .iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// `known` (an object) over the kept unknown entries.
fn with_extra(extra: &Map<String, Value>, known: Value) -> Value {
    let mut object = extra.clone();
    if let Value::Object(known) = known {
        object.extend(known);
    }
    Value::Object(object)
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

    fn warehouse() -> ConnectionRef {
        ConnectionRef {
            id: Some(ConnectionId::new()),
            name: "warehouse".into(),
        }
    }

    fn sample() -> Report {
        let mut report = Report {
            title: "Revenue".into(),
            description: "By month".into(),
            parameters: vec![ParameterSpec {
                kind: ParameterKind::Date,
                default: Some("2024-01-01".into()),
                ..ParameterSpec::new("start")
            }],
            ..Report::default()
        };
        report
            .add_panel(
                warehouse(),
                "SELECT month, total\nFROM sales\nWHERE day >= :start",
                SessionContext::default(),
            )
            .unwrap();
        report.panels[0].chart = Some(ChartSpec {
            kind: ChartKind::Line,
            label: "month".into(),
            values: vec!["total".into()],
        });
        let other = report
            .add_panel(
                ConnectionRef {
                    id: None,
                    name: "shop db".into(),
                },
                "SELECT count(*) FROM orders WHERE day >= :start",
                SessionContext {
                    catalog: None,
                    schema: Some("shop".into()),
                },
            )
            .unwrap();
        let panel = report.panels.iter_mut().find(|p| p.id == other).unwrap();
        panel.title = "Orders".into();
        panel.width = Width::Half;
        panel.table = false;
        panel.chart = Some(ChartSpec {
            kind: ChartKind::Number,
            label: String::new(),
            values: vec!["count".into()],
        });
        report
    }

    #[test]
    fn a_library_survives_a_round_trip() {
        let mut library = ReportLibrary::default();
        let id = library.save(sample(), 100);
        let read = ReportLibrary::from_value(Some(library.to_value())).unwrap();
        assert_eq!(read, library);
        assert_eq!(read.get(id).unwrap().created_at, 100);
        assert_eq!(read.to_value()["version"], 3);
        let panels = &read.get(id).unwrap().panels;
        assert_eq!(panels.len(), 2);
        assert_eq!(panels[0].connection.name, "warehouse");
        assert!(panels[0].connection.id.is_some());
        assert_eq!(panels[1].connection.name, "shop db");
        assert_eq!(panels[1].context.schema.as_deref(), Some("shop"));
        assert_eq!(panels[1].width, Width::Half);
        assert_eq!(panels[1].chart.as_ref().unwrap().kind, ChartKind::Number);
        assert!(read.to_value()["reports"][0].get("connection").is_none());
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
    fn a_first_version_report_is_read_as_one_panel_on_its_connection() {
        let stored = json!({
            "version": 1,
            "next_id": 2,
            "reports": [{
                "id": 1, "title": "Old", "sql": "SELECT a, b FROM t",
                "connection": {"id": null, "name": "db"},
                "chart": {"kind": "bar", "label": "a", "values": ["b"]},
                "parameters": [],
            }],
        });
        let library = ReportLibrary::from_value(Some(stored)).unwrap();
        let report = &library.reports[0];
        assert_eq!(report.panels.len(), 1);
        let panel = &report.panels[0];
        assert_eq!(panel.id, PanelId(1));
        assert_eq!(panel.connection.name, "db");
        assert_eq!(panel.sql, "SELECT a, b FROM t");
        assert_eq!(panel.chart.as_ref().unwrap().kind, ChartKind::Bar);
        assert_eq!(panel.context, SessionContext::default());
        assert!(panel.table);
        // Written back in the new format, without the old fields.
        let written = library.to_value();
        assert_eq!(written["version"], 3);
        assert!(written["reports"][0].get("sql").is_none());
        assert!(written["reports"][0].get("connection").is_none());
        assert_eq!(
            written["reports"][0]["panels"][0]["connection"]["name"],
            "db"
        );
    }

    #[test]
    fn a_second_version_report_gives_its_connection_to_every_panel() {
        let id = ConnectionId::new();
        let stored = json!({
            "version": 2,
            "reports": [{
                "id": 1, "title": "Two",
                "connection": {"id": id.to_string(), "name": "db"},
                "panels": [
                    {"id": 1, "sql": "SELECT 1"},
                    {"id": 2, "sql": "SELECT 2", "connection": {"id": null, "name": "other"}},
                ],
            }],
        });
        let library = ReportLibrary::from_value(Some(stored)).unwrap();
        let panels = &library.reports[0].panels;
        assert_eq!(
            panels[0].connection,
            ConnectionRef {
                id: Some(id),
                name: "db".into()
            }
        );
        assert_eq!(panels[1].connection.name, "other");
        assert_eq!(library.reports[0].connections().len(), 2);
    }

    #[test]
    fn unknown_fields_are_kept_at_every_level_and_missing_ones_take_defaults() {
        let stored = json!({
            "version": 9,
            "future": {"a": 1},
            "reports": [
                {"title": "New", "shared_with": ["x"],
                 "parameters": [
                     {"name": "n", "kind": "money", "choices": ["a", 1, "b"], "hint": "h"},
                     {"name": "1bad"},
                 ],
                 "panels": [
                     {"sql": "SELECT 1", "chart": {"kind": "radar", "label": "a", "values": ["b"]},
                      "connection": {"id": "not-a-uuid", "name": "db"},
                      "color": "red", "context": {"catalog": "", "schema": "s"}},
                     "not a panel",
                     {"id": 7, "sql": "SELECT 2", "width": "half"},
                     {"id": 7, "sql": "SELECT 3"},
                 ]},
                "not a report",
            ],
        });
        let library = ReportLibrary::from_value(Some(stored)).unwrap();
        assert_eq!(library.reports.len(), 1);
        let report = &library.reports[0];
        assert_eq!(report.id, 1);
        assert_eq!(
            report.panels[0].connection,
            ConnectionRef {
                id: None,
                name: "db".into()
            }
        );
        assert_eq!(report.panels[1].connection, ConnectionRef::default());
        assert_eq!(report.parameters.len(), 1);
        assert_eq!(report.parameters[0].kind, ParameterKind::Text);
        assert_eq!(report.parameters[0].choices, ["a", "b"]);
        let ids: Vec<_> = report.panels.iter().map(|panel| panel.id).collect();
        assert_eq!(ids, [PanelId(8), PanelId(7), PanelId(9)]);
        assert_eq!(report.panels[0].chart, None);
        assert!(report.panels[0].table);
        assert_eq!(
            report.panels[0].context,
            SessionContext {
                catalog: None,
                schema: Some("s".into())
            }
        );
        assert_eq!(report.panels[1].width, Width::Half);
        let written = library.to_value();
        assert_eq!(written["future"], json!({"a": 1}));
        assert_eq!(written["reports"][0]["shared_with"], json!(["x"]));
        assert_eq!(written["reports"][0]["panels"][0]["color"], "red");
        assert_eq!(written["reports"][0]["parameters"][0]["hint"], "h");
        assert_eq!(written["version"], 3);
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
    fn panels_are_added_moved_duplicated_and_removed_up_to_the_limit() {
        let mut report = Report::draft("R", warehouse(), "SELECT 1", SessionContext::default());
        let first = report.panels[0].id;
        let second = report
            .add_panel(warehouse(), "SELECT 2", SessionContext::default())
            .unwrap();
        assert!(report.move_panel(second, Direction::Up));
        assert!(!report.move_panel(second, Direction::Up));
        assert_eq!(report.panels[0].id, second);
        let copy = report.duplicate_panel(second).unwrap().unwrap();
        assert_eq!(report.panels[1].id, copy);
        assert_eq!(report.panels[1].sql, "SELECT 2");
        assert_eq!(
            report.remove_panel(first).map(|panel| panel.sql),
            Some("SELECT 1".into())
        );
        while report.panels.len() < MAX_PANELS {
            report
                .add_panel(warehouse(), "SELECT", SessionContext::default())
                .unwrap();
        }
        assert_eq!(
            report.add_panel(warehouse(), "SELECT", SessionContext::default()),
            Err(Full)
        );
        assert_eq!(report.duplicate_panel(copy), Err(Full));
        let mut ids: Vec<_> = report.panels.iter().map(|panel| panel.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), MAX_PANELS, "every panel keeps its own identity");
    }

    #[test]
    fn a_dropped_panel_takes_the_place_of_the_one_it_lands_on() {
        // Forward: the panel goes after the one it lands on.
        assert_eq!(drop_index(0, 2, 4), Some(2));
        // Backward: before it.
        assert_eq!(drop_index(3, 1, 4), Some(1));
        // On itself: nothing moves.
        assert_eq!(drop_index(2, 2, 4), None);
        // After the last panel: to the end.
        assert_eq!(drop_index(1, 4, 4), Some(3));
        assert_eq!(drop_index(3, 4, 4), None);
        assert_eq!(drop_index(5, 1, 4), None);
        let mut report = sample();
        report
            .add_panel(warehouse(), "SELECT 3", SessionContext::default())
            .unwrap();
        let ids: Vec<_> = report.panels.iter().map(|panel| panel.id).collect();
        assert!(report.move_panel_to(ids[0], 2));
        let moved: Vec<_> = report.panels.iter().map(|panel| panel.id).collect();
        assert_eq!(moved, [ids[1], ids[2], ids[0]]);
        assert!(!report.move_panel_to(ids[0], 2));
        assert!(report.move_panel_to(ids[0], 0));
        assert!(!report.move_panel_to(PanelId(99), 0));
    }

    #[test]
    fn parameters_follow_the_markers_of_every_panel_in_its_dialect() {
        let mut report = sample();
        report.panels[1].sql = "SELECT count(*) FROM orders WHERE region = :region".into();
        report.reconcile_parameters(|_| None);
        let names: Vec<_> = report.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["start", "region"]);
        assert_eq!(report.parameters[0].kind, ParameterKind::Date);
        report.panels[0].sql = "SELECT 1".into();
        report.reconcile_parameters(|_| None);
        let names: Vec<_> = report.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["region"]);
    }

    #[test]
    fn matching_covers_every_panel_and_its_connection() {
        let report = sample();
        assert!(report.matches(""));
        assert!(report.matches("REVENUE"));
        assert!(report.matches("warehouse"));
        assert!(report.matches("shop db"));
        assert!(report.matches(":start"));
        assert!(report.matches("orders"));
        assert!(!report.matches("inventory"));
        assert_eq!(report.preview(2), "SELECT month, total FROM sales");
        let names: Vec<_> = report
            .connections()
            .into_iter()
            .map(|connection| connection.name)
            .collect();
        assert_eq!(names, ["warehouse", "shop db"]);
        assert_eq!(report.connections_label(), "warehouse, shop db");
        let mut many = report.clone();
        many.add_panel(
            ConnectionRef {
                id: None,
                name: "third".into(),
            },
            "SELECT 3",
            SessionContext::default(),
        )
        .unwrap();
        assert_eq!(many.connections_label(), "3 connections");
        assert_eq!(report.panels[0].source_label(), "warehouse");
        assert_eq!(report.panels[1].source_label(), "shop db · shop");
        let mut same = report.clone();
        assert!(same.same_definition(&report));
        same.panels[1].width = Width::Full;
        assert!(!same.same_definition(&report));
        let mut moved = report.clone();
        moved.panels[1].connection = warehouse();
        assert!(!moved.same_definition(&report));
    }
}
