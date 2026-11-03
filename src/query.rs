//! The state of a query run, independent of the view that shows it.
//!
//! A [`QueryRun`] owns one value of [`RunState`] plus the status of each
//! statement in the editor. The footer, the gutter and the buttons all derive
//! from it, so they cannot disagree. Every transition takes the [`RunId`] that
//! [`QueryRun::start`] returned, so the outcome of a superseded run is
//! discarded instead of overwriting a newer one.
use crate::drivers::{DriverError, ErrorCode};
use crate::{sql, tr};
use std::ops::Range;
use std::time::Instant;

/// Which statements a run executes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Every statement in the editor, in order (Ctrl+Shift+Enter).
    All,
    /// The statement at this index (its run button).
    Statement(usize),
    /// The statement the cursor at this byte offset picks
    /// ([`sql::statement_at`]; Ctrl+Enter without a selection).
    Cursor(usize),
    /// Exactly the selected text, split into statements; a fragment that is
    /// not a whole statement runs as written (Ctrl+Enter with a selection).
    Selection(Range<usize>),
}

/// What the gutter shows next to one statement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatementStatus {
    #[default]
    Ready,
    /// Part of the active run, waiting for the statements before it.
    Pending,
    Running,
    Success,
    Error,
    Cancelled,
}

/// How the last run is going or how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    /// Nothing has run yet, or the connection changed.
    Idle,
    Running {
        started_at: Instant,
    },
    /// Cancellation was requested; the driver is still stopping.
    Cancelling,
    Done,
    /// The run stopped at an error. The message is ready to show.
    Failed {
        error: String,
    },
    Cancelled,
}

impl RunState {
    /// Running or still stopping.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running { .. } | Self::Cancelling)
    }
}

/// Identity of one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RunId(u64);

impl RunId {
    /// A number for logs and diagnostics; it grows with every run of a tab.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// The statements a run will execute, as `(index in the editor, SQL)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub id: RunId,
    pub statements: Vec<(usize, String)>,
    /// Byte offset in the editor where each statement's text starts, to mark
    /// the position of an error there.
    pub starts: Vec<usize>,
}

/// How the backend finished a run.
#[derive(Debug)]
pub enum Outcome {
    /// The first `completed` statements finished in order; `error`, if any,
    /// stopped the next one. Completed statements were not rolled back.
    Batch {
        completed: usize,
        error: Option<DriverError>,
    },
    /// The run failed as a whole (connection, cancellation, a task failure).
    Failed(DriverError),
}

/// A tab's run state and the status of each statement in its editor.
#[derive(Debug)]
pub struct QueryRun {
    state: RunState,
    statuses: Vec<StatementStatus>,
    /// Normalised text of each statement when its status was recorded. A
    /// status survives edits only while its statement is unchanged.
    status_sql: Vec<String>,
    /// The editor's text as last seen.
    sql: String,
    current: RunId,
    planned: Vec<usize>,
    executions: usize,
}

impl QueryRun {
    pub fn new(sql: &str) -> Self {
        let status_sql = sql::normalized_statements(sql);
        Self {
            state: RunState::Idle,
            statuses: vec![StatementStatus::Ready; status_sql.len()],
            status_sql,
            sql: sql.to_owned(),
            current: RunId(0),
            planned: Vec::new(),
            executions: 0,
        }
    }

    pub fn state(&self) -> &RunState {
        &self.state
    }

