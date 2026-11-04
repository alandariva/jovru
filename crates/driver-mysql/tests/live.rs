#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use jykre_core::*;
use jykre_driver_mysql::MySqlDriver;
use serde_json::json;
use std::time::Duration;
#[derive(Default)]
struct Sink {
    columns: usize,
    rows: Vec<Row>,
}
impl RowSink for Sink {
    fn columns(&mut self, c: Vec<ColumnInfo>) -> SinkControl {
        self.columns = c.len();
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
fn input() -> ResolvedConnection {
    // The fixture port is chosen by Docker for each run: `cargo xtask check
    // --full` sets it, and docs/testing.md shows how to run these by hand.
    let port: u16 = std::env::var("JYKRE_TEST_PORT")
        .expect("JYKRE_TEST_PORT must name the fixture port (see docs/testing.md)")
        .parse()
        .expect("JYKRE_TEST_PORT must be a valid port");
    ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::MySql,
            options: json!({"host":"127.0.0.1","port":port,"database":"jykre","user":"jykre","sslMode":"required"}),
        },
        credentials: Credentials {
            password: Some(SecretString::from("jykre-test".to_owned())),
        },
    }
}
/// Session state that must survive a failed, cancelled or interrupted
/// statement.
async fn keep_state(session: &mut Box<dyn Session>) {
    session
        .execute(
            &QueryRequest {
                parameters: Vec::new(),
                sql: "SET @jykre_kept := 'jykre-kept'".into(),
                options: ExecutionOptions::default(),
            },
            &mut Sink::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
}
/// The session still runs statements and kept the state [`keep_state`] set.
async fn assert_kept(session: &mut Box<dyn Session>) {
    assert!(session.is_open());
    let mut sink = Sink::default();
    session
        .execute(
            &QueryRequest {
                parameters: Vec::new(),
                sql: "SELECT CAST(@jykre_kept AS CHAR)".into(),
                options: ExecutionOptions::default(),
            },
            &mut sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(sink.rows, vec![vec![Cell::Text("jykre-kept".into())]]);
}
#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_connection_queries_metadata_and_timeout() {
    tokio::time::timeout(Duration::from_secs(90), async {
    let driver = MySqlDriver;
    let input = input();
    let cancel = CancellationToken::new();
    assert!(
        driver
            .test(&input, cancel.clone())
            .await
            .unwrap()
            .version
            .is_some()
    );
    let mut session = driver.open_session(&input, cancel.clone()).await.unwrap();
    keep_state(&mut session).await;
    let mut sink = Sink::default();
    session.execute(&QueryRequest{parameters:Vec::new(),sql:"SELECT 42 AS answer, NULL AS missing, CAST(123456789.12 AS DECIMAL(20,2)) AS amount".into(),options:ExecutionOptions::default()},&mut sink,cancel.clone()).await.unwrap();
    assert_eq!(sink.columns, 3);
    assert_eq!(sink.rows.len(), 1);
    assert_eq!(sink.rows[0][1], Cell::Null);
    assert_eq!(sink.rows[0][2], Cell::Text("123456789.12".into()));
    let mut empty = Sink::default();
    session
        .execute(
            &QueryRequest {
                parameters: Vec::new(),
                sql: "SELECT 1 AS answer WHERE 1=0".into(),
                options: ExecutionOptions::default(),
            },
            &mut empty,
            cancel.clone(),
        )
        .await
        .unwrap();
    assert_eq!(empty.columns, 1);
    assert!(empty.rows.is_empty());
    session.close().await.unwrap();
    let mut metadata = Metadata::default();
    driver
        .introspect(&input, &mut metadata, cancel.clone())
        .await
        .unwrap();
    for (kind, name) in [
        (ObjectKind::Table, "gate_fixture"),
        (ObjectKind::View, "gate_fixture_view"),
        (ObjectKind::Column, "amount"),
    ] {
        assert!(
            metadata
                .0
                .iter()
                .any(|object| object.kind == kind && object.name == name),
            "missing fixture object: {name}"
        );
    }
    let table = metadata.0.iter().find(|object| object.kind == ObjectKind::Table && object.name == "gate_fixture").unwrap();
    for scope in [table.path.clone(), table.path.child(ObjectKind::Column, "amount")] {
        let mut partial = Metadata::default();
        driver.introspect_scoped(&input, &scope, &mut partial, cancel.clone()).await.unwrap();
        assert!(!partial.0.is_empty());
        assert!(partial.0.iter().all(|object| object.path.is_within(&scope)));
        let expected: Vec<_> = metadata.0.iter().filter(|object| object.path.is_within(&scope)).cloned().collect();
        assert_eq!(partial.0, expected);
    }
    let mut missing = Metadata::default();
    driver.introspect_scoped(&input, &table.path.parent().unwrap().child(ObjectKind::Table, "absent_fixture"), &mut missing, cancel.clone()).await.unwrap();
    assert!(missing.0.is_empty());
    let mut session = driver.open_session(&input, cancel.clone()).await.unwrap();
    keep_state(&mut session).await;
    let err = session
        .execute(
            &QueryRequest {
                parameters: Vec::new(),
                sql: "SELECT SLEEP(10)".into(),
                options: ExecutionOptions {
                    timeout: Some(Duration::from_millis(100)),
                    ..ExecutionOptions::default()
                },
            },
            &mut Sink::default(),
            cancel,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Timeout);
    assert_kept(&mut session).await;
    let mut bad = input.clone();
    bad.config.options["sslMode"] = json!("required");
    bad.credentials.password = Some(SecretString::from("wrong-password".to_owned()));
    assert_eq!(
        driver
            .test(&bad, CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        ErrorCode::Authentication
    );
    let mut session = driver
        .open_session(&input, CancellationToken::new())
        .await
        .unwrap();
    keep_state(&mut session).await;
    let token = CancellationToken::new();
    let trigger = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let cancelled = session
        .execute(
            &QueryRequest {
                parameters: Vec::new(),
                sql: "SELECT SLEEP(10)".into(),
                options: ExecutionOptions::default(),
            },
            &mut Sink::default(),
            token,
        )
        .await
        .unwrap_err();
    assert_eq!(cancelled.code, ErrorCode::Cancelled);
    assert_kept(&mut session).await;
    let mut session = driver
        .open_session(&input, CancellationToken::new())
        .await
        .unwrap();
    keep_state(&mut session).await;
    let query = QueryRequest {
        parameters: Vec::new(),
        sql: "SELECT 1 UNION ALL SELECT 2".into(),
        options: ExecutionOptions {
            max_rows: 1,
            ..ExecutionOptions::default()
        },
    };
    let mut limited = Sink::default();
    let first = session
        .execute(&query, &mut limited, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(limited.rows.len(), 1);
    assert!(first.has_more);
    // The next page comes from the open result, not from running it again.
    let mut rest = Sink::default();
    let second = session
        .fetch(&query.options, &mut rest, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(rest.rows.len(), 1);
    assert_ne!(rest.rows[0], limited.rows[0]);
    assert!(!second.has_more);
    assert_eq!(
        session
            .fetch(&query.options, &mut rest, CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        ErrorCode::Validation
    );
    // Another statement discards an open result and the session goes on.
    session
        .execute(&query, &mut Sink::default(), CancellationToken::new())
        .await
        .unwrap();
    assert_kept(&mut session).await;

    let mut session = driver
        .open_session(&input, CancellationToken::new())
        .await
        .unwrap();
    keep_state(&mut session).await;
    let failed = session
        .execute(
            &QueryRequest {
                parameters: Vec::new(),
                sql: "SELECT * FROM jykre_missing_table".into(),
                options: ExecutionOptions::default(),
            },
            &mut Sink::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(failed.code, ErrorCode::Database);
    // The server's own message, error number and SQLSTATE reach the user.
    assert!(failed.message.contains("jykre_missing_table"), "{failed:?}");
    let server = failed.server.clone().unwrap();
    assert_eq!(server.vendor_code.as_deref(), Some("1146"));
    assert_eq!(server.sqlstate.as_deref(), Some("42S02"));
    assert_kept(&mut session).await;
    // A long result cut by the row limit, then one cancelled while its rows
    // stream: the rest is drained or stopped, and the session is kept.
    let big = QueryRequest {
        parameters: Vec::new(),
        sql: "SELECT a.n * 400 + b.n + 1 AS n, REPEAT('x', 200) FROM (WITH RECURSIVE s(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM s WHERE n < 499) SELECT n FROM s) a CROSS JOIN (WITH RECURSIVE s(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM s WHERE n < 399) SELECT n FROM s) b ORDER BY n".into(),
        options: ExecutionOptions {
            max_rows: 10,
            ..ExecutionOptions::default()
        },
    };
    let mut limited = Sink::default();
    session
        .execute(&big, &mut limited, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(limited.rows.len(), 10);
    // Three more pages of the long result, in order.
    for _ in 0..3 {
        let outcome = session
            .fetch(&big.options, &mut limited, CancellationToken::new())
            .await
            .unwrap();
        assert!(outcome.has_more);
    }
    assert_eq!(limited.rows.len(), 40);
    let firsts: Vec<_> = [0, 10, 20, 30].iter().map(|&i| limited.rows[i][0].clone()).collect();
    assert_eq!(
        firsts,
        ["1", "11", "21", "31"].map(|n| Cell::Text(n.into())).to_vec()
    );
    assert_kept(&mut session).await;
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
    let unlimited = QueryRequest {
        parameters: Vec::new(),
        options: ExecutionOptions::default(),
        ..big
    };
    let cancelled = session
        .execute(&unlimited, &mut CancelAfter(token.clone(), 0), token)
        .await;
    // The cancellation may land after the last row was read.
    assert!(cancelled.is_ok() || cancelled.unwrap_err().code == ErrorCode::Cancelled);
    assert_kept(&mut session).await;
    session.close().await.unwrap();

    }).await.expect("live scenario exceeded its time budget");
}

/// SQLx reads a packet's header and its payload in separate steps. Rows of
/// several megabytes keep the reader inside a payload most of the time, so a
/// cancellation or a time limit almost always lands halfway through a row;
/// the session must still be kept, with its state.
#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_interrupting_a_half_read_row_keeps_the_session() {
    tokio::time::timeout(Duration::from_secs(60), async {
        struct CancelAtFirst(CancellationToken);
        impl RowSink for CancelAtFirst {
            fn columns(&mut self, _: Vec<ColumnInfo>) -> SinkControl {
                SinkControl::Continue
            }
            fn row(&mut self, _: Row) -> SinkControl {
                self.0.cancel();
                SinkControl::Continue
            }
        }
        let wide = "SELECT n, REPEAT('x', 4194304) FROM (WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 32) SELECT n FROM s) t";
        let driver = MySqlDriver;
        let mut session = driver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        keep_state(&mut session).await;
        for _ in 0..5 {
            let token = CancellationToken::new();
            let cancelled = session
                .execute(
                    &QueryRequest {
                        parameters: Vec::new(),
                        sql: wide.into(),
                        options: ExecutionOptions::default(),
                    },
                    &mut CancelAtFirst(token.clone()),
                    token,
                )
                .await;
            assert_eq!(cancelled.unwrap_err().code, ErrorCode::Cancelled);
            assert_kept(&mut session).await;
            let timed_out = session
                .execute(
                    &QueryRequest {
                        parameters: Vec::new(),
                        sql: wide.into(),
                        options: ExecutionOptions {
                            timeout: Some(Duration::from_millis(50)),
                            ..ExecutionOptions::default()
                        },
                    },
                    &mut Sink::default(),
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(timed_out.unwrap_err().code, ErrorCode::Timeout);
            assert_kept(&mut session).await;
        }
        session.close().await.unwrap();
    })
    .await
    .expect("live scenario exceeded its time budget");
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn receiver_stop_keeps_the_session_and_a_fresh_session_works() {
    tokio::time::timeout(Duration::from_secs(20), async {
        struct Stop {
            columns: usize,
            rows: usize,
            at_columns: bool,
        }
        impl RowSink for Stop {
            fn columns(&mut self, _: Vec<ColumnInfo>) -> SinkControl {
                self.columns += 1;
                assert_eq!(self.columns, 1);
                if self.at_columns {
                    SinkControl::Stop
                } else {
                    SinkControl::Continue
                }
            }
            fn row(&mut self, _: Row) -> SinkControl {
                assert_eq!(self.columns, 1);
                self.rows += 1;
                assert_eq!(self.rows, 1, "delivery must stop immediately");
                SinkControl::Stop
            }
        }
        let driver = MySqlDriver;
        let query = QueryRequest {
            parameters: Vec::new(),
            sql: "SELECT 1 UNION ALL SELECT 2".into(),
            options: ExecutionOptions::default(),
        };
        for at_columns in [true, false] {
            let mut session = driver
                .open_session(&input(), CancellationToken::new())
                .await
                .unwrap();
            keep_state(&mut session).await;
            let mut sink = Stop {
                columns: 0,
                rows: 0,
                at_columns,
            };
            session
                .execute(&query, &mut sink, CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(sink.columns, 1);
            assert_eq!(sink.rows, usize::from(!at_columns));
            assert_kept(&mut session).await;
            let mut fresh = driver
                .open_session(&input(), CancellationToken::new())
                .await
                .unwrap();
            let mut rows = Sink::default();
            fresh
                .execute(&query, &mut rows, CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(rows.rows.len(), 2);
            fresh.close().await.unwrap();
        }
    })
    .await
    .expect("live scenario exceeded its time budget");
}
#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_discovery_lists_databases_without_system_ones() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = MySqlDriver;
        let input = input();
        let cancel = CancellationToken::new();
        let databases = driver
            .discover_namespaces(&input, &ObjectPath::default(), cancel.clone())
            .await
            .unwrap();
        assert!(databases.iter().any(|database| database.name == "jykre"));
        assert!(databases.iter().all(|database| {
            database.kind == ObjectKind::Database
                && !["mysql", "information_schema", "performance_schema", "sys"]
                    .contains(&database.name.as_str())
        }));
        let jykre = databases
            .iter()
            .find(|database| database.name == "jykre")
            .unwrap();
        assert!(
            driver
                .discover_namespaces(&input, &jykre.path, cancel)
                .await
                .unwrap()
                .is_empty()
        );
    })
    .await
    .expect("live discovery took too long");
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_metadata_has_exact_types_keys_defaults_and_comments() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = MySqlDriver;
        let input = input();
        let cancel = CancellationToken::new();
        let database = ObjectPath::default().child(ObjectKind::Database, "jykre");
        assert_eq!(
            driver.default_namespaces(&input.config),
            std::slice::from_ref(&database)
        );
        let mut metadata = Metadata::default();
        driver
            .introspect_scoped(&input, &database, &mut metadata, cancel)
            .await
            .unwrap();
        let find = |path: &ObjectPath| {
            let found = metadata.0.iter().find(|object| object.path == *path);
            assert!(found.is_some(), "missing {path:?}");
            found.cloned().unwrap()
        };
        let fixture = database.child(ObjectKind::Table, "gate_fixture");
        let id = find(&fixture.child(ObjectKind::Column, "id"));
        assert_eq!(id.primary_key, Some(1));
        assert_eq!(
            find(&fixture.child(ObjectKind::Column, "label"))
                .data_type
                .as_deref(),
            Some("varchar(80)")
        );
        assert_eq!(find(&fixture).comment, None);
        assert_eq!(
            find(&database.child(ObjectKind::View, "gate_fixture_view")).comment,
            None
        );
        let child = database.child(ObjectKind::Table, "gate_child");
        assert_eq!(find(&child).comment.as_deref(), Some("Synthetic child"));
        let created = find(&child.child(ObjectKind::Column, "created"));
        assert_eq!(created.default_value.as_deref(), Some("none"));
        assert_eq!(created.comment.as_deref(), Some("Synthetic default"));
        let key = find(&child.child(ObjectKind::ForeignKey, "gate_child_fixture"));
        assert_eq!(
            key.references,
            Some(ForeignKeyTarget {
                relation: fixture.clone(),
                columns: vec!["fixture_id".into()],
                referenced: vec!["id".into()],
            })
        );
    })
    .await
    .expect("live metadata took too long");
}

/// Runs `sql` with `context` and returns the first cell as text, or an empty
/// text for a statement without rows.
async fn text_in(session: &mut Box<dyn Session>, sql: &str, context: &SessionContext) -> String {
    let mut sink = Sink::default();
    session
        .execute(
            &QueryRequest {
                parameters: Vec::new(),
                sql: sql.into(),
                options: ExecutionOptions {
                    context: context.clone(),
                    ..ExecutionOptions::default()
                },
            },
            &mut sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    match sink.rows.first().and_then(|row| row.first()) {
        Some(Cell::Text(text)) => text.clone(),
        _ => String::new(),
    }
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_the_tab_context_is_applied_once_and_reported() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = MySqlDriver;
        let mut session = driver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        let chosen = SessionContext {
            catalog: None,
            schema: Some("jykre".into()),
        };
        assert_eq!(
            text_in(&mut session, "SELECT DATABASE()", &chosen).await,
            "jykre"
        );
        // The user's USE is not undone by the next statement of the same tab.
        text_in(&mut session, "USE information_schema", &chosen).await;
        assert_eq!(
            text_in(&mut session, "SELECT DATABASE()", &chosen).await,
            "information_schema"
        );
        let reported = session.context(CancellationToken::new()).await.unwrap();
        assert_eq!(reported.schema.as_deref(), Some("information_schema"));
        assert_eq!(reported.catalog, None);
        // Choosing again applies the choice once.
        assert_eq!(
            text_in(&mut session, "SELECT DATABASE()", &chosen).await,
            "jykre"
        );
        session.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_levels_inspection_definitions_and_permissions() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let driver = MySqlDriver;
        let input = input();
        let cancel = CancellationToken::new();
        let db = ObjectPath::default().child(ObjectKind::Database, "jykre");
        // The default database and TLS do not change which catalog a cache holds.
        let mut tuned = input.config.clone();
        tuned.options["database"] = json!("other");
        tuned.options["sslMode"] = json!("preferred");
        assert_eq!(
            driver.cache_identity(&tuned),
            driver.cache_identity(&input.config)
        );
        let mut names = Metadata::default();
        driver
            .introspect_level(
                &input,
                &db,
                IntrospectionLevel::Names,
                &mut names,
                cancel.clone(),
            )
            .await
            .unwrap();
        assert!(names.0.iter().any(|object| object.name == "gate_fixture"));
        assert!(
            !names
                .0
                .iter()
                .any(|object| matches!(object.kind, ObjectKind::Column | ObjectKind::ForeignKey))
        );
        let fixture = db.child(ObjectKind::Table, "gate_fixture");
        let inspection = driver
            .inspect(&input, &fixture, cancel.clone())
            .await
            .unwrap();
        let has = |kind, name: &str| {
            inspection
                .items
                .iter()
                .any(|item| item.kind == kind && item.name == name)
        };
        assert!(has(InspectedKind::Index, "gate_fixture_label"));
        assert!(has(InspectedKind::Check, "gate_fixture_amount"));
        assert!(has(InspectedKind::PrimaryKey, "PRIMARY"));
        assert!(has(InspectedKind::Trigger, "gate_fixture_touch"));
        let routines = driver.inspect(&input, &db, cancel.clone()).await.unwrap();
        let touch = routines
            .items
            .iter()
            .find(|item| item.name == "gate_touch")
            .expect("the procedure");
        assert_eq!(touch.kind, InspectedKind::Procedure);
        assert_eq!(touch.detail, "(IN steps int, OUT total int)");
        let table = driver
            .definition(&input, &fixture, cancel.clone())
            .await
            .unwrap();
        assert!(table.starts_with("CREATE TABLE `gate_fixture`"));
        let view = driver
            .definition(
                &input,
                &db.child(ObjectKind::View, "gate_fixture_view"),
                cancel.clone(),
            )
            .await
            .unwrap();
        assert!(view.contains("VIEW `gate_fixture_view`"));
        // A database the login may not use is a warning. MySQL answers a
        // login without global privileges the same way for a database that
        // does not exist, so that one is a warning too.
        let mut reader = input.clone();
        reader.config.options["user"] = json!("gate_reader");
        let private = ObjectPath::default().child(ObjectKind::Database, "gate_private");
        let report = driver
            .introspect_scoped(&reader, &private, &mut Metadata::default(), cancel.clone())
            .await
            .unwrap();
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].path, private);
        let gone = ObjectPath::default().child(ObjectKind::Database, "gate_gone");
        let report = driver
            .introspect_scoped(&input, &gone, &mut Metadata::default(), cancel)
            .await
            .unwrap();
        assert_eq!(report.warnings.len(), 1);
    })
    .await
    .expect("live metadata extensions took too long");
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_autocommit_off_is_reported() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = MySqlDriver;
        let mut session = driver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        let none = SessionContext::default();
        // A BEGIN is not visible without privileges; it is left to inference.
        text_in(&mut session, "BEGIN", &none).await;
        assert_eq!(
            session.transaction(CancellationToken::new()).await.unwrap(),
            None
        );
        text_in(&mut session, "ROLLBACK", &none).await;
        text_in(&mut session, "SET autocommit = 0", &none).await;
        assert_eq!(
            session.transaction(CancellationToken::new()).await.unwrap(),
            Some(TransactionStatus::Manual)
        );
        text_in(&mut session, "SET autocommit = 1", &none).await;
        assert_eq!(
            session.transaction(CancellationToken::new()).await.unwrap(),
            None
        );
        session.close().await.unwrap();
    })
    .await
    .unwrap();
}

