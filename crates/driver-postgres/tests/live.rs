#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use jykre_core::*;
use jykre_driver_postgres::PostgresDriver;
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
    // --full` sets it, and docs/standards/testing.md shows how to run these by hand.
    let port: u16 = std::env::var("JYKRE_TEST_PORT")
        .expect("JYKRE_TEST_PORT must name the fixture port (see docs/standards/testing.md)")
        .parse()
        .expect("JYKRE_TEST_PORT must be a valid port");
    ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::Postgres,
            options: json!({"host":"127.0.0.1","port":port,"database":"jykre","user":"jykre","sslMode":"disable"}),
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
                sql: "SET application_name TO 'jykre-kept'".into(),
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
                sql: "SELECT current_setting('application_name')".into(),
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
    let driver = PostgresDriver;
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
    // The fixture's profile disables TLS: the server says it is plain text.
    assert_eq!(
        session.encryption(cancel.clone()).await.unwrap(),
        Encryption::Plaintext
    );
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
                sql: "SELECT pg_sleep(10)".into(),
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
                sql: "SELECT pg_sleep(10)".into(),
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
    // The server's own message, SQLSTATE and position reach the user.
    assert!(failed.message.contains("jykre_missing_table"), "{failed:?}");
    let server = failed.server.clone().unwrap();
    assert_eq!(server.sqlstate.as_deref(), Some("42P01"));
    assert_eq!(server.position, Some(15));
    assert_kept(&mut session).await;
    // A long result cut by the row limit, then one cancelled while its rows
    // stream: the rest is drained or stopped, and the session is kept.
    let big = QueryRequest {
        parameters: Vec::new(),
        sql: "SELECT n, repeat('x', 200) FROM generate_series(1, 200000) AS n".into(),
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
        let driver = PostgresDriver;
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
async fn live_discovery_lists_the_database_and_its_schemas_only() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = PostgresDriver;
        let input = input();
        let cancel = CancellationToken::new();
        let root = driver
            .discover_namespaces(&input, &ObjectPath::default(), cancel.clone())
            .await
            .unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].kind, ObjectKind::Database);
        assert_eq!(root[0].name, "jykre");
        let schemas = driver
            .discover_namespaces(&input, &root[0].path, cancel.clone())
            .await
            .unwrap();
        assert!(schemas.iter().any(|schema| schema.name == "public"));
        assert!(
            schemas
                .iter()
                .all(|schema| schema.kind == ObjectKind::Schema
                    && !schema.name.starts_with("pg_")
                    && schema.name != "information_schema")
        );
        let public = schemas
            .iter()
            .find(|schema| schema.name == "public")
            .unwrap();
        assert!(
            driver
                .discover_namespaces(&input, &public.path, cancel.clone())
                .await
                .unwrap()
                .is_empty()
        );
        let other = ObjectPath::default().child(ObjectKind::Database, "another_database");
        assert_eq!(
            driver
                .discover_namespaces(&input, &other, cancel)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
    })
    .await
    .expect("live discovery took too long");
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_metadata_has_exact_types_keys_comments_and_relation_kinds() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = PostgresDriver;
        let input = input();
        let cancel = CancellationToken::new();
        let public = ObjectPath::default()
            .child(ObjectKind::Database, "jykre")
            .child(ObjectKind::Schema, "public");
        assert_eq!(
            driver.default_namespaces(&input.config),
            std::slice::from_ref(&public)
        );
        let mut metadata = Metadata::default();
        driver
            .introspect_scoped(&input, &public, &mut metadata, cancel.clone())
            .await
            .unwrap();
        let find = |path: &ObjectPath| {
            let found = metadata.0.iter().find(|object| object.path == *path);
            assert!(found.is_some(), "missing {path:?}");
            found.cloned().unwrap()
        };
        let fixture = public.child(ObjectKind::Table, "gate_fixture");
        assert_eq!(
            find(&fixture).comment.as_deref(),
            Some("Synthetic gate table")
        );
        let id = find(&fixture.child(ObjectKind::Column, "id"));
        assert_eq!(id.primary_key, Some(1));
        assert_eq!(id.nullable, Some(false));
        assert_eq!(
            find(&fixture.child(ObjectKind::Column, "amount"))
                .data_type
                .as_deref(),
            Some("numeric(20,2)")
        );
        let label = find(&fixture.child(ObjectKind::Column, "label"));
        assert_eq!(label.data_type.as_deref(), Some("character varying(80)"));
        assert_eq!(label.comment.as_deref(), Some("Free text"));
        assert_eq!(label.primary_key, None);
        let child = public.child(ObjectKind::Table, "gate_child");
        let created = find(&child.child(ObjectKind::Column, "created"));
        assert!(
            created
                .default_value
                .as_deref()
                .is_some_and(|value| value.contains("CURRENT_DATE"))
        );
        assert_eq!(
            find(&child.child(ObjectKind::Column, "tags"))
                .data_type
                .as_deref(),
            Some("text[]")
        );
        let key = find(&child.child(ObjectKind::ForeignKey, "gate_child_fixture_id_fkey"));
        assert_eq!(
            key.references,
            Some(ForeignKeyTarget {
                relation: fixture.clone(),
                columns: vec!["fixture_id".into()],
                referenced: vec!["id".into()],
            })
        );
        let totals = public.child(ObjectKind::MaterializedView, "gate_fixture_totals");
        find(&totals.child(ObjectKind::Column, "total"));
        let events = public.child(ObjectKind::Table, "gate_events");
        find(&events.child(ObjectKind::Partition, "gate_events_2026"));
        // A partition is listed under its table, not as a table of its own.
        assert!(!metadata
            .0
            .iter()
            .any(|object| object.kind == ObjectKind::Table && object.name == "gate_events_2026"));
        assert_eq!(
            driver.relation_name(&events.child(ObjectKind::Partition, "gate_events_2026")),
            "\"public\".\"gate_events_2026\""
        );
        // Refreshing the partitioned table keeps its partitions.
        let mut table = Metadata::default();
        driver
            .introspect_scoped(&input, &events, &mut table, cancel)
            .await
            .unwrap();
        assert!(
            table
                .0
                .iter()
                .any(|object| object.kind == ObjectKind::Partition)
        );
    })
    .await
    .expect("live metadata took too long");
}

