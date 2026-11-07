//! The one way a query's values reach the model (ADR-0028). When the model
//! asks for `run_query`, the user approves the statement, the workspace
//! runs it in the assistant's own session and hands its result here as a
//! bounded [`QueryPreview`]; the user then sends it, with the columns they
//! chose hidden, or withholds it. [`release`] is the only function that puts
//! a result's values into text for a request: no other code of the
//! assistant may name a result (`cargo xtask check`, `assistant_privacy`).
use super::facts::ErrorFacts;
use super::prompts;
use super::run::{QueryDecision, QueryRecord, ToolStep};
use crate::backend::QueryBatch;
use crate::error_report::ErrorReport;
use jykre_assistant::ToolCall;
use jykre_core::DriverError;
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// Rows a result may send.
pub const MAX_ROWS: usize = 50;
/// Characters a cell may send; longer ones are cut with `…`.
pub const MAX_CELL: usize = 200;
/// Bytes the whole answer may take.
pub const MAX_BYTES: usize = 16 * 1024;
/// Rows the query reads: one more than may be sent, to know there are more.
pub const READ_ROWS: u32 = MAX_ROWS as u32 + 1;
/// What a hidden column's cells become.
pub const HIDDEN: &str = "[hidden]";

/// A column of a preview, as the model may see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewColumn {
    pub name: String,
    pub data_type: String,
}

/// What a query left, cut to what may be sent: shown to the user for
/// approval, and nothing of it leaves until [`release`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryPreview {
    pub columns: Vec<PreviewColumn>,
    /// At most [`MAX_ROWS`], each cell at most [`MAX_CELL`] characters;
    /// `None` is NULL.
    pub rows: Vec<Vec<Option<String>>>,
    /// More rows existed than [`Self::rows`] holds.
    pub more: bool,
    /// A cell was cut at [`MAX_CELL`].
    pub clipped: bool,
    pub rows_affected: Option<u64>,
    /// The statement failed with this error.
    pub error: Option<ErrorFacts>,
    pub elapsed_ms: u64,
}

impl QueryPreview {
    /// The preview of one statement's run (`sql`, for an error's position).
    pub fn from_batch(batch: &QueryBatch, sql: &str) -> Self {
        let mut preview = Self {
            error: batch
                .error
                .as_ref()
                .map(|error| ErrorFacts::new(&ErrorReport::new(error), sql)),
            ..Self::default()
        };
        let Some(result) = batch.completed.first() else {
            return preview;
        };
        let data = &result.data;
        preview.elapsed_ms = u64::try_from(result.elapsed_ms).unwrap_or(u64::MAX);
        preview.rows_affected = result.outcome.rows_affected;
        preview.columns = data
            .columns()
            .iter()
            .map(|column| PreviewColumn {
                name: column.name.clone(),
                data_type: column.data_type.clone(),
            })
            .collect();
        let kept = data.row_count().min(MAX_ROWS);
        for row in 0..kept {
            preview.rows.push(
                data.row(row)
                    .map(|cell| {
                        cell.map(|text| {
                            let (text, cut) = clip(text, MAX_CELL);
                            preview.clipped |= cut;
                            text
                        })
                    })
                    .collect(),
            );
        }
        preview.more = data.row_count() > MAX_ROWS
            || result.has_more
            || result.outcome.has_more
            || result.outcome.truncated;
        preview
    }

    /// The preview of a run that could not start.
    pub fn from_error(error: &DriverError, sql: &str) -> Self {
        Self {
            error: Some(ErrorFacts::new(&ErrorReport::new(error), sql)),
            ..Self::default()
        }
    }

    /// The columns to hide at first: those whose name the user hid earlier
    /// in the conversation, ignoring case.
    pub fn hidden_by_name(&self, names: &BTreeSet<String>) -> BTreeSet<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, column)| names.contains(&column.name.to_lowercase()))
            .map(|(index, _)| index)
            .collect()
    }

    /// How many rows it returned, as the model may always be told.
    fn count(&self) -> String {
        if self.more {
            format!("more than {} rows", self.rows.len())
        } else {
            format!("{} rows", self.rows.len())
        }
    }
}

fn clip(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((at, _)) => (format!("{}…", &text[..at]), true),
        None => (text.to_owned(), false),
    }
}

/// A cell inside a `|` table: one line, its pipes escaped.
fn table_cell(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace("\r\n", "\\n")
        .replace(['\n', '\r'], "\\n")
}

