//! The driver against real SQLite files, created by each test in a folder of
//! its own under the system's temporary folder with synthetic data. Nothing
//! needs Docker, so these run in the basic gate.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use jykre_core::*;
use jykre_driver_sqlite::SqliteDriver;
use serde_json::json;
use sqlx::{ConnectOptions, Connection};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A folder removed with everything in it when the test ends.
struct Folder(PathBuf);

impl Folder {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "jykre-sqlite-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        // Best effort: a leftover temporary folder harms nothing.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const SEED: &str = r#"
CREATE TABLE customers (
    id INTEGER PRIMARY KEY,
    name VARCHAR(80) NOT NULL DEFAULT 'anonymous',
    balance NUMERIC(12, 2)
);
CREATE TABLE orders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    customer_id INTEGER NOT NULL REFERENCES customers ON DELETE CASCADE,
    note TEXT,
    UNIQUE (customer_id, note)
);
CREATE INDEX orders_by_note ON orders (note) WHERE note IS NOT NULL;
CREATE TRIGGER orders_touch AFTER INSERT ON orders BEGIN
    UPDATE customers SET balance = coalesce(balance, 0) + 1 WHERE id = NEW.customer_id;
END;
CREATE VIEW big_customers AS SELECT id, name FROM customers WHERE balance > 100;
INSERT INTO customers (id, name, balance) VALUES (1, 'Ana', 150.5), (2, 'Bo', NULL);
INSERT INTO orders (customer_id, note) VALUES (1, 'first'), (2, NULL);
"#;

/// A database file with [`SEED`], created outside the driver, which never
/// creates files.
async fn seeded(folder: &Folder) -> PathBuf {
    let path = folder.file("shop.sqlite");
    let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    sqlx::raw_sql(SEED).execute(&mut connection).await.unwrap();
    connection.close().await.unwrap();
    path
}

fn input(path: &Path, read_only: bool) -> ResolvedConnection {
    ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::Sqlite,
            options: json!({ "path": path.to_string_lossy(), "readOnly": read_only }),
        },
        credentials: Credentials::default(),
    }
}

#[derive(Debug, Default)]
struct Sink {
    columns: Vec<ColumnInfo>,
    rows: Vec<Row>,
    /// Stop after this many rows.
    stop_after: Option<usize>,
}

impl RowSink for Sink {
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> SinkControl {
        self.columns = columns;
        SinkControl::Continue
    }
    fn row(&mut self, row: Row) -> SinkControl {
        self.rows.push(row);
        match self.stop_after {
            Some(limit) if self.rows.len() >= limit => SinkControl::Stop,
            _ => SinkControl::Continue,
        }
    }
}

#[derive(Default)]
struct Metadata(Vec<MetadataObject>);

#[async_trait]
impl IntrospectionSink for Metadata {
    async fn batch(&mut self, objects: Vec<MetadataObject>) -> DriverResult<()> {
        self.0.extend(objects);
        Ok(())
    }
}

fn request(sql: &str) -> QueryRequest {
    QueryRequest::new(sql, ExecutionOptions::default())
}

async fn run(session: &mut Box<dyn Session>, sql: &str) -> DriverResult<(DriverOutcome, Sink)> {
    let mut sink = Sink::default();
    let outcome = session
        .execute(&request(sql), &mut sink, CancellationToken::new())
        .await?;
    Ok((outcome, sink))
}

/// A statement that keeps SQLite busy far longer than any test waits.
const ENDLESS: &str =
    "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT count(*) FROM n";

/// The temporary table set up by [`keep_state`] is still there: the session
/// kept its connection.
async fn assert_kept(session: &mut Box<dyn Session>) {
    assert!(session.is_open());
    let (_, sink) = run(session, "SELECT value FROM temp.kept").await.unwrap();
    assert_eq!(sink.rows, vec![vec![Cell::Text("jykre-kept".into())]]);
}

