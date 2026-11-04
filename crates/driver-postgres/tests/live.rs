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
    // --full` sets it, and docs/testing.md shows how to run these by hand.
    let port: u16 = std::env::var("JYKRE_TEST_PORT")
        .expect("JYKRE_TEST_PORT must name the fixture port (see docs/testing.md)")
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
