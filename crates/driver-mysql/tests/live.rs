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
fn fixture_port() -> u16 {
    // The fixture port is chosen by Docker for each run: `cargo xtask check
    // --full` sets it, and docs/standards/testing.md shows how to run these by hand.
    std::env::var("JYKRE_TEST_PORT")
        .expect("JYKRE_TEST_PORT must name the fixture port (see docs/standards/testing.md)")
        .parse()
        .expect("JYKRE_TEST_PORT must be a valid port")
}
fn input() -> ResolvedConnection {
    input_at(fixture_port())
}
fn input_at(port: u16) -> ResolvedConnection {
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
/// A relay on 127.0.0.1 to the fixture that hands the server's bytes on at
/// [`SlowLink::RATE`], a piece at a time, so a client reading a long row
/// waits for the rest of it between pieces. A direct connection does not
/// guarantee that: under load the client falls behind the server, which
/// through Docker's port forwarding can be many megabytes ahead, and its
/// reads then never wait. Requests go through unchanged, and TLS is still
/// negotiated end to end.
struct SlowLink {
    port: u16,
    stop: CancellationToken,
}
impl SlowLink {
    /// Well below what this debug client reads on a loaded machine (130 to
    /// 160 MiB/s at a load average of 17 on 8 cores), and fast enough to
    /// drain what the server sent ahead of an interruption well within the
    /// driver's 5-second recovery (at most 0.7 s there).
    const RATE: f64 = 32.0 * 1024.0 * 1024.0;
    const PIECE: usize = 16 * 1024;
    /// How far a late wake-up is made up for, so a loaded machine does not
    /// slow the link down while it never bursts more than a few pieces.
    const CATCH_UP: Duration = Duration::from_millis(5);

    fn to(upstream: u16) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the relay");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        // Its own thread and runtime, like a network link: the test's
        // runtime runs the relay only when the client lets it.
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("start the relay's runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                loop {
                    tokio::select! {
                        _ = stopped.cancelled() => return,
                        accepted = listener.accept() => match accepted {
                            Ok((client, _)) => {
                                tokio::spawn(Self::relay(client, upstream));
                            }
                            Err(_) => return,
                        },
                    }
                }
            });
        });
        Self { port, stop }
    }

    async fn relay(client: tokio::net::TcpStream, upstream: u16) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let Ok(server) = tokio::net::TcpStream::connect(("127.0.0.1", upstream)).await else {
            return;
        };
        let (mut from_client, mut to_client) = client.into_split();
        let (mut from_server, mut to_server) = server.into_split();
        let requests = async {
            let _ = tokio::io::copy(&mut from_client, &mut to_server).await;
            let _ = to_server.shutdown().await;
        };
        let responses = async {
            let mut piece = vec![0; Self::PIECE];
            let mut due = tokio::time::Instant::now();
            loop {
                let read = match from_server.read(&mut piece).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                if to_client.write_all(&piece[..read]).await.is_err() {
                    break;
                }
                let now = tokio::time::Instant::now();
                due = due.max(now.checked_sub(Self::CATCH_UP).unwrap_or(now))
                    + Duration::from_secs_f64(read as f64 / Self::RATE);
                tokio::time::sleep_until(due).await;
            }
            let _ = to_client.shutdown().await;
        };
        tokio::join!(requests, responses);
    }
}
impl Drop for SlowLink {
    /// Stops accepting and closes the relayed connections.
    fn drop(&mut self) {
        self.stop.cancel();
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
    // The fixture's profile requires TLS: the server names its version.
    assert!(matches!(
        session.encryption(cancel.clone()).await.unwrap(),
        Encryption::Tls { version: Some(version) } if version.starts_with("TLS")
    ));
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
    assert_eq!(server.position, None);
    assert_kept(&mut session).await;
    // A syntax error gives no position of its own; the driver finds the
    // token MySQL quotes in the message: the second `FROM`, on the second
    // line.
    let failed = session
        .execute(
            &QueryRequest {
                parameters: Vec::new(),
                sql: "SELECT 1 AS n,\n  2 FROM FROM dual".into(),
                options: ExecutionOptions::default(),
            },
            &mut Sink::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    let server = failed.server.clone().unwrap();
    assert_eq!(server.vendor_code.as_deref(), Some("1064"), "{failed:?}");
    assert_eq!(server.position, Some(25), "{failed:?}");
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
/// several megabytes, handed on by a [`SlowLink`], keep the reader waiting
/// inside a payload most of the time, so a cancellation or a time limit
/// almost always lands halfway through a row; the session must still be
/// kept, with its state.
///
/// Without the link, under load the server stays ahead of the client: the
/// socket always holds more of the result, SQLx's reads never wait, and on
/// this single-threaded runtime the sink that cancels does not run before
/// the whole result has been read, while the time limit is only seen
/// between rows (docs/standards/testing.md, *Flaky tests*).
#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_interrupting_a_half_read_row_keeps_the_session() {
    let link = SlowLink::to(fixture_port());
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
            .open_session(&input_at(link.port), CancellationToken::new())
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

/// The transaction state the session reports.
async fn state(session: &mut Box<dyn Session>) -> Option<TransactionStatus> {
    session.transaction(CancellationToken::new()).await.unwrap()
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_the_transaction_state_is_read_from_the_status_flags() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = MySqlDriver;
        let mut session = driver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        let none = SessionContext::default();
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Idle));
        let open = Some(TransactionStatus::Open { changes: None });
        text_in(&mut session, "BEGIN", &none).await;
        assert_eq!(state(&mut session).await, open);
        // A failed statement keeps the transaction.
        let mut sink = Sink::default();
        let failed = session
            .execute(
                &QueryRequest::new("SELECT * FROM gate_missing", ExecutionOptions::default()),
                &mut sink,
                CancellationToken::new(),
            )
            .await;
        assert!(failed.is_err());
        assert_eq!(state(&mut session).await, open);
        text_in(&mut session, "ROLLBACK", &none).await;
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Idle));
        // A transaction begun inside a procedure.
        text_in(&mut session, "DROP PROCEDURE IF EXISTS gate_begin", &none).await;
        text_in(
            &mut session,
            "CREATE PROCEDURE gate_begin() START TRANSACTION",
            &none,
        )
        .await;
        text_in(&mut session, "CALL gate_begin()", &none).await;
        assert_eq!(state(&mut session).await, open);
        text_in(&mut session, "ROLLBACK", &none).await;
        text_in(&mut session, "DROP PROCEDURE gate_begin", &none).await;
        // With autocommit off every statement waits for a COMMIT.
        text_in(&mut session, "SET autocommit = 0", &none).await;
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Manual));
        text_in(&mut session, "SET autocommit = 1", &none).await;
        assert_eq!(state(&mut session).await, Some(TransactionStatus::Idle));
        session.close().await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_a_deadlock_victim_reports_its_transaction_rolled_back() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = MySqlDriver;
        let none = SessionContext::default();
        let mut setup = driver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        text_in(&mut setup, "DROP TABLE IF EXISTS gate_deadlock", &none).await;
        text_in(
            &mut setup,
            "CREATE TABLE gate_deadlock (id INT PRIMARY KEY, v INT) ENGINE = InnoDB",
            &none,
        )
        .await;
        text_in(
            &mut setup,
            "INSERT INTO gate_deadlock VALUES (1, 0), (2, 0)",
            &none,
        )
        .await;
        let mut a = driver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        let mut b = driver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        text_in(&mut a, "BEGIN", &none).await;
        text_in(&mut b, "BEGIN", &none).await;
        text_in(&mut a, "UPDATE gate_deadlock SET v = 1 WHERE id = 1", &none).await;
        text_in(&mut b, "UPDATE gate_deadlock SET v = 1 WHERE id = 2", &none).await;
        // Each waits for the other's row; InnoDB rolls one back, and the
        // error it answers with carries no status flags.
        let update = |sql: &str| QueryRequest::new(sql, ExecutionOptions::default());
        let (first_update, second_update) = (
            update("UPDATE gate_deadlock SET v = 2 WHERE id = 2"),
            update("UPDATE gate_deadlock SET v = 2 WHERE id = 1"),
        );
        let (mut first_sink, mut second_sink) = (Sink::default(), Sink::default());
        let (first, second) = tokio::join!(
            a.execute(&first_update, &mut first_sink, CancellationToken::new()),
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                b.execute(&second_update, &mut second_sink, CancellationToken::new())
                    .await
            }
        );
        assert_ne!(
            first.is_err(),
            second.is_err(),
            "expected one deadlock victim: {first:?}, {second:?}"
        );
        let (victim, survivor) = if first.is_err() {
            (&mut a, &mut b)
        } else {
            (&mut b, &mut a)
        };
        assert_eq!(state(victim).await, Some(TransactionStatus::Idle));
        assert_eq!(
            state(survivor).await,
            Some(TransactionStatus::Open { changes: None })
        );
        text_in(survivor, "ROLLBACK", &none).await;
        text_in(&mut setup, "DROP TABLE gate_deadlock", &none).await;
        for session in [a, b, setup] {
            session.close().await.unwrap();
        }
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

