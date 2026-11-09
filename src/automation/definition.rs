//! Job definitions as canonical JSON (specification, sections 4.1, 6.2,
//! 9.1.2, 9.3.2 and 9.3.3): a SQL Job, an external program or a chain.
//!
//! Reading tolerates missing fields and keeps the ones it does not know, at
//! the top level, in each parameter and in each chain step, so a version that
//! writes a definition back loses nothing another version stored. A
//! definition whose `schema_version` has another major is refused.
//!
//! A definition holds no input value: a parameter's default is part of the
//! definition, as in reports, and a chain's constants are written in it.
use crate::reports::{ParameterSpec, unknown, with_extra};
use jykre_core::automation::{JobId, JobKind, parameter_kind_str, parse_parameter_kind};
use jykre_core::{ConnectionId, ParameterKind, SessionContext, is_parameter_name};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

/// The format written; a reader refuses another major.
pub const SCHEMA_VERSION: &str = "1.0";
const SCHEMA_MAJOR: &str = "1";
/// A job's deadline when it sets none (`ExecutionOptions::default`).
pub const DEFAULT_TIMEOUT_MS: u64 = 300_000;
/// A chain's deadline when it sets none (section 9.3.3).
pub const DEFAULT_CHAIN_TIMEOUT_MS: u64 = 3_600_000;
/// Rows a SQL Job keeps when it sets no limit.
pub const DEFAULT_MAX_ROWS: u32 = 1_000;
/// The fewest and the most steps of a chain.
pub const MIN_STEPS: usize = 2;
pub const MAX_STEPS: usize = 16;
/// The most outputs a job declares.
pub const MAX_OUTPUTS: usize = 32;
/// The longest name or description kept.
const MAX_NAME: usize = 200;

/// A saved job: identity, name, and what its kind runs.
#[derive(Clone, Debug, PartialEq)]
pub struct JobDefinition {
    pub id: JobId,
    pub name: String,
    pub description: String,
    pub body: JobBody,
    extra: Map<String, Value>,
}

/// What a job runs.
#[derive(Clone, Debug, PartialEq)]
pub enum JobBody {
    Sql(SqlBody),
    Process(ProcessBody),
    Chain(ChainBody),
}

/// One statement with `:name` inputs on a connection and a pinned context.
#[derive(Clone, Debug, PartialEq)]
pub struct SqlBody {
    /// The profile's identity, never its name; `None` until chosen.
    pub connection: Option<ConnectionId>,
    pub context: SessionContext,
    pub sql: String,
    pub parameters: Vec<ParameterSpec>,
    pub timeout_ms: u64,
    pub max_rows: u32,
    /// What a chain may read, each from a result column by position.
    pub outputs: Vec<SqlOutput>,
    /// How the outputs are taken from zero, one or several rows.
    pub row: RowRule,
    /// Store the outputs' values under ADR-0035.
    pub persist_outputs: bool,
}

/// A declared output of a SQL Job: a result column by its 1-based position
/// (names may repeat in a result, so a name would be ambiguous).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlOutput {
    pub name: String,
    pub kind: ParameterKind,
    pub column: u32,
}

/// The rule for every output of a SQL Job (section 9.3.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RowRule {
    /// Zero or several rows fail with cause `Output`.
    #[default]
    ExactlyOne,
    /// Zero rows give NULL outputs; several fail.
    AtMostOne,
    /// Zero rows give NULL outputs; several give the first.
    First,
}

impl RowRule {
    pub const ALL: [RowRule; 3] = [RowRule::ExactlyOne, RowRule::AtMostOne, RowRule::First];

    pub fn as_str(self) -> &'static str {
        match self {
            RowRule::ExactlyOne => "exactly_one",
            RowRule::AtMostOne => "at_most_one",
            RowRule::First => "first",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|rule| rule.as_str() == text)
    }
}

/// How a program talks to Studio (section 9.1.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// Fixed arguments, no inputs and no outputs; success is exit 0.
    #[default]
    None,
    /// `jjp/1`: inputs through `hello`, outputs through `complete`.
    Jjp1,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::None => "none",
            Protocol::Jjp1 => "jjp/1",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "none" => Some(Protocol::None),
            "jjp/1" => Some(Protocol::Jjp1),
            _ => None,
        }
    }
}

/// What a program's environment loses and gains beyond Studio's own
/// (section 9.1.3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvironmentSpec {
    pub unset: Vec<String>,
    /// Constants, never secrets: stored in the definition and shown.
    pub set: Vec<(String, String)>,
}

/// A declared output of a `jjp/1` program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSpec {
    pub name: String,
    pub kind: ParameterKind,
}

/// The file a published version of a program pins (Q-16): its path and
/// SHA-256 when it was published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedFile {
    pub path: String,
    /// `sha256:` followed by 64 lowercase hex digits.
    pub sha256: String,
}

/// An external program started without a shell.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessBody {
    /// An absolute path, or a name looked up on `PATH` when the run starts.
    pub program: String,
    pub arguments: Vec<String>,
    /// The file a version pins; the program itself when absent.
    pub entrypoint: Option<String>,
    /// Defaults to the entrypoint's folder.
    pub working_directory: Option<String>,
    pub environment: EnvironmentSpec,
    pub protocol: Protocol,
    /// Only under `jjp/1`.
    pub inputs: Vec<ParameterSpec>,
    /// Only under `jjp/1`.
    pub outputs: Vec<OutputSpec>,
    pub timeout_ms: u64,
    pub persist_outputs: bool,
    /// Set in a published version only.
    pub pinned: Option<PinnedFile>,
}