    /// The identity of the last run started.
    pub fn current_id(&self) -> RunId {
        self.current
    }

    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running { .. })
    }

    /// Status of the statement at `index` in the editor.
    pub fn status(&self, index: usize) -> StatementStatus {
        self.statuses.get(index).copied().unwrap_or_default()
    }

    pub fn statuses(&self) -> &[StatementStatus] {
        &self.statuses
    }

    /// Statements that finished successfully in this tab's lifetime.
    pub fn executions(&self) -> usize {
        self.executions
    }

    /// Records the editor's text. Statements whose text changed (ignoring
    /// layout) go back to `Ready`; while a run is active, statuses wait until
    /// it ends.
    pub fn edit(&mut self, sql: &str) {
        if self.sql != sql {
            self.sql = sql.to_owned();
        }
        if !self.is_active() {
            self.sync();
        }
    }

    /// Starts a run of the statements of `sql` that `scope` picks. Returns
    /// `None` when there is nothing to run or a run is still active: one
    /// that is `Cancelling` may still change data on the server, so the next
    /// run waits until its outcome arrives.
    pub fn start(&mut self, sql: &str, scope: Scope) -> Option<Plan> {
        if self.is_active() {
            return None;
        }
        let ranges = sql::statements(sql);
        let whole = |index: usize| -> Option<(usize, String)> {
            Some((index, sql.get(ranges.get(index)?.clone())?.to_owned()))
        };
        let whole_at = |index: usize| -> Option<((usize, String), usize)> {
            Some((whole(index)?, ranges.get(index)?.start))
        };
        let (statements, starts): (Vec<_>, Vec<_>) = match scope {
            Scope::All => (0..ranges.len()).filter_map(whole_at).unzip(),
            Scope::Statement(index) => whole_at(index).into_iter().unzip(),
            Scope::Cursor(cursor) => sql::statement_at(sql, cursor)
                .and_then(whole_at)
                .into_iter()
                .unzip(),
            Scope::Selection(selection) => {
                let text = sql.get(selection.clone()).unwrap_or_default();
                sql::statements(text)
                    .into_iter()
                    .map(|range| {
                        // The gutter shows the outcome on the editor statement
                        // the selected one starts in.
                        let start = selection.start + range.start;
                        let index = ranges
                            .iter()
                            .position(|statement| statement.end > start)
                            .unwrap_or(ranges.len().saturating_sub(1));
                        ((index, text[range].to_owned()), start)
                    })
                    .unzip()
            }
        };
        if statements.is_empty() {
            return None;
        }
        self.sql = sql.to_owned();
        self.sync();
        self.statuses.resize(ranges.len(), StatementStatus::Ready);
        self.current = RunId(self.current.0 + 1);
        self.planned = statements.iter().map(|(index, _)| *index).collect();
        self.state = RunState::Running {
            started_at: Instant::now(),
        };
        Some(Plan {
            id: self.current,
            statements,
            starts,
        })
    }

    /// Marks one planned statement as running. Returns `false` when the run
    /// is no longer current or is stopping; a stopping run then ends as
    /// `Cancelled`.
    pub fn begin_statement(&mut self, id: RunId, index: usize) -> bool {
        if !self.accepts(id) {
            return false;
        }
        self.set(index, StatementStatus::Running);
        true
    }

    /// Marks every planned statement as waiting, for runs executed as one
    /// batch; [`QueryRun::statement_started`] then follows the server.
    pub fn queue_all(&mut self, id: RunId) -> bool {
        if !self.accepts(id) {
            return false;
        }
        for index in self.planned.clone() {
            self.set(index, StatementStatus::Pending);
        }
        true
    }

    /// The statement at `position` in the plan was sent to the server: it
    /// shows as running, and the ones before it, which finished, as done.
    /// Ignored for a run that is no longer current or is stopping.
    pub fn statement_started(&mut self, id: RunId, position: usize) {
        if id != self.current || !self.is_running() {
            return;
        }
        for (at, index) in self.planned.clone().into_iter().enumerate() {
            match at.cmp(&position) {
                std::cmp::Ordering::Less => self.set(index, StatementStatus::Success),
                std::cmp::Ordering::Equal => self.set(index, StatementStatus::Running),
                std::cmp::Ordering::Greater => {}
            }
        }
    }

    /// Requests cancellation of a running run. The run is `Cancelling` until
    /// its outcome arrives, and statements in progress show as cancelled.
    pub fn cancel(&mut self) -> bool {
        if !self.is_running() {
            return false;
        }
        self.state = RunState::Cancelling;
        for status in &mut self.statuses {
            if matches!(*status, StatementStatus::Running | StatementStatus::Pending) {
                *status = StatementStatus::Cancelled;
            }
        }
        true
    }

    /// Ends an active run at once, for example when the tab's connection
    /// changes: its outcome, when it arrives, is discarded.
    pub fn abandon(&mut self) {
        if self.is_active() {
            self.current = RunId(self.current.0 + 1);
            self.settle_cancelled();
        }
    }

    /// Records the outcome of a statement run on its own (the demo runs them
    /// one at a time). Returns `true` when the result should be shown.
    pub fn finish_statement(&mut self, id: RunId, index: usize, error: Option<&str>) -> bool {
        if !self.accepts(id) {
            return false;
        }
        match error {
            None => {
                self.set(index, StatementStatus::Success);
                self.executions += 1;
                true
            }
            Some(error) => {
                self.set(index, StatementStatus::Error);
                // The statements after it never run.
                for later in std::mem::take(&mut self.planned) {
                    if self.status(later) == StatementStatus::Pending {
                        self.set(later, StatementStatus::Ready);
                    }
                }
                self.state = RunState::Failed {
                    error: tr!("Error in query {0}: {1}", index + 1, error),
                };
                self.sync();
                false
            }
        }
    }

    /// Ends a run whose statements were recorded one by one.
    pub fn complete(&mut self, id: RunId) {
        if self.accepts(id) {
            self.state = RunState::Done;
            self.sync();
        }
    }

    /// Records the outcome of a batch run. Returns `true` when the completed
    /// results belong to the current run and should be shown; `false` for a
    /// superseded or cancelled run, whose results are discarded.
    pub fn finish(&mut self, id: RunId, outcome: Outcome) -> bool {
        if !self.accepts(id) {
            return false;
        }
        let planned = std::mem::take(&mut self.planned);
        let accepted = match outcome {
            Outcome::Batch { completed, error } => {
                for (position, index) in planned.iter().enumerate() {
                    let status = match &error {
                        _ if position < completed => StatementStatus::Success,
                        Some(_) if position == completed => StatementStatus::Error,
                        _ => StatementStatus::Ready,
                    };
                    self.set(*index, status);
                }
                self.executions += completed.min(planned.len());
                self.state = match error {
                    None => RunState::Done,
                    Some(error) if completed == 0 => RunState::Failed {
                        error: error.to_string(),
                    },
                    Some(error) => RunState::Failed {
                        error: tr!(
                            "{0} statement(s) completed before the failure: {1}. Previous statements were not rolled back automatically.",
                            completed,
                            error
                        ),
                    },
                };
                true
            }
            Outcome::Failed(error) if error.code == ErrorCode::Cancelled => {
                for index in &planned {
                    self.set(*index, StatementStatus::Cancelled);
                }
                self.state = RunState::Cancelled;
                false
            }
            Outcome::Failed(error) => {
                for index in &planned {
                    self.set(*index, StatementStatus::Error);
                }
                self.state = RunState::Failed {
                    error: error.to_string(),
                };
                false
            }
        };
        self.sync();
        accepted
    }

    /// Back to `Idle`, for a tab whose connection changed. An active run is
    /// abandoned first.
    pub fn reset(&mut self) {
        self.abandon();
        self.state = RunState::Idle;
    }

    /// Whether `id` may still change the state. A stopping run that reports
    /// back ends here as `Cancelled`.
    fn accepts(&mut self, id: RunId) -> bool {
        if id != self.current {
            return false;
        }
        match self.state {
            RunState::Running { .. } => true,
            RunState::Cancelling => {
                self.settle_cancelled();
                false
            }
            _ => false,
        }
    }

    fn settle_cancelled(&mut self) {
        for index in std::mem::take(&mut self.planned) {
            if matches!(
                self.status(index),
                StatementStatus::Pending | StatementStatus::Running | StatementStatus::Cancelled
            ) {
                self.set(index, StatementStatus::Cancelled);
            }
        }
        self.state = RunState::Cancelled;
        self.sync();
    }

    fn set(&mut self, index: usize, status: StatementStatus) {
        if let Some(slot) = self.statuses.get_mut(index) {
            *slot = status;
        }
    }

    fn sync(&mut self) {
        let statements = sql::normalized_statements(&self.sql);
        self.statuses = carry_statuses(&self.status_sql, &self.statuses, &statements);
        self.status_sql = statements;
    }
}

