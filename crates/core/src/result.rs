use crate::{DriverError, DriverResult};
use std::ops::Range;
use std::time::Duration;

/// Where unqualified names resolve in a session: the catalog (a Databricks
/// catalog, the PostgreSQL database) and the schema (a PostgreSQL or
/// Databricks schema, the MySQL database). `None` leaves that level as the
/// session has it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionContext {
    pub catalog: Option<String>,
    pub schema: Option<String>,
}

/// The transaction state of a session as the server reports it
/// ([`crate::Session::transaction`]).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    /// No transaction is open; each statement commits on its own.
    Idle,
    /// A transaction is open. `changes` tells whether it already wrote
    /// something a rollback would undo; `None` when the server does not say.
    Open { changes: Option<bool> },
    /// A statement failed inside the transaction: the server rejects every
    /// other statement until it is rolled back.
    Failed,
    /// Statements do not commit on their own (MySQL with `autocommit = 0`):
    /// each one waits for a `COMMIT`. Whether one ran since the last commit
    /// is not known.
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionOptions {
    pub max_rows: u32,
    /// None disables the timeout; Some must be greater than zero.
    pub timeout: Option<Duration>,
    /// The context the tab chose. A session applies it only when it differs
    /// from the last context it applied or reported
    /// ([`crate::Session::context`]), so a `USE` or `SET` the user ran is not
    /// undone by the next statement.
    pub context: SessionContext,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            max_rows: 1_000,
            timeout: Some(Duration::from_secs(300)),
            context: SessionContext::default(),
        }
    }
}

/// The type a parameter's value is read as (ADR-0023).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParameterKind {
    Text,
    /// An exact decimal number, such as `-12`, `3.50` or `1e6`.
    Number,
    /// `true` or `false`.
    Boolean,
    /// `YYYY-MM-DD`.
    Date,
    /// `YYYY-MM-DD HH:MM[:SS[.fraction]]`, with a space or `T`, no time zone.
    Timestamp,
}

/// A named value the driver binds: it travels apart from the SQL text and is
/// never written into it (ADR-0023). The caller finds the markers; `markers`
/// holds the byte ranges of this parameter's `:name` markers in
/// [`QueryRequest::sql`]. A driver replaces each marker with a fixed
/// reference to the bound value, or sends the SQL unchanged when the server
/// reads `:name` itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryParameter {
    pub name: String,
    pub kind: ParameterKind,
    /// The value's exact text; `None` is NULL.
    pub value: Option<String>,
    pub markers: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryRequest {
    pub sql: String,
    pub options: ExecutionOptions,
    /// Empty for SQL run as written. Drivers whose
    /// [`crate::DriverCapabilities::named_parameters`] is false reject a
    /// request that carries any.
    pub parameters: Vec<QueryParameter>,
}

impl QueryRequest {
    /// A request without parameters.
    pub fn new(sql: impl Into<String>, options: ExecutionOptions) -> Self {
        Self {
            sql: sql.into(),
            options,
            parameters: Vec::new(),
        }
    }

    /// The SQL with each parameter marker replaced by `reference(index,
    /// parameter)`, where `index` is the parameter's position in
    /// [`QueryRequest::parameters`]. Call after [`QueryRequest::validate`]:
    /// a marker out of place is left as it is.
    pub fn with_references(
        &self,
        mut reference: impl FnMut(usize, &QueryParameter) -> String,
    ) -> String {
        let mut markers: Vec<(Range<usize>, usize)> = self
            .parameters
            .iter()
            .enumerate()
            .flat_map(|(index, parameter)| {
                parameter
                    .markers
                    .iter()
                    .map(move |range| (range.clone(), index))
            })
            .collect();
        markers.sort_by_key(|(range, _)| range.start);
        let mut sql = String::with_capacity(self.sql.len());
        let mut end = 0;
        for (range, index) in markers {
            let (Some(before), Some(parameter)) =
                (self.sql.get(end..range.start), self.parameters.get(index))
            else {
                continue;
            };
            sql.push_str(before);
            sql.push_str(&reference(index, parameter));
            end = range.end;
        }
        sql.push_str(self.sql.get(end..).unwrap_or_default());
        sql
    }

    /// Local validation only; no SQL parsing or rewriting.
    pub fn validate(&self) -> DriverResult<()> {
        if self.sql.trim().is_empty() {
            return Err(DriverError::validation("SQL must not be empty"));
        }
        self.validate_parameters()?;
        if self.options.max_rows == 0 {
            return Err(DriverError::validation(
                "Row limit must be greater than zero",
            ));
        }
        if self
            .options
            .timeout
            .is_some_and(|timeout| timeout.is_zero())
        {
            return Err(DriverError::validation("Timeout must be greater than zero"));
        }
        Ok(())
    }