/// A linear chain of published versions.
#[derive(Clone, Debug, PartialEq)]
pub struct ChainBody {
    pub inputs: Vec<ParameterSpec>,
    pub steps: Vec<ChainStep>,
    pub timeout_ms: u64,
}

/// One step: a published version of a SQL Job or a program, and where each
/// of its inputs comes from.
#[derive(Clone, Debug, PartialEq)]
pub struct ChainStep {
    /// Unique in the chain: lowercase letters, digits and `_`.
    pub id: String,
    pub job: JobId,
    pub version: i64,
    /// In the order written; each input of the version at most once.
    pub inputs: Vec<(String, Source)>,
    extra: Map<String, Value>,
}

impl ChainStep {
    pub fn new(id: impl Into<String>, job: JobId, version: i64) -> Self {
        Self {
            id: id.into(),
            job,
            version,
            inputs: Vec::new(),
            extra: Map::new(),
        }
    }

    /// The source bound to `input`, if any.
    pub fn source(&self, input: &str) -> Option<&Source> {
        self.inputs
            .iter()
            .find(|(name, _)| name == input)
            .map(|(_, source)| source)
    }
}

/// Where a step's input comes from (section 9.3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// An input of the chain, from its form.
    Input(String),
    /// A declared output of an earlier step.
    Step { step: String, output: String },
    /// A constant written in the definition.
    Value(String),
}

/// Why a stored definition cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefinitionError {
    /// Not JSON, or not an object.
    Malformed,
    /// Written in a format this version does not read.
    UnsupportedVersion(String),
    /// No valid `id`.
    MissingId,
    /// No known `kind`.
    UnknownKind,
}

impl std::fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DefinitionError::Malformed => f.write_str("The job definition is not valid JSON"),
            DefinitionError::UnsupportedVersion(version) => {
                write!(f, "The job definition's format {version} is not supported")
            }
            DefinitionError::MissingId => f.write_str("The job definition has no identity"),
            DefinitionError::UnknownKind => f.write_str("The job definition's kind is unknown"),
        }
    }
}

impl std::error::Error for DefinitionError {}

impl JobDefinition {
    /// A new SQL Job with the defaults of a tab.
    pub fn new_sql(name: impl Into<String>, connection: Option<ConnectionId>) -> Self {
        Self::new(
            name,
            JobBody::Sql(SqlBody {
                connection,
                context: SessionContext::default(),
                sql: String::new(),
                parameters: Vec::new(),
                timeout_ms: DEFAULT_TIMEOUT_MS,
                max_rows: DEFAULT_MAX_ROWS,
                outputs: Vec::new(),
                row: RowRule::default(),
                persist_outputs: false,
            }),
        )
    }

    /// A new external program without a protocol.
    pub fn new_process(name: impl Into<String>) -> Self {
        Self::new(
            name,
            JobBody::Process(ProcessBody {
                program: String::new(),
                arguments: Vec::new(),
                entrypoint: None,
                working_directory: None,
                environment: EnvironmentSpec::default(),
                protocol: Protocol::None,
                inputs: Vec::new(),
                outputs: Vec::new(),
                timeout_ms: DEFAULT_TIMEOUT_MS,
                persist_outputs: false,
                pinned: None,
            }),
        )
    }

    /// A new chain without steps.
    pub fn new_chain(name: impl Into<String>) -> Self {
        Self::new(
            name,
            JobBody::Chain(ChainBody {
                inputs: Vec::new(),
                steps: Vec::new(),
                timeout_ms: DEFAULT_CHAIN_TIMEOUT_MS,
            }),
        )
    }

    fn new(name: impl Into<String>, body: JobBody) -> Self {
        Self {
            id: JobId::new(),
            name: name.into(),
            description: String::new(),
            body,
            extra: Map::new(),
        }
    }

    pub fn kind(&self) -> JobKind {
        match self.body {
            JobBody::Sql(_) => JobKind::Sql,
            JobBody::Process(_) => JobKind::Process,
            JobBody::Chain(_) => JobKind::Chain,
        }
    }

    /// The inputs the job's form asks for. A `none` program has none.
    pub fn inputs(&self) -> &[ParameterSpec] {
        match &self.body {
            JobBody::Sql(sql) => &sql.parameters,
            JobBody::Process(process) if process.protocol == Protocol::Jjp1 => &process.inputs,
            JobBody::Process(_) => &[],
            JobBody::Chain(chain) => &chain.inputs,
        }
    }

    /// The outputs a chain may read, by name and kind. A chain itself
    /// declares none; a `none` program has none.
    pub fn outputs(&self) -> Vec<OutputSpec> {
        match &self.body {
            JobBody::Sql(sql) => sql
                .outputs
                .iter()
                .map(|output| OutputSpec {
                    name: output.name.clone(),
                    kind: output.kind,
                })
                .collect(),
            JobBody::Process(process) if process.protocol == Protocol::Jjp1 => {
                process.outputs.clone()
            }
            JobBody::Process(_) | JobBody::Chain(_) => Vec::new(),
        }
    }

