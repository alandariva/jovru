use super::*;
use crate::export::ExportFormat;
use std::path::Path;
use std::sync::Mutex;

fn columns() -> Vec<ResultColumn> {
    vec![
        ResultColumn::new("n", "int8"),
        ResultColumn::new("note", "text"),
    ]
}

/// Rows `from..to` as a held page, as the grid has them.
fn page(from: u32, to: u32) -> Arc<ResultData> {
    let rows: Vec<[String; 2]> = (from..to)
        .map(|n| [n.to_string(), format!("row {n}")])
        .collect();
    Arc::new(ResultData::from_rows(
        columns(),
        rows.iter()
            .map(|row| row.iter().map(|cell| Some(cell.as_str()))),
    ))
}

/// What the fake session was asked.
#[derive(Debug, Default)]
struct Calls {
    executed: usize,
    /// The parameter names and values of each statement run.
    parameters: Vec<Vec<(String, Option<String>)>>,
    /// The row limit of each fetch.
    fetches: Vec<u32>,
    contexts: usize,
}

/// A result of `total` rows, `sent` of them already read, paged like a
/// driver: at most `cap` rows per call, whatever the limit asks.
#[derive(Debug)]
struct PagedSession {
    total: u32,
    sent: u32,
    cap: u32,
    calls: Arc<Mutex<Calls>>,
    /// The fetch (counting from 1) that fails with a database error.
    fail_on: Option<usize>,
    /// The fetch (counting from 1) during which the user cancels.
    cancel_on: Option<usize>,
    open: bool,
}

impl PagedSession {
    fn new(total: u32, sent: u32, calls: &Arc<Mutex<Calls>>) -> Self {
        Self {
            total,
            sent,
            cap: u32::MAX,
            calls: calls.clone(),
            fail_on: None,
            cancel_on: None,
            open: true,
        }
    }

    fn send(&mut self, max_rows: u32, sink: &mut dyn RowSink) -> DriverOutcome {
        let end = self
            .total
            .min(self.sent.saturating_add(max_rows.min(self.cap)));
        while self.sent < end {
            let n = self.sent;
            self.sent += 1;
            let row = vec![Cell::Integer(i64::from(n)), Cell::Text(format!("row {n}"))];
            if sink.row(row) == SinkControl::Stop {
                // The rest of the result is dropped, as a driver does.
                self.sent = self.total;
                return DriverOutcome::default();
            }
        }
        DriverOutcome {
            has_more: self.sent < self.total,
            ..DriverOutcome::default()
        }
    }
}

#[async_trait]
impl Session for PagedSession {
    async fn execute(
        &mut self,
        request: &QueryRequest,
        sink: &mut dyn RowSink,
        _: CancellationToken,
    ) -> DriverResult<DriverOutcome> {
        {
            let mut calls = self.calls.lock().unwrap();
            calls.executed += 1;
            calls.parameters.push(
                request
                    .parameters
                    .iter()
                    .map(|parameter| (parameter.name.clone(), parameter.value.clone()))
                    .collect(),
            );
        }
        self.sent = 0;
        sink.columns(vec![
            ColumnInfo {
                name: "n".into(),
                data_type: "int8".into(),
                nullable: Some(false),
                origin: Default::default(),
            },
            ColumnInfo {
                name: "note".into(),
                data_type: "text".into(),
                nullable: Some(true),
                origin: Default::default(),
            },
        ]);
        Ok(self.send(request.options.max_rows, sink))
    }

    async fn fetch(
        &mut self,
        options: &ExecutionOptions,
        sink: &mut dyn RowSink,
        cancel: CancellationToken,
    ) -> DriverResult<DriverOutcome> {
        let call = {
            let mut calls = self.calls.lock().unwrap();
            calls.fetches.push(options.max_rows);
            calls.fetches.len()
        };
        if self.cancel_on == Some(call) {
            cancel.cancel();
            return Err(DriverError::cancelled());
        }
        if self.fail_on == Some(call) {
            return Err(DriverError::new(
                ErrorCode::Database,
                "synthetic server failure",
            ));
        }
        Ok(self.send(options.max_rows, sink))
    }

    fn is_open(&self) -> bool {
        self.open
    }

    async fn context(&mut self, _: CancellationToken) -> DriverResult<SessionContext> {
        self.calls.lock().unwrap().contexts += 1;
        Ok(SessionContext::default())
    }

    async fn close(self: Box<Self>) -> DriverResult<()> {
        Ok(())
    }
}

/// Opens a [`PagedSession`] of `total` rows for statements run again.
#[derive(Debug)]
struct PagedDriver {
    total: u32,
    calls: Arc<Mutex<Calls>>,
}