async fn keep_state(session: &mut Box<dyn Session>) {
    run(session, "CREATE TEMP TABLE kept (value TEXT)")
        .await
        .unwrap();
    run(session, "INSERT INTO kept VALUES ('jykre-kept')")
        .await
        .unwrap();
}

#[tokio::test]
async fn the_test_opens_only_existing_databases_and_never_creates_a_file() {
    let folder = Folder::new();
    let path = seeded(&folder).await;
    let driver = SqliteDriver;
    let info = driver
        .test(&input(&path, false), CancellationToken::new())
        .await
        .unwrap();
    assert!(info.version.is_some_and(|version| version.starts_with('3')));

    let missing = folder.file("typo.sqlite");
    let error = driver
        .test(&input(&missing, false), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
    assert!(!error.message.contains("typo"), "{error:?}");
    assert!(!missing.exists());

    let text = folder.file("notes.txt");
    std::fs::write(
        &text,
        "this is not a database, just enough text to fill a header and then some more",
    )
    .unwrap();
    let error = driver
        .test(&input(&text, false), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Database, "{error:?}");

    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        driver
            .test(&input(&path, false), cancel)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Cancelled
    );
}

#[tokio::test]
async fn values_keep_their_storage_class_and_null_is_not_text() {
    let folder = Folder::new();
    let path = seeded(&folder).await;
    let mut session = SqliteDriver
        .open_session(&input(&path, false), CancellationToken::new())
        .await
        .unwrap();
    // A file on this computer: nothing travels on a network.
    assert_eq!(
        session.encryption(CancellationToken::new()).await.unwrap(),
        Encryption::Local
    );
    let (outcome, sink) = run(
        &mut session,
        "SELECT 42 AS i, 2.5 AS r, 'NULL' AS t, NULL AS n, x'00ff' AS b, \
         CAST(x'c328' AS TEXT) AS bad, 9223372036854775807 AS big, name FROM customers WHERE id = 1",
    )
    .await
    .unwrap();
    assert_eq!(outcome.rows_affected, None);
    assert_eq!(
        sink.rows,
        vec![vec![
            Cell::Integer(42),
            Cell::Float(2.5),
            Cell::Text("NULL".into()),
            Cell::Null,
            Cell::Binary(vec![0x00, 0xff]),
            Cell::Binary(vec![0xc3, 0x28]),
            Cell::Integer(i64::MAX),
            Cell::Text("Ana".into()),
        ]]
    );
    // A declared column carries its affinity; an expression carries none.
    let types: Vec<_> = sink.columns.iter().map(|c| c.data_type.as_str()).collect();
    assert_eq!(types.last(), Some(&"TEXT"));
    assert_eq!(types.first(), Some(&""));

    let (_, empty) = run(&mut session, "SELECT id, name FROM customers WHERE 0")
        .await
        .unwrap();
    assert_eq!(empty.columns.len(), 2);
    assert!(empty.rows.is_empty());
    session.close().await.unwrap();
}