/// The status each statement should show after an edit: it is kept only when
/// the statement at the same position still has the same text.
fn carry_statuses(
    old_sql: &[String],
    old_statuses: &[StatementStatus],
    statements: &[String],
) -> Vec<StatementStatus> {
    statements
        .iter()
        .enumerate()
        .map(|(index, sql)| {
            if old_sql.get(index) == Some(sql) {
                old_statuses.get(index).copied().unwrap_or_default()
            } else {
                StatementStatus::Ready
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use StatementStatus::{Cancelled, Error, Pending, Ready, Running, Success};

    const TWO: &str = "SELECT 1;\nSELECT 2;";

    fn started(sql: &str) -> (QueryRun, Plan) {
        let mut run = QueryRun::new(sql);
        let plan = run.start(sql, Scope::All).expect("statements to run");
        assert!(run.queue_all(plan.id));
        (run, plan)
    }

    #[test]
    fn a_new_tab_is_idle_with_every_statement_ready() {
        let run = QueryRun::new(TWO);
        assert_eq!(run.state(), &RunState::Idle);
        assert_eq!(run.statuses(), [Ready, Ready]);
        assert!(!run.is_active());
    }

    #[test]
    fn nothing_to_run_starts_nothing() {
        let mut run = QueryRun::new("-- only a comment");
        assert_eq!(run.start("-- only a comment", Scope::All), None);
        assert_eq!(run.state(), &RunState::Idle);
        assert_eq!(run.start(TWO, Scope::Statement(5)), None);
    }

    #[test]
    fn the_cursor_and_a_selection_pick_what_runs() {
        let sql = "SELECT * FROM users; SELECT 1;\n\nSELECT * FROM teste";
        let planned = |scope: Scope| {
            let mut run = QueryRun::new(sql);
            run.start(sql, scope).map(|plan| plan.statements)
        };
        let cursor = |marker: &str| Scope::Cursor(sql.find(marker).unwrap() + marker.len());
        assert_eq!(
            planned(cursor("users")),
            Some(vec![(0, "SELECT * FROM users;".to_owned())])
        );
        assert_eq!(
            planned(cursor("SELECT 1;")),
            Some(vec![(1, "SELECT 1;".to_owned())])
        );
        assert_eq!(
            planned(Scope::Cursor(sql.find("\n\n").unwrap() + 1)),
            Some(vec![(2, "SELECT * FROM teste".to_owned())])
        );
        let select = |text: &str| {
            let start = sql.find(text).unwrap();
            Scope::Selection(start..start + text.len())
        };
        assert_eq!(
            planned(select("SELECT 1")),
            Some(vec![(1, "SELECT 1".to_owned())])
        );
        let starts = |scope: Scope| QueryRun::new(sql).start(sql, scope).map(|plan| plan.starts);
        assert_eq!(starts(Scope::All), Some(vec![0, 21, 32]));
        assert_eq!(starts(select("1")), Some(vec![sql.find("1;").unwrap()]));
        assert_eq!(
            planned(select("users; SELECT 1;")),
            Some(vec![(0, "users;".to_owned()), (1, "SELECT 1;".to_owned())])
        );
        // A fragment runs as selected, even when it is not valid SQL.
        assert_eq!(
            planned(select("* FROM tes")),
            Some(vec![(2, "* FROM tes".to_owned())])
        );
        // Running everything ignores the cursor and any selection.
        assert_eq!(planned(Scope::All).map(|plan| plan.len()), Some(3));
        // Blank or comment-only selections and empty editors run nothing.
        assert_eq!(planned(select("\n\n")), None);
        assert_eq!(planned(Scope::Selection(1_000..1_010)), None);
        let mut empty = QueryRun::new("-- nothing");
        assert_eq!(empty.start("-- nothing", Scope::Cursor(0)), None);
    }

    #[test]
    fn a_run_plans_all_statements_or_only_one() {
        let mut run = QueryRun::new(TWO);
        let plan = run.start(TWO, Scope::Statement(1)).expect("one statement");
        assert_eq!(plan.statements, [(1, "SELECT 2;".to_owned())]);
        assert!(matches!(run.state(), RunState::Running { .. }));
        // A running run is not restarted.
        assert_eq!(run.start(TWO, Scope::All), None);
        assert!(run.queue_all(plan.id));
        assert_eq!(run.statuses(), [Ready, Pending]);
        run.statement_started(plan.id, 0);
        assert_eq!(run.statuses(), [Ready, Running]);
    }

    #[test]
    fn statements_wait_then_run_one_at_a_time() {
        let sql = "SELECT 1; SELECT 2; SELECT 3;";
        let (mut run, plan) = started(sql);
        assert_eq!(run.statuses(), [Pending, Pending, Pending]);
        run.statement_started(plan.id, 0);
        assert_eq!(run.statuses(), [Running, Pending, Pending]);
        run.statement_started(plan.id, 1);
        assert_eq!(run.statuses(), [Success, Running, Pending]);
        // A stale run's progress changes nothing.
        let stale = RunId(plan.id.0 - 1);
        run.statement_started(stale, 2);
        assert_eq!(run.statuses(), [Success, Running, Pending]);
        // Stopping cancels the running and the waiting statements, and
        // late progress no longer moves them.
        assert!(run.cancel());
        assert_eq!(run.statuses(), [Success, Cancelled, Cancelled]);
        run.statement_started(plan.id, 2);
        assert_eq!(run.statuses(), [Success, Cancelled, Cancelled]);
        assert_eq!(run.state(), &RunState::Cancelling);
        // The failure of the second leaves the third ready.
        let (mut run, plan) = started(sql);
        run.statement_started(plan.id, 1);
        let outcome = Outcome::Batch {
            completed: 1,
            error: Some(DriverError::new(ErrorCode::Database, "boom")),
        };
        assert!(run.finish(plan.id, outcome));
        assert_eq!(run.statuses(), [Success, Error, Ready]);
    }

    #[test]
    fn a_successful_batch_is_done() {
        let (mut run, plan) = started(TWO);
        assert_eq!(run.statuses(), [Pending, Pending]);
        let outcome = Outcome::Batch {
            completed: 2,
            error: None,
        };
        assert!(run.finish(plan.id, outcome));
        assert_eq!(run.state(), &RunState::Done);
        assert_eq!(run.statuses(), [Success, Success]);
        assert_eq!(run.executions(), 2);
    }

    #[test]
    fn a_failure_after_completed_statements_says_they_were_not_rolled_back() {
        let (mut run, plan) = started("SELECT 1; SELECT x; SELECT 3;");
        let outcome = Outcome::Batch {
            completed: 1,
            error: Some(DriverError::new(ErrorCode::Database, "no column x")),
        };
        assert!(run.finish(plan.id, outcome));
        assert_eq!(run.statuses(), [Success, Error, Ready]);
        let error = match run.state() {
            RunState::Failed { error } => error.as_str(),
            _ => "",
        };
        assert!(error.contains("1 statement(s) completed"), "{error}");
        assert!(error.contains("not rolled back"), "{error}");
    }

    #[test]
    fn a_failure_on_the_first_statement_shows_its_error() {
        let (mut run, plan) = started(TWO);
        let outcome = Outcome::Batch {
            completed: 0,
            error: Some(DriverError::new(ErrorCode::Database, "syntax error")),
        };
        assert!(run.finish(plan.id, outcome));
        assert_eq!(run.statuses(), [Error, Ready]);
        assert!(matches!(run.state(), RunState::Failed { error } if error == "syntax error"));
    }

    #[test]
    fn a_failed_run_marks_its_statements_as_errors() {
        let (mut run, plan) = started(TWO);
        let error = DriverError::new(ErrorCode::Connection, "refused");
        assert!(!run.finish(plan.id, Outcome::Failed(error)));
        assert_eq!(run.statuses(), [Error, Error]);
        assert!(matches!(run.state(), RunState::Failed { .. }));
    }

    #[test]
    fn cancelling_waits_for_the_driver_and_discards_its_results() {
        let (mut run, plan) = started(TWO);
        assert!(run.cancel());
        assert_eq!(run.state(), &RunState::Cancelling);
        assert!(run.is_active() && !run.is_running());
        assert_eq!(run.statuses(), [Cancelled, Cancelled]);
        // Cancelling twice changes nothing.
        assert!(!run.cancel());
        // Results that arrive after the request are not shown.
        let outcome = Outcome::Batch {
            completed: 2,
            error: None,
        };
        assert!(!run.finish(plan.id, outcome));
        assert_eq!(run.state(), &RunState::Cancelled);
        assert_eq!(run.statuses(), [Cancelled, Cancelled]);
    }

    #[test]
    fn a_driver_cancellation_ends_as_cancelled() {
        let (mut run, plan) = started(TWO);
        assert!(!run.finish(plan.id, Outcome::Failed(DriverError::cancelled())));
        assert_eq!(run.state(), &RunState::Cancelled);
        assert_eq!(run.statuses(), [Cancelled, Cancelled]);
    }

    #[test]
    fn a_new_run_waits_until_the_stopping_one_ends() {
        let (mut run, old) = started(TWO);
        run.cancel();
        // The stopping run may still change data: nothing new starts.
        assert_eq!(run.start(TWO, Scope::Statement(0)), None);
        assert_eq!(run.start(TWO, Scope::All), None);
        assert_eq!(run.state(), &RunState::Cancelling);
        // Its outcome ends it, and then the next run starts.
        let late = Outcome::Failed(DriverError::new(ErrorCode::Connection, "late"));
        assert!(!run.finish(old.id, late));
        assert_eq!(run.state(), &RunState::Cancelled);
        let new = run.start(TWO, Scope::Statement(0)).expect("a new run");
        assert_ne!(old.id, new.id);
        assert!(run.queue_all(new.id));
        assert_eq!(run.statuses(), [Pending, Cancelled]);
        let outcome = Outcome::Batch {
            completed: 1,
            error: None,
        };
        assert!(run.finish(new.id, outcome));
        assert_eq!(run.statuses(), [Success, Cancelled]);
    }

    #[test]
    fn an_abandoned_run_is_cancelled_and_its_outcome_ignored() {
        let (mut run, plan) = started(TWO);
        run.abandon();
        assert_eq!(run.state(), &RunState::Cancelled);
        assert_eq!(run.statuses(), [Cancelled, Cancelled]);
        let outcome = Outcome::Batch {
            completed: 2,
            error: None,
        };
        assert!(!run.finish(plan.id, outcome));
        assert_eq!(run.state(), &RunState::Cancelled);
        run.reset();
        assert_eq!(run.state(), &RunState::Idle);
    }

    #[test]
    fn statements_recorded_one_by_one_end_as_done_or_failed() {
        let mut run = QueryRun::new(TWO);
        let plan = run.start(TWO, Scope::All).expect("statements");
        assert!(run.begin_statement(plan.id, 0));
        assert_eq!(run.statuses(), [Running, Ready]);
        assert!(run.finish_statement(plan.id, 0, None));
        assert!(run.begin_statement(plan.id, 1));
        assert!(!run.finish_statement(plan.id, 1, Some("boom")));
        assert_eq!(run.statuses(), [Success, Error]);
        assert!(matches!(run.state(), RunState::Failed { error } if error.contains("query 2")));
        // A failed run takes no further steps.
        assert!(!run.begin_statement(plan.id, 1));

        let plan = run.start(TWO, Scope::All).expect("statements");
        assert!(run.begin_statement(plan.id, 0));
        run.cancel();
        // The next step finds the run stopping and ends it.
        assert!(!run.finish_statement(plan.id, 0, None));
        assert_eq!(run.state(), &RunState::Cancelled);
        let plan = run.start(TWO, Scope::Statement(0)).expect("statement");
        run.complete(plan.id);
        assert_eq!(run.state(), &RunState::Done);
    }

    #[test]
    fn edits_keep_the_status_of_unchanged_statements() {
        let (mut run, plan) = started(TWO);
        let outcome = Outcome::Batch {
            completed: 1,
            error: Some(DriverError::new(ErrorCode::Database, "x")),
        };
        run.finish(plan.id, outcome);
        assert_eq!(run.statuses(), [Success, Error]);
        // A new line after the last statement changes nothing.
        run.edit("SELECT 1;\nSELECT 2;\n");
        assert_eq!(run.statuses(), [Success, Error]);
        // Re-indenting keeps the statement; editing it resets only that one.
        run.edit("SELECT\n  1;\nSELECT 3;");
        assert_eq!(run.statuses(), [Success, Ready]);
        // A new statement starts ready; removed ones disappear.
        run.edit("SELECT 1;\nSELECT 3;\nSELECT 4;");
        assert_eq!(run.statuses(), [Success, Ready, Ready]);
        run.edit("");
        assert!(run.statuses().is_empty());
    }

    #[test]
    fn edits_during_a_run_apply_when_it_ends() {
        let (mut run, plan) = started(TWO);
        run.statement_started(plan.id, 1);
        run.edit("SELECT 1;\nSELECT 20;");
        assert_eq!(run.statuses(), [Success, Running]);
        let outcome = Outcome::Batch {
            completed: 2,
            error: None,
        };
        run.finish(plan.id, outcome);
        assert_eq!(run.statuses(), [Success, Ready]);
    }
}