#[async_trait]
impl Driver for PagedDriver {
    fn descriptor(&self) -> DriverDescriptor {
        PostgresDriver.descriptor()
    }
    fn validate_config(&self, _: &serde_json::Value) -> DriverResult<()> {
        Ok(())
    }
    async fn test(&self, _: &ResolvedConnection, _: CancellationToken) -> DriverResult<ServerInfo> {
        Ok(ServerInfo { version: None })
    }
    async fn open_session(
        &self,
        _: &ResolvedConnection,
        _: CancellationToken,
    ) -> DriverResult<Box<dyn Session>> {
        Ok(Box::new(PagedSession::new(self.total, 0, &self.calls)))
    }
    async fn introspect(
        &self,
        _: &ResolvedConnection,
        _: &mut dyn IntrospectionSink,
        _: CancellationToken,
    ) -> DriverResult<IntrospectionReport> {
        Ok(IntrospectionReport::default())
    }
}

fn input() -> ResolvedConnection {
    ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::Postgres,
            options: serde_json::json!({"host":"localhost","user":"fixture","database":"fixture","sslMode":"disable"}),
        },
        credentials: Credentials {
            password: Some(SecretString::from("synthetic-password-1234")),
        },
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jykre-exporting-{name}-{}", ConnectionId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn request(target: PathBuf, pages: Vec<Arc<ResultData>>, read: ExportRead) -> ExportRequest {
    ExportRequest {
        target,
        options: crate::export::ExportOptions::default(),
        columns: columns(),
        shown: None,
        sql: SqlTarget::default(),
        pages,
        read,
    }
}

/// The data rows of a CSV export, as `(n, note)`.
fn exported_rows(path: &Path) -> Vec<(u32, String)> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .skip(1)
        .map(|line| {
            let (n, note) = line.split_once(';').unwrap();
            (
                n.trim_matches('"').parse().unwrap(),
                note.trim_matches('"').to_owned(),
            )
        })
        .collect()
}

fn in_order(rows: &[(u32, String)], total: u32) {
    assert_eq!(rows.len(), total as usize);
    for (index, (n, note)) in rows.iter().enumerate() {
        assert_eq!(*n as usize, index);
        assert_eq!(note, &format!("row {n}"));
    }
}

