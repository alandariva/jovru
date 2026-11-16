//! Statements a production connection asks about before running: those that
//! change or delete every row of a table, and those that drop data.
//!
//! Read from the tokenizer, never the server. The check only adds a
//! question, so it errs towards asking: a `WHERE` that a subquery holds
//! does not count, and anything it cannot read is left alone.
use crate::drivers::SqlDialect;
use jovru_sql::names::{Part, parts};

/// Why a statement deserves a second look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Risk {
    /// `UPDATE` or `DELETE` without a `WHERE` of its own: every row.
    EveryRow { verb: &'static str },
    /// `DROP`, `TRUNCATE`, or an `ALTER` that drops something.
    Drops { verb: &'static str },
}

/// The risk of one statement, if it has one.
pub fn risk(sql: &str, dialect: Option<SqlDialect>) -> Option<Risk> {
    let parts = parts(sql, dialect);
    risk_of(explained(&parts))
}

fn word(part: &Part) -> Option<&str> {
    match part {
        Part::Word(upper, _) => Some(upper.as_str()),
        _ => None,
    }
}

fn is_symbol(part: &Part, symbol: &str) -> bool {
    matches!(part, Part::Symbol(text) if text == symbol)
}

/// `EXPLAIN ANALYZE <statement>` runs the statement, so the statement is
/// what decides the risk. A plain `EXPLAIN` only plans it: nothing runs.
fn explained(parts: &[Part]) -> &[Part] {
    if parts.first().and_then(word) != Some("EXPLAIN") {
        return parts;
    }
    let mut depth = 0usize;
    let mut analyzes = false;
    for (index, part) in parts.iter().enumerate().skip(1) {
        if is_symbol(part, "(") {
            depth += 1;
        } else if is_symbol(part, ")") {
            depth = depth.saturating_sub(1);
        } else if let Some(word) = word(part) {
            if matches!(word, "ANALYZE" | "ANALYSE") {
                analyzes = true;
            } else if depth == 0 && word != "VERBOSE" {
                // The first word outside the options starts the statement.
                return if analyzes { &parts[index..] } else { &[] };
            }
        }
    }
    &[]
}

/// `UPDATE` or `DELETE` without a `WHERE` of its own inside a `WITH`
/// (PostgreSQL's data-modifying CTE): it changes every row whatever the
/// final `SELECT` reads.
fn cte_every_row(parts: &[Part]) -> Option<&'static str> {
    for (index, part) in parts.iter().enumerate() {
        if !is_symbol(part, "(") {
            continue;
        }
        let verb = match parts.get(index + 1).and_then(word) {
            Some("UPDATE") => "UPDATE",
            Some("DELETE") => "DELETE",
            _ => continue,
        };
        let mut depth = 0usize;
        let mut filtered = false;
        for inner in &parts[index..] {
            if is_symbol(inner, "(") {
                depth += 1;
            } else if is_symbol(inner, ")") {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            } else if depth == 1 && word(inner) == Some("WHERE") {
                filtered = true;
            }
        }
        if !filtered {
            return Some(verb);
        }
    }
    None
}

fn risk_of(parts: &[Part]) -> Option<Risk> {
    if parts.first().and_then(word) == Some("WITH")
        && let Some(verb) = cte_every_row(parts)
    {
        return Some(Risk::EveryRow { verb });
    }
    // A leading `WITH` names subqueries; the statement follows them.
    let mut depth = 0usize;
    let mut verb = None;
    let mut top_words = Vec::new();
    for part in parts {
        match part {
            Part::Symbol(symbol) if symbol == "(" => depth += 1,
            Part::Symbol(symbol) if symbol == ")" => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                if let Some(word) = word(part) {
                    if verb.is_none()
                        && matches!(word, "UPDATE" | "DELETE" | "DROP" | "TRUNCATE" | "ALTER")
                    {
                        verb = Some(word);
                    }
                    top_words.push(word);
                }
            }
            _ => {}
        }
        // Only the first words decide what the statement is: past them, a
        // `DELETE` would be a name or part of a clause.
        if verb.is_none()
            && depth == 0
            && top_words.len() > 1
            && !top_words.first().is_some_and(|first| *first == "WITH")
        {
            return None;
        }
    }
    let verb = verb?;
    let has_where = top_words.contains(&"WHERE");
    match verb {
        "UPDATE" if !has_where => Some(Risk::EveryRow { verb: "UPDATE" }),
        "DELETE" if !has_where => Some(Risk::EveryRow { verb: "DELETE" }),
        "DROP" => Some(Risk::Drops { verb: "DROP" }),
        "TRUNCATE" => Some(Risk::Drops { verb: "TRUNCATE" }),
        "ALTER" if top_words.contains(&"DROP") => Some(Risk::Drops {
            verb: "ALTER … DROP",
        }),
        _ => None,
    }
}

