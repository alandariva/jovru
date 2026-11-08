//! Measures the SQL Job completion rule (docs/plans/automation-sql-job-poc.md,
//! section 5): a write that returns more rows than the row limit, read to
//! its end. Prints the rows read and discarded, the time from the first
//! page to the confirmed end, rows per second and memory.
//!
//! Without `JYKRE_CONNECTION` it uses a new SQLite file in the temporary
//! folder. With it (a non-sensitive ConnectionConfig JSON, password in
//! `JYKRE_PASSWORD`), it creates and drops a table of its own there: use only
//! the local fixtures. `JYKRE_ROWS` sets the rows (default 100,000).
// A command-line example: printing the measurement is its output.
#![allow(clippy::print_stdout)]
use jykre::automation::definition::{JobBody, JobDefinition};
use jykre::automation::{Availability, ConnectionInput, Engine, JobRequest, RunStatus};
use jykre::drivers::*;
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rows: u64 = match std::env::var("JYKRE_ROWS") {
        Ok(rows) => rows.parse().map_err(|_| "JYKRE_ROWS must be a number")?,
        Err(_) => 100_000,
    };
    let (input, file) = match std::env::var("JYKRE_CONNECTION") {
        Ok(json) => {
            let config: ConnectionConfig =
                serde_json::from_str(&json).map_err(|_| "Invalid connection configuration JSON")?;
            let password = std::env::var("JYKRE_PASSWORD").ok().map(SecretString::from);
            (
                ResolvedConnection {
                    config,
                    credentials: Credentials { password },
                },
                None,
            )
        }
        Err(_) => {
            let path = std::env::temp_dir().join(format!(
                "jykre-automation-drain-{}.sqlite",
                ConnectionId::new()
            ));
            std::fs::write(&path, b"")?;
            let config = ConnectionConfig {
                driver: DriverKind::Sqlite,
                options: serde_json::json!({"path": path.to_string_lossy()}),
            };
            (
                ResolvedConnection {
                    config,
                    credentials: Credentials::default(),
                },
                Some(path),
            )
        }
    };
    let kind = input.config.driver;
    let driver = builtin_registry()?.get(kind)?;
    let table = format!(
        "automation_drain_{}",
        ConnectionId::new().to_string().replace('-', "")
    );
    let (setup, sql, cleanup) = statements(kind, &table, rows);
    let mut session = driver
        .open_session(&input, CancellationToken::new())
        .await?;
    for statement in &setup {
        let mut sink = Discard;
        session
            .execute(
                &QueryRequest::new(statement.as_str(), ExecutionOptions::default()),
                &mut sink,
                CancellationToken::new(),
            )
            .await?;
    }

    let data_dir = std::env::temp_dir().join(format!(
        "jykre-automation-drain-data-{}",
        ConnectionId::new()
    ));
    let engine = Engine::start(tokio::runtime::Handle::current(), data_dir.clone());
    if engine.started().await != Availability::Ready {
        return Err("automation is not available".into());
    }
    let connection = ConnectionId::new();
    let mut job = JobDefinition::new_sql("drain", Some(connection));
    if let JobBody::Sql(body) = &mut job.body {
        body.sql = sql;
    }
    let mut handle = engine
        .submit(JobRequest {
            definition: job,
            stored: None,
            values: HashMap::new(),
            connection: Some(ConnectionInput {
                id: connection,
                driver: driver.clone(),
                resolved: input.clone(),
                revision: 1,
            }),
        })
        .map_err(|refusal| format!("refused: {refusal:?}"))?;
    let RunStatus::Finished(outcome) = handle.finished().await else {
        return Err("the run ended without an outcome".into());
    };
    let result = outcome.sql.as_ref().ok_or("not a SQL run")?;
    let completion = result.completion.unwrap_or_default();
    let read = result.data.row_count() as u64 + result.rows_discarded;
    println!("driver: {kind:?}, rows: {rows}");
    println!(
        "state: {:?}, cause: {:?}, effect unknown: {}, truncated: {}",
        outcome.state, outcome.cause, outcome.effect_unknown, result.truncated
    );
    println!(
        "rows kept: {}, discarded: {}, run: {} ms",
        result.data.row_count(),
        result.rows_discarded,
        outcome.elapsed.as_millis()
    );
    println!(
        "first page to confirmed end: {} ms, {:.0} rows/s",
        completion.as_millis(),
        result.rows_discarded as f64 / completion.as_secs_f64().max(f64::EPSILON)
    );
    println!(
        "result held: {} bytes; process peak: {}",
        result.data.memory_bytes(),
        peak_memory().unwrap_or_else(|| "unknown".into())
    );
    drop(outcome);
    engine.shutdown().await;
    drop(engine);
    let _ = std::fs::remove_dir_all(&data_dir);
    for statement in &cleanup {
        let mut sink = Discard;
        session
            .execute(
                &QueryRequest::new(statement.as_str(), ExecutionOptions::default()),
                &mut sink,
                CancellationToken::new(),
            )
            .await?;
    }
    session.close().await?;
    if let Some(path) = file {
        std::fs::remove_file(path)?;
    }
    if read != rows {
        return Err(format!("read {read} rows of {rows}").into());
    }
    Ok(())
}