/// Runs `sql` with `context` and returns the first cell as text.
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
        // A statement without rows, such as SET.
        _ => String::new(),
    }
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_the_tab_context_is_applied_once_and_reported() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = PostgresDriver;
        let mut session = driver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        let none = SessionContext::default();
        text_in(
            &mut session,
            "CREATE SCHEMA IF NOT EXISTS gate_context",
            &none,
        )
        .await;
        let chosen = SessionContext {
            catalog: Some("jykre".into()),
            schema: Some("gate_context".into()),
        };
        assert_eq!(
            text_in(&mut session, "SELECT current_schema()", &chosen).await,
            "gate_context"
        );
        // The user's SET is not undone by the next statement of the same tab.
        text_in(&mut session, "SET search_path TO public", &chosen).await;
        assert_eq!(
            text_in(&mut session, "SELECT current_schema()", &chosen).await,
            "public"
        );
        let reported = session.context(CancellationToken::new()).await.unwrap();
        assert_eq!(reported.catalog.as_deref(), Some("jykre"));
        assert_eq!(reported.schema.as_deref(), Some("public"));
        // Choosing again applies the new choice once.
        assert_eq!(
            text_in(&mut session, "SELECT current_schema()", &chosen).await,
            "gate_context"
        );
        // Another database cannot be chosen within the session.
        let other = SessionContext {
            catalog: Some("other".into()),
            schema: None,
        };
        assert!(
            session
                .execute(
                    &QueryRequest {
                        parameters: Vec::new(),
                        sql: "SELECT 1".into(),
                        options: ExecutionOptions {
                            context: other,
                            ..ExecutionOptions::default()
                        },
                    },
                    &mut Sink::default(),
                    CancellationToken::new(),
                )
                .await
                .is_err()
        );
        text_in(&mut session, "DROP SCHEMA gate_context", &none).await;
        session.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_a_report_session_opens_on_another_database() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = PostgresDriver;
        let other = SessionContext {
            catalog: Some("postgres".into()),
            schema: Some("public".into()),
        };
        // Without the server's other databases, the context is refused.
        assert_eq!(
            driver
                .session_connection(&input(), &other)
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
        let mut every = input();
        every.config.options["allDatabases"] = json!(true);
        let connection = driver.session_connection(&every, &other).unwrap();
        let mut session = driver
            .open_session(&connection, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            text_in(&mut session, "SELECT current_database()", &other).await,
            "postgres"
        );
        let reported = session.context(CancellationToken::new()).await.unwrap();
        assert_eq!(reported.catalog.as_deref(), Some("postgres"));
        assert_eq!(reported.schema.as_deref(), Some("public"));
        // The session stays in the database it opened on.
        let home = SessionContext {
            catalog: Some("jykre".into()),
            schema: None,
        };
        assert!(
            session
                .execute(
                    &QueryRequest {
                        parameters: Vec::new(),
                        sql: "SELECT 1".into(),
                        options: ExecutionOptions {
                            context: home,
                            ..ExecutionOptions::default()
                        },
                    },
                    &mut Sink::default(),
                    CancellationToken::new(),
                )
                .await
                .is_err()
        );
        session.close().await.unwrap();
    })
    .await
    .unwrap();
}