#[tokio::test]
async fn held_pages_are_the_whole_file_when_nothing_is_left() {
    let dir = scratch("held");
    let target = dir.join("result.csv");
    let (progress, watched) = watch::channel(ExportProgress::default());
    let run = export(
        request(
            target.clone(),
            vec![page(0, 1_000), page(1_000, 1_500)],
            ExportRead::Held,
        ),
        CancellationToken::new(),
        Some(progress),
    )
    .await;
    let exported = run.outcome.unwrap();
    assert_eq!(exported.rows, 1_500);
    assert!(!exported.truncated);
    in_order(&exported_rows(&target), 1_500);
    assert_eq!(watched.borrow().rows, 1_500);
    assert_eq!(watched.borrow().bytes, exported.bytes);
    assert!(run.session.is_none());
    assert_eq!(names(&dir), ["result.csv"]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn the_rest_of_an_open_result_follows_the_held_pages_in_larger_parts() {
    let dir = scratch("rest");
    let target = dir.join("result.csv");
    let calls = Arc::new(Mutex::new(Calls::default()));
    let session = PagedSession::new(250_000, 1_000, &calls);
    let run = export(
        request(
            target.clone(),
            vec![page(0, 1_000)],
            ExportRead::Rest {
                session: Box::new(session),
                options: ExecutionOptions {
                    max_rows: 1_000,
                    ..ExecutionOptions::default()
                },
            },
        ),
        CancellationToken::new(),
        None,
    )
    .await;
    let exported = run.outcome.unwrap();
    assert_eq!(exported.rows, 250_000);
    in_order(&exported_rows(&target), 250_000);
    let calls = calls.lock().unwrap();
    // Parts are sized by the bytes of a row, far above the tab's page.
    assert!(calls.fetches[0] > 10_000, "{:?}", calls.fetches);
    assert!(calls.fetches.len() <= 3, "{:?}", calls.fetches);
    // Read to its end, so the session reports its context again.
    assert_eq!(calls.contexts, 1);
    assert!(run.context.is_some());
    assert!(run.session.is_some() && !run.ended);
    assert_eq!(names(&dir), ["result.csv"]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_cancelled_or_failed_export_leaves_the_destination_as_it_was() {
    let dir = scratch("stop");
    let target = dir.join("result.csv");
    std::fs::write(&target, "previous export").unwrap();
    for (cancel_on, fail_on, code) in [
        (Some(2), None, ErrorCode::Cancelled),
        (None, Some(3), ErrorCode::Database),
    ] {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let mut session = PagedSession::new(10_000, 100, &calls);
        session.cap = 100;
        session.cancel_on = cancel_on;
        session.fail_on = fail_on;
        let run = export(
            request(
                target.clone(),
                vec![page(0, 100)],
                ExportRead::Rest {
                    session: Box::new(session),
                    options: ExecutionOptions::default(),
                },
            ),
            CancellationToken::new(),
            None,
        )
        .await;
        assert_eq!(run.outcome.unwrap_err().code, code);
        // The session goes back to the tab; nothing asks it for a context,
        // which would discard what is left of the result.
        assert!(run.session.is_some());
        assert!(run.context.is_none());
        assert_eq!(calls.lock().unwrap().contexts, 0);
        assert_eq!(names(&dir), ["result.csv"]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "previous export");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_folder_that_cannot_be_written_fails_before_the_session_is_read() {
    let dir = scratch("folder");
    let calls = Arc::new(Mutex::new(Calls::default()));
    let run = export(
        request(
            dir.join("missing").join("result.csv"),
            vec![page(0, 10)],
            ExportRead::Rest {
                session: Box::new(PagedSession::new(100, 10, &calls)),
                options: ExecutionOptions::default(),
            },
        ),
        CancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(run.outcome.unwrap_err().code, ErrorCode::NotFound);
    assert!(run.session.is_some());
    assert!(calls.lock().unwrap().fetches.is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_part_over_the_memory_ceiling_fails_instead_of_leaving_rows_out() {
    let dir = scratch("ceiling");
    let calls = Arc::new(Mutex::new(Calls::default()));
    let run = export_with(
        None,
        request(
            dir.join("result.csv"),
            vec![page(0, 10)],
            ExportRead::Rest {
                session: Box::new(PagedSession::new(100_000, 10, &calls)),
                options: ExecutionOptions::default(),
            },
        ),
        4_096,
        CancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(run.outcome.unwrap_err().code, ErrorCode::Validation);
    assert!(names(&dir).is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_statement_run_again_sends_everything_it_returns_to_the_file() {
    let dir = scratch("again");
    let target = dir.join("result.xlsx");
    let calls = Arc::new(Mutex::new(Calls::default()));
    let driver: Arc<dyn Driver> = Arc::new(PagedDriver {
        total: 30_000,
        calls: calls.clone(),
    });
    let mut again = request(
        target.clone(),
        // Held pages are not written: the statement returns them again.
        vec![page(0, 1_000)],
        ExportRead::Again {
            input: input(),
            session: None,
            sql: "SELECT n, note FROM synthetic WHERE n >= :low".into(),
            parameters: vec![QueryParameter {
                name: "low".into(),
                kind: ParameterKind::Number,
                value: Some("0".into()),
                markers: std::iter::once(41..45).collect(),
            }],
            options: ExecutionOptions::default(),
        },
    );
    again.options.format = ExportFormat::Xlsx;
    let run = export_with(
        Some(driver.clone()),
        again,
        DEFAULT_MEMORY_CEILING,
        CancellationToken::new(),
        None,
    )
    .await;
    let exported = run.outcome.unwrap();
    assert_eq!(exported.rows, 30_000);
    assert!(std::fs::read(&target).unwrap().starts_with(b"PK"));
    assert_eq!(calls.lock().unwrap().executed, 1);
    // The values bound when the result ran are bound again.
    assert_eq!(
        calls.lock().unwrap().parameters[0],
        [("low".to_owned(), Some("0".to_owned()))]
    );
    assert!(run.session.is_some() && !run.reopened);

    // Other columns than when it ran: nothing is written.
    let mut other = request(
        dir.join("other.csv"),
        Vec::new(),
        ExportRead::Again {
            input: input(),
            session: run.session,
            sql: "SELECT n, note FROM synthetic".into(),
            parameters: Vec::new(),
            options: ExecutionOptions::default(),
        },
    );
    other.columns = vec![ResultColumn::new("id", "int8")];
    let run = export_with(
        Some(driver),
        other,
        DEFAULT_MEMORY_CEILING,
        CancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(run.outcome.unwrap_err().code, ErrorCode::Validation);
    assert!(run.session.is_some());
    assert_eq!(names(&dir), ["result.xlsx"]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn parts_hold_about_the_same_bytes_whatever_the_rows() {
    let small = page(0, 1_000);
    let rows = rows_per_part(&small, 1);
    assert!(rows > 100_000 / 2, "{rows}");
    let wide = ResultData::from_rows(
        vec![ResultColumn::new("blob", "text")],
        [[Some("x".repeat(1 << 20).as_str())]]
            .iter()
            .map(|row| row.iter().copied()),
    );
    // 16 MiB of rows of a little over 1 MiB each.
    assert_eq!(rows_per_part(&wide, 1_000), 15);
    assert_eq!(rows_per_part(&ResultData::new(columns()), 7), 7);
}