    /// Whether the job stores its outputs' values (ADR-0035).
    pub fn persists_outputs(&self) -> bool {
        match &self.body {
            JobBody::Sql(sql) => sql.persist_outputs && !sql.outputs.is_empty(),
            JobBody::Process(process) => {
                process.persist_outputs
                    && process.protocol == Protocol::Jjp1
                    && !process.outputs.is_empty()
            }
            JobBody::Chain(_) => false,
        }
    }

    /// The job's own deadline.
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(match &self.body {
            JobBody::Sql(sql) => sql.timeout_ms,
            JobBody::Process(process) => process.timeout_ms,
            JobBody::Chain(chain) => chain.timeout_ms,
        })
    }

    /// Reads a stored definition.
    pub fn from_json(text: &str) -> Result<Self, DefinitionError> {
        let value: Value = serde_json::from_str(text).map_err(|_| DefinitionError::Malformed)?;
        Self::from_value(&value)
    }

    pub fn from_value(value: &Value) -> Result<Self, DefinitionError> {
        let object = value.as_object().ok_or(DefinitionError::Malformed)?;
        let version = object
            .get("schema_version")
            .and_then(Value::as_str)
            .unwrap_or(SCHEMA_VERSION);
        if version.split('.').next() != Some(SCHEMA_MAJOR) {
            return Err(DefinitionError::UnsupportedVersion(version.to_owned()));
        }
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| id.parse().ok())
            .ok_or(DefinitionError::MissingId)?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .and_then(JobKind::parse)
            .ok_or(DefinitionError::UnknownKind)?;
        let mut known = vec!["schema_version", "id", "kind", "name", "description"];
        let body = match kind {
            JobKind::Sql => {
                known.extend([
                    "connection",
                    "context",
                    "sql",
                    "parameters",
                    "timeout_ms",
                    "max_rows",
                    "outputs",
                    "row",
                    "persist_outputs",
                ]);
                JobBody::Sql(SqlBody {
                    connection: object
                        .get("connection")
                        .and_then(Value::as_str)
                        .and_then(|id| id.parse().ok()),
                    context: context_from(object.get("context")),
                    sql: text_of(object.get("sql")),
                    parameters: specs_from(object.get("parameters")),
                    timeout_ms: timeout_from(object.get("timeout_ms"), DEFAULT_TIMEOUT_MS),
                    max_rows: object
                        .get("max_rows")
                        .and_then(Value::as_u64)
                        .and_then(|rows| u32::try_from(rows).ok())
                        .filter(|rows| *rows > 0)
                        .unwrap_or(DEFAULT_MAX_ROWS),
                    outputs: object
                        .get("outputs")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|output| {
                            let name = output.get("name")?.as_str()?.to_owned();
                            Some(SqlOutput {
                                name,
                                kind: kind_of(output.get("kind")),
                                column: output
                                    .get("column")
                                    .and_then(Value::as_u64)
                                    .and_then(|column| u32::try_from(column).ok())?,
                            })
                        })
                        .take(MAX_OUTPUTS)
                        .collect(),
                    row: object
                        .get("row")
                        .and_then(Value::as_str)
                        .and_then(RowRule::parse)
                        .unwrap_or_default(),
                    persist_outputs: bool_of(object.get("persist_outputs")),
                })
            }
            JobKind::Process => {
                known.extend([
                    "program",
                    "arguments",
                    "entrypoint",
                    "working_directory",
                    "environment",
                    "protocol",
                    "inputs",
                    "outputs",
                    "timeout_ms",
                    "persist_outputs",
                    "pinned",
                ]);
                let environment = object.get("environment");
                JobBody::Process(ProcessBody {
                    program: text_of(object.get("program")),
                    arguments: strings_of(object.get("arguments")),
                    entrypoint: optional_text(object.get("entrypoint")),
                    working_directory: optional_text(object.get("working_directory")),
                    environment: EnvironmentSpec {
                        unset: strings_of(environment.and_then(|value| value.get("unset"))),
                        set: environment
                            .and_then(|value| value.get("set"))
                            .and_then(Value::as_object)
                            .into_iter()
                            .flatten()
                            .filter_map(|(name, value)| {
                                Some((name.clone(), value.as_str()?.to_owned()))
                            })
                            .collect(),
                    },
                    protocol: object
                        .get("protocol")
                        .and_then(Value::as_str)
                        .and_then(Protocol::parse)
                        .unwrap_or_default(),
                    inputs: specs_from(object.get("inputs")),
                    outputs: object
                        .get("outputs")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|output| {
                            Some(OutputSpec {
                                name: output.get("name")?.as_str()?.to_owned(),
                                kind: kind_of(output.get("kind")),
                            })
                        })
                        .take(MAX_OUTPUTS)
                        .collect(),
                    timeout_ms: timeout_from(object.get("timeout_ms"), DEFAULT_TIMEOUT_MS),
                    persist_outputs: bool_of(object.get("persist_outputs")),
                    pinned: object.get("pinned").and_then(|pinned| {
                        Some(PinnedFile {
                            path: pinned.get("path")?.as_str()?.to_owned(),
                            sha256: pinned.get("sha256")?.as_str()?.to_owned(),
                        })
                    }),
                })
            }
            JobKind::Chain => {
                known.extend(["inputs", "steps", "timeout_ms"]);
                JobBody::Chain(ChainBody {
                    inputs: specs_from(object.get("inputs")),
                    steps: object
                        .get("steps")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(step_from)
                        .take(MAX_STEPS * 2)
                        .collect(),
                    timeout_ms: timeout_from(object.get("timeout_ms"), DEFAULT_CHAIN_TIMEOUT_MS),
                })
            }
        };
        Ok(Self {
            id,
            name: truncated(&text_of(object.get("name"))),
            description: text_of(object.get("description")),
            body,
            extra: unknown(object, &known),
        })
    }

    /// The stored value, unknown fields included.
    pub fn to_value(&self) -> Value {
        let mut known = json!({
            "schema_version": SCHEMA_VERSION,
            "id": self.id.to_string(),
            "kind": self.kind().as_str(),
            "name": self.name,
            "description": self.description,
        });
        let fields = match &self.body {
            JobBody::Sql(sql) => json!({
                "connection": sql.connection.map(|id| id.to_string()),
                "context": {"catalog": sql.context.catalog, "schema": sql.context.schema},
                "sql": sql.sql,
                "parameters": sql.parameters.iter().map(ParameterSpec::to_value).collect::<Vec<_>>(),
                "timeout_ms": sql.timeout_ms,
                "max_rows": sql.max_rows,
                "outputs": sql.outputs.iter().map(|output| json!({
                    "name": output.name,
                    "kind": parameter_kind_str(output.kind),
                    "column": output.column,
                })).collect::<Vec<_>>(),
                "row": sql.row.as_str(),
                "persist_outputs": sql.persist_outputs,
            }),
            JobBody::Process(process) => json!({
                "program": process.program,
                "arguments": process.arguments,
                "entrypoint": process.entrypoint,
                "working_directory": process.working_directory,
                "environment": {
                    "unset": process.environment.unset,
                    "set": process.environment.set.iter()
                        .map(|(name, value)| (name.clone(), Value::String(value.clone())))
                        .collect::<Map<String, Value>>(),
                },
                "protocol": process.protocol.as_str(),
                "inputs": process.inputs.iter().map(ParameterSpec::to_value).collect::<Vec<_>>(),
                "outputs": process.outputs.iter().map(|output| json!({
                    "name": output.name,
                    "kind": parameter_kind_str(output.kind),
                })).collect::<Vec<_>>(),
                "timeout_ms": process.timeout_ms,
                "persist_outputs": process.persist_outputs,
                "pinned": process.pinned.as_ref().map(|pinned| json!({
                    "path": pinned.path,
                    "sha256": pinned.sha256,
                })),
            }),
            JobBody::Chain(chain) => json!({
                "inputs": chain.inputs.iter().map(ParameterSpec::to_value).collect::<Vec<_>>(),
                "steps": chain.steps.iter().map(step_to_value).collect::<Vec<_>>(),
                "timeout_ms": chain.timeout_ms,
            }),
        };
        if let (Value::Object(known), Value::Object(fields)) = (&mut known, fields) {
            known.extend(fields);
        }
        with_extra(&self.extra, known)
    }

    /// Canonical JSON: object keys sorted at every level, no whitespace.
    /// Two equal definitions give the same text, so fingerprints match.
    pub fn to_json(&self) -> String {
        canonical_json(&self.to_value())
    }

    /// A copy with a new identity and `name`.
    pub fn duplicate(&self, name: impl Into<String>) -> Self {
        let mut copy = self.clone();
        copy.id = JobId::new();
        copy.name = name.into();
        if let JobBody::Process(process) = &mut copy.body {
            process.pinned = None;
        }
        copy
    }

    /// Whether both define the same work, ignoring identity.
    pub fn same_definition(&self, other: &JobDefinition) -> bool {
        let mut other = other.clone();
        other.id = self.id;
        self.to_json() == other.to_json()
    }
}