/// The answer `run_query` gives once the user sends the result: a `|`
/// table with each column's name and type, `[hidden]` in the columns of
/// `hidden`, and notes on what was left out. At most [`MAX_BYTES`].
pub fn release(preview: &QueryPreview, hidden: &BTreeSet<usize>) -> String {
    let mut text = String::new();
    if let Some(error) = &preview.error {
        text.push_str("The query failed.\n");
        text.push_str(&prompts::error_text(error));
        return cut(text);
    }
    if preview.columns.is_empty() {
        text.push_str("The statement ran and returned no rows.");
        if let Some(affected) = preview.rows_affected {
            let _ = write!(text, " Rows affected: {affected}.");
        }
        text.push('\n');
        return text;
    }
    let header: Vec<String> = preview
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            // A computed column may have no type.
            let notes: Vec<&str> = [
                Some(column.data_type.trim()).filter(|kind| !kind.is_empty()),
                hidden.contains(&index).then_some("hidden by the user"),
            ]
            .into_iter()
            .flatten()
            .collect();
            if notes.is_empty() {
                table_cell(&column.name)
            } else {
                format!("{} ({})", table_cell(&column.name), notes.join(", "))
            }
        })
        .collect();
    let _ = writeln!(text, "| {} |", header.join(" | "));
    let _ = writeln!(text, "|{}", "---|".repeat(header.len()));
    let mut sent = 0;
    for row in &preview.rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                if hidden.contains(&index) {
                    HIDDEN.to_owned()
                } else {
                    cell.as_deref()
                        .map_or_else(|| "NULL".to_owned(), table_cell)
                }
            })
            .collect();
        let line = format!("| {} |\n", cells.join(" | "));
        // Room is kept for the notes below.
        if text.len() + line.len() > MAX_BYTES - 512 {
            break;
        }
        text.push_str(&line);
        sent += 1;
    }
    let _ = write!(text, "\nThe query returned {}", preview.count());
    if let Some(affected) = preview.rows_affected {
        let _ = write!(text, " and affected {affected} rows");
    }
    text.push_str(".\n");
    if sent < preview.rows.len() {
        let _ = writeln!(text, "Only the first {sent} rows fit in 16 KiB.");
    } else if preview.more {
        let _ = writeln!(text, "Only the first {sent} rows are shown.");
    }
    if preview.clipped {
        let _ = writeln!(
            text,
            "Cells longer than {MAX_CELL} characters were cut and end with …."
        );
    }
    if !hidden.is_empty() {
        text.push_str("The user hid the values of the columns marked hidden.\n");
    }
    cut(text)
}

fn cut(mut text: String) -> String {
    if text.len() > MAX_BYTES {
        let mut end = MAX_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n… (cut at 16 KiB)\n");
    }
    text
}

/// The answer when the statement could not be asked for: the model is
/// told why, and nothing runs.
pub fn rejected(call: ToolCall, sql: String, reason: String) -> ToolStep {
    step(call, reason, sql, QueryDecision::Rejected, Vec::new(), 0, 0)
}

/// The answer when the user declined to run the statement.
pub fn declined(call: ToolCall, sql: String) -> ToolStep {
    step(
        call,
        "The user declined to run this query.".into(),
        sql,
        QueryDecision::Declined,
        Vec::new(),
        0,
        0,
    )
}

/// The answer when the user ran the statement but kept its result: only
/// that it ran, failed, and how many rows it returned.
pub fn withheld(call: ToolCall, sql: String, preview: &QueryPreview) -> ToolStep {
    let output = if preview.error.is_some() {
        "The user ran the query; it failed, and the user chose not to share the error.".into()
    } else if preview.columns.is_empty() {
        let mut text = "The user ran the query but chose not to share its result.".to_owned();
        if let Some(affected) = preview.rows_affected {
            let _ = write!(text, " It affected {affected} rows.");
        }
        text
    } else {
        format!(
            "The user ran the query but chose not to share its result. It returned {}.",
            preview.count()
        )
    };
    step(
        call,
        output,
        sql,
        QueryDecision::Withheld,
        Vec::new(),
        0,
        preview.elapsed_ms,
    )
}

/// The answer when the user sent the result, with `hidden` columns.
pub fn sent(
    call: ToolCall,
    sql: String,
    preview: &QueryPreview,
    hidden: &BTreeSet<usize>,
) -> ToolStep {
    let output = release(preview, hidden);
    let rows = preview.rows.len().min(
        output
            .lines()
            .filter(|line| line.starts_with("| "))
            .count()
            .saturating_sub(1),
    );
    let names = hidden
        .iter()
        .filter_map(|index| preview.columns.get(*index))
        .map(|column| column.name.clone())
        .collect();
    tracing::info!(
        columns = preview.columns.len(),
        hidden = hidden.len(),
        rows,
        failed = preview.error.is_some(),
        "assistant query result sent"
    );
    step(
        call,
        output,
        sql,
        QueryDecision::Sent,
        names,
        rows,
        preview.elapsed_ms,
    )
}