/// A parameter with a marker at every `:name` in `sql`.
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
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_named_parameters_are_bound_and_never_interpolated() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = MySqlDriver;
        let cancel = CancellationToken::new();
        let mut session = driver.open_session(&input(), cancel.clone()).await.unwrap();
        let sql = "SELECT :who AS who, :n + 1 AS next, :d AS day, :b AS flag, :ts AS at, \
                   :missing AS missing, CHAR_LENGTH(:who) AS len";
        for text in [
            "O'Brien",
            "x'); DROP TABLE t; --",
            r"back\slash '' \'",
            "/* c */ -- d # e",
            ":who",
            "",
        ] {
            let request = QueryRequest {
                parameters: vec![
                    marked(sql, "who", ParameterKind::Text, Some(text)),
                    marked(sql, "n", ParameterKind::Number, Some("41.50")),
                    marked(sql, "d", ParameterKind::Date, Some("2024-02-29")),
                    marked(sql, "b", ParameterKind::Boolean, Some("true")),
                    marked(
                        sql,
                        "ts",
                        ParameterKind::Timestamp,
                        Some("2024-02-29T23:59:59.5"),
                    ),
                    marked(sql, "missing", ParameterKind::Text, None),
                ],
                ..QueryRequest::new(sql, ExecutionOptions::default())
            };
            let mut sink = Sink::default();
            session
                .execute(&request, &mut sink, cancel.clone())
                .await
                .unwrap();
            assert_eq!(
                sink.rows,
                vec![vec![
                    Cell::Text(text.into()),
                    Cell::Text("42.50".into()),
                    Cell::Text("2024-02-29".into()),
                    Cell::Text("1".into()),
                    Cell::Text("2024-02-29 23:59:59.500000".into()),
                    Cell::Null,
                    Cell::Text(text.chars().count().to_string()),
                ]],
                "{text:?}"
            );
        }
        // Paging keeps reading the result the bound values chose.
        let sql = "WITH RECURSIVE s(g) AS (SELECT 1 UNION ALL SELECT g + 1 FROM s WHERE g < :last) \
                   SELECT g FROM s WHERE g > :first";
        let options = ExecutionOptions {
            max_rows: 2,
            ..ExecutionOptions::default()
        };
        let request = QueryRequest {
            parameters: vec![
                marked(sql, "last", ParameterKind::Number, Some("5")),
                marked(sql, "first", ParameterKind::Number, Some("1")),
            ],
            ..QueryRequest::new(sql, options.clone())
        };
        let mut first = Sink::default();
        let outcome = session
            .execute(&request, &mut first, cancel.clone())
            .await
            .unwrap();
        assert!(outcome.has_more);
        let mut next = Sink::default();
        session
            .fetch(&options, &mut next, cancel.clone())
            .await
            .unwrap();
        let read: Vec<_> = first.rows.into_iter().chain(next.rows).collect();
        assert_eq!(
            read,
            ["2", "3", "4", "5"].map(|g| vec![Cell::Text(g.into())])
        );
        // A later run reads its own values: a NULL never reads what an
        // earlier run bound.
        let sql = "SELECT :who";
        for value in [Some("second"), None] {
            let request = QueryRequest {
                parameters: vec![marked(sql, "who", ParameterKind::Text, value)],
                ..QueryRequest::new(sql, ExecutionOptions::default())
            };
            let mut sink = Sink::default();
            session
                .execute(&request, &mut sink, cancel.clone())
                .await
                .unwrap();
            let expected = value.map_or(Cell::Null, |value| Cell::Text(value.into()));
            assert_eq!(sink.rows, vec![vec![expected]]);
        }
        session.close().await.unwrap();
    })
    .await
    .expect("live named parameters took too long");
}