/// `value` with object keys sorted at every level, written without
/// whitespace.
pub fn canonical_json(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let mut keys: Vec<&String> = object.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|key| (key.clone(), sorted(&object[key])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

fn text_of(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn optional_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

fn bool_of(value: Option<&Value>) -> bool {
    value.and_then(Value::as_bool).unwrap_or(false)
}

fn strings_of(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_owned))
        .collect()
}

fn kind_of(value: Option<&Value>) -> ParameterKind {
    value
        .and_then(Value::as_str)
        .and_then(parse_parameter_kind)
        .unwrap_or(ParameterKind::Text)
}

fn timeout_from(value: Option<&Value>, default: u64) -> u64 {
    value
        .and_then(Value::as_u64)
        .filter(|timeout| *timeout > 0)
        .unwrap_or(default)
}

fn truncated(text: &str) -> String {
    text.chars().take(MAX_NAME).collect()
}

fn context_from(value: Option<&Value>) -> SessionContext {
    let field = |name: &str| {
        value
            .and_then(|value| value.get(name))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    SessionContext {
        catalog: field("catalog"),
        schema: field("schema"),
    }
}

fn specs_from(value: Option<&Value>) -> Vec<ParameterSpec> {
    let mut seen = BTreeSet::new();
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(ParameterSpec::from_value)
        .filter(|spec| seen.insert(spec.name.clone()))
        .collect()
}

fn step_from(value: &Value) -> Option<ChainStep> {
    let object = value.as_object()?;
    let inputs = object
        .get("inputs")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, source)| {
            let source = if let Some(input) = source.get("from_input").and_then(Value::as_str) {
                Source::Input(input.to_owned())
            } else if let (Some(step), Some(output)) = (
                source.get("from_step").and_then(Value::as_str),
                source.get("from_output").and_then(Value::as_str),
            ) {
                Source::Step {
                    step: step.to_owned(),
                    output: output.to_owned(),
                }
            } else {
                Source::Value(source.get("value")?.as_str()?.to_owned())
            };
            Some((name.clone(), source))
        })
        .collect();
    Some(ChainStep {
        id: object.get("id")?.as_str()?.to_owned(),
        job: object.get("job")?.as_str()?.parse().ok()?,
        version: object.get("version")?.as_i64()?,
        inputs,
        extra: unknown(object, &["id", "job", "version", "inputs"]),
    })
}