/// Every column name a statement delivered, and its rows.
#[derive(Default)]
struct Described {
    columns: Vec<Vec<String>>,
    rows: Vec<Row>,
}
impl RowSink for Described {
    fn columns(&mut self, c: Vec<ColumnInfo>) -> SinkControl {
        self.columns
            .push(c.into_iter().map(|column| column.name).collect());
        SinkControl::Continue
    }
    fn row(&mut self, r: Row) -> SinkControl {
        self.rows.push(r);
        SinkControl::Continue
    }
}

/// Runs `sql` to the end of its first page, with at most `max_rows` rows.
async fn page(
    session: &mut Box<dyn Session>,
    sql: &str,
    max_rows: u32,
) -> (Described, DriverOutcome) {
    let mut sink = Described::default();
    let outcome = session
        .execute(
            &QueryRequest {
                parameters: Vec::new(),
                sql: sql.into(),
                options: ExecutionOptions {
                    max_rows,
                    ..ExecutionOptions::default()
                },
            },
            &mut sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    (sink, outcome)
}

fn texts(values: &[&str]) -> Row {
    values
        .iter()
        .map(|value| Cell::Text((*value).into()))
        .collect()
}

/// A prepared `CALL` describes no columns: they come with the procedure's
/// first row, which keeps its exact values and NULL. A run delivers the
/// first result set that has rows; the procedure still runs to its end.
#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_a_procedure_call_delivers_its_columns_and_values() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let driver = MySqlDriver;
        let mut session = driver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        keep_state(&mut session).await;
        for sql in [
            "DROP PROCEDURE IF EXISTS jykre_call_rows",
            "DROP PROCEDURE IF EXISTS jykre_call_sets",
            "DROP PROCEDURE IF EXISTS jykre_call_write_read",
            "DROP PROCEDURE IF EXISTS jykre_call_write",
            "DROP TABLE IF EXISTS jykre_call_table",
            "CREATE PROCEDURE jykre_call_rows() BEGIN \
             SELECT 1 AS a, 'x' AS b, NULL AS c, CAST(123456789.12 AS DECIMAL(20,2)) AS d \
             UNION ALL SELECT 2, 'NULL', 'z', CAST(-0.01 AS DECIMAL(20,2)); END",
            "CREATE PROCEDURE jykre_call_sets() BEGIN \
             SELECT 0 AS nothing FROM DUAL WHERE 1 = 0; \
             SELECT 1 AS first UNION ALL SELECT 2; \
             SELECT 'a' AS second, 'b' AS third; \
             SET @jykre_call_last := 'reached'; END",
            "CREATE TABLE jykre_call_table (id INT PRIMARY KEY, v INT NOT NULL)",
            "INSERT INTO jykre_call_table WITH RECURSIVE s(n) AS \
             (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 50) SELECT n, 0 FROM s",
            "CREATE PROCEDURE jykre_call_write_read() BEGIN \
             UPDATE jykre_call_table SET v = v + 1; \
             SELECT id, v FROM jykre_call_table ORDER BY id; END",
            "CREATE PROCEDURE jykre_call_write() BEGIN \
             UPDATE jykre_call_table SET v = v + 1; END",
        ] {
            page(&mut session, sql, 1_000).await;
        }

        // The same columns and values as the SELECT run directly.
        let (call, outcome) = page(&mut session, "CALL jykre_call_rows()", 1_000).await;
        assert_eq!(call.columns, vec![vec!["a", "b", "c", "d"]]);
        assert_eq!(
            call.rows,
            vec![
                vec![
                    Cell::Text("1".into()),
                    Cell::Text("x".into()),
                    Cell::Null,
                    Cell::Text("123456789.12".into()),
                ],
                texts(&["2", "NULL", "z", "-0.01"]),
            ]
        );
        assert_eq!(outcome.rows_affected, None, "rows came back");
        assert!(!outcome.has_more && !outcome.truncated);

        // Pages of a call's result come from the open result.
        let (first, outcome) = page(&mut session, "CALL jykre_call_rows()", 1).await;
        assert_eq!(first.columns.len(), 1);
        assert_eq!(first.rows.len(), 1);
        assert!(outcome.has_more);
        let mut rest = Described::default();
        let outcome = session
            .fetch(
                &ExecutionOptions::default(),
                &mut rest,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(rest.columns.is_empty(), "columns are sent once");
        assert_eq!(rest.rows, vec![texts(&["2", "NULL", "z", "-0.01"])]);
        assert!(!outcome.has_more);

        // An empty result set shows nothing; the first one with rows is
        // delivered and later ones are dropped, the procedure ending.
        let (sets, outcome) = page(&mut session, "CALL jykre_call_sets()", 1_000).await;
        assert_eq!(sets.columns, vec![vec!["first"]]);
        assert_eq!(sets.rows, vec![texts(&["1"]), texts(&["2"])]);
        assert_eq!(outcome.rows_affected, None);
        assert!(!outcome.has_more && !outcome.truncated);
        // A full page reads one row ahead, which may belong to the next set.
        let (sets, outcome) = page(&mut session, "CALL jykre_call_sets()", 2).await;
        assert_eq!(sets.rows, vec![texts(&["1"]), texts(&["2"])]);
        assert!(!outcome.has_more);
        let (last, _) = page(&mut session, "SELECT @jykre_call_last", 1_000).await;
        assert_eq!(last.rows, vec![texts(&["reached"])]);

        // A procedure that writes and then returns rows is read to its end:
        // every row, written once, also when a page cuts its result.
        let (all, outcome) = page(&mut session, "CALL jykre_call_write_read()", 1_000).await;
        assert_eq!(all.columns, vec![vec!["id", "v"]]);
        assert_eq!(all.rows.len(), 50);
        assert_eq!(all.rows[49], texts(&["50", "1"]));
        assert_eq!(outcome.rows_affected, None);
        let (cut, outcome) = page(&mut session, "CALL jykre_call_write_read()", 10).await;
        assert_eq!(cut.rows.len(), 10);
        assert_eq!(cut.rows[0], texts(&["1", "2"]));
        assert!(outcome.has_more);
        let (values, _) = page(
            &mut session,
            "SELECT GROUP_CONCAT(DISTINCT v) FROM jykre_call_table",
            1_000,
        )
        .await;
        assert_eq!(values.rows, vec![texts(&["2"])], "every row written twice");

        // A procedure without rows still reports what its last statement
        // changed.
        let (write, outcome) = page(&mut session, "CALL jykre_call_write()", 1_000).await;
        assert!(write.columns.is_empty() && write.rows.is_empty());
        assert_eq!(outcome.rows_affected, Some(50));
        assert_kept(&mut session).await;

        for sql in [
            "DROP PROCEDURE jykre_call_rows",
            "DROP PROCEDURE jykre_call_sets",
            "DROP PROCEDURE jykre_call_write_read",
            "DROP PROCEDURE jykre_call_write",
            "DROP TABLE jykre_call_table",
        ] {
            page(&mut session, sql, 1_000).await;
        }
        session.close().await.unwrap();
    })
    .await
    .expect("live scenario exceeded its time budget");
}