/// Setup, the measured statement and cleanup. MySQL has no `RETURNING`: a
/// procedure writes, then returns the rows.
fn statements(kind: DriverKind, table: &str, rows: u64) -> (Vec<String>, String, Vec<String>) {
    let create = format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, v INTEGER NOT NULL)");
    match kind {
        DriverKind::MySql => {
            let digits = "(SELECT 0 AS d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL \
                 SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL \
                 SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9)";
            let procedure = format!("{table}_p");
            (
                vec![
                    create,
                    format!(
                        "INSERT INTO {table} (id, v) SELECT n, 0 FROM (SELECT a.d + 10 * b.d \
                         + 100 * c.d + 1000 * e.d + 10000 * f.d + 100000 * g.d + 1 AS n \
                         FROM {digits} a CROSS JOIN {digits} b CROSS JOIN {digits} c \
                         CROSS JOIN {digits} e CROSS JOIN {digits} f CROSS JOIN {digits} g) \
                         AS numbers WHERE n <= {rows}"
                    ),
                    format!(
                        "CREATE PROCEDURE {procedure}() BEGIN UPDATE {table} SET v = v + 1; \
                         SELECT id, REPEAT('x', 200) AS padding FROM {table} ORDER BY id; END"
                    ),
                ],
                format!("CALL {procedure}()"),
                vec![
                    format!("DROP PROCEDURE {procedure}"),
                    format!("DROP TABLE {table}"),
                ],
            )
        }
        DriverKind::Postgres => (
            vec![
                create,
                format!(
                    "INSERT INTO {table} (id, v) SELECT n, 0 FROM generate_series(1, {rows}) AS n"
                ),
            ],
            format!("UPDATE {table} SET v = v + 1 RETURNING id, repeat('x', 200) AS padding"),
            vec![format!("DROP TABLE {table}")],
        ),
        _ => (
            vec![
                create,
                format!(
                    "INSERT INTO {table} WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL \
                     SELECT i + 1 FROM n WHERE i < {rows}) SELECT i, 0 FROM n"
                ),
            ],
            format!("UPDATE {table} SET v = v + 1 RETURNING id, printf('%.200c', 'x') AS padding"),
            vec![format!("DROP TABLE {table}")],
        ),
    }
}

/// The process's peak resident memory, on Linux.
fn peak_memory() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .map(|value| value.trim().to_owned())
}

struct Discard;
impl RowSink for Discard {
    fn columns(&mut self, _columns: Vec<ColumnInfo>) -> SinkControl {
        SinkControl::Continue
    }
    fn row(&mut self, _row: Row) -> SinkControl {
        SinkControl::Continue
    }
}