/// The statements of a run that deserve a second look, with their position
/// in the run.
pub fn risks(statements: &[String], dialect: Option<SqlDialect>) -> Vec<(usize, Risk)> {
    statements
        .iter()
        .enumerate()
        .filter_map(|(index, sql)| Some((index, risk(sql, dialect)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: Option<SqlDialect> = Some(SqlDialect::Postgres);

    #[test]
    fn changes_to_every_row_and_drops_are_risky() {
        assert_eq!(
            risk("UPDATE t SET a = 1", PG),
            Some(Risk::EveryRow { verb: "UPDATE" })
        );
        assert_eq!(
            risk("delete from t;", PG),
            Some(Risk::EveryRow { verb: "DELETE" })
        );
        assert_eq!(risk("DROP TABLE t", PG), Some(Risk::Drops { verb: "DROP" }));
        assert_eq!(
            risk("TRUNCATE t", PG),
            Some(Risk::Drops { verb: "TRUNCATE" })
        );
        assert_eq!(
            risk("ALTER TABLE t DROP COLUMN a", PG),
            Some(Risk::Drops {
                verb: "ALTER … DROP"
            })
        );
        // A WHERE inside a subquery does not filter the outer statement.
        assert_eq!(
            risk("DELETE FROM t USING (SELECT id FROM u WHERE x) s", PG),
            Some(Risk::EveryRow { verb: "DELETE" })
        );
        assert_eq!(
            risk("WITH old AS (SELECT 1) DELETE FROM t", PG),
            Some(Risk::EveryRow { verb: "DELETE" })
        );
        // A comment before the statement does not hide it.
        assert_eq!(
            risk("-- clean up\nDELETE FROM t", PG),
            Some(Risk::EveryRow { verb: "DELETE" })
        );
    }

    #[test]
    fn a_data_modifying_cte_is_read_as_the_statement_it_holds() {
        assert_eq!(
            risk(
                "WITH d AS (DELETE FROM t RETURNING *) SELECT count(*) FROM d",
                PG
            ),
            Some(Risk::EveryRow { verb: "DELETE" })
        );
        assert_eq!(
            risk(
                "WITH u AS (UPDATE t SET a = 1 RETURNING id) SELECT * FROM u",
                PG
            ),
            Some(Risk::EveryRow { verb: "UPDATE" })
        );
        // The CTE's own WHERE filters it, and a SELECT-only CTE is a read.
        assert_eq!(
            risk(
                "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM d",
                PG
            ),
            None
        );
        assert_eq!(
            risk("WITH s AS (SELECT 1) SELECT * FROM s WHERE true", PG),
            None
        );
    }

    #[test]
    fn explain_analyze_runs_the_statement_it_explains() {
        assert_eq!(
            risk("EXPLAIN ANALYZE DELETE FROM t", PG),
            Some(Risk::EveryRow { verb: "DELETE" })
        );
        assert_eq!(
            risk("explain (analyze, verbose) DROP TABLE t", PG),
            Some(Risk::Drops { verb: "DROP" })
        );
        assert_eq!(
            risk("EXPLAIN ANALYZE UPDATE t SET a = 1 WHERE id = 1", PG),
            None
        );
        // Without ANALYZE the statement is only planned.
        assert_eq!(risk("EXPLAIN DELETE FROM t", PG), None);
    }

    #[test]
    fn filtered_changes_and_reads_are_not() {
        assert_eq!(risk("UPDATE t SET a = 1 WHERE id = 2", PG), None);
        assert_eq!(risk("DELETE FROM t WHERE id IN (1, 2)", PG), None);
        assert_eq!(risk("SELECT * FROM delete_log", PG), None);
        assert_eq!(risk("SELECT 'DROP TABLE t'", PG), None);
        assert_eq!(risk("ALTER TABLE t ADD COLUMN b int", PG), None);
        assert_eq!(risk("INSERT INTO t SELECT * FROM u", PG), None);
        assert_eq!(risk("", PG), None);
        let run = ["SELECT 1".to_owned(), "DROP VIEW v".to_owned()];
        assert_eq!(risks(&run, PG), [(1, Risk::Drops { verb: "DROP" })]);
    }
}
