//! The log of what a query tab ran, shown in Messages.
//!
//! Result tabs are replaced by later runs; the log is not. Each statement
//! that ends (done, failed or cancelled) adds an entry with when it ended,
//! the statement on one line, its outcome and how long it took, so the
//! outcome of an `UPDATE` survives the tab that showed it. The log holds the
//! last [`LOG_LIMIT`] entries.
use std::collections::VecDeque;
use std::time::SystemTime;

/// Entries kept per query tab; older ones are dropped.
pub const LOG_LIMIT: usize = 500;

/// Characters of a statement kept in its entry.
const SUMMARY_CHARS: usize = 200;

/// How a statement ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogOutcome {
    /// Rows read on the first page; `more` when the result had more rows.
    Rows {
        rows: usize,
        more: bool,
    },
    Affected(u64),
    Completed,
    /// The error's title and the first line of its message.
    Failed(String),
    Cancelled,
}

impl LogOutcome {
    /// How a statement that completed ended: the rows it returned (with
    /// `more` when the result had more than the first page), else the rows
    /// it affected.
    pub fn completed(columns: usize, rows: usize, more: bool, affected: Option<u64>) -> Self {
        match affected {
            _ if columns > 0 => Self::Rows { rows, more },
            Some(affected) => Self::Affected(affected),
            None => Self::Completed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub at: SystemTime,
    /// The statement on one line, shortened.
    pub sql: String,
    pub outcome: LogOutcome,
    pub elapsed_ms: Option<u128>,
}

/// The log of one query tab.
#[derive(Debug, Default)]
pub struct RunLog {
    entries: VecDeque<LogEntry>,
}

impl RunLog {
    /// Adds the outcome of `sql`, dropping the oldest entry past the limit.
    pub fn push(&mut self, sql: &str, outcome: LogOutcome, elapsed_ms: Option<u128>) {
        if self.entries.len() == LOG_LIMIT {
            self.entries.pop_front();
        }
        self.entries.push_back(LogEntry {
            at: SystemTime::now(),
            sql: summary(sql),
            outcome,
            elapsed_ms,
        });
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry at `index`, newest first.
    pub fn newest(&self, index: usize) -> Option<&LogEntry> {
        let last = self.entries.len().checked_sub(1)?;
        self.entries.get(last.checked_sub(index)?)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// `sql` on one line, with runs of whitespace collapsed and long statements
/// cut with an ellipsis.
pub fn summary(sql: &str) -> String {
    let mut text = String::new();
    for (count, word) in sql.split_whitespace().enumerate() {
        if count > 0 {
            text.push(' ');
        }
        text.push_str(word);
        if text.chars().count() > SUMMARY_CHARS {
            break;
        }
    }
    match text.char_indices().nth(SUMMARY_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_survive_in_order_up_to_the_limit() {
        let mut log = RunLog::default();
        assert!(log.is_empty() && log.newest(0).is_none());
        log.push("UPDATE t\n  SET a = 1", LogOutcome::Affected(5), Some(12));
        log.push(
            "SELECT 1",
            LogOutcome::Rows {
                rows: 1,
                more: false,
            },
            Some(3),
        );
        assert_eq!(log.len(), 2);
        assert_eq!(log.newest(0).unwrap().sql, "SELECT 1");
        let update = log.newest(1).unwrap();
        assert_eq!(update.sql, "UPDATE t SET a = 1");
        assert_eq!(update.outcome, LogOutcome::Affected(5));
        assert!(log.newest(2).is_none());
        for index in 0..LOG_LIMIT {
            log.push(&format!("SELECT {index}"), LogOutcome::Completed, None);
        }
        assert_eq!(log.len(), LOG_LIMIT);
        assert_eq!(log.newest(LOG_LIMIT - 1).unwrap().sql, "SELECT 0");
        log.clear();
        assert!(log.is_empty());
    }

    #[test]
    fn outcomes_of_completed_statements() {
        assert_eq!(
            LogOutcome::completed(2, 10, true, None),
            LogOutcome::Rows {
                rows: 10,
                more: true
            }
        );
        assert_eq!(
            LogOutcome::completed(0, 0, false, Some(5)),
            LogOutcome::Affected(5)
        );
        assert_eq!(
            LogOutcome::completed(0, 0, false, None),
            LogOutcome::Completed
        );
    }

    #[test]
    fn long_statements_are_cut_on_one_line() {
        let long = format!("SELECT '{}'", "é".repeat(500));
        let text = summary(&long);
        assert_eq!(text.chars().count(), SUMMARY_CHARS + 1);
        assert!(text.ends_with('…') && !text.contains('\n'));
        assert_eq!(summary("  a\n\tb  "), "a b");
    }
}