#[tokio::test]
async fn rows_affected_belong_to_the_statement_that_wrote() {
    let folder = Folder::new();
    let path = seeded(&folder).await;
    let mut session = SqliteDriver
        .open_session(&input(&path, false), CancellationToken::new())
        .await
        .unwrap();
    let (outcome, _) = run(&mut session, "UPDATE customers SET name = name || '!'")
        .await
        .unwrap();
    assert_eq!(outcome.rows_affected, Some(2));
    // SQLite's changes() still says 2 after a statement that wrote nothing.
    let (outcome, _) = run(&mut session, "CREATE TABLE scratch (a)").await.unwrap();
    assert_eq!(outcome.rows_affected, Some(0));
    let (outcome, _) = run(&mut session, "DELETE FROM customers WHERE id = 99")
        .await
        .unwrap();
    assert_eq!(outcome.rows_affected, Some(0));
    // RETURNING gives rows, not a count.
    let (outcome, sink) = run(
        &mut session,
        "INSERT INTO scratch VALUES (1), (2) RETURNING a",
    )
    .await
    .unwrap();
    assert_eq!(outcome.rows_affected, None);
    assert_eq!(sink.rows.len(), 2);
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_failed_statement_keeps_the_session_and_its_transaction() {
    let folder = Folder::new();
    let path = seeded(&folder).await;
    let mut session = SqliteDriver
        .open_session(&input(&path, false), CancellationToken::new())
        .await
        .unwrap();
    keep_state(&mut session).await;
    run(&mut session, "BEGIN").await.unwrap();
    run(
        &mut session,
        "INSERT INTO customers (id, name) VALUES (3, 'Cy')",
    )
    .await
    .unwrap();
    let error = run(&mut session, "SELECT * FROM missing_table")
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Database);
    assert!(error.message.contains("missing_table"), "{error:?}");
    assert_eq!(
        error
            .server
            .and_then(|server| server.vendor_code)
            .as_deref(),
        Some("1")
    );
    let error = run(
        &mut session,
        "INSERT INTO customers (id, name) VALUES (3, 'again')",
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Database);
    run(&mut session, "ROLLBACK").await.unwrap();
    let (_, sink) = run(&mut session, "SELECT count(*) FROM customers")
        .await
        .unwrap();
    assert_eq!(sink.rows, vec![vec![Cell::Integer(2)]]);
    assert_kept(&mut session).await;
    session.close().await.unwrap();
}

/// The transaction state the session reports.
async fn state(session: &mut Box<dyn Session>) -> Option<TransactionStatus> {
    session.transaction(CancellationToken::new()).await.unwrap()
}