/// The transaction state the session reports.
async fn state(session: &mut Box<dyn Session>) -> Option<TransactionStatus> {
    session.transaction(CancellationToken::new()).await.unwrap()
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_the_transaction_state_is_reported_without_taking_a_snapshot() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = PostgresDriver;
        let mut session = driver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        let none = SessionContext::default();
        text_in(&mut session, "SELECT 1", &none).await;
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Idle));
        text_in(&mut session, "BEGIN", &none).await;
        assert_eq!(
            state(&mut session).await,
            Some(TransactionStatus::Open {
                changes: Some(false)
            })
        );
        // Asking (and the context) took no snapshot: SET TRANSACTION, which
        // must come before any query, still works in a later run.
        let context = session.context(CancellationToken::new()).await.unwrap();
        assert_eq!(context.catalog.as_deref(), Some("jykre"));
        text_in(
            &mut session,
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            &none,
        )
        .await;
        assert_eq!(
            text_in(&mut session, "SHOW transaction_isolation", &none).await,
            "repeatable read"
        );
        text_in(&mut session, "CREATE TEMP TABLE gate_tx (a int)", &none).await;
        assert_eq!(
            state(&mut session).await,
            Some(TransactionStatus::Open {
                changes: Some(true)
            })
        );
        // A failure leaves the transaction failed until ROLLBACK.
        let mut sink = Sink::default();
        let failed = session
            .execute(
                &QueryRequest {
                    parameters: Vec::new(),
                    sql: "SELECT missing_column FROM gate_tx".into(),
                    options: ExecutionOptions::default(),
                },
                &mut sink,
                CancellationToken::new(),
            )
            .await;
        assert!(failed.is_err());
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Failed));
        text_in(&mut session, "ROLLBACK", &none).await;
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Idle));
        session.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_commit_and_chain_opens_a_transaction_without_taking_its_snapshot() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = PostgresDriver;
        let mut session = driver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        let none = SessionContext::default();
        text_in(&mut session, "BEGIN", &none).await;
        text_in(&mut session, "SELECT 1", &none).await;
        // The chained transaction is open, and neither the state nor the
        // context took its snapshot: SET TRANSACTION still works.
        text_in(&mut session, "COMMIT AND CHAIN", &none).await;
        assert_eq!(
            state(&mut session).await,
            Some(TransactionStatus::Open {
                changes: Some(false)
            })
        );
        session.context(CancellationToken::new()).await.unwrap();
        text_in(
            &mut session,
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            &none,
        )
        .await;
        assert_eq!(
            text_in(&mut session, "SHOW transaction_isolation", &none).await,
            "serializable"
        );
        // AND NO CHAIN leaves nothing open.
        text_in(&mut session, "COMMIT AND NO CHAIN", &none).await;
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Idle));
        session.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_levels_partitions_inspection_permissions_and_databases() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let driver = PostgresDriver;
        let input = input();
        let cancel = CancellationToken::new();
        let db = ObjectPath::default().child(ObjectKind::Database, "jykre");
        let public = db.child(ObjectKind::Schema, "public");
        // TLS and the search path do not change which catalog a cache holds.
        let mut tuned = input.config.clone();
        tuned.options["sslMode"] = json!("require");
        tuned.options["searchPath"] = json!("sales, public");
        assert_eq!(
            driver.cache_identity(&tuned),
            driver.cache_identity(&input.config)
        );
        assert_eq!(
            driver.default_namespaces(&tuned),
            [db.child(ObjectKind::Schema, "sales")]
        );
        // A session starts with the configured search_path.
        let mut archive_first = input.clone();
        archive_first.config.options["searchPath"] = json!("gate_archive, public");
        let mut session = driver
            .open_session(&archive_first, cancel.clone())
            .await
            .unwrap();
        assert_eq!(
            text_in(
                &mut session,
                "SELECT current_schema()",
                &SessionContext::default()
            )
            .await,
            "gate_archive"
        );
        session.close().await.unwrap();
        // Names only: relations without their columns.
        let mut names = Metadata::default();
        driver
            .introspect_level(
                &input,
                &public,
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
                .any(|object| object.kind == ObjectKind::Column)
        );
        // A partition kept in another schema hangs under its root table and
        // is read where it is stored.
        let mut full = Metadata::default();
        driver
            .introspect_scoped(&input, &public, &mut full, cancel.clone())
            .await
            .unwrap();
        let events = public.child(ObjectKind::Table, "gate_events");
        let archived = full
            .0
            .iter()
            .find(|object| object.path == events.child(ObjectKind::Partition, "gate_events_2025"))
            .expect("the archived partition under its root");
        assert_eq!(
            archived.stored_at,
            Some(
                db.child(ObjectKind::Schema, "gate_archive")
                    .child(ObjectKind::Table, "gate_events_2025")
            )
        );
        let mut archive = Metadata::default();
        driver
            .introspect_scoped(
                &input,
                &db.child(ObjectKind::Schema, "gate_archive"),
                &mut archive,
                cancel.clone(),
            )
            .await
            .unwrap();
        assert!(
            !archive
                .0
                .iter()
                .any(|object| object.kind == ObjectKind::Table)
        );
        // Inspection: what the refresh does not read.
        let fixture = public.child(ObjectKind::Table, "gate_fixture");
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
        assert!(has(InspectedKind::PrimaryKey, "gate_fixture_pkey"));
        assert!(has(InspectedKind::Trigger, "gate_fixture_touch"));
        assert!(
            !full
                .0
                .iter()
                .any(|object| object.name == "gate_fixture_label")
        );
        let schema = driver
            .inspect(&input, &public, cancel.clone())
            .await
            .unwrap();
        let detail = |kind, name: &str| {
            schema
                .items
                .iter()
                .find(|item| item.kind == kind && item.name == name)
                .map(|item| item.detail.clone())
        };
        assert_eq!(
            detail(InspectedKind::Function, "gate_touch").as_deref(),
            Some("(day date, steps integer) → date")
        );
        assert!(
            detail(InspectedKind::Sequence, "gate_counter")
                .is_some_and(|text| text.contains("start 10"))
        );
        assert_eq!(
            detail(InspectedKind::Type, "gate_mood").as_deref(),
            Some("enum: calm, busy")
        );
        // Definitions only by explicit action.
        let view = driver
            .definition(
                &input,
                &public.child(ObjectKind::View, "gate_fixture_view"),
                cancel.clone(),
            )
            .await
            .unwrap();
        assert!(view.starts_with("CREATE VIEW \"public\".\"gate_fixture_view\" AS"));
        let table = driver
            .definition(&input, &fixture, cancel.clone())
            .await
            .unwrap();
        assert!(table.contains("\"amount\" numeric(20,2)"));
        assert!(table.contains("CONSTRAINT \"gate_fixture_amount\" CHECK"));
        // A chosen schema the login may not use is a warning, not a gap.
        let mut reader = input.clone();
        reader.config.options["user"] = json!("gate_reader");
        let private = db.child(ObjectKind::Schema, "gate_private");
        let mut denied = Metadata::default();
        let report = driver
            .introspect_scoped(&reader, &private, &mut denied, cancel.clone())
            .await
            .unwrap();
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].path, private);
        assert!(denied.0.is_empty());
        // The server's other databases, when the profile offers them.
        let mut every = input.clone();
        every.config.options["allDatabases"] = json!(true);
        let databases = driver
            .discover_namespaces(&every, &ObjectPath::default(), cancel.clone())
            .await
            .unwrap();
        let names: Vec<_> = databases
            .iter()
            .map(|object| object.name.as_str())
            .collect();
        assert!(names.contains(&"jykre") && names.contains(&"postgres"));
        let other = ObjectPath::default().child(ObjectKind::Database, "postgres");
        driver
            .discover_namespaces(&every, &other, cancel.clone())
            .await
            .unwrap();
        assert!(
            driver
                .discover_namespaces(&input, &other, cancel)
                .await
                .is_err()
        );
    })
    .await
    .expect("live metadata extensions took too long");
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
        let driver = PostgresDriver;
        let cancel = CancellationToken::new();
        let mut session = driver.open_session(&input(), cancel.clone()).await.unwrap();
        let sql = "SELECT :who AS who, :n + 1 AS next, :d AS day, :b AS flag, :ts AS at, \
                   :missing AS missing, length(:who) AS len";
        for text in [
            "O'Brien",
            "x'); DROP TABLE t; --",
            "back\\slash ''",
            "$$ dollar $$",
            "/* c */ -- d",
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
                    Cell::Text("t".into()),
                    Cell::Text("2024-02-29 23:59:59.5".into()),
                    Cell::Null,
                    Cell::Text(text.chars().count().to_string()),
                ]],
                "{text:?}"
            );
        }
        // Paging keeps reading the result the bound values chose.
        let sql = "SELECT g FROM generate_series(1, :last) AS g WHERE g > :first";
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

