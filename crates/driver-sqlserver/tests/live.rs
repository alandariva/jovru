#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use jykre_core::*;
use jykre_driver_sqlserver::SqlServerDriver;
use serde_json::json;
use std::time::Duration;

#[derive(Debug, Default)]
struct Sink {
    columns: Vec<ColumnInfo>,
    rows: Vec<Row>,
}
impl RowSink for Sink {
    fn columns(&mut self, c: Vec<ColumnInfo>) -> SinkControl {
        self.columns = c;
        SinkControl::Continue
    }
    fn row(&mut self, r: Row) -> SinkControl {
        self.rows.push(r);
        SinkControl::Continue
    }
}
#[derive(Default)]
struct Metadata(Vec<MetadataObject>);
#[async_trait]
impl IntrospectionSink for Metadata {
    async fn batch(&mut self, b: Vec<MetadataObject>) -> DriverResult<()> {
        self.0.extend(b);
        Ok(())
    }
}
fn login(user: &str) -> ResolvedConnection {
    // The fixture port is chosen by Docker for each run: `cargo xtask check
    // --full` sets it, and docs/testing.md shows how to run these by hand.
    let port: u16 = std::env::var("JYKRE_TEST_PORT")
        .expect("JYKRE_TEST_PORT must name the fixture port (see docs/testing.md)")
        .parse()
        .expect("JYKRE_TEST_PORT must be a valid port");
    ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::SqlServer,
            // The fixture's certificate is self-signed.
            options: json!({"host":"127.0.0.1","port":port,"database":"jykre","user":user,"encrypt":"mandatory","trustServerCertificate":true}),
        },
        credentials: Credentials {
            password: Some(SecretString::from("jykre-test".to_owned())),
        },
    }
}
fn input() -> ResolvedConnection {
    login("jykre")
}
fn request(sql: &str) -> QueryRequest {
    QueryRequest::new(sql, ExecutionOptions::default())
}
async fn run(session: &mut Box<dyn Session>, sql: &str) -> DriverResult<Sink> {
    let mut sink = Sink::default();
    session
        .execute(&request(sql), &mut sink, CancellationToken::new())
        .await?;
    Ok(sink)
}
async fn state(session: &mut Box<dyn Session>) -> Option<TransactionStatus> {
    session.transaction(CancellationToken::new()).await.unwrap()
}
async fn one(session: &mut Box<dyn Session>, sql: &str) -> Cell {
    let sink = run(session, sql).await.unwrap();
    assert_eq!(sink.rows.len(), 1, "{sql}");
    sink.rows[0][0].clone()
}
/// Session state that must survive a failed, cancelled or interrupted
/// statement: a session value and an open transaction holding a row.
async fn keep_state(session: &mut Box<dyn Session>) {
    run(
        session,
        "EXEC sp_set_session_context N'jykre_kept', N'jykre-kept'",
    )
    .await
    .unwrap();
    run(session, "CREATE TABLE #kept (id int)").await.unwrap();
    run(session, "BEGIN TRANSACTION").await.unwrap();
    run(session, "INSERT INTO #kept VALUES (7)").await.unwrap();
}
/// The session still runs statements and kept the state [`keep_state`] set.
async fn assert_kept(session: &mut Box<dyn Session>) {
    assert!(session.is_open());
    assert_eq!(
        one(
            session,
            "SELECT CAST(SESSION_CONTEXT(N'jykre_kept') AS nvarchar(20))"
        )
        .await,
        Cell::Text("jykre-kept".into())
    );
    assert_eq!(
        one(session, "SELECT CAST(@@TRANCOUNT AS nvarchar(5)) + N':' + CAST((SELECT COUNT(*) FROM #kept) AS nvarchar(5))").await,
        Cell::Text("1:1".into())
    );
    run(session, "ROLLBACK").await.unwrap();
    run(session, "DROP TABLE #kept").await.unwrap();
    keep_state(session).await;
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_values_are_exact_and_errors_keep_the_session() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let driver = SqlServerDriver;
        let input = input();
        let version = driver
            .test(&input, CancellationToken::new())
            .await
            .unwrap()
            .version
            .unwrap();
        assert!(version.starts_with("SQL Server "), "{version}");
        let mut session = driver
            .open_session(&input, CancellationToken::new())
            .await
            .unwrap();
        keep_state(&mut session).await;
        let sink = run(
            &mut session,
            "SELECT 42 AS answer, NULL AS missing, CAST(123456789.12 AS decimal(20,2)) AS amount, \
             CAST(-922337203685477.5808 AS money) AS low, CAST(450359962737.0493 AS money) AS high, \
             CAST(-214748.3648 AS smallmoney) AS small, CAST(N'ação 総額' AS nvarchar(20)) AS label, \
             CAST(1 AS bit) AS flag, CAST(0x00FF AS varbinary(4)) AS raw, \
             CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier) AS id, \
             CAST('2024-02-29' AS date) AS day, CAST('2024-02-29 23:59:59.997' AS datetime) AS stamp, \
             CAST('2024-02-29 10:11:12.1234567' AS datetime2(7)) AS precise, \
             CAST('2024-02-29 23:30:00 -03:00' AS datetimeoffset(0)) AS offset, \
             CAST('10:11:12.5' AS time(1)) AS clock, CAST(0.1 AS float) AS approx, \
             CAST(N'<a>x</a>' AS xml) AS doc, CAST(N'' AS nvarchar(5)) AS blank",
        )
        .await
        .unwrap_err();
        // A money value beyond what tiberius' float identifies is refused,
        // never shown approximated.
        assert_eq!(sink.code, ErrorCode::Protocol);
        assert!(sink.message.contains("decimal(19,4)"), "{sink:?}");
        assert_kept(&mut session).await;
        let sink = run(
            &mut session,
            "SELECT 42 AS answer, NULL AS missing, CAST(123456789.12 AS decimal(20,2)) AS amount, \
             CAST(-450359962737.0493 AS money) AS low, CAST(450359962737.0493 AS money) AS high, \
             CAST(-214748.3648 AS smallmoney) AS small, CAST(N'ação 総額' AS nvarchar(20)) AS label, \
             CAST(1 AS bit) AS flag, CAST(0x00FF AS varbinary(4)) AS raw, \
             CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier) AS id, \
             CAST('2024-02-29' AS date) AS day, CAST('2024-02-29 23:59:59.997' AS datetime) AS stamp, \
             CAST('2024-02-29 10:11:12.1234567' AS datetime2(7)) AS precise, \
             CAST('2024-02-29 23:30:00 -03:00' AS datetimeoffset(0)) AS offset, \
             CAST('10:11:12.5' AS time(1)) AS clock, CAST(0.1 AS float) AS approx, \
             CAST(N'<a>x</a>' AS xml) AS doc, CAST(N'' AS nvarchar(5)) AS blank",
        )
        .await
        .unwrap();
        let text = |value: &str| Cell::Text(value.into());
        assert_eq!(
            sink.rows,
            vec![vec![
                text("42"),
                Cell::Null,
                text("123456789.12"),
                text("-450359962737.0493"),
                text("450359962737.0493"),
                text("-214748.3648"),
                text("ação 総額"),
                text("1"),
                Cell::Binary(vec![0, 255]),
                text("6F9619FF-8B86-D011-B42D-00C04FC964FF"),
                text("2024-02-29"),
                text("2024-02-29 23:59:59.997"),
                text("2024-02-29 10:11:12.1234567"),
                text("2024-02-29 23:30:00 -03:00"),
                text("10:11:12.5"),
                text("0.1"),
                text("<a>x</a>"),
                text(""),
            ]]
        );
        let types: Vec<_> = sink.columns.iter().map(|c| c.data_type.as_str()).collect();
        assert_eq!(types[2], "decimal");
        assert_eq!(types[3], "money");
        assert_eq!(types[13], "datetimeoffset");
        // A SELECT without rows still has its columns.
        let empty = run(&mut session, "SELECT 1 AS answer WHERE 1 = 0").await.unwrap();
        assert_eq!(empty.columns.len(), 1);
        assert!(empty.rows.is_empty());
        // A statement without a result reports the rows it changed.
        run(&mut session, "CREATE TABLE #changed (id int)").await.unwrap();
        let mut sink = Sink::default();
        let outcome = session
            .execute(
                &request("INSERT INTO #changed VALUES (1), (2), (3)"),
                &mut sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.rows_affected, Some(3));
        // The server's own message and error number reach the user.
        let failed = run(&mut session, "SELECT * FROM jykre_missing_table")
            .await
            .unwrap_err();
        assert_eq!(failed.code, ErrorCode::Database);
        assert!(failed.message.contains("jykre_missing_table"), "{failed:?}");
        assert_eq!(
            failed.server.unwrap().vendor_code.as_deref(),
            Some("208")
        );
        assert_kept(&mut session).await;
        // Only the first result of a batch is shown; the batch still runs to
        // its end.
        let sink = run(
            &mut session,
            "SELECT 1 AS first; SELECT 2 AS second; INSERT INTO #kept VALUES (8)",
        )
        .await
        .unwrap();
        assert_eq!(sink.rows, vec![vec![text("1")]]);
        assert_eq!(
            one(&mut session, "SELECT CAST(COUNT(*) AS nvarchar(5)) FROM #kept").await,
            text("2")
        );
        run(&mut session, "DELETE FROM #kept WHERE id = 8").await.unwrap();
        assert_kept(&mut session).await;
        session.close().await.unwrap();
        let mut bad = input.clone();
        bad.credentials.password = Some(SecretString::from("wrong-password".to_owned()));
        assert_eq!(
            driver
                .test(&bad, CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            ErrorCode::Authentication
        );
    })
    .await
    .expect("live scenario exceeded its time budget");
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_limits_cancellation_and_pages() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let driver = SqlServerDriver;
        let input = input();
        let mut session = driver
            .open_session(&input, CancellationToken::new())
            .await
            .unwrap();
        keep_state(&mut session).await;
        // A statement still running when its time runs out.
        let timed = session
            .execute(
                &QueryRequest::new(
                    "WAITFOR DELAY '00:00:10'",
                    ExecutionOptions {
                        timeout: Some(Duration::from_millis(200)),
                        ..ExecutionOptions::default()
                    },
                ),
                &mut Sink::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(timed.code, ErrorCode::Timeout);
        // tiberius cannot interrupt a statement and keep the connection in
        // step (TD-009): the session closes and a new one works.
        assert!(!session.is_open());
        let mut session = driver
            .open_session(&input, CancellationToken::new())
            .await
            .unwrap();
        keep_state(&mut session).await;
        // A statement cancelled while running.
        let token = CancellationToken::new();
        let trigger = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let cancelled = session
            .execute(
                &request("SELECT COUNT(*) FROM sys.all_objects; WAITFOR DELAY '00:00:10'"),
                &mut Sink::default(),
                token,
            )
            .await
            .unwrap_err();
        assert_eq!(cancelled.code, ErrorCode::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!session.is_open());
        let mut session = driver
            .open_session(&input, CancellationToken::new())
            .await
            .unwrap();
        keep_state(&mut session).await;
        // Pages of an open result.
        let query = QueryRequest::new(
            "SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3",
            ExecutionOptions {
                max_rows: 1,
                ..ExecutionOptions::default()
            },
        );
        let mut limited = Sink::default();
        let first = session
            .execute(&query, &mut limited, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(limited.rows.len(), 1);
        assert!(first.has_more);
        let mut rest = Sink::default();
        let second = session
            .fetch(&query.options, &mut rest, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(rest.rows, vec![vec![Cell::Text("2".into())]]);
        assert!(second.has_more);
        // Another statement discards the open result and the session goes on.
        assert_kept(&mut session).await;
        assert_eq!(
            session
                .fetch(&query.options, &mut rest, CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
        // A long result: pages in order, then cut, then cancelled while its
        // rows stream.
        let big = QueryRequest::new(
            "SELECT TOP (20000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n, REPLICATE('x', 200) AS pad \
             FROM sys.all_columns a CROSS JOIN sys.all_columns b",
            ExecutionOptions {
                max_rows: 10,
                ..ExecutionOptions::default()
            },
        );
        let mut limited = Sink::default();
        session
            .execute(&big, &mut limited, CancellationToken::new())
            .await
            .unwrap();
        for _ in 0..3 {
            let outcome = session
                .fetch(&big.options, &mut limited, CancellationToken::new())
                .await
                .unwrap();
            assert!(outcome.has_more);
        }
        let firsts: Vec<_> = [0, 10, 20, 30]
            .iter()
            .map(|&i| limited.rows[i][0].clone())
            .collect();
        assert_eq!(
            firsts,
            ["1", "11", "21", "31"].map(|n| Cell::Text(n.into())).to_vec()
        );
        assert_kept(&mut session).await;
        // A receiver that stops: the rest is read and dropped, and the
        // session is kept.
        struct StopAfter(usize);
        impl RowSink for StopAfter {
            fn columns(&mut self, _: Vec<ColumnInfo>) -> SinkControl {
                SinkControl::Continue
            }
            fn row(&mut self, _: Row) -> SinkControl {
                self.0 += 1;
                if self.0 == 3 {
                    SinkControl::Stop
                } else {
                    SinkControl::Continue
                }
            }
        }
        let unlimited = QueryRequest {
            options: ExecutionOptions::default(),
            ..big.clone()
        };
        session
            .execute(&unlimited, &mut StopAfter(0), CancellationToken::new())
            .await
            .unwrap();
        assert_kept(&mut session).await;
        // Cancelled while its rows stream.
        struct CancelAfter(CancellationToken, usize);
        impl RowSink for CancelAfter {
            fn columns(&mut self, _: Vec<ColumnInfo>) -> SinkControl {
                SinkControl::Continue
            }
            fn row(&mut self, _: Row) -> SinkControl {
                self.1 += 1;
                if self.1 == 5 {
                    self.0.cancel();
                }
                SinkControl::Continue
            }
        }
        let token = CancellationToken::new();
        let cancelled = session
            .execute(&unlimited, &mut CancelAfter(token.clone(), 0), token)
            .await;
        match cancelled {
            // The cancellation may land after the last row was read.
            Ok(_) => assert_kept(&mut session).await,
            Err(error) => {
                assert_eq!(error.code, ErrorCode::Cancelled);
                assert!(!session.is_open());
            }
        }
        session.close().await.unwrap();
    })
    .await
    .expect("live scenario exceeded its time budget");
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_context_transaction_explain_and_parameters() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let driver = SqlServerDriver;
        let input = input();
        let mut session = driver
            .open_session(&input, CancellationToken::new())
            .await
            .unwrap();
        let context = session.context(CancellationToken::new()).await.unwrap();
        assert_eq!(context.catalog.as_deref(), Some("jykre"));
        assert_eq!(context.schema.as_deref(), Some("dbo"));
        // The tab's database is applied once; a USE the user ran stays.
        let in_master = QueryRequest::new(
            "SELECT DB_NAME()",
            ExecutionOptions {
                context: SessionContext {
                    catalog: Some("master".into()),
                    schema: None,
                },
                ..ExecutionOptions::default()
            },
        );
        let mut sink = Sink::default();
        session
            .execute(&in_master, &mut sink, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(sink.rows, vec![vec![Cell::Text("master".into())]]);
        run(&mut session, "USE jykre").await.unwrap();
        let mut sink = Sink::default();
        session
            .execute(&in_master, &mut sink, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(sink.rows, vec![vec![Cell::Text("jykre".into())]]);
        // Transaction states as the server sees them.
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Idle));
        run(&mut session, "BEGIN TRANSACTION").await.unwrap();
        // The fixture login may not read the transaction's log records.
        assert_eq!(
            state(&mut session).await,
            Some(TransactionStatus::Open { changes: None })
        );
        run(&mut session, "ROLLBACK").await.unwrap();
        run(&mut session, "SET IMPLICIT_TRANSACTIONS ON")
            .await
            .unwrap();
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Manual));
        run(&mut session, "SET IMPLICIT_TRANSACTIONS OFF")
            .await
            .unwrap();
        // A plan instead of a run; the plan mode never outlives it, even when
        // the statement fails.
        let plan = run(
            &mut session,
            "SET SHOWPLAN_ALL ON;\nSELECT id FROM gate_fixture WHERE id = 1",
        )
        .await
        .unwrap();
        assert!(plan.columns.iter().any(|c| c.name == "StmtText"));
        assert!(!plan.rows.is_empty());
        let failed = run(
            &mut session,
            "SET SHOWPLAN_ALL ON;\nSELECT * FROM jykre_missing_table",
        )
        .await
        .unwrap_err();
        assert_eq!(failed.code, ErrorCode::Database);
        assert_eq!(one(&mut session, "SELECT 5").await, Cell::Text("5".into()));
        // Parameters are bound, never written into the SQL.
        let sql = "SELECT :who, :amount, :flag, :day, :at, :none";
        let marked = |name: &str, kind, value: Option<&str>| QueryParameter {
            name: name.into(),
            kind,
            value: value.map(Into::into),
            markers: sql
                .match_indices(&format!(":{name}"))
                .map(|(start, found)| start..start + found.len())
                .collect(),
        };
        let bound = QueryRequest {
            parameters: vec![
                marked(
                    "who",
                    ParameterKind::Text,
                    Some("x'; DROP TABLE gate_fixture; --"),
                ),
                marked("amount", ParameterKind::Number, Some("-1.25e2")),
                marked("flag", ParameterKind::Boolean, Some("true")),
                marked("day", ParameterKind::Date, Some("2024-02-29")),
                marked("at", ParameterKind::Timestamp, Some("2024-02-29T10:00")),
                marked("none", ParameterKind::Date, None),
            ],
            ..request(sql)
        };
        let mut sink = Sink::default();
        session
            .execute(&bound, &mut sink, CancellationToken::new())
            .await
            .unwrap();
        let text = |value: &str| Cell::Text(value.into());
        assert_eq!(
            sink.rows,
            vec![vec![
                text("x'; DROP TABLE gate_fixture; --"),
                text("-125"),
                text("1"),
                text("2024-02-29"),
                text("2024-02-29 10:00:00.0000000"),
                Cell::Null,
            ]]
        );
        assert_eq!(
            one(
                &mut session,
                "SELECT CAST(COUNT(*) AS nvarchar(5)) FROM gate_fixture"
            )
            .await,
            text("1")
        );
        session.close().await.unwrap();
    })
    .await
    .expect("live scenario exceeded its time budget");
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_metadata_inspection_definitions_and_permissions() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let driver = SqlServerDriver;
        let input = input();
        let cancel = CancellationToken::new();
        let databases = driver
            .discover_namespaces(&input, &ObjectPath::default(), cancel.clone())
            .await
            .unwrap();
        let names: Vec<_> = databases.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"jykre"), "{names:?}");
        assert!(!names.contains(&"master") && !names.contains(&"tempdb"));
        let db = ObjectPath::default().child(ObjectKind::Database, "jykre");
        let schemas = driver
            .discover_namespaces(&input, &db, cancel.clone())
            .await
            .unwrap();
        let names: Vec<_> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["dbo", "gate_archive"]);
        let dbo = db.child(ObjectKind::Schema, "dbo");
        let mut metadata = Metadata::default();
        driver
            .introspect_scoped(&input, &dbo, &mut metadata, cancel.clone())
            .await
            .unwrap();
        let find = |kind, name: &str| {
            metadata
                .0
                .iter()
                .find(|o| o.kind == kind && o.name == name)
                .cloned()
                .expect(name)
        };
        let table = find(ObjectKind::Table, "gate_fixture");
        assert_eq!(table.comment.as_deref(), Some("Synthetic gate table"));
        find(ObjectKind::View, "gate_fixture_view");
        let amount = find(ObjectKind::Column, "amount");
        assert_eq!(amount.data_type.as_deref(), Some("decimal(20,2)"));
        let label = find(ObjectKind::Column, "label");
        assert_eq!(label.data_type.as_deref(), Some("nvarchar(80)"));
        assert_eq!(label.comment.as_deref(), Some("Free text"));
        let id = metadata
            .0
            .iter()
            .find(|o| {
                o.kind == ObjectKind::Column && o.name == "id" && o.path.is_within(&table.path)
            })
            .unwrap();
        assert_eq!(id.primary_key, Some(1));
        assert_eq!(id.nullable, Some(false));
        let created = find(ObjectKind::Column, "created");
        assert_eq!(created.default_value.as_deref(), Some("('none')"));
        let key = find(ObjectKind::ForeignKey, "gate_child_fixture");
        let target = key.references.unwrap();
        assert_eq!(
            target.relation,
            dbo.child(ObjectKind::Table, "gate_fixture")
        );
        assert_eq!(target.columns, ["fixture_id"]);
        assert_eq!(target.referenced, ["id"]);
        // A scope reads exactly its subtree.
        for scope in [
            table.path.clone(),
            table.path.child(ObjectKind::Column, "amount"),
        ] {
            let mut partial = Metadata::default();
            driver
                .introspect_scoped(&input, &scope, &mut partial, cancel.clone())
                .await
                .unwrap();
            let expected: Vec<_> = metadata
                .0
                .iter()
                .filter(|o| o.path.is_within(&scope))
                .cloned()
                .collect();
            assert_eq!(partial.0, expected);
        }
        let mut names_only = Metadata::default();
        driver
            .introspect_level(
                &input,
                &dbo,
                IntrospectionLevel::Names,
                &mut names_only,
                cancel.clone(),
            )
            .await
            .unwrap();
        assert!(names_only.0.iter().all(|o| o.kind != ObjectKind::Column));
        assert!(names_only.0.iter().any(|o| o.name == "gate_fixture"));
        // What the refreshed tree leaves out.
        let inspection = driver
            .inspect(&input, &table.path, cancel.clone())
            .await
            .unwrap();
        for (kind, name) in [
            (InspectedKind::Index, "gate_fixture_label"),
            (InspectedKind::Check, "gate_fixture_amount"),
            (InspectedKind::Trigger, "gate_fixture_touch"),
            (InspectedKind::PrimaryKey, ""),
        ] {
            assert!(
                inspection
                    .items
                    .iter()
                    .any(|item| item.kind == kind && (name.is_empty() || item.name == name)),
                "missing {kind:?} {name}: {inspection:?}"
            );
        }
        let namespace = driver.inspect(&input, &dbo, cancel.clone()).await.unwrap();
        for (kind, name) in [
            (InspectedKind::Procedure, "gate_touch"),
            (InspectedKind::Sequence, "gate_counter"),
            (InspectedKind::Type, "gate_code"),
        ] {
            assert!(
                namespace
                    .items
                    .iter()
                    .any(|item| item.kind == kind && item.name == name),
                "missing {kind:?} {name}: {namespace:?}"
            );
        }
        let procedure = namespace
            .items
            .iter()
            .find(|i| i.name == "gate_touch")
            .unwrap();
        assert_eq!(procedure.detail, "(@steps int, @total int OUTPUT)");
        let view = driver
            .definition(
                &input,
                &dbo.child(ObjectKind::View, "gate_fixture_view"),
                cancel.clone(),
            )
            .await
            .unwrap();
        assert!(view.starts_with("CREATE VIEW gate_fixture_view"), "{view}");
        let ddl = driver
            .definition(
                &input,
                &dbo.child(ObjectKind::Table, "gate_child"),
                cancel.clone(),
            )
            .await
            .unwrap();
        assert!(
            ddl.starts_with("CREATE TABLE [dbo].[gate_child] ("),
            "{ddl}"
        );
        assert!(ddl.contains("[fixture_id] int NOT NULL"), "{ddl}");
        assert!(
            ddl.contains("CONSTRAINT [gate_child_created] DEFAULT ('none')"),
            "{ddl}"
        );
        assert!(
            ddl.contains("FOREIGN KEY ([fixture_id]) REFERENCES [dbo].[gate_fixture] ([id])"),
            "{ddl}"
        );
        // A database the login may not use is reported, not hidden.
        let reader = login("gate_reader");
        let private = ObjectPath::default()
            .child(ObjectKind::Database, "gate_private")
            .child(ObjectKind::Schema, "dbo");
        let report = driver
            .introspect_scoped(&reader, &private, &mut Metadata::default(), cancel.clone())
            .await
            .unwrap();
        assert_eq!(
            report.warnings.first().map(|w| w.path.clone()),
            Some(ObjectPath::default().child(ObjectKind::Database, "gate_private"))
        );
        let visible = driver
            .discover_namespaces(&reader, &ObjectPath::default(), cancel)
            .await
            .unwrap();
        assert!(visible.iter().all(|d| d.name != "gate_private"));
    })
    .await
    .expect("live scenario exceeded its time budget");
}
