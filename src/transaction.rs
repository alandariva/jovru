//! What a query tab knows about the transaction of its session.
//!
//! After each run the driver reports what the server says
//! ([`Session::transaction`](crate::drivers::Session::transaction)), and
//! that wins ([`Transaction::reported`]). What it leaves unknown, and every
//! driver that cannot report, falls back to inference from the SQL that ran
//! and how each statement ended: `BEGIN` or `START TRANSACTION` opens a
//! transaction, `COMMIT` and `ROLLBACK` end it, MySQL's `SET autocommit = 0`
//! makes every statement wait for a commit and its DDL commits implicitly,
//! and a failure inside a PostgreSQL transaction leaves it failed until
//! `ROLLBACK`. Inference alone does not see a transaction opened inside a
//! procedure ([technical debt TD-001](../docs/technical-debt.md)).
use crate::drivers::{SqlDialect, TransactionStatus};
use crate::sql::{self, TokenKind};

/// The transaction state of a tab's session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transaction {
    /// Each statement commits on its own.
    #[default]
    AutoCommit,
    /// A transaction is open: an explicit one, or every statement since the
    /// last commit when `manual` (MySQL with `autocommit = 0`). `changes`
    /// when a statement that may write ran in it.
    Open { changes: bool, manual: bool },
    /// A statement failed inside a PostgreSQL transaction: the server
    /// ignores everything until `ROLLBACK`.
    Failed { manual: bool },
}

impl Transaction {
    /// Closing the session now would roll something back, or the user must
    /// end the transaction.
    pub fn is_open(self) -> bool {
        self != Self::AutoCommit
    }

    /// Something ran that a rollback would undo, or the transaction failed.
    pub fn has_changes(self) -> bool {
        matches!(self, Self::Open { changes: true, .. } | Self::Failed { .. })
    }

    /// The state once the session's own report is taken into account. The
    /// server's answer wins; what it leaves unknown (whether a transaction
    /// wrote, a `BEGIN` under MySQL) keeps what the statements suggested.
    /// `None` (the driver cannot tell, or was not asked) keeps the inferred
    /// state.
    pub fn reported(self, status: Option<TransactionStatus>) -> Self {
        let manual = match self {
            Self::AutoCommit => false,
            Self::Open { manual, .. } | Self::Failed { manual } => manual,
        };
        match status {
            Some(TransactionStatus::Idle) => Self::AutoCommit,
            Some(TransactionStatus::Open { changes }) => Self::Open {
                changes: changes.unwrap_or(self.has_changes()),
                manual,
            },
            Some(TransactionStatus::Failed) => Self::Failed { manual },
            Some(TransactionStatus::Manual) => match self {
                Self::Failed { .. } => self,
                _ => Self::Open {
                    changes: self.has_changes(),
                    manual: true,
                },
            },
            _ => self,
        }
    }