/// A sink that keeps the columns, for editing (ADR-0039).
#[derive(Default)]
struct Described {
    columns: Vec<ColumnInfo>,
    rows: Vec<Row>,
}
impl RowSink for Described {
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> SinkControl {
        self.columns = columns;
        SinkControl::Continue
    }
    fn row(&mut self, row: Row) -> SinkControl {
        self.rows.push(row);
        SinkControl::Continue
    }
}

async fn read(session: &mut Box<dyn Session>, sql: &str) -> DriverResult<Described> {
    let mut sink = Described::default();
    session
        .execute(
            &QueryRequest::new(sql, ExecutionOptions::default()),
            &mut sink,
            CancellationToken::new(),
        )
        .await?;
    Ok(sink)
}

async fn target_of(session: &mut Box<dyn Session>, sql: &str) -> write::EditTarget {
    let sink = read(session, sql).await.unwrap();
    session
        .edit_target(sql, &sink.columns, CancellationToken::new())
        .await
        .unwrap()
}

async fn write_changes(
    session: &mut Box<dyn Session>,
    target: &write::EditableTable,
    changes: Vec<write::RowChange>,
) -> DriverResult<write::WriteOutcome> {
    let request = write::WriteRequest {
        target: target.clone(),
        changes,
        timeout: Some(Duration::from_secs(30)),
        scope: write::WriteScope::Atomic,
    };
    session.write(&request, CancellationToken::new()).await
}

