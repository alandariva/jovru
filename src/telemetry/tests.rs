//! Tests of telemetry: preferences, the collector's address and headers, the
//! allowlist, the switch, the filters, and what reaches a collector.
use super::switch::{HELD_LIMIT, Switch, sanitize};
use super::*;
use crate::drivers::{ServerDiagnostic, SqlDialect};
use opentelemetry::KeyValue;
use opentelemetry::trace::Status;
use std::sync::Mutex;

#[test]
fn preferences_round_trip_and_default_to_off() {
    assert_eq!(
        TelemetryPreferences::from_value(None),
        TelemetryPreferences::default()
    );
    let defaults = TelemetryPreferences::default();
    assert!(!defaults.logs && !defaults.traces && !defaults.sends());
    let chosen = TelemetryPreferences {
        logs: true,
        traces: false,
        protocol: Protocol::Grpc,
        endpoint: "https://collector.test:4317".into(),
    };
    assert_eq!(
        TelemetryPreferences::from_value(Some(&chosen.to_value())),
        chosen
    );
    let odd = json!({"logs": "yes", "traces": true, "protocol": "smoke", "extra": 1});
    let read = TelemetryPreferences::from_value(Some(&odd));
    assert!(!read.logs && read.traces);
    assert_eq!(read.protocol, Protocol::Http);
    assert!(read.endpoint.is_empty());
}

#[test]
fn the_collector_is_https_or_on_this_computer() {
    for good in [
        "https://collector.test:4318",
        "https://collector.test/otlp/",
        "http://localhost:4318",
        "http://127.0.0.1:4317",
        "http://[::1]:4318",
    ] {
        assert!(validate_endpoint(good).is_ok(), "{good}");
    }
    for bad in [
        "",
        "collector.test:4318",
        "http://collector.test:4318",
        "ftp://collector.test",
        "https://user:secret@collector.test",
        "https://collector.test/?token=secret",
        "https://collector.test/#part",
    ] {
        assert!(validate_endpoint(bad).is_err(), "{bad}");
    }
    let off = TelemetryPreferences::default();
    assert!(off.validate().is_ok(), "off needs no address");
    let on = TelemetryPreferences {
        traces: true,
        ..TelemetryPreferences::default()
    };
    assert!(on.validate().is_err(), "a signal on needs an address");
}