    /// The state after `statement` ran in the session; `ok` is `false` when
    /// it failed.
    pub fn after(self, statement: &str, ok: bool, dialect: Option<SqlDialect>) -> Self {
        let words = leading_words(statement, dialect);
        let word = |index: usize| words.get(index).map(String::as_str);
        let postgres = matches!(dialect, Some(SqlDialect::Postgres));
        let mysql = matches!(dialect, Some(SqlDialect::MySql));
        let manual = match self {
            Self::AutoCommit => false,
            Self::Open { manual, .. } | Self::Failed { manual } => manual,
        };
        // What the session is left in once a transaction ends.
        let ended = if manual {
            Self::Open {
                changes: false,
                manual,
            }
        } else {
            Self::AutoCommit
        };
        let ends = match word(0) {
            Some("COMMIT" | "ABORT") => true,
            Some("END") => postgres,
            Some("ROLLBACK") => word(1) != Some("TO") && word(2) != Some("TO"),
            Some("PREPARE") => word(1) == Some("TRANSACTION"),
            _ => false,
        };
        if ends {
            // A failed COMMIT in PostgreSQL rolls the transaction back.
            return if ok || postgres { ended } else { self };
        }
        if !ok {
            return match self {
                Self::Open { .. } if postgres => Self::Failed { manual },
                _ => self,
            };
        }
        if let Self::Failed { .. } = self {
            // Only ROLLBACK TO SAVEPOINT makes a failed transaction usable.
            return if word(0) == Some("ROLLBACK") {
                Self::Open {
                    changes: true,
                    manual,
                }
            } else {
                self
            };
        }
        let opens = match (word(0), word(1)) {
            (Some("START"), Some("TRANSACTION")) => true,
            (Some("BEGIN"), None | Some("WORK" | "TRANSACTION")) => true,
            (Some("BEGIN"), Some("ISOLATION" | "READ" | "NOT" | "DEFERRABLE")) => postgres,
            _ => false,
        };
        if opens {
            // MySQL commits the open transaction before starting another;
            // PostgreSQL only warns and keeps it.
            return match self {
                Self::Open { changes, .. } if !mysql => Self::Open { changes, manual },
                _ => Self::Open {
                    changes: false,
                    manual,
                },
            };
        }
        if mysql && word(0) == Some("SET") {
            let assignment: Vec<_> = words
                .iter()
                .skip(1)
                .filter(|word| !matches!(word.as_str(), "SESSION" | "@" | "."))
                .collect();
            if assignment
                .first()
                .is_some_and(|name| name.ends_with("AUTOCOMMIT"))
            {
                let off = assignment
                    .iter()
                    .skip(1)
                    .find(|value| value.as_str() != "=")
                    .is_some_and(|value| matches!(value.as_str(), "0" | "OFF" | "FALSE"));
                // Turning autocommit on commits what was pending.
                return if off {
                    match self {
                        Self::Open { changes, .. } => Self::Open {
                            changes,
                            manual: true,
                        },
                        _ => Self::Open {
                            changes: false,
                            manual: true,
                        },
                    }
                } else {
                    Self::AutoCommit
                };
            }
        }
        let Self::Open { changes, .. } = self else {
            return self;
        };
        if mysql && implicit_commit(word(0)) {
            return ended;
        }
        Self::Open {
            changes: changes || writes(&words),
            manual,
        }
    }
}

/// The first words of a statement, upper case, skipping comments. Symbols
/// count as words so `SET @@autocommit` and `ROLLBACK TO` read right.
fn leading_words(statement: &str, dialect: Option<SqlDialect>) -> Vec<String> {
    sql::tokens(statement, dialect)
        .into_iter()
        .filter(|(_, kind)| !matches!(kind, TokenKind::Comment | TokenKind::Executable))
        .take_while(|(range, _)| &statement[range.clone()] != ";")
        .take(8)
        .map(|(range, _)| statement[range].to_ascii_uppercase())
        .collect()
}

/// MySQL statements that commit the open transaction first.
fn implicit_commit(first: Option<&str>) -> bool {
    matches!(
        first,
        Some("CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE" | "LOCK")
    )
}