fn step_to_value(step: &ChainStep) -> Value {
    let inputs: Map<String, Value> = step
        .inputs
        .iter()
        .map(|(name, source)| {
            let source = match source {
                Source::Input(input) => json!({"from_input": input}),
                Source::Step { step, output } => json!({"from_step": step, "from_output": output}),
                Source::Value(value) => json!({"value": value}),
            };
            (name.clone(), source)
        })
        .collect();
    with_extra(
        &step.extra,
        json!({
            "id": step.id,
            "job": step.job.to_string(),
            "version": step.version,
            "inputs": inputs,
        }),
    )
}

// ---------------------------------------------------------------------------
// Checks when saving.

/// What stops a definition from being saved or run, found without I/O.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The name is empty.
    NoName,
    /// A SQL Job without a connection.
    NoConnection,
    /// A SQL Job without SQL.
    NoSql,
    /// A program without a program.
    NoProgram,
    /// `.bat` or `.cmd`, which Windows runs through `cmd.exe`.
    ShellScript,
    /// A timeout of zero.
    NoTimeout,
    /// An output's name is not a valid name, or repeats one.
    OutputName(String),
    /// A SQL output's column is not a position (0).
    OutputColumn(String),
    /// Inputs or outputs on a program without `jjp/1` (Q-25).
    NeedsProtocol,
    /// An environment variable name that is empty or holds `=` or NUL.
    EnvironmentName(String),
    /// A chain with fewer than [`MIN_STEPS`] or more than [`MAX_STEPS`] steps.
    StepCount(usize),
    /// A step's ID is empty, not lowercase letters, digits and `_`, or
    /// repeats one.
    StepId(String),
    /// A version below 1.
    StepVersion(String),
}

/// Checks `definition` as it stands, without looking at other jobs.
pub fn problems(definition: &JobDefinition) -> Vec<Problem> {
    let mut found = Vec::new();
    if definition.name.trim().is_empty() {
        found.push(Problem::NoName);
    }
    if definition.timeout().is_zero() {
        found.push(Problem::NoTimeout);
    }
    let check_outputs = |names: Vec<&str>, found: &mut Vec<Problem>| {
        let mut seen = BTreeSet::new();
        for name in names {
            if !is_parameter_name(name) || !seen.insert(name) {
                found.push(Problem::OutputName(name.to_owned()));
            }
        }
    };
    match &definition.body {
        JobBody::Sql(sql) => {
            if sql.connection.is_none() {
                found.push(Problem::NoConnection);
            }
            if sql.sql.trim().is_empty() {
                found.push(Problem::NoSql);
            }
            check_outputs(
                sql.outputs
                    .iter()
                    .map(|output| output.name.as_str())
                    .collect(),
                &mut found,
            );
            for output in &sql.outputs {
                if output.column == 0 {
                    found.push(Problem::OutputColumn(output.name.clone()));
                }
            }
        }
        JobBody::Process(process) => {
            if process.program.trim().is_empty() {
                found.push(Problem::NoProgram);
            } else if is_shell_script(&process.program) {
                found.push(Problem::ShellScript);
            }
            if process.protocol == Protocol::None
                && (!process.inputs.is_empty() || !process.outputs.is_empty())
            {
                found.push(Problem::NeedsProtocol);
            }
            check_outputs(
                process
                    .outputs
                    .iter()
                    .map(|output| output.name.as_str())
                    .collect(),
                &mut found,
            );
            for name in process
                .environment
                .unset
                .iter()
                .chain(process.environment.set.iter().map(|(name, _)| name))
            {
                if name.is_empty() || name.contains(['=', '\0']) {
                    found.push(Problem::EnvironmentName(name.clone()));
                }
            }
        }
        JobBody::Chain(chain) => {
            if !(MIN_STEPS..=MAX_STEPS).contains(&chain.steps.len()) {
                found.push(Problem::StepCount(chain.steps.len()));
            }
            let mut seen = BTreeSet::new();
            for step in &chain.steps {
                if !is_step_id(&step.id) || !seen.insert(step.id.as_str()) {
                    found.push(Problem::StepId(step.id.clone()));
                }
                if step.version < 1 {
                    found.push(Problem::StepVersion(step.id.clone()));
                }
            }
        }
    }
    found
}