    /// Each parameter has a unique name, a value of its kind and at least one
    /// marker; the markers spell `:name` in the SQL and do not overlap.
    fn validate_parameters(&self) -> DriverResult<()> {
        let mut ranges = Vec::new();
        for (index, parameter) in self.parameters.iter().enumerate() {
            if !is_parameter_name(&parameter.name) {
                return Err(DriverError::validation("Invalid parameter name"));
            }
            if self
                .parameters
                .iter()
                .take(index)
                .any(|other| other.name == parameter.name)
            {
                return Err(DriverError::validation(format!(
                    "Parameter {} appears more than once",
                    parameter.name
                )));
            }
            if let Some(value) = &parameter.value
                && !is_parameter_value(parameter.kind, value)
            {
                return Err(DriverError::validation(format!(
                    "Parameter {} has an invalid value",
                    parameter.name
                )));
            }
            if parameter.markers.is_empty() {
                return Err(DriverError::validation(format!(
                    "Parameter {} has no marker in the SQL",
                    parameter.name
                )));
            }
            for range in &parameter.markers {
                let spelled = self
                    .sql
                    .get(range.clone())
                    .and_then(|marker| marker.strip_prefix(':'));
                if spelled != Some(parameter.name.as_str()) {
                    return Err(DriverError::validation(format!(
                        "Parameter {} has a marker out of place",
                        parameter.name
                    )));
                }
                ranges.push(range.clone());
            }
        }
        ranges.sort_by_key(|range| range.start);
        if ranges.windows(2).any(|pair| pair[0].end > pair[1].start) {
            return Err(DriverError::validation("Parameter markers overlap"));
        }
        Ok(())
    }
}

/// A letter or `_`, then letters, digits or `_`, at most 64 ASCII characters.
pub fn is_parameter_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 64
}

/// Whether `value` is valid text for `kind` (see [`ParameterKind`]). Only
/// the shape is checked; the server checks the rest (for example, February
/// 30th).
pub fn is_parameter_value(kind: ParameterKind, value: &str) -> bool {
    if value.contains('\0') {
        return false;
    }
    match kind {
        ParameterKind::Text => true,
        ParameterKind::Number => is_number(value),
        ParameterKind::Boolean => matches!(value, "true" | "false"),
        ParameterKind::Date => is_date(value),
        ParameterKind::Timestamp => is_timestamp(value),
    }
}

fn digits(text: &str, count: usize) -> bool {
    text.len() == count && text.bytes().all(|b| b.is_ascii_digit())
}

fn is_number(value: &str) -> bool {
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (unsigned, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let all_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = (!whole.is_empty() || !fraction.is_empty())
        && all_digits(whole)
        && all_digits(fraction)
        && !(mantissa.ends_with('.') && whole.is_empty());
    let exponent_ok = exponent.is_none_or(|exponent| {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        !digits.is_empty() && all_digits(digits)
    });
    mantissa_ok && exponent_ok
}

fn is_date(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    digits(year, 4)
        && digits(month, 2)
        && digits(day, 2)
        && (1..=12).contains(&month.parse::<u8>().unwrap_or(0))
        && (1..=31).contains(&day.parse::<u8>().unwrap_or(0))
}

fn is_timestamp(value: &str) -> bool {
    let Some((date, time)) = value.split_once([' ', 'T']) else {
        return false;
    };
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut parts = clock.split(':');
    let (Some(hour), Some(minute), seconds, None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let below = |text: &str, limit: u8| digits(text, 2) && text.parse::<u8>().unwrap_or(99) < limit;
    is_date(date)
        && below(hour, 24)
        && below(minute, 60)
        && seconds.is_none_or(|seconds| below(seconds, 60))
        && match time.split_once('.') {
            None => true,
            Some(_) => {
                seconds.is_some()
                    && !fraction.is_empty()
                    && fraction.len() <= 9
                    && fraction.bytes().all(|b| b.is_ascii_digit())
            }
        }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: Option<bool>,
}

/// Exact decimals and temporal values keep their original textual representation.
/// Null remains distinct from an empty string or the literal text "NULL".
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Boolean(bool),
    Integer(i64),
    UnsignedInteger(u64),
    Float(f64),
    Decimal(String),
    Text(String),
    Binary(Vec<u8>),
    Temporal(String),
}

pub type Row = Vec<Cell>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkControl {
    Continue,
    Stop,
}

/// A bounded synchronous receiver. Call columns before rows, and never send
/// further data after Stop. Receivers must not perform blocking I/O.
pub trait RowSink: Send {
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> SinkControl;
    fn row(&mut self, row: Row) -> SinkControl;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriverOutcome {
    pub rows_affected: Option<u64>,
    /// The row limit ended this page with rows left. They stay readable with
    /// `Session::fetch` until the session runs, pings or closes anything
    /// else, which discards them.
    pub has_more: bool,
    /// The driver itself stopped reading before the end of the result (for
    /// example, a server-side row cap), and the rows left cannot be read.
    pub truncated: bool,
}