#[tokio::test]
async fn the_transaction_state_is_read_from_the_connection() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let folder = Folder::new();
        let path = seeded(&folder).await;
        let mut session = SqliteDriver
            .open_session(&input(&path, false), CancellationToken::new())
            .await
            .unwrap();
        let open = Some(TransactionStatus::Open { changes: None });
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Idle));
        run(&mut session, "BEGIN IMMEDIATE").await.unwrap();
        assert_eq!(state(&mut session).await, open);
        run(&mut session, "END").await.unwrap();
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Idle));

        // A conflict with OR ROLLBACK ends the whole transaction, which the
        // statements alone would not show.
        run(&mut session, "BEGIN").await.unwrap();
        run(
            &mut session,
            "INSERT INTO customers (id, name) VALUES (3, 'Cy')",
        )
        .await
        .unwrap();
        run(
            &mut session,
            "INSERT OR ROLLBACK INTO customers (id, name) VALUES (3, 'again')",
        )
        .await
        .unwrap_err();
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Idle));

        // So does interrupting a write inside a transaction.
        run(&mut session, "BEGIN").await.unwrap();
        run(
            &mut session,
            "INSERT INTO customers (id, name) VALUES (4, 'Di')",
        )
        .await
        .unwrap();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            trigger.cancel();
        });
        let endless_write = "INSERT INTO orders (customer_id, note) \
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) \
             SELECT 1, 'n' || i FROM n";
        let error = session
            .execute(&request(endless_write), &mut Sink::default(), cancel)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Idle));
        let (_, sink) = run(&mut session, "SELECT count(*) FROM customers")
            .await
            .unwrap();
        assert_eq!(sink.rows, vec![vec![Cell::Integer(2)]]);
        session.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_read_only_connection_refuses_to_write() {
    let folder = Folder::new();
    let path = seeded(&folder).await;
    let mut session = SqliteDriver
        .open_session(&input(&path, true), CancellationToken::new())
        .await
        .unwrap();
    let error = run(&mut session, "DELETE FROM orders").await.unwrap_err();
    assert_eq!(error.code, ErrorCode::PermissionDenied, "{error:?}");
    let (_, sink) = run(&mut session, "SELECT count(*) FROM orders")
        .await
        .unwrap();
    assert_eq!(sink.rows, vec![vec![Cell::Integer(2)]]);
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_long_result_is_read_page_by_page_and_discarded_by_the_next_statement() {
    let folder = Folder::new();
    let path = seeded(&folder).await;
    let mut session = SqliteDriver
        .open_session(&input(&path, false), CancellationToken::new())
        .await
        .unwrap();
    let options = ExecutionOptions {
        max_rows: 1_000,
        ..ExecutionOptions::default()
    };
    let numbers = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2500) SELECT i FROM n";
    let mut first = Sink::default();
    let outcome = session
        .execute(
            &QueryRequest::new(numbers, options.clone()),
            &mut first,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(outcome.has_more);
    assert_eq!(first.rows.len(), 1_000);
    let mut second = Sink::default();
    let outcome = session
        .fetch(&options, &mut second, CancellationToken::new())
        .await
        .unwrap();
    assert!(outcome.has_more);
    assert_eq!(second.rows.first(), Some(&vec![Cell::Integer(1_001)]));
    // Another statement discards the rest, and the session goes on.
    let (_, sink) = run(&mut session, "SELECT 'next'").await.unwrap();
    assert_eq!(sink.rows, vec![vec![Cell::Text("next".into())]]);
    let error = session
        .fetch(&options, &mut Sink::default(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Validation);

    // The last page ends the result.
    let mut sink = Sink::default();
    session
        .execute(
            &QueryRequest::new(numbers, options.clone()),
            &mut sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    for _ in 0..2 {
        session
            .fetch(&options, &mut sink, CancellationToken::new())
            .await
            .unwrap();
    }
    assert_eq!(sink.rows.len(), 2_500);
    assert_eq!(sink.rows.last(), Some(&vec![Cell::Integer(2_500)]));
    session.close().await.unwrap();
}

#[tokio::test]
async fn cancelling_or_timing_out_interrupts_the_statement_and_keeps_the_session() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let folder = Folder::new();
        let path = seeded(&folder).await;
        let mut session = SqliteDriver
            .open_session(&input(&path, false), CancellationToken::new())
            .await
            .unwrap();
        keep_state(&mut session).await;

        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let error = session
            .execute(&request(ENDLESS), &mut Sink::default(), cancel)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_kept(&mut session).await;

        let options = ExecutionOptions {
            timeout: Some(Duration::from_millis(300)),
            ..ExecutionOptions::default()
        };
        let error = session
            .execute(
                &QueryRequest::new(ENDLESS, options),
                &mut Sink::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout);
        assert_kept(&mut session).await;

        // A sink that stops leaves the session as it was too.
        let mut sink = Sink {
            stop_after: Some(1),
            ..Sink::default()
        };
        session
            .execute(
                &request("SELECT value FROM generate_series(1, 100000)"),
                &mut sink,
                CancellationToken::new(),
            )
            .await
            .or_else(|_| Ok::<_, DriverError>(DriverOutcome::default()))
            .unwrap();
        let mut sink = Sink {
            stop_after: Some(1),
            ..Sink::default()
        };
        session
            .execute(
                &request("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100000) SELECT i FROM n"),
                &mut sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(sink.rows.len(), 1);
        session.ping().await.unwrap();
        assert_kept(&mut session).await;
        session.close().await.unwrap();
    })
    .await
    .unwrap();
}

fn marked(sql: &str, name: &str, kind: ParameterKind, value: Option<&str>) -> QueryParameter {
    QueryParameter {
        name: name.into(),
        kind,
        value: value.map(Into::into),
        markers: sql
            .match_indices(&format!(":{name}"))
            .map(|(start, found)| start..start + found.len())
            .collect(),
    }
}

#[tokio::test]
async fn named_parameters_are_bound_and_never_interpolated() {
    let folder = Folder::new();
    let path = seeded(&folder).await;
    let mut session = SqliteDriver
        .open_session(&input(&path, false), CancellationToken::new())
        .await
        .unwrap();
    let sql = "SELECT :person, :amount + 0, :whole, :flag, :day, :none, typeof(:none), \
               (SELECT name FROM customers WHERE id = :whole)";
    let request = QueryRequest {
        parameters: vec![
            marked(
                sql,
                "person",
                ParameterKind::Text,
                Some("x'); DROP TABLE customers; --"),
            ),
            marked(sql, "amount", ParameterKind::Number, Some("12.5")),
            marked(sql, "whole", ParameterKind::Number, Some("1")),
            marked(sql, "flag", ParameterKind::Boolean, Some("true")),
            marked(sql, "day", ParameterKind::Date, Some("2024-02-29")),
            marked(sql, "none", ParameterKind::Text, None),
        ],
        ..QueryRequest::new(sql, ExecutionOptions::default())
    };
    let mut sink = Sink::default();
    session
        .execute(&request, &mut sink, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        sink.rows,
        vec![vec![
            Cell::Text("x'); DROP TABLE customers; --".into()),
            Cell::Float(12.5),
            Cell::Integer(1),
            Cell::Integer(1),
            Cell::Text("2024-02-29".into()),
            Cell::Null,
            Cell::Text("null".into()),
            Cell::Text("Ana".into()),
        ]]
    );
    let (_, sink) = run(&mut session, "SELECT count(*) FROM customers")
        .await
        .unwrap();
    assert_eq!(sink.rows, vec![vec![Cell::Integer(2)]]);
    session.close().await.unwrap();
}

#[tokio::test]
async fn only_main_is_a_context_and_it_is_reported() {
    let folder = Folder::new();
    let path = seeded(&folder).await;
    let mut session = SqliteDriver
        .open_session(&input(&path, false), CancellationToken::new())
        .await
        .unwrap();
    let context = session.context(CancellationToken::new()).await.unwrap();
    assert_eq!(context.schema.as_deref(), Some("main"));
    let in_context = |schema: &str| QueryRequest {
        options: ExecutionOptions {
            context: SessionContext {
                catalog: None,
                schema: Some(schema.into()),
            },
            ..ExecutionOptions::default()
        },
        ..request("SELECT count(*) FROM customers")
    };
    session
        .execute(
            &in_context("main"),
            &mut Sink::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let error = session
        .execute(
            &in_context("aux"),
            &mut Sink::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Validation);
    assert!(session.is_open());
    session.close().await.unwrap();
}

fn names(objects: &[MetadataObject], kind: ObjectKind) -> Vec<String> {
    objects
        .iter()
        .filter(|object| object.kind == kind)
        .map(|object| object.name.clone())
        .collect()
}

#[tokio::test]
async fn metadata_lists_relations_columns_keys_and_defaults() {
    let folder = Folder::new();
    let path = seeded(&folder).await;
    let driver = SqliteDriver;
    let input = input(&path, true);
    let root = ObjectPath::default();
    let schemas = driver
        .discover_namespaces(&input, &root, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(names(&schemas, ObjectKind::Schema), ["main"]);
    let main = root.child(ObjectKind::Schema, "main");
    assert!(
        driver
            .discover_namespaces(&input, &main, CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(driver.default_namespaces(&input.config), vec![main.clone()]);

    let mut metadata = Metadata::default();
    driver
        .introspect_scoped(&input, &main, &mut metadata, CancellationToken::new())
        .await
        .unwrap();
    let objects = metadata.0;
    // SQLite's own sqlite_sequence is left out.
    assert_eq!(names(&objects, ObjectKind::Table), ["customers", "orders"]);
    assert_eq!(names(&objects, ObjectKind::View), ["big_customers"]);
    let column = |table: &str, name: &str| {
        objects
            .iter()
            .find(|object| {
                object.kind == ObjectKind::Column
                    && object.name == name
                    && object.path.relation() == Some(table)
            })
            .unwrap()
            .clone()
    };
    let name = column("customers", "name");
    assert_eq!(name.data_type.as_deref(), Some("VARCHAR(80)"));
    assert_eq!(name.nullable, Some(false));
    assert_eq!(name.default_value.as_deref(), Some("'anonymous'"));
    assert_eq!(name.ordinal, Some(2));
    assert_eq!(column("customers", "id").primary_key, Some(1));
    assert_eq!(column("big_customers", "name").primary_key, None);
    let key = objects
        .iter()
        .find(|object| object.kind == ObjectKind::ForeignKey)
        .unwrap();
    assert_eq!(key.path.relation(), Some("orders"));
    assert_eq!(
        key.references,
        Some(ForeignKeyTarget {
            relation: main.child(ObjectKind::Table, "customers"),
            columns: vec!["customer_id".into()],
            // The key names no column: the parent's primary key.
            referenced: vec!["id".into()],
        })
    );
    assert!(objects.iter().all(|object| object.path.is_within(&main)));

    let mut names_only = Metadata::default();
    driver
        .introspect_level(
            &input,
            &root,
            IntrospectionLevel::Names,
            &mut names_only,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        names_only
            .0
            .iter()
            .all(|object| !matches!(object.kind, ObjectKind::Column | ObjectKind::ForeignKey))
    );
    assert_eq!(names(&names_only.0, ObjectKind::Schema), ["main"]);

    let orders = main.child(ObjectKind::Table, "orders");
    let mut one = Metadata::default();
    driver
        .introspect_scoped(&input, &orders, &mut one, CancellationToken::new())
        .await
        .unwrap();
    assert!(one.0.iter().all(|object| object.path.is_within(&orders)));
    assert_eq!(
        names(&one.0, ObjectKind::Column),
        ["id", "customer_id", "note"]
    );
    let mut gone = Metadata::default();
    driver
        .introspect_scoped(
            &input,
            &main.child(ObjectKind::Table, "dropped"),
            &mut gone,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(gone.0.is_empty());
}

#[tokio::test]
async fn inspection_and_definitions_come_from_the_schema() {
    let folder = Folder::new();
    let path = seeded(&folder).await;
    let driver = SqliteDriver;
    let input = input(&path, false);
    let main = ObjectPath::default().child(ObjectKind::Schema, "main");
    let orders = main.child(ObjectKind::Table, "orders");
    let inspection = driver
        .inspect(&input, &orders, CancellationToken::new())
        .await
        .unwrap();
    let find = |kind: InspectedKind| {
        inspection
            .items
            .iter()
            .filter(|item| item.kind == kind)
            .collect::<Vec<_>>()
    };
    assert!(
        find(InspectedKind::Index)[0]
            .detail
            .contains("WHERE note IS NOT NULL")
    );
    assert_eq!(
        find(InspectedKind::Unique)[0].detail,
        "UNIQUE (customer_id, note)"
    );
    assert_eq!(find(InspectedKind::PrimaryKey)[0].detail, "(id)");
    assert_eq!(
        find(InspectedKind::ForeignKey)[0].detail,
        "REFERENCES customers(id) ON DELETE CASCADE"
    );
    assert_eq!(find(InspectedKind::Trigger)[0].name, "orders_touch");
    let counters = driver
        .inspect(&input, &main, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(counters.items.len(), 1);
    assert_eq!(counters.items[0].name, "orders");
    assert_eq!(counters.items[0].kind, InspectedKind::Sequence);

    let ddl = driver
        .definition(&input, &orders, CancellationToken::new())
        .await
        .unwrap();
    assert!(ddl.starts_with("CREATE TABLE orders"), "{ddl}");
    assert!(ddl.contains("CREATE INDEX orders_by_note"), "{ddl}");
    assert!(ddl.contains("CREATE TRIGGER orders_touch"), "{ddl}");
    let view = driver
        .definition(
            &input,
            &main.child(ObjectKind::View, "big_customers"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(view.starts_with("CREATE VIEW big_customers"), "{view}");
    let error = driver
        .definition(
            &input,
            &main.child(ObjectKind::Table, "dropped"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
}