fn step(
    call: ToolCall,
    output: String,
    sql: String,
    decision: QueryDecision,
    hidden: Vec<String>,
    rows_sent: usize,
    elapsed_ms: u64,
) -> ToolStep {
    if decision != QueryDecision::Sent {
        tracing::info!(decision = ?decision, "assistant query");
    }
    ToolStep {
        call,
        output,
        elapsed_ms,
        query: Some(QueryRecord {
            sql,
            decision,
            hidden,
            rows_sent,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::QueryResult;
    use crate::result::{ResultColumn, ResultData};
    use jykre_core::{DriverOutcome, ErrorCode};
    use serde_json::json;

    fn batch(rows: usize, cell: &str) -> QueryBatch {
        let data = ResultData::from_rows(
            vec![
                ResultColumn::new("id", "integer"),
                ResultColumn::new("secret", "text"),
                ResultColumn::new("note", "text"),
            ],
            (0..rows)
                .map(|row| {
                    vec![
                        Some(row.to_string()),
                        Some("synthetic-hidden-c4d9".to_owned()),
                        (row != 1).then(|| cell.to_owned()),
                    ]
                })
                .collect::<Vec<_>>()
                .iter()
                .map(|row| row.iter().map(Option::as_deref).collect::<Vec<_>>()),
        );
        QueryBatch {
            completed: vec![QueryResult {
                data,
                outcome: DriverOutcome::default(),
                elapsed_ms: 7,
                limit: None,
                has_more: false,
            }],
            error: None,
        }
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "q".into(),
            name: "run_query".into(),
            arguments: json!({"sql": "SELECT 1"}),
        }
    }

    #[test]
    fn a_hidden_column_keeps_its_name_and_type_but_never_a_value() {
        let preview = QueryPreview::from_batch(&batch(3, "a|b\nc"), "SELECT");
        let text = release(&preview, &BTreeSet::from([1]));
        assert!(
            text.contains("| id (integer) | secret (text, hidden by the user) | note (text) |")
        );
        assert!(text.contains("| 0 | [hidden] | a\\|b\\nc |"));
        assert!(text.contains("| 1 | [hidden] | NULL |"));
        assert!(!text.contains("synthetic-hidden-c4d9"));
        assert!(text.contains("returned 3 rows"));
        let shown = release(&preview, &BTreeSet::new());
        assert!(shown.contains("synthetic-hidden-c4d9"));

        let mut untyped = preview.clone();
        untyped.columns[2].data_type = String::new();
        assert!(release(&untyped, &BTreeSet::new()).contains("| note |\n"));
        assert!(
            release(&untyped, &BTreeSet::from([2])).contains("| note (hidden by the user) |\n")
        );
    }

    #[test]
    fn rows_cells_and_bytes_are_bounded_without_breaking_characters() {
        let preview = QueryPreview::from_batch(&batch(60, &"é".repeat(300)), "SELECT");
        assert_eq!(preview.rows.len(), MAX_ROWS);
        assert!(preview.more && preview.clipped);
        let cell = preview.rows[0][2].as_deref().unwrap();
        assert_eq!(cell.chars().count(), MAX_CELL + 1);
        let text = release(&preview, &BTreeSet::new());
        assert!(text.len() <= MAX_BYTES + 32, "{}", text.len());
        assert!(text.contains("more than 50 rows"));
        assert!(text.contains("fit in 16 KiB"));
        assert!(text.contains("were cut"));

        let small = QueryPreview::from_batch(&batch(60, "x"), "SELECT");
        let text = release(&small, &BTreeSet::new());
        assert!(text.contains("Only the first 50 rows are shown"));
    }

    #[test]
    fn errors_counts_and_withheld_results_say_only_what_they_may() {
        let error = DriverError::new(ErrorCode::Database, "relation \"nope\" does not exist");
        let preview = QueryPreview::from_error(&error, "SELECT * FROM nope");
        assert!(release(&preview, &BTreeSet::new()).contains("relation \"nope\" does not exist"));
        let step = withheld(call(), "SELECT * FROM nope".into(), &preview);
        assert!(!step.output.contains("nope"));

        let preview = QueryPreview::from_batch(&batch(3, "x"), "SELECT");
        let step = withheld(call(), "SELECT".into(), &preview);
        assert_eq!(
            step.output,
            "The user ran the query but chose not to share its result. It returned 3 rows."
        );
        assert_eq!(step.query.unwrap().decision, QueryDecision::Withheld);

        let mut affected = QueryBatch {
            completed: Vec::new(),
            error: None,
        };
        affected.completed.push(QueryResult {
            data: ResultData::new(Vec::new()),
            outcome: DriverOutcome {
                rows_affected: Some(4),
                ..DriverOutcome::default()
            },
            elapsed_ms: 1,
            limit: None,
            has_more: false,
        });
        let preview = QueryPreview::from_batch(&affected, "UPDATE t SET a = 1");
        assert!(release(&preview, &BTreeSet::new()).contains("Rows affected: 4"));

        let step = sent(
            call(),
            "SELECT".into(),
            &QueryPreview::from_batch(&batch(3, "x"), "SELECT"),
            &BTreeSet::from([1]),
        );
        let record = step.query.unwrap();
        assert_eq!(record.hidden, ["secret"]);
        assert_eq!(record.rows_sent, 3);
    }

    #[test]
    fn columns_hidden_earlier_start_hidden_ignoring_case() {
        let preview = QueryPreview::from_batch(&batch(1, "x"), "SELECT");
        let names = BTreeSet::from(["secret".to_owned(), "other".to_owned()]);
        assert_eq!(preview.hidden_by_name(&names), BTreeSet::from([1]));
        let mut upper = preview.clone();
        upper.columns[2].name = "NOTE".into();
        let note = BTreeSet::from(["note".to_owned()]);
        assert_eq!(upper.hidden_by_name(&note), BTreeSet::from([2]));
    }
}