/// A sink that keeps the columns, for editing (ADR-0039).
#[derive(Default)]
struct Edited {
    columns: Vec<ColumnInfo>,
    rows: Vec<Row>,
}
impl RowSink for Edited {
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> SinkControl {
        self.columns = columns;
        SinkControl::Continue
    }
    fn row(&mut self, row: Row) -> SinkControl {
        self.rows.push(row);
        SinkControl::Continue
    }
}

async fn read(session: &mut Box<dyn Session>, sql: &str) -> DriverResult<Edited> {
    let mut sink = Edited::default();
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
    };
    session.write(&request, CancellationToken::new()).await
}

fn text(value: &str) -> write::WriteValue {
    write::WriteValue::Text(value.into())
}

const EDIT_SETUP: [&str; 11] = [
    "DROP VIEW IF EXISTS jykre_edit_view",
    "DROP TABLE IF EXISTS jykre_edit_refs, jykre_edit_items, jykre_edit_loose, jykre_edit_measures, jykre_edit_stamps, jykre_edit_myisam",
    "CREATE TABLE jykre_edit_items (id bigint unsigned PRIMARY KEY, amount decimal(65,30), note text, at datetime(3), raw varbinary(16), label varchar(3), doubled bigint GENERATED ALWAYS AS (id * 2) STORED) ENGINE=InnoDB",
    "CREATE TABLE jykre_edit_refs (id int PRIMARY KEY, item bigint unsigned, FOREIGN KEY (item) REFERENCES jykre_edit_items (id) ON DELETE CASCADE) ENGINE=InnoDB",
    "CREATE TABLE jykre_edit_loose (a text) ENGINE=InnoDB",
    "CREATE TABLE jykre_edit_measures (v double PRIMARY KEY, w text) ENGINE=InnoDB",
    "CREATE TABLE jykre_edit_stamps (t timestamp PRIMARY KEY, w text) ENGINE=InnoDB",
    "CREATE TABLE jykre_edit_myisam (id int PRIMARY KEY) ENGINE=MyISAM",
    "CREATE VIEW jykre_edit_view AS SELECT id, note FROM jykre_edit_items",
    "INSERT INTO jykre_edit_items (id, amount, note) VALUES (1, 1, 'one'), (2, 2, 'two')",
    "SET autocommit = 1",
];