#[test]
fn headers_are_checked_and_never_shown_by_debug() {
    let headers = Headers::parse(" Authorization=Bearer synthetic-token-1, x-team = a=b ").unwrap();
    assert_eq!(
        headers.pairs(),
        vec![
            (
                "Authorization".to_owned(),
                "Bearer synthetic-token-1".to_owned()
            ),
            ("x-team".to_owned(), "a=b".to_owned()),
        ]
    );
    assert!(!format!("{headers:?}").contains("synthetic-token-1"));
    for bad in ["", "no-equals", "=value", "bad name=1", "x=", "x=line\u{7}"] {
        assert!(Headers::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn operation_names_are_only_known_keywords() {
    let postgres = Some(SqlDialect::Postgres);
    assert_eq!(attributes::operation_name("select 1", postgres), "SELECT");
    assert_eq!(
        attributes::operation_name("  -- note\n/* x */ InSeRt INTO t VALUES (1)", postgres),
        "INSERT"
    );
    assert_eq!(
        attributes::operation_name("WITH a AS (SELECT 1) SELECT * FROM a", postgres),
        "WITH"
    );
    assert_eq!(
        attributes::operation_name("frobnicate secret_table", postgres),
        "OTHER"
    );
    assert_eq!(
        attributes::operation_name("'synthetic-value'", postgres),
        "OTHER"
    );
    assert_eq!(attributes::operation_name("(SELECT 1)", postgres), "OTHER");
    assert_eq!(attributes::operation_name("", None), "OTHER");
}

#[test]
fn error_types_are_codes_and_namespaces_join_names() {
    assert_eq!(
        attributes::error_type(ErrorCode::PermissionDenied),
        "permission_denied"
    );
    let context = crate::drivers::SessionContext {
        catalog: Some("main".into()),
        schema: Some("sales".into()),
    };
    assert_eq!(
        attributes::namespace(&context).as_deref(),
        Some("main.sales")
    );
    assert_eq!(
        attributes::namespace(&crate::drivers::SessionContext::default()),
        None
    );
}

fn span_data(attributes: Vec<KeyValue>, status: Status) -> opentelemetry_sdk::trace::SpanData {
    let mut span = test_span();
    span.attributes = attributes;
    span.status = status;
    span
}

#[test]
fn mcp_outcome_is_an_exported_attribute() {
    assert!(attributes::allowed("jykre.mcp.outcome"));
}

#[test]
fn spans_lose_every_attribute_outside_the_allowlist() {
    let mut span = span_data(
        vec![
            KeyValue::new("db.system.name", "postgresql"),
            KeyValue::new("db.operation.name", "SELECT"),
            KeyValue::new("sql", "SELECT 'synthetic-sql-marker'"),
            KeyValue::new("db.query.text", "SELECT 1"),
            KeyValue::new("server.address", "db.internal"),
            KeyValue::new("password", "synthetic-password-1234"),
            KeyValue::new("code.file.path", "C:/Users/someone/src/x.rs"),
            KeyValue::new("driver", "Postgres"),
        ],
        Status::error("duplicate key value (synthetic-value)"),
    );
    sanitize(&mut span);
    let keys: Vec<_> = span
        .attributes
        .iter()
        .map(|pair| pair.key.as_str().to_owned())
        .collect();
    assert_eq!(keys, ["db.system.name", "db.operation.name"]);
    assert_eq!(span.status, Status::error(""));
}

/// A processor for the switch's tests: what it received.
type Received = Arc<Mutex<Vec<u32>>>;

fn take(received: &Received) -> Vec<u32> {
    std::mem::take(&mut *received.lock().unwrap())
}

fn route(switch: &Switch<Received, u32>, item: u32) {
    switch.route(
        || item,
        |received, item| received.lock().unwrap().push(item),
    );
}

#[test]
fn a_pending_switch_holds_a_bounded_amount_until_it_is_set() {
    let switch = Switch::<Received, u32>::new();
    assert!(
        switch
            .capturing()
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    for item in 0..(HELD_LIMIT as u32 + 5) {
        route(&switch, item);
    }
    let received = Received::default();
    let released = switch.set(Some(received.clone()));
    assert_eq!(released.held.len(), HELD_LIMIT);
    assert_eq!(released.dropped, 5);
    assert!(released.previous.is_none());
    route(&switch, 7);
    assert_eq!(take(&received), [7]);
}

#[test]
fn an_off_switch_drops_what_it_held_and_what_comes() {
    let switch = Switch::<Received, u32>::new();
    route(&switch, 1);
    let released = switch.set(None);
    assert_eq!(released.held, [1]);
    assert!(
        !switch
            .capturing()
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    route(&switch, 2);
    let received = Received::default();
    assert!(switch.set(Some(received.clone())).held.is_empty());
    route(&switch, 3);
    assert_eq!(take(&received), [3]);
    // Turned off again, the processor is handed back to be shut down.
    assert!(switch.set(None).previous.is_some());
    route(&switch, 4);
    assert!(take(&received).is_empty());
}

#[test]
fn spans_held_at_start_reach_the_collector_once_it_is_on() {
    let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let switch = switch::SpanSwitch(Arc::new(Switch::new()));
    opentelemetry_sdk::trace::SpanProcessor::on_end(
        &switch,
        span_data(vec![KeyValue::new("sql", "x")], Status::Unset),
    );
    switch.activate(Some(Box::new(
        opentelemetry_sdk::trace::SimpleSpanProcessor::new(exporter.clone()),
    )));
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    assert!(
        spans[0].attributes.is_empty(),
        "sanitized before it is held"
    );
}

#[test]
fn only_exportable_events_become_logs_and_only_spans_become_traces() {
    let (subscriber, capture) = testing::capture("debug");
    tracing::subscriber::with_default(subscriber, || {
        let span = crate::client_span!("telemetry_test_span", db.system.name = "postgresql");
        let _entered = span.enter();
        tracing::info!(rows = 3, "kept event");
        tracing::info!(sql = "SELECT 'synthetic-sql-marker'", "event with sql");
        tracing::warn!(file = "C:/Users/someone/x.sql", "event with a file");
        tracing::warn!(error = "synthetic free text", "event with an error text");
        tracing::trace!("trace event");
        tracing::info!(target: "jykre::probe", "frame probe");
        tracing::debug!(target: "jykre::ui_thread", "ui-thread timing");
        tracing::warn!(target: "jykre::ui_thread", "slow ui-thread work");
        tracing::warn!(target: "opentelemetry_sdk", "exporter warning");
    });
    let text = capture.exported_text();
    assert!(text.contains("kept event"), "{text}");
    assert!(text.contains("slow ui-thread work"), "{text}");
    for local in [
        "synthetic-sql-marker",
        "event with sql",
        "someone",
        "synthetic free text",
        "trace event",
        "frame probe",
        "ui-thread timing",
        "exporter warning",
    ] {
        assert!(
            !text.contains(local),
            "{local} reached the collector: {text}"
        );
    }
    let spans = capture.spans.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0].name, "telemetry_test_span");
    assert!(spans[0].events.is_empty(), "events stay logs");
    let logs = capture.logs.get_emitted_logs().unwrap();
    let kept = logs
        .iter()
        .find(|log| format!("{:?}", log.record.body()).contains("kept event"))
        .unwrap();
    assert_eq!(
        kept.record.trace_context().map(|context| context.trace_id),
        Some(spans[0].span_context.trace_id()),
        "a log inside a span carries its trace"
    );
    assert_eq!(kept.record.event_name(), None, "no source path");
}

#[test]
fn a_failure_records_its_code_and_sqlstate_but_never_its_message() {
    let (subscriber, capture) = testing::capture("info");
    tracing::subscriber::with_default(subscriber, || {
        let span =
            crate::client_span!("statement", db.response.status_code = tracing::field::Empty);
        let mut error = DriverError::new(
            ErrorCode::Database,
            "duplicate key value violates constraint (synthetic-value)",
        );
        error.server = Some(ServerDiagnostic {
            sqlstate: Some("23505".into()),
            ..ServerDiagnostic::default()
        });
        attributes::record_error(&span, &error);
    });
    let spans = capture.spans.get_finished_spans().unwrap();
    let span = &spans[0];
    let value = |key: &str| {
        span.attributes
            .iter()
            .find(|pair| pair.key.as_str() == key)
            .map(|pair| pair.value.to_string())
    };
    assert_eq!(value("error.type").as_deref(), Some("database"));
    assert_eq!(value("db.response.status_code").as_deref(), Some("23505"));
    assert_eq!(span.status, Status::error(""));
    assert_eq!(span.span_kind, opentelemetry::trace::SpanKind::Client);
    assert!(!capture.exported_text().contains("synthetic-value"));
}

#[test]
fn a_switch_turned_off_stops_capturing_at_once() {
    let (subscriber, capture) = testing::capture("info");
    capture.span_switch.activate(None);
    capture.log_switch.activate(None);
    tracing::subscriber::with_default(subscriber, || {
        let span = crate::client_span!("after_off");
        let _entered = span.enter();
        tracing::info!("after off");
    });
    assert!(capture.exported_text().is_empty());
}

#[test]
fn export_failures_are_classified_from_the_library_text() {
    use opentelemetry_sdk::error::OTelSdkError;
    let code =
        |text: &str| export::export_failure(&OTelSdkError::InternalFailure(text.into())).code;
    assert_eq!(
        code("HTTP export failed with status code: 401"),
        ErrorCode::Authentication
    );
    assert_eq!(
        code("TonicTracesClient export failed with gRPC code: Unauthenticated"),
        ErrorCode::Authentication
    );
    assert_eq!(
        code("HTTP export failed with status code: 404"),
        ErrorCode::NotFound
    );
    assert_eq!(
        code("HTTP export failed with status code: 500"),
        ErrorCode::Protocol
    );
    assert_eq!(
        code("TonicTracesClient export failed with gRPC code: Unavailable: tcp connect error"),
        ErrorCode::Connection
    );
    assert_eq!(code("error sending request"), ErrorCode::Connection);
    assert_eq!(
        export::export_failure(&OTelSdkError::Timeout(Duration::from_secs(1))).code,
        ErrorCode::Timeout
    );
}

/// A collector on this computer that answers every request with `status`
/// and keeps the request line and headers it received.
async fn fake_collector(status: u16) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let mut requests = Vec::new();
        let Ok((mut stream, _)) = listener.accept().await else {
            return requests;
        };
        let mut received = Vec::new();
        let mut buffer = [0u8; 4096];
        // Headers, then the body its Content-Length announces.
        loop {
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&received).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end]
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if received.len() >= end + 4 + length {
                    requests.push(text[..end].to_owned());
                    break;
                }
            }
        }
        let reply = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
        let _ = stream.write_all(reply.as_bytes()).await;
        let _ = stream.shutdown().await;
        requests
    });
    (address, served)
}