fn text(value: &str) -> write::WriteValue {
    write::WriteValue::Text(value.into())
}

/// The tables an editing test reads and writes, with the names starting
/// with `prefix`: each test has its own, since tests run at the same time.
fn edit_setup(prefix: &str) -> Vec<String> {
    EDIT_SETUP
        .iter()
        .map(|sql| sql.replace("jykre_edit", prefix))
        .collect()
}

const EDIT_SETUP: [&str; 9] = [
    "DROP VIEW IF EXISTS jykre_edit_view",
    "DROP TABLE IF EXISTS jykre_edit_refs, jykre_edit_items, jykre_edit_loose, jykre_edit_measures",
    "CREATE TABLE jykre_edit_items (id bigint PRIMARY KEY, amount numeric(38,10), note text, at timestamptz, raw bytea, label varchar(3), doubled bigint GENERATED ALWAYS AS (id * 2) STORED)",
    "CREATE TABLE jykre_edit_refs (id int PRIMARY KEY, item bigint REFERENCES jykre_edit_items ON DELETE CASCADE)",
    "CREATE TABLE jykre_edit_loose (a text)",
    "CREATE TABLE jykre_edit_measures (v double precision PRIMARY KEY, w text)",
    "CREATE VIEW jykre_edit_view AS SELECT id, note FROM jykre_edit_items",
    "INSERT INTO jykre_edit_items (id, amount, note) VALUES (1, 1, 'one'), (2, 2, 'two')",
    "SET TIME ZONE 'UTC'",
];

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_results_say_whether_they_can_be_written_back() {
    use write::{EditTarget, KeyKind, ReadOnlyReason, ReferenceAction, SideEffect};
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut session = PostgresDriver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        for sql in edit_setup("jykre_target") {
            read(&mut session, &sql).await.unwrap();
        }
        let items = target_of(&mut session, "SELECT * FROM jykre_target_items")
            .await
            .editable()
            .unwrap();
        assert_eq!((items.key.clone(), items.key_kind), (vec![0], KeyKind::PrimaryKey));
        assert!(!items.columns[6].as_ref().unwrap().writable, "generated");
        assert_eq!(items.table.relation(), Some("jykre_target_items"));
        assert!(items.side_effects.contains(&SideEffect::Reference {
            table: "jykre_target_refs".into(),
            name: "jykre_target_refs_item_fkey".into(),
            on_delete: Some(ReferenceAction::Cascade),
            on_update: None,
        }));
        for (sql, reason) in [
            ("SELECT 1 AS n", ReadOnlyReason::ComputedOnly),
            (
                "SELECT i.note, r.id FROM jykre_target_items i JOIN jykre_target_refs r ON r.item = i.id",
                ReadOnlyReason::SeveralTables,
            ),
            (
                "SELECT * FROM jykre_target_view",
                ReadOnlyReason::NotATable {
                    kind: "view".into(),
                },
            ),
            (
                "SELECT note FROM jykre_target_items",
                ReadOnlyReason::KeyNotInResult {
                    missing: vec!["id".into()],
                },
            ),
            ("SELECT a FROM jykre_target_loose", ReadOnlyReason::NoKey),
            (
                "SELECT * FROM jykre_target_measures",
                ReadOnlyReason::KeyType { column: "v".into() },
            ),
        ] {
            assert_eq!(
                target_of(&mut session, sql).await,
                EditTarget::ReadOnly(reason),
                "{sql}"
            );
        }
        for sql in edit_setup("jykre_target").iter().take(2) {
            read(&mut session, sql).await.unwrap();
        }
        session.close().await.unwrap();
    })
    .await
    .expect("live edit targets took too long");
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_edits_are_bound_exact_and_honour_the_transaction() {
    use write::{RowChange, WriteOutcome, WriteValue};
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut session = PostgresDriver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        for sql in edit_setup("jykre_edit") {
            read(&mut session, &sql).await.unwrap();
        }
        let sql = "SELECT id, amount, note, at, raw, label FROM jykre_edit_items";
        let items = target_of(&mut session, sql).await.editable().unwrap();

        // Outside a transaction: exact numbers, NULL apart from the text
        // NULL, a timestamp with its offset and bytes as their \x text.
        let written = write_changes(
            &mut session,
            &items,
            vec![
                RowChange::Update {
                    key: vec![text("1")],
                    set: vec![
                        (1, text("12345678901234567890.0123456789")),
                        (2, text("NULL")),
                        (3, text("2026-11-02 10:00:00+03")),
                        (4, text("\\x00ff")),
                    ],
                },
                RowChange::Insert {
                    values: vec![
                        (0, text("9007199254740993")),
                        (2, WriteValue::Null),
                        (5, text("abc")),
                    ],
                },
                RowChange::Update {
                    key: vec![text("2")],
                    set: vec![(2, text(""))],
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            written,
            WriteOutcome::Written {
                rows: 3,
                committed: true
            }
        );
        let rows = read(
            &mut session,
            "SELECT id, amount, note, at, raw, label FROM jykre_edit_items ORDER BY id",
        )
        .await
        .unwrap()
        .rows;
        let text_cell = |value: &str| Cell::Text(value.into());
        assert_eq!(
            rows,
            vec![
                vec![
                    text_cell("1"),
                    text_cell("12345678901234567890.0123456789"),
                    text_cell("NULL"),
                    text_cell("2026-11-02 07:00:00+00"),
                    text_cell("\\x00ff"),
                    Cell::Null
                ],
                vec![
                    text_cell("2"),
                    text_cell("2.0000000000"),
                    text_cell(""),
                    Cell::Null,
                    Cell::Null,
                    Cell::Null
                ],
                vec![
                    text_cell("9007199254740993"),
                    Cell::Null,
                    Cell::Null,
                    Cell::Null,
                    Cell::Null,
                    text_cell("abc")
                ],
            ]
        );

        // A text too long for its column fails; it is never cut.
        let refused = write_changes(
            &mut session,
            &items,
            vec![RowChange::Update {
                key: vec![text("1")],
                set: vec![(5, text("abcd"))],
            }],
        )
        .await
        .unwrap_err();
        assert_eq!(refused.code, ErrorCode::Database);

        // A change that matches no row rolls the whole request back.
        let rolled = write_changes(
            &mut session,
            &items,
            vec![
                RowChange::Delete {
                    key: vec![text("2")],
                },
                RowChange::Update {
                    key: vec![text("999")],
                    set: vec![(2, text("nobody"))],
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            rolled,
            WriteOutcome::RolledBack {
                index: 1,
                matched: 0
            }
        );
        let count = read(&mut session, "SELECT count(*) FROM jykre_edit_items")
            .await
            .unwrap()
            .rows;
        assert_eq!(count, vec![vec![text_cell("3")]]);

        // Inside an open transaction the change stays uncommitted, and a
        // failure there leaves the transaction usable.
        read(&mut session, "BEGIN").await.unwrap();
        read(
            &mut session,
            "UPDATE jykre_edit_items SET note = 'mine' WHERE id = 2",
        )
        .await
        .unwrap();
        let inside = write_changes(
            &mut session,
            &items,
            vec![RowChange::Update {
                key: vec![text("1")],
                set: vec![(2, text("inside"))],
            }],
        )
        .await
        .unwrap();
        assert_eq!(
            inside,
            WriteOutcome::Written {
                rows: 1,
                committed: false
            }
        );
        let failed = write_changes(
            &mut session,
            &items,
            vec![RowChange::Update {
                key: vec![text("1")],
                set: vec![(1, text("not a number"))],
            }],
        )
        .await;
        assert!(failed.is_err());
        assert_eq!(
            session.transaction(CancellationToken::new()).await.unwrap(),
            Some(TransactionStatus::Open {
                changes: Some(true)
            })
        );
        let mine = read(
            &mut session,
            "SELECT note FROM jykre_edit_items WHERE id IN (1, 2) ORDER BY id",
        )
        .await
        .unwrap()
        .rows;
        assert_eq!(
            mine,
            vec![vec![text_cell("inside")], vec![text_cell("mine")]]
        );
        read(&mut session, "ROLLBACK").await.unwrap();
        let after = read(
            &mut session,
            "SELECT note FROM jykre_edit_items WHERE id = 1",
        )
        .await
        .unwrap()
        .rows;
        assert_eq!(after, vec![vec![text_cell("NULL")]]);

        // A table changed since the result was read is not written. The view
        // on the column would keep its type from changing.
        read(&mut session, "DROP VIEW jykre_edit_view")
            .await
            .unwrap();
        read(
            &mut session,
            "ALTER TABLE jykre_edit_items ALTER COLUMN note TYPE varchar(100)",
        )
        .await
        .unwrap();
        let changed = write_changes(
            &mut session,
            &items,
            vec![RowChange::Delete {
                key: vec![text("1")],
            }],
        )
        .await
        .unwrap_err();
        assert_eq!(changed.code, ErrorCode::Validation);
        for sql in edit_setup("jykre_edit").iter().take(2) {
            read(&mut session, sql).await.unwrap();
        }
        session.close().await.unwrap();
    })
    .await
    .expect("live edits took too long");
}

/// An import's writes (ADR-0040): the table's description, one transaction
/// the import begins and ends, every value bound as text the server
/// converts, a boolean as a boolean, and a rejected value failing at its
/// change with nothing left behind.
#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_an_import_writes_in_one_transaction_or_leaves_nothing() {
    use write::{RowChange, WriteOutcome, WriteScope, WriteValue};
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut session = PostgresDriver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        for sql in [
            "DROP TABLE IF EXISTS jykre_import",
            "CREATE TABLE jykre_import (id bigint GENERATED BY DEFAULT AS IDENTITY, amount numeric(38,10), big bigint, day date, note text NOT NULL, flag boolean, doubled bigint GENERATED ALWAYS AS (big * 2) STORED)",
        ] {
            read(&mut session, sql).await.unwrap();
        }
        let path = ObjectPath::default()
            .child(ObjectKind::Database, "jykre")
            .child(ObjectKind::Schema, "public")
            .child(ObjectKind::Table, "jykre_import");
        let target = session
            .table_target(&path, CancellationToken::new())
            .await
            .unwrap()
            .editable()
            .unwrap();
        assert!(target.key.is_empty());
        let names: Vec<(&str, bool, bool)> = target
            .columns
            .iter()
            .flatten()
            .map(|column| (column.name.as_str(), column.writable, column.has_default))
            .collect();
        assert_eq!(
            names,
            [
                ("id", true, true),
                ("amount", true, true),
                ("big", true, true),
                ("day", true, true),
                ("note", true, false),
                ("flag", true, true),
                ("doubled", false, true),
            ]
        );
        let row = |amount: &str, big: &str, day: &str, note: WriteValue, flag: WriteValue| {
            RowChange::Insert {
                values: vec![
                    (1, text(amount)),
                    (2, text(big)),
                    (3, text(day)),
                    (4, note),
                    (5, flag),
                ],
            }
        };
        let request = |changes: Vec<RowChange>| write::WriteRequest {
            target: target.clone(),
            changes,
            timeout: Some(Duration::from_secs(30)),
            scope: WriteScope::Caller,
        };
        session.begin_writes(CancellationToken::new()).await.unwrap();
        let written = session
            .write(
                &request(vec![
                    row(
                        "12345678901234567890123456.1234567891",
                        "9007199254740993",
                        "2025-01-31 00:00:00",
                        text("NULL"),
                        WriteValue::Boolean(true),
                    ),
                    row("0.1", "-1", "2025-02-01", text(""), WriteValue::Null),
                ]),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            written,
            WriteOutcome::Written {
                rows: 2,
                committed: false
            }
        );
        session.end_writes(true, CancellationToken::new()).await.unwrap();
        let rows = read(
            &mut session,
            "SELECT amount::text, big::text, day::text, note, flag::text, doubled::text FROM jykre_import ORDER BY id",
        )
        .await
        .unwrap()
        .rows;
        assert_eq!(
            rows[0],
            [
                Cell::Text("12345678901234567890123456.1234567891".into()),
                Cell::Text("9007199254740993".into()),
                Cell::Text("2025-01-31".into()),
                Cell::Text("NULL".into()),
                Cell::Text("true".into()),
                Cell::Text("18014398509481986".into()),
            ],
            "exact numbers, the text NULL and a boolean as written"
        );
        assert_eq!(rows[1][3], Cell::Text(String::new()), "empty text stays text");
        // A value the database rejects fails at its change; the rollback
        // leaves the table as it was.
        session.begin_writes(CancellationToken::new()).await.unwrap();
        let failed = session
            .write(
                &request(vec![
                    row("1", "1", "2025-01-01", text("kept?"), WriteValue::Null),
                    row("not a number", "1", "2025-01-01", text("x"), WriteValue::Null),
                ]),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(failed.change(), Some(1));
        assert!(failed.server.is_some(), "the server's own message");
        session.end_writes(false, CancellationToken::new()).await.unwrap();
        let count = read(&mut session, "SELECT count(*)::text FROM jykre_import")
            .await
            .unwrap()
            .rows;
        assert_eq!(count, [vec![Cell::Text("2".into())]]);
        // A required column left NULL is rejected the same way.
        session.begin_writes(CancellationToken::new()).await.unwrap();
        let failed = session
            .write(
                &request(vec![row("1", "1", "2025-01-01", WriteValue::Null, WriteValue::Null)]),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(failed.change(), Some(0));
        session.end_writes(false, CancellationToken::new()).await.unwrap();
        read(&mut session, "DROP TABLE jykre_import").await.unwrap();
        session.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_a_write_left_open_is_not_undone_by_the_next_statement() {
    /// Takes the first rows, then stops reading.
    struct Few(usize);
    impl RowSink for Few {
        fn columns(&mut self, _: Vec<ColumnInfo>) -> SinkControl {
            SinkControl::Continue
        }
        fn row(&mut self, _: Row) -> SinkControl {
            self.0 += 1;
            if self.0 >= 10 {
                SinkControl::Stop
            } else {
                SinkControl::Continue
            }
        }
    }
    async fn run(session: &mut Box<dyn Session>, sql: &str, sink: &mut dyn RowSink) -> bool {
        session
            .execute(
                &QueryRequest {
                    parameters: Vec::new(),
                    sql: sql.into(),
                    options: ExecutionOptions {
                        max_rows: 100,
                        ..ExecutionOptions::default()
                    },
                },
                sink,
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .has_more
    }
    async fn total(session: &mut Box<dyn Session>) -> Row {
        let mut sink = Sink::default();
        run(
            session,
            "SELECT sum(n)::text FROM live_cut_write",
            &mut sink,
        )
        .await;
        sink.rows.remove(0)
    }
    let driver = PostgresDriver;
    let input = input();
    let mut session = driver
        .open_session(&input, CancellationToken::new())
        .await
        .unwrap();
    for sql in [
        "DROP TABLE IF EXISTS live_cut_write",
        "CREATE TABLE live_cut_write AS SELECT 0 AS n FROM generate_series(1, 5000)",
        "CREATE OR REPLACE FUNCTION live_cut_log(i int) RETURNS text LANGUAGE sql AS \
         $$ INSERT INTO live_cut_write VALUES (1) RETURNING repeat('x', 10000) $$",
    ] {
        run(&mut session, sql, &mut Sink::default()).await;
    }
    // About 50 MB of returned rows each, so the server is still sending
    // them when the next statement arrives. PostgreSQL finishes an `UPDATE
    // … RETURNING` before it sends its first row, but a `SELECT` that writes
    // as it goes (here, a row per call) is still running.
    let update = "UPDATE live_cut_write SET n = n + 1 RETURNING n, repeat('x', 10000)";
    let select = "SELECT live_cut_log(i) FROM generate_series(1, 5000) AS i";

    // A page of each write is shown, then the tab runs something else.
    assert!(run(&mut session, update, &mut Sink::default()).await);
    assert_eq!(total(&mut session).await, vec![Cell::Text("5000".into())]);
    assert!(run(&mut session, select, &mut Sink::default()).await);
    assert_eq!(total(&mut session).await, vec![Cell::Text("10000".into())]);

    // The sink stops reading (as the memory ceiling does).
    run(&mut session, select, &mut Few(0)).await;
    assert_eq!(total(&mut session).await, vec![Cell::Text("15000".into())]);

    // Another session sees the writes committed.
    let mut other = driver
        .open_session(&input, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(total(&mut other).await, vec![Cell::Text("15000".into())]);
    for sql in [
        "DROP FUNCTION live_cut_log(int)",
        "DROP TABLE live_cut_write",
    ] {
        run(&mut other, sql, &mut Sink::default()).await;
    }
    other.close().await.unwrap();
    session.close().await.unwrap();
}