/// Whether `program` names a `.bat` or `.cmd` file, as Windows reads the
/// name: it drops trailing dots and spaces (`deploy.bat.` opens
/// `deploy.bat`, and Rust's standard library then starts it through
/// `cmd.exe`), and `::$DATA` names a file's own contents.
pub fn is_shell_script(program: &str) -> bool {
    let mut name = program.trim().to_ascii_lowercase();
    loop {
        let trimmed = name.trim_end_matches(['.', ' ']);
        let trimmed = trimmed.strip_suffix("::$data").unwrap_or(trimmed);
        if trimmed.len() == name.len() {
            break;
        }
        name.truncate(trimmed.len());
    }
    name.ends_with(".bat") || name.ends_with(".cmd")
}

/// Lowercase ASCII letters, digits and `_`, at most 64.
pub fn is_step_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

// ---------------------------------------------------------------------------
// A chain against the versions it names (section 9.3.3).

/// What is wrong with one step of a chain, against the versions it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainProblem {
    /// The job or the version does not exist.
    UnknownVersion { step: String },
    /// A step names a chain.
    NestedChain { step: String },
    /// A required input without a source or a default.
    Unbound { step: String, input: String },
    /// A source bound to a name the version has no input for.
    UnknownInput { step: String, input: String },
    /// `from_input` names an input the chain does not have.
    UnknownChainInput {
        step: String,
        input: String,
        source: String,
    },
    /// `from_step` names a step that is not earlier (or does not exist).
    ForwardReference {
        step: String,
        input: String,
        source: String,
    },
    /// `from_output` names an output the earlier step's version does not
    /// declare.
    UnknownOutput {
        step: String,
        input: String,
        source: String,
        output: String,
    },
    /// The source's kind is not the target's (only a `Text` target takes
    /// any kind).
    KindMismatch { step: String, input: String },
    /// A constant that is not a value of the target's kind, or not one of
    /// its choices.
    InvalidConstant { step: String, input: String },
}

impl ChainProblem {
    /// The step the problem is in.
    pub fn step(&self) -> &str {
        match self {
            ChainProblem::UnknownVersion { step }
            | ChainProblem::NestedChain { step }
            | ChainProblem::Unbound { step, .. }
            | ChainProblem::UnknownInput { step, .. }
            | ChainProblem::UnknownChainInput { step, .. }
            | ChainProblem::ForwardReference { step, .. }
            | ChainProblem::UnknownOutput { step, .. }
            | ChainProblem::KindMismatch { step, .. }
            | ChainProblem::InvalidConstant { step, .. } => step,
        }
    }
}