fn http_preferences(endpoint: String) -> TelemetryPreferences {
    TelemetryPreferences {
        logs: false,
        traces: true,
        protocol: Protocol::Http,
        endpoint,
    }
}

#[tokio::test]
async fn the_test_sends_protobuf_with_the_headers_to_the_traces_path() {
    let (address, served) = fake_collector(200).await;
    let headers = Headers::parse("x-synthetic-team=synthetic-team-key").unwrap();
    test(
        http_preferences(address),
        Some(headers),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let requests = served.await.unwrap();
    let request = requests[0].to_ascii_lowercase();
    assert!(request.starts_with("post /v1/traces "), "{request}");
    assert!(
        request.contains("content-type: application/x-protobuf"),
        "{request}"
    );
    assert!(
        request.contains("x-synthetic-team: synthetic-team-key"),
        "{request}"
    );
}

#[tokio::test]
async fn a_collector_that_refuses_the_headers_is_an_authentication_failure() {
    let (address, served) = fake_collector(401).await;
    let failure = test(http_preferences(address), None, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(failure.code, ErrorCode::Authentication);
    served.await.unwrap();
}

#[tokio::test]
async fn a_closed_grpc_port_is_a_connection_failure_within_the_timeout() {
    let port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let started = std::time::Instant::now();
    let failure = test(
        TelemetryPreferences {
            protocol: Protocol::Grpc,
            endpoint: format!("http://127.0.0.1:{port}"),
            ..http_preferences(String::new())
        },
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, ErrorCode::Connection, "{failure:?}");
    assert!(started.elapsed() < TEST_TIMEOUT + Duration::from_secs(3));
}

#[tokio::test]
async fn a_cancelled_test_stops_at_once() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    // A routable address that never answers, so only the cancel can end it.
    let failure = test(
        http_preferences("https://10.255.255.1:4318".into()),
        None,
        cancel,
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code, ErrorCode::Cancelled);
}

#[test]
fn log_records_leave_without_where_the_code_lives() {
    use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
    let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder().build();
    let logger = provider.logger("test");
    let mut from = logger.create_log_record();
    from.set_body("window handle invalid".into());
    from.set_severity_text("ERROR");
    from.add_attribute(
        "log.file",
        "C:/Users/someone/.cargo/registry/src/gpui/window.rs",
    );
    from.add_attribute("log.module_path", "gpui::window");
    from.add_attribute("code.filepath", "src/x.rs");
    from.add_attribute("log.line", 616);
    from.add_attribute("rows", 3);
    let mut to = logger.create_log_record();
    switch::copy_exportable(&from, &mut to);
    let keys: Vec<_> = to
        .attributes_iter()
        .map(|(key, _)| key.as_str().to_owned())
        .collect();
    assert_eq!(keys, ["log.line", "rows"]);
    assert_eq!(to.body(), from.body());
    assert_eq!(to.severity_text(), Some("ERROR"));
}