#[tokio::test]
#[ignore = "requires jykre disposable Docker fixtures (cargo xtask check --full)"]
async fn live_results_say_whether_they_can_be_written_back() {
    use write::{EditTarget, KeyKind, ReadOnlyReason, ReferenceAction, SideEffect};
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut session = MySqlDriver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        for sql in EDIT_SETUP {
            read(&mut session, sql).await.unwrap();
        }
        let items = target_of(&mut session, "SELECT * FROM jykre_edit_items")
            .await
            .editable()
            .unwrap();
        assert_eq!(
            (items.key.clone(), items.key_kind),
            (vec![0], KeyKind::PrimaryKey)
        );
        assert!(!items.columns[6].as_ref().unwrap().writable, "generated");
        assert!(items.columns[4].as_ref().unwrap().binary, "varbinary");
        assert!(items.side_effects.contains(&SideEffect::Reference {
            table: "jykre.jykre_edit_refs".into(),
            name: "jykre_edit_refs_ibfk_1".into(),
            on_delete: Some(ReferenceAction::Cascade),
            on_update: None,
        }));
        for (sql, reason) in [
            ("SELECT 1 AS n", ReadOnlyReason::ComputedOnly),
            (
                "SELECT i.note, r.id FROM jykre_edit_items i JOIN jykre_edit_refs r ON r.item = i.id",
                ReadOnlyReason::SeveralTables,
            ),
            (
                "SELECT note FROM jykre_edit_items",
                ReadOnlyReason::KeyNotInResult {
                    missing: vec!["id".into()],
                },
            ),
            ("SELECT a FROM jykre_edit_loose", ReadOnlyReason::NoKey),
            (
                "SELECT * FROM jykre_edit_measures",
                ReadOnlyReason::KeyType { column: "v".into() },
            ),
            (
                "SELECT * FROM jykre_edit_stamps",
                ReadOnlyReason::KeyType { column: "t".into() },
            ),
            (
                "SELECT * FROM jykre_edit_myisam",
                ReadOnlyReason::NotTransactional,
            ),
        ] {
            assert_eq!(
                target_of(&mut session, sql).await,
                EditTarget::ReadOnly(reason),
                "{sql}"
            );
        }
        // Whether a view's columns name the view or its table depends on
        // the server's view algorithm; either way, never another table.
        match target_of(&mut session, "SELECT * FROM jykre_edit_view").await {
            EditTarget::Editable(table) => {
                assert_eq!(table.table.relation(), Some("jykre_edit_items"))
            }
            other => assert_eq!(
                other,
                EditTarget::ReadOnly(ReadOnlyReason::NotATable {
                    kind: "view".into()
                })
            ),
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
        let mut session = MySqlDriver
            .open_session(&input(), CancellationToken::new())
            .await
            .unwrap();
        for sql in EDIT_SETUP {
            read(&mut session, sql).await.unwrap();
        }
        let sql = "SELECT id, amount, note, at, raw, label FROM jykre_edit_items";
        let items = target_of(&mut session, sql).await.editable().unwrap();

        // Outside a transaction: exact numbers above 2^53, NULL apart from
        // the text NULL, temporal text as typed and bytes as bytes.
        let written = write_changes(
            &mut session,
            &items,
            vec![
                RowChange::Update {
                    key: vec![text("1")],
                    set: vec![
                        (1, text("12345678901234567890.0123456789")),
                        (2, text("NULL")),
                        (3, text("2026-11-02 10:00:00.123")),
                        (4, WriteValue::Bytes(vec![0, 0xff])),
                    ],
                },
                RowChange::Insert {
                    values: vec![
                        (0, text("18446744073709551615")),
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
                    text_cell("12345678901234567890.012345678900000000000000000000"),
                    text_cell("NULL"),
                    text_cell("2026-11-02 10:00:00.123"),
                    Cell::Binary(vec![0, 0xff]),
                    Cell::Null
                ],
                vec![
                    text_cell("2"),
                    text_cell("2.000000000000000000000000000000"),
                    text_cell(""),
                    Cell::Null,
                    Cell::Null,
                    Cell::Null
                ],
                vec![
                    text_cell("18446744073709551615"),
                    Cell::Null,
                    Cell::Null,
                    Cell::Null,
                    Cell::Null,
                    text_cell("abc")
                ],
            ]
        );

        // A value MySQL would cut or convert is refused, and nothing of the
        // request is written.
        for (column, value) in [(5, "abcd"), (1, "12x")] {
            let refused = write_changes(
                &mut session,
                &items,
                vec![
                    RowChange::Update {
                        key: vec![text("2")],
                        set: vec![(2, text("first"))],
                    },
                    RowChange::Update {
                        key: vec![text("1")],
                        set: vec![(column, text(value))],
                    },
                ],
            )
            .await;
            assert!(refused.is_err(), "{value}");
            let note = read(
                &mut session,
                "SELECT note FROM jykre_edit_items WHERE id = 2",
            )
            .await
            .unwrap()
            .rows;
            assert_eq!(note, vec![vec![text_cell("")]], "{value}");
        }

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
        let count = read(&mut session, "SELECT COUNT(*) FROM jykre_edit_items")
            .await
            .unwrap()
            .rows;
        assert_eq!(count, vec![vec![text_cell("3")]]);

        // Inside an open transaction, and with autocommit off, the change
        // stays uncommitted until the user ends it.
        for open in ["BEGIN", "SET autocommit = 0"] {
            read(&mut session, open).await.unwrap();
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
                },
                "{open}"
            );
            assert_ne!(
                session.transaction(CancellationToken::new()).await.unwrap(),
                Some(TransactionStatus::Idle),
                "{open}"
            );
            read(&mut session, "ROLLBACK").await.unwrap();
            read(&mut session, "SET autocommit = 1").await.unwrap();
            let after = read(
                &mut session,
                "SELECT note FROM jykre_edit_items WHERE id = 1",
            )
            .await
            .unwrap()
            .rows;
            assert_eq!(after, vec![vec![text_cell("NULL")]], "{open}");
        }

        // A table changed since the result was read is not written.
        read(
            &mut session,
            "ALTER TABLE jykre_edit_items MODIFY note varchar(100)",
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
        for sql in EDIT_SETUP.iter().take(2) {
            read(&mut session, sql).await.unwrap();
        }
        session.close().await.unwrap();
    })
    .await
    .expect("live edits took too long");
}