/// Checks each step of `chain` against the version it names, given by
/// `version(job, version)`: unknown versions, nested chains, unbound
/// required inputs, sources that name nothing or a later step, and kinds
/// that do not match. Reported by step and input, in order.
pub fn chain_problems<'a>(
    chain: &ChainBody,
    version: impl Fn(JobId, i64) -> Option<&'a JobDefinition>,
) -> Vec<ChainProblem> {
    let mut found = Vec::new();
    let chain_inputs: HashMap<&str, &ParameterSpec> = chain
        .inputs
        .iter()
        .map(|spec| (spec.name.as_str(), spec))
        .collect();
    // The outputs of the steps before the current one.
    let mut earlier: HashMap<&str, Vec<OutputSpec>> = HashMap::new();
    for step in &chain.steps {
        let Some(definition) = version(step.job, step.version) else {
            found.push(ChainProblem::UnknownVersion {
                step: step.id.clone(),
            });
            earlier.insert(&step.id, Vec::new());
            continue;
        };
        if definition.kind() == JobKind::Chain {
            found.push(ChainProblem::NestedChain {
                step: step.id.clone(),
            });
            earlier.insert(&step.id, Vec::new());
            continue;
        }
        let targets = definition.inputs();
        for (name, _) in &step.inputs {
            if !targets.iter().any(|target| &target.name == name) {
                found.push(ChainProblem::UnknownInput {
                    step: step.id.clone(),
                    input: name.clone(),
                });
            }
        }
        for target in targets {
            let input = target.name.clone();
            let Some(source) = step.source(&target.name) else {
                if target.required && target.default.as_deref().is_none_or(str::is_empty) {
                    found.push(ChainProblem::Unbound {
                        step: step.id.clone(),
                        input,
                    });
                }
                continue;
            };
            let kind = match source {
                Source::Input(name) => match chain_inputs.get(name.as_str()) {
                    Some(spec) => Some(spec.kind),
                    None => {
                        found.push(ChainProblem::UnknownChainInput {
                            step: step.id.clone(),
                            input,
                            source: name.clone(),
                        });
                        continue;
                    }
                },
                Source::Step { step: from, output } => match earlier.get(from.as_str()) {
                    None => {
                        found.push(ChainProblem::ForwardReference {
                            step: step.id.clone(),
                            input,
                            source: from.clone(),
                        });
                        continue;
                    }
                    Some(outputs) => match outputs.iter().find(|spec| &spec.name == output) {
                        Some(spec) => Some(spec.kind),
                        None => {
                            found.push(ChainProblem::UnknownOutput {
                                step: step.id.clone(),
                                input,
                                source: from.clone(),
                                output: output.clone(),
                            });
                            continue;
                        }
                    },
                },
                Source::Value(value) => {
                    if crate::reports::params::value(target, value).is_err() {
                        found.push(ChainProblem::InvalidConstant {
                            step: step.id.clone(),
                            input: input.clone(),
                        });
                    }
                    None
                }
            };
            if let Some(kind) = kind
                && target.kind != ParameterKind::Text
                && kind != target.kind
            {
                found.push(ChainProblem::KindMismatch {
                    step: step.id.clone(),
                    input,
                });
            }
        }
        earlier.insert(&step.id, definition.outputs());
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql_job() -> JobDefinition {
        let mut job = JobDefinition::new_sql("Lookup", Some(ConnectionId::new()));
        if let JobBody::Sql(sql) = &mut job.body {
            sql.sql = "SELECT address FROM t WHERE k = :key".into();
            sql.parameters = vec![ParameterSpec::new("key")];
            sql.outputs = vec![SqlOutput {
                name: "address".into(),
                kind: ParameterKind::Text,
                column: 1,
            }];
        }
        job
    }

    fn program_job() -> JobDefinition {
        let mut job = JobDefinition::new_process("Export");
        if let JobBody::Process(process) = &mut job.body {
            process.program = "node".into();
            process.arguments = vec!["/jobs/export.js".into()];
            process.protocol = Protocol::Jjp1;
            let mut address = ParameterSpec::new("address");
            address.kind = ParameterKind::Text;
            let mut rows = ParameterSpec::new("rows");
            rows.kind = ParameterKind::Number;
            rows.required = false;
            process.inputs = vec![address, rows];
            process.outputs = vec![OutputSpec {
                name: "count".into(),
                kind: ParameterKind::Number,
            }];
        }
        job
    }

    #[test]
    fn every_kind_round_trips_and_writes_canonical_json() {
        for job in [sql_job(), program_job(), JobDefinition::new_chain("Chain")] {
            let text = job.to_json();
            let read = JobDefinition::from_json(&text).unwrap();
            assert_eq!(read, job);
            assert_eq!(read.to_json(), text);
            assert!(!text.contains(' ') || text.contains("SELECT"));
        }
        let text = sql_job().to_json();
        let keys: Vec<usize> = ["\"connection\"", "\"context\"", "\"description\"", "\"id\""]
            .iter()
            .map(|key| text.find(key).unwrap())
            .collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn unknown_fields_are_kept_and_another_major_is_refused() {
        let id = JobId::new();
        let stored = json!({
            "schema_version": "1.4", "id": id.to_string(), "kind": "chain",
            "name": "c", "future": [1, 2],
            "steps": [{"id": "a", "job": JobId::new().to_string(), "version": 1,
                       "inputs": {"x": {"value": "1"}}, "note": "kept"}],
        });
        let read = JobDefinition::from_value(&stored).unwrap();
        let written = read.to_value();
        assert_eq!(written["future"], json!([1, 2]));
        assert_eq!(written["steps"][0]["note"], json!("kept"));
        assert_eq!(written["schema_version"], json!(SCHEMA_VERSION));

        let mut newer = stored.clone();
        newer["schema_version"] = json!("2.0");
        assert_eq!(
            JobDefinition::from_value(&newer),
            Err(DefinitionError::UnsupportedVersion("2.0".into()))
        );
        assert_eq!(
            JobDefinition::from_json("[]"),
            Err(DefinitionError::Malformed)
        );
        let mut no_kind = stored;
        no_kind["kind"] = json!("workflow");
        assert_eq!(
            JobDefinition::from_value(&no_kind),
            Err(DefinitionError::UnknownKind)
        );
    }

    #[test]
    fn a_none_program_has_no_inputs_or_outputs_and_shell_scripts_are_refused() {
        let mut job = program_job();
        if let JobBody::Process(process) = &mut job.body {
            process.protocol = Protocol::None;
            process.program = "C:\\jobs\\run.CMD".into();
        }
        assert!(job.inputs().is_empty());
        assert!(job.outputs().is_empty());
        let found = problems(&job);
        assert!(found.contains(&Problem::ShellScript));
        assert!(found.contains(&Problem::NeedsProtocol));
        // Windows drops trailing dots and spaces, and `::$DATA` names the
        // file itself: the same scripts.
        for program in [
            "C:\\jobs\\deploy.bat.",
            "deploy.bat..",
            "deploy.cmd. ",
            "deploy.Bat . .",
            "C:\\jobs\\deploy.bat::$DATA",
            "deploy.cmd::$data.",
        ] {
            assert!(is_shell_script(program), "{program}");
        }
        for program in ["deploy.exe", "deploy.bat.exe", "batch", "deploy.cmdx"] {
            assert!(!is_shell_script(program), "{program}");
        }
    }

    #[test]
    fn save_checks_report_names_columns_and_steps() {
        let mut job = sql_job();
        if let JobBody::Sql(sql) = &mut job.body {
            sql.outputs.push(SqlOutput {
                name: "address".into(),
                kind: ParameterKind::Text,
                column: 0,
            });
        }
        job.name = " ".into();
        let found = problems(&job);
        assert!(found.contains(&Problem::NoName));
        assert!(found.contains(&Problem::OutputName("address".into())));
        assert!(found.contains(&Problem::OutputColumn("address".into())));

        let mut chain = JobDefinition::new_chain("c");
        if let JobBody::Chain(body) = &mut chain.body {
            body.steps = vec![ChainStep::new("Bad-Id", JobId::new(), 0)];
        }
        let found = problems(&chain);
        assert!(found.contains(&Problem::StepCount(1)));
        assert!(found.contains(&Problem::StepId("Bad-Id".into())));
        assert!(found.contains(&Problem::StepVersion("Bad-Id".into())));
    }

    #[test]
    fn chains_are_checked_against_the_versions_they_name() {
        let lookup = sql_job();
        let export = program_job();
        let nested = JobDefinition::new_chain("inner");
        let versions = [
            ((lookup.id, 3), lookup.clone()),
            ((export.id, 1), export.clone()),
            ((nested.id, 1), nested.clone()),
        ];
        let resolve = |job: JobId, version: i64| {
            versions
                .iter()
                .find(|((id, number), _)| *id == job && *number == version)
                .map(|(_, definition)| definition)
        };
        let mut key = ParameterSpec::new("external_key");
        key.kind = ParameterKind::Text;
        let mut first = ChainStep::new("lookup", lookup.id, 3);
        first
            .inputs
            .push(("key".into(), Source::Input("external_key".into())));
        let mut second = ChainStep::new("export", export.id, 1);
        second.inputs.push((
            "address".into(),
            Source::Step {
                step: "lookup".into(),
                output: "address".into(),
            },
        ));
        second
            .inputs
            .push(("rows".into(), Source::Value("12".into())));
        let good = ChainBody {
            inputs: vec![key],
            steps: vec![first.clone(), second.clone()],
            timeout_ms: DEFAULT_CHAIN_TIMEOUT_MS,
        };
        assert_eq!(chain_problems(&good, resolve), Vec::new());

        // A forward reference, an unknown output, an unbound input, a
        // constant of the wrong kind, an unknown version and a nested chain.
        let mut forward = first.clone();
        forward.inputs = vec![(
            "key".into(),
            Source::Step {
                step: "export".into(),
                output: "count".into(),
            },
        )];
        let mut wrong = second.clone();
        wrong.inputs = vec![
            (
                "address".into(),
                Source::Step {
                    step: "lookup".into(),
                    output: "missing".into(),
                },
            ),
            ("rows".into(), Source::Value("many".into())),
            ("extra".into(), Source::Value("1".into())),
        ];
        let unknown_version = ChainStep::new("gone", lookup.id, 9);
        let inner = ChainStep::new("inner", nested.id, 1);
        let bad = ChainBody {
            inputs: Vec::new(),
            steps: vec![forward, wrong, unknown_version, inner],
            timeout_ms: DEFAULT_CHAIN_TIMEOUT_MS,
        };
        let found = chain_problems(&bad, resolve);
        assert_eq!(
            found,
            vec![
                ChainProblem::ForwardReference {
                    step: "lookup".into(),
                    input: "key".into(),
                    source: "export".into()
                },
                ChainProblem::UnknownInput {
                    step: "export".into(),
                    input: "extra".into()
                },
                ChainProblem::UnknownOutput {
                    step: "export".into(),
                    input: "address".into(),
                    source: "lookup".into(),
                    output: "missing".into()
                },
                ChainProblem::InvalidConstant {
                    step: "export".into(),
                    input: "rows".into()
                },
                ChainProblem::UnknownVersion {
                    step: "gone".into()
                },
                ChainProblem::NestedChain {
                    step: "inner".into()
                },
            ]
        );

        // A number output into a date input; and a text target takes any kind.
        let mut dated = export.clone();
        if let JobBody::Process(process) = &mut dated.body {
            process.inputs[0].kind = ParameterKind::Date;
        }
        let mut count_to_text = lookup.clone();
        if let JobBody::Sql(sql) = &mut count_to_text.body {
            sql.outputs[0].kind = ParameterKind::Number;
        }
        let resolve = |job: JobId, _version: i64| {
            if job == export.id {
                Some(&dated)
            } else {
                Some(&count_to_text)
            }
        };
        let found = chain_problems(&good, resolve);
        assert_eq!(
            found,
            vec![ChainProblem::KindMismatch {
                step: "export".into(),
                input: "address".into()
            }]
        );
    }

    #[test]
    fn an_unbound_required_input_is_reported_unless_it_has_a_default() {
        let lookup = sql_job();
        let mut with_default = lookup.clone();
        if let JobBody::Sql(sql) = &mut with_default.body {
            sql.parameters[0].default = Some("k1".into());
        }
        let steps = vec![
            ChainStep::new("a", lookup.id, 1),
            ChainStep::new("b", lookup.id, 2),
        ];
        let chain = ChainBody {
            inputs: Vec::new(),
            steps,
            timeout_ms: DEFAULT_CHAIN_TIMEOUT_MS,
        };
        let found = chain_problems(&chain, |_, version| {
            Some(if version == 1 { &lookup } else { &with_default })
        });
        assert_eq!(
            found,
            vec![ChainProblem::Unbound {
                step: "a".into(),
                input: "key".into()
            }]
        );
    }
}