/// Whether a statement may change data. Unknown commands count as writes,
/// so the indicator errs on the side of "uncommitted changes".
fn writes(words: &[String]) -> bool {
    match words.first().map(String::as_str) {
        None => false,
        Some("WITH") => words
            .iter()
            .any(|word| matches!(word.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE")),
        Some(first) => !matches!(
            first,
            "SELECT"
                | "SHOW"
                | "EXPLAIN"
                | "DESCRIBE"
                | "DESC"
                | "VALUES"
                | "TABLE"
                | "SET"
                | "USE"
                | "SAVEPOINT"
                | "RELEASE"
                | "FETCH"
                | "MOVE"
                | "DECLARE"
                | "CLOSE"
                | "LISTEN"
                | "UNLISTEN"
                | "RESET"
                | "HELP"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::Transaction::{self, AutoCommit, Failed, Open};
    use crate::drivers::SqlDialect;

    const POSTGRES: Option<SqlDialect> = Some(SqlDialect::Postgres);
    const MYSQL: Option<SqlDialect> = Some(SqlDialect::MySql);

    fn run(steps: &[(&str, bool)], dialect: Option<SqlDialect>) -> Transaction {
        steps.iter().fold(AutoCommit, |state, (sql, ok)| {
            state.after(sql, *ok, dialect)
        })
    }

    #[test]
    fn begin_opens_and_commit_or_rollback_ends() {
        let open = run(&[("BEGIN;", true)], POSTGRES);
        assert_eq!(
            open,
            Open {
                changes: false,
                manual: false
            }
        );
        assert!(open.is_open() && !open.has_changes());
        let changed = run(&[("begin", true), ("UPDATE t SET a = 1", true)], POSTGRES);
        assert!(changed.has_changes());
        assert_eq!(changed.after("COMMIT;", true, POSTGRES), AutoCommit);
        assert_eq!(changed.after("ROLLBACK", true, POSTGRES), AutoCommit);
        assert_eq!(changed.after("END", true, POSTGRES), AutoCommit);
        // Reads do not count as changes; unknown commands do.
        let read = run(&[("START TRANSACTION", true), ("SELECT 1", true)], MYSQL);
        assert!(!read.has_changes());
        assert!(read.after("CALL p()", true, MYSQL).has_changes());
        // Outside a transaction nothing changes, and a BEGIN ATOMIC body is
        // not a transaction.
        assert_eq!(run(&[("UPDATE t SET a = 1", true)], POSTGRES), AutoCommit);
        assert_eq!(
            run(&[("-- x\nBEGIN ATOMIC SELECT 1; END", true)], POSTGRES),
            AutoCommit
        );
    }

    #[test]
    fn a_postgres_failure_leaves_the_transaction_failed() {
        let failed = run(&[("BEGIN", true), ("SELECT x", false)], POSTGRES);
        assert_eq!(failed, Failed { manual: false });
        // Everything else fails too, until ROLLBACK.
        assert_eq!(failed.after("SELECT 1", true, POSTGRES), failed);
        assert!(
            failed
                .after("ROLLBACK TO SAVEPOINT a", true, POSTGRES)
                .is_open()
        );
        assert_eq!(failed.after("ROLLBACK", true, POSTGRES), AutoCommit);
        // COMMIT of a failed transaction rolls it back.
        assert_eq!(failed.after("COMMIT", true, POSTGRES), AutoCommit);
        // MySQL keeps the transaction after a failed statement.
        let mysql = run(&[("BEGIN", true), ("SELECT x", false)], MYSQL);
        assert!(matches!(mysql, Open { .. }));
    }

    #[test]
    fn the_server_report_wins_over_inference() {
        use crate::drivers::TransactionStatus as Status;
        // A transaction the statements did not show (opened in a procedure).
        let report = AutoCommit.reported(Some(Status::Open {
            changes: Some(true),
        }));
        assert!(report.is_open() && report.has_changes());
        // A COMMIT that failed and rolled back is idle, whatever was inferred.
        let open = run(&[("BEGIN", true), ("UPDATE t SET a = 1", true)], POSTGRES);
        assert_eq!(open.reported(Some(Status::Idle)), AutoCommit);
        assert_eq!(
            open.reported(Some(Status::Failed)),
            Failed { manual: false }
        );
        // What the server leaves unknown keeps what was inferred.
        assert_eq!(open.reported(Some(Status::Open { changes: None })), open);
        assert_eq!(open.reported(None), open);
        // MySQL reports only autocommit = 0.
        assert_eq!(
            AutoCommit.reported(Some(Status::Manual)),
            Open {
                changes: false,
                manual: true
            }
        );
        let begun = run(&[("BEGIN", true)], MYSQL);
        assert_eq!(begun.reported(None), begun);
    }

    #[test]
    fn mysql_autocommit_off_and_implicit_commits() {
        let manual = run(&[("SET autocommit = 0", true)], MYSQL);
        assert_eq!(
            manual,
            Open {
                changes: false,
                manual: true
            }
        );
        let pending = manual.after("INSERT INTO t VALUES (1)", true, MYSQL);
        assert!(pending.has_changes());
        // Committing keeps the manual mode.
        assert_eq!(pending.after("COMMIT", true, MYSQL), manual);
        // DDL commits implicitly.
        assert_eq!(pending.after("CREATE TABLE x (a int)", true, MYSQL), manual);
        assert_eq!(
            run(&[("SET @@session.autocommit=1", true)], MYSQL),
            AutoCommit
        );
        assert_eq!(
            run(&[("BEGIN", true), ("DROP TABLE t", true)], MYSQL),
            AutoCommit
        );
        // In PostgreSQL DDL is transactional.
        assert!(run(&[("BEGIN", true), ("DROP TABLE t", true)], POSTGRES).has_changes());
    }
}
