//! Phase 4 (specification, section 11): framing across chunks and UTF-8
//! boundaries, the preallocation cap, invalid and reused tokens, duplicate
//! `hello`, conflicting terminals, disconnects, cancellation while the
//! program reads nothing, the method allowlist, quotas, and tokens and
//! values absent from logs and `Debug`. The test plays the program over a
//! real loopback connection; no child process is needed.
use super::*;
use crate::automation::logs;
use jykre_core::ParameterKind;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

const WAIT: Duration = Duration::from_secs(10);

fn data_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jykre-jjp-{label}-{}", RunId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn declared() -> Vec<OutputSpec> {
    vec![
        OutputSpec {
            name: "rows".into(),
            kind: ParameterKind::Number,
        },
        OutputSpec {
            name: "note".into(),
            kind: ParameterKind::Text,
        },
    ]
}

async fn register(host: &JjpHost, inputs: NamedValues, label: &str) -> (JjpSession, PathBuf) {
    let data = data_dir(label);
    let session = host
        .register(JjpRun {
            run: RunId::new(),
            inputs,
            outputs: declared(),
            deadline: Instant::now() + Duration::from_secs(60),
            data_dir: data.clone(),
        })
        .await
        .unwrap();
    (session, data)
}

fn token_of(session: &JjpSession) -> String {
    let line: Value = serde_json::from_slice(session.token_line().as_bytes()).unwrap();
    assert_eq!(line["jjp"], "1");
    line["token"].as_str().unwrap().to_owned()
}

/// The next event, which must come.
async fn event(session: &mut JjpSession) -> Option<JjpEvent> {
    tokio::time::timeout(WAIT, session.next())
        .await
        .expect("the session answers")
}

/// Events until the session closes or goes quiet for `quiet`.
async fn events_until_quiet(session: &mut JjpSession, quiet: Duration) -> Vec<JjpEvent> {
    let mut events = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(quiet, session.next()).await {
        events.push(event);
    }
    events
}

/// The program's side of the connection.
struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl Client {
    async fn connect(address: SocketAddr) -> Self {
        Self::from_stream(TcpStream::connect(address).await.unwrap())
    }

    fn from_stream(stream: TcpStream) -> Self {
        stream.set_nodelay(true).unwrap();
        let (reader, writer) = stream.into_split();
        Self {
            reader: BufReader::new(reader),
            writer,
            next_id: 1,
        }
    }

    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_vec(&message).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.unwrap();
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let message = self.recv().await.expect("an answer");
            if message["id"] == id {
                return message;
            }
        }
    }

    async fn hello(&mut self, token: &str) -> Value {
        self.request("hello", json!({"token": token, "protocol": "jjp/1"}))
            .await
    }

    /// The next message, or `None` at the end of the connection.
    async fn recv(&mut self) -> Option<Value> {
        let mut line = String::new();
        match tokio::time::timeout(WAIT, self.reader.read_line(&mut line))
            .await
            .expect("the host answers or closes")
        {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(serde_json::from_str(&line).unwrap()),
        }
    }

    /// Whether the host closed the connection, reading past any answers.
    async fn closed(&mut self) -> bool {
        tokio::time::timeout(WAIT, async {
            let mut line = String::new();
            loop {
                line.clear();
                match self.reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return true,
                    Ok(_) => {}
                }
            }
        })
        .await
        .unwrap_or(false)
    }
}

/// The outputs of a reported `complete`.
fn complete_outputs(event: Option<JjpEvent>) -> NamedValues {
    match event {
        Some(JjpEvent::Terminal(Terminal::Complete { outputs })) => Some(outputs),
        _ => None,
    }
    .expect("the session reports the accepted complete")
}

fn error_code_of(message: &Value) -> i64 {
    message["error"]["code"].as_i64().unwrap_or_default()
}

#[tokio::test]
async fn hello_answers_with_the_run_inputs_deadline_and_cap() {
    let host = JjpHost::new();
    let inputs = NamedValues::new(vec![
        ("address".into(), Some("Rua 1".into())),
        ("rows".into(), None),
    ]);
    let (mut session, data) = register(&host, inputs, "hello").await;
    let run = session.run.run;
    let mut client = Client::connect(session.address()).await;
    let answer = client.hello(&token_of(&session)).await;
    let result = &answer["result"];
    assert_eq!(result["run_id"], run.to_string());
    assert_eq!(result["inputs"], json!({"address": "Rua 1", "rows": null}));
    let left = result["deadline_ms"].as_u64().unwrap();
    assert!(left > 50_000 && left <= 60_000, "{left} ms left");
    assert_eq!(result["max_message_bytes"], 1_048_576);
    assert_eq!(event(&mut session).await, Some(JjpEvent::Hello));
    assert!(session.connected());
    session.finish(Duration::ZERO).await;
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn wrong_tokens_are_refused_and_the_right_one_still_works() {
    let host = JjpHost::new();
    let (mut session, data) = register(&host, NamedValues::default(), "wrong-token").await;
    let mut stranger = Client::connect(session.address()).await;
    let answer = stranger.hello(&"0".repeat(64)).await;
    assert_eq!(error_code_of(&answer), error_code::TOKEN);
    assert!(stranger.closed().await);
    let mut short = Client::connect(session.address()).await;
    assert_eq!(error_code_of(&short.hello("abc").await), error_code::TOKEN);

    let mut wrong_protocol = Client::connect(session.address()).await;
    let answer = wrong_protocol
        .request(
            "hello",
            json!({"token": token_of(&session), "protocol": "jjp/2"}),
        )
        .await;
    assert_eq!(error_code_of(&answer), error_code::INVALID_PARAMS);
    assert!(wrong_protocol.closed().await);

    let mut program = Client::connect(session.address()).await;
    assert!(program.hello(&token_of(&session)).await["result"].is_object());
    assert_eq!(event(&mut session).await, Some(JjpEvent::Hello));
    session.finish(Duration::ZERO).await;
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn a_token_is_consumed_once_so_a_second_connection_is_refused() {
    let host = JjpHost::new();
    let (session, data) = register(&host, NamedValues::default(), "reuse").await;
    let token = token_of(&session);
    let mut first = Client::connect(session.address()).await;
    assert!(first.hello(&token).await["result"].is_object());
    let mut second = Client::connect(session.address()).await;
    assert_eq!(
        error_code_of(&second.hello(&token).await),
        error_code::TOKEN
    );
    assert!(second.closed().await);
    // The first connection is untouched.
    let answer = first
        .request("complete", json!({"outputs": {"rows": "1", "note": null}}))
        .await;
    assert_eq!(answer["result"], json!({}));
    session.finish(Duration::ZERO).await;
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn anything_before_hello_is_refused_and_closes_the_connection() {
    let host = JjpHost::new();
    let (session, data) = register(&host, NamedValues::default(), "before-hello").await;
    let mut early = Client::connect(session.address()).await;
    let answer = early.request("complete", json!({"outputs": {}})).await;
    assert_eq!(error_code_of(&answer), error_code::HELLO);
    assert!(early.closed().await);
    let mut quiet = Client::connect(session.address()).await;
    quiet
        .notify("log", json!({"level": "info", "message": "x"}))
        .await;
    assert!(quiet.closed().await);
    assert!(!session.connected());
    session.finish(Duration::ZERO).await;
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn a_duplicate_hello_is_refused_and_the_run_goes_on() {
    let host = JjpHost::new();
    let (mut session, data) = register(&host, NamedValues::default(), "duplicate").await;
    let token = token_of(&session);
    let mut client = Client::connect(session.address()).await;
    client.hello(&token).await;
    let again = client.hello(&token).await;
    assert_eq!(error_code_of(&again), error_code::HELLO);
    let answer = client
        .request("complete", json!({"outputs": {"rows": "2", "note": "n"}}))
        .await;
    assert_eq!(answer["result"], json!({}));
    assert_eq!(event(&mut session).await, Some(JjpEvent::Hello));
    assert!(matches!(
        event(&mut session).await,
        Some(JjpEvent::Terminal(Terminal::Complete { .. }))
    ));
    session.finish(Duration::ZERO).await;
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn no_hello_in_time_fails_the_run_and_revokes_the_token() {
    let host = JjpHost::with_config(JjpConfig {
        hello_timeout: Duration::from_millis(200),
        ..JjpConfig::default()
    });
    let (mut session, data) = register(&host, NamedValues::default(), "hello-timeout").await;
    // A connection that never speaks is closed after the same wait.
    let mut silent = Client::connect(session.address()).await;
    assert_eq!(
        event(&mut session).await,
        Some(JjpEvent::Failed(ProtocolProblem::HelloTimeout))
    );
    assert!(silent.closed().await);
    let mut late = Client::connect(session.address()).await;
    assert_eq!(
        error_code_of(&late.hello(&token_of(&session)).await),
        error_code::TOKEN
    );
    assert_eq!(event(&mut session).await, None);
    let summary = session.finish(Duration::ZERO).await;
    assert_eq!(summary.problem, Some(ProtocolProblem::HelloTimeout));
    assert!(!summary.hello);
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn complete_is_checked_and_conflicting_terminals_are_refused() {
    let host = JjpHost::new();
    let (mut session, data) = register(&host, NamedValues::default(), "terminals").await;
    let mut client = Client::connect(session.address()).await;
    client.hello(&token_of(&session)).await;

    let missing = client
        .request("complete", json!({"outputs": {"rows": "1"}}))
        .await;
    assert_eq!(error_code_of(&missing), error_code::INVALID_PARAMS);
    assert_eq!(
        missing["error"]["data"],
        json!({"output": "note", "problem": "missing"})
    );
    let wrong_kind = client
        .request(
            "complete",
            json!({"outputs": {"rows": "many", "note": null}}),
        )
        .await;
    assert_eq!(wrong_kind["error"]["data"]["problem"], "kind");
    assert!(!wrong_kind.to_string().contains("many"));

    // The program may try again.
    let accepted = client
        .request(
            "complete",
            json!({"outputs": {"rows": "-1.5e3", "note": "done"}}),
        )
        .await;
    assert_eq!(accepted["result"], json!({}));
    for (method, params) in [
        ("complete", json!({"outputs": {"rows": "1", "note": null}})),
        ("fail", json!({"code": "E"})),
        ("hello", json!({"token": "x", "protocol": "jjp/1"})),
        ("whatever", json!({})),
    ] {
        let answer = client.request(method, params).await;
        assert_eq!(error_code_of(&answer), error_code::TERMINAL, "{method}");
    }
    // Notifications after the terminal are dropped without an answer.
    client
        .notify("log", json!({"level": "info", "message": "late"}))
        .await;
    drop(client);
    assert_eq!(event(&mut session).await, Some(JjpEvent::Hello));
    let outputs = complete_outputs(event(&mut session).await);
    assert_eq!(outputs.get("rows"), Some(Some("-1.5e3")));
    // A disconnect after a terminal is not a failure.
    assert_eq!(event(&mut session).await, None);
    let summary = session.finish(Duration::ZERO).await;
    assert_eq!(summary.problem, None);
    let jjp = std::fs::read_to_string(logs::log_path(&data, summary_run(&data), Stream::Jjp));
    assert!(jjp.is_ok_and(|text| !text.contains("late")));
    std::fs::remove_dir_all(data).unwrap();
}

/// The one run whose logs are in `data`.
fn summary_run(data: &Path) -> RunId {
    let runs: Vec<RunId> = std::fs::read_dir(logs::logs_root(data))
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    assert_eq!(runs.len(), 1);
    runs[0]
}

#[tokio::test]
async fn fail_is_recorded_and_its_message_goes_to_jjp_log_only() {
    let (subscriber, captured) = crate::logging::capture::subscriber(tracing::Level::TRACE);
    let _default = tracing::subscriber::set_default(subscriber);
    let host = JjpHost::new();
    let (mut session, data) = register(&host, NamedValues::default(), "fail").await;
    let mut client = Client::connect(session.address()).await;
    client.hello(&token_of(&session)).await;
    let bad = client
        .request(
            "fail",
            json!({"code": "has space", "message": "fail-sentinel-8841"}),
        )
        .await;
    assert_eq!(error_code_of(&bad), error_code::INVALID_PARAMS);
    assert!(!bad.to_string().contains("fail-sentinel-8841"));
    let long = format!("fail-sentinel-8841 {}", "x".repeat(10_000));
    let good = client
        .request("fail", json!({"code": "E_EXPORT.1", "message": long}))
        .await;
    assert_eq!(good["result"], json!({}));
    assert_eq!(event(&mut session).await, Some(JjpEvent::Hello));
    assert_eq!(
        event(&mut session).await,
        Some(JjpEvent::Terminal(Terminal::Fail {
            code: "E_EXPORT.1".into()
        }))
    );
    let summary = session.finish(Duration::ZERO).await;
    assert!(!format!("{summary:?}").contains("fail-sentinel"));
    let text =
        std::fs::read_to_string(logs::log_path(&data, summary_run(&data), Stream::Jjp)).unwrap();
    let line = text
        .lines()
        .find(|line| line.starts_with("[fail E_EXPORT.1] fail-sentinel-8841"))
        .unwrap();
    assert!(line.ends_with("[cut at 4 KiB]"));
    assert!(line.len() < 4096 + 64);
    assert!(!captured.text().contains("fail-sentinel-8841"));
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn a_disconnect_without_a_terminal_fails_the_run() {
    let host = JjpHost::new();
    let (mut session, data) = register(&host, NamedValues::default(), "disconnect").await;
    let mut client = Client::connect(session.address()).await;
    client.hello(&token_of(&session)).await;
    drop(client);
    assert_eq!(event(&mut session).await, Some(JjpEvent::Hello));
    assert_eq!(
        event(&mut session).await,
        Some(JjpEvent::Failed(ProtocolProblem::Disconnected))
    );
    assert_eq!(event(&mut session).await, None);
    let summary = session.finish(Duration::ZERO).await;
    assert_eq!(summary.problem, Some(ProtocolProblem::Disconnected));
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn unknown_methods_and_invalid_requests_get_their_errors_and_the_run_goes_on() {
    let host = JjpHost::new();
    let (mut session, data) = register(&host, NamedValues::default(), "methods").await;
    let mut client = Client::connect(session.address()).await;
    client.hello(&token_of(&session)).await;
    assert_eq!(
        error_code_of(&client.request("execute", json!({})).await),
        error_code::METHOD_NOT_FOUND
    );
    // `cancel` goes from the host to the program only.
    assert_eq!(
        error_code_of(&client.request("cancel", json!({})).await),
        error_code::METHOD_NOT_FOUND
    );
    assert_eq!(
        error_code_of(
            &client
                .request("log", json!({"level": "info", "message": "m"}))
                .await
        ),
        error_code::INVALID_REQUEST
    );
    for invalid in [
        json!({"id": 50, "method": "complete"}),
        json!([{"jsonrpc": "2.0", "id": 51, "method": "complete"}]),
        json!({"jsonrpc": "2.0", "id": 52, "result": {}}),
    ] {
        client.send(invalid).await;
        let answer = client.recv().await.unwrap();
        assert_eq!(error_code_of(&answer), error_code::INVALID_REQUEST);
    }
    // Unknown notifications get no answer; the next request is answered.
    client.notify("nonsense", json!({})).await;
    let answer = client
        .request("complete", json!({"outputs": {"rows": "3", "note": null}}))
        .await;
    assert_eq!(answer["result"], json!({}));
    assert_eq!(event(&mut session).await, Some(JjpEvent::Hello));
    assert!(matches!(
        event(&mut session).await,
        Some(JjpEvent::Terminal(_))
    ));
    session.finish(Duration::ZERO).await;
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn framing_survives_joined_messages_and_byte_by_byte_writes() {
    let host = JjpHost::new();
    let (mut session, data) = register(&host, NamedValues::default(), "framing").await;
    let token = token_of(&session);
    let mut client = Client::connect(session.address()).await;
    // Every message in one write.
    let joined = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": {"token": token, "protocol": "jjp/1"}}),
        json!({"jsonrpc": "2.0", "method": "log", "params": {"level": "warn", "message": "ação"}}),
        json!({"jsonrpc": "2.0", "method": "progress", "params": {"percent": 10}}),
    ]
    .iter()
    .map(|message| format!("{message}\n"))
    .collect::<String>();
    client.writer.write_all(joined.as_bytes()).await.unwrap();
    assert_eq!(client.recv().await.unwrap()["id"], 1);
    // A request written one byte at a time, splitting `ç` and `ã`.
    let split = format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "id": 2, "method": "complete", "params": {"outputs": {"rows": "7", "note": "ação"}}})
    );
    for byte in split.as_bytes() {
        client.writer.write_all(&[*byte]).await.unwrap();
        tokio::task::yield_now().await;
    }
    let answer = client.recv().await.unwrap();
    assert_eq!(answer, json!({"jsonrpc": "2.0", "id": 2, "result": {}}));
    let events = events_until_quiet(&mut session, Duration::from_millis(300)).await;
    assert!(events.contains(&JjpEvent::Hello));
    let outputs = complete_outputs(
        events
            .iter()
            .find(|event| matches!(event, JjpEvent::Terminal(_)))
            .cloned(),
    );
    assert_eq!(outputs.get("note"), Some(Some("ação")));
    let summary = session.finish(Duration::ZERO).await;
    let text =
        std::fs::read_to_string(logs::log_path(&data, summary_run(&data), Stream::Jjp)).unwrap();
    assert!(text.contains("[warn] ação\n"));
    assert_eq!(summary.log.bytes, text.len() as u64);
    std::fs::remove_dir_all(data).unwrap();
}

/// After `hello`, sends `bytes` and checks the run fails with `problem`.
async fn fails_with(bytes: &[u8], problem: ProtocolProblem, label: &str) -> Option<Value> {
    let host = JjpHost::new();
    let (mut session, data) = register(&host, NamedValues::default(), label).await;
    let mut client = Client::connect(session.address()).await;
    client.hello(&token_of(&session)).await;
    // The host may close while this is still being written.
    let _ = client.writer.write_all(bytes).await;
    let answer = client.recv().await;
    assert!(client.closed().await, "{label}: the connection stays open");
    assert_eq!(event(&mut session).await, Some(JjpEvent::Hello));
    assert_eq!(event(&mut session).await, Some(JjpEvent::Failed(problem)));
    let summary = session.finish(Duration::ZERO).await;
    assert_eq!(summary.problem, Some(problem));
    let text =
        std::fs::read_to_string(logs::log_path(&data, summary_run(&data), Stream::Jjp)).unwrap();
    assert!(text.contains(problem.note()), "{label}: {text}");
    std::fs::remove_dir_all(data).unwrap();
    answer
}

#[tokio::test]
async fn invalid_utf8_fails_the_run() {
    fails_with(b"{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"params\":{\"level\":\"info\",\"message\":\"\xff\xfe\"}}\n", ProtocolProblem::InvalidUtf8, "utf8").await;
}

#[tokio::test]
async fn invalid_json_fails_the_run_after_a_parse_error() {
    let answer = fails_with(b"{not json}\n", ProtocolProblem::InvalidJson, "json").await;
    assert_eq!(
        answer.map(|answer| error_code_of(&answer)),
        Some(error_code::PARSE_ERROR)
    );
}

#[tokio::test]
async fn a_line_past_the_cap_fails_the_run_without_waiting_for_its_end() {
    // The cap plus one byte, and no newline: the host must not wait for one.
    let line = vec![b'a'; MAX_MESSAGE_BYTES + 1];
    fails_with(&line, ProtocolProblem::LineTooLong, "cap").await;
}

#[tokio::test]
async fn log_messages_past_the_quota_are_dropped_and_counted() {
    let host = JjpHost::new();
    let (mut session, data) = register(&host, NamedValues::default(), "log-quota").await;
    let mut client = Client::connect(session.address()).await;
    client.hello(&token_of(&session)).await;
    let message = "m".repeat(64 * 1024);
    for _ in 0..20 {
        client
            .notify("log", json!({"level": "info", "message": message}))
            .await;
    }
    // Answered in order, so every log was read before this.
    let answer = client
        .request("fail", json!({"code": "E_LAST", "message": "last words"}))
        .await;
    assert_eq!(answer["result"], json!({}));
    assert_eq!(event(&mut session).await, Some(JjpEvent::Hello));
    let summary = session.finish(Duration::ZERO).await;
    let text =
        std::fs::read_to_string(logs::log_path(&data, summary_run(&data), Stream::Jjp)).unwrap();
    assert!(text.len() as u64 <= LOG_LIMIT);
    assert_eq!(summary.log.bytes, text.len() as u64);
    let written = text
        .lines()
        .filter(|line| line.starts_with("[info] "))
        .count();
    let dropped = summary.log.dropped_messages as usize;
    assert_eq!(written + dropped, 20);
    assert!(dropped > 0);
    // The `fail` line always fits, even with the log full.
    assert!(text.contains("[fail E_LAST] last words\n"));
    assert!(text.ends_with(&format!(
        "[jykre] {dropped} messages dropped past the 1 MiB limit\n"
    )));
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn traffic_beyond_ten_times_the_log_quota_ends_the_run() {
    let host = JjpHost::new();
    let (mut session, data) = register(&host, NamedValues::default(), "traffic").await;
    let mut client = Client::connect(session.address()).await;
    client.hello(&token_of(&session)).await;
    let message = "t".repeat(1_000_000);
    let line = format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "method": "log", "params": {"level": "info", "message": message}})
    );
    let started = Instant::now();
    for _ in 0..11 {
        // The host closes once past the quota.
        if client.writer.write_all(line.as_bytes()).await.is_err() {
            break;
        }
    }
    assert!(client.closed().await);
    assert_eq!(event(&mut session).await, Some(JjpEvent::Hello));
    assert_eq!(
        event(&mut session).await,
        Some(JjpEvent::Failed(ProtocolProblem::Quota))
    );
    assert!(started.elapsed() < WAIT);
    let summary = session.finish(Duration::ZERO).await;
    assert!(summary.log.bytes <= LOG_LIMIT);
    assert!(summary.log.dropped_messages > 0);
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn progress_is_coalesced_and_the_latest_wins() {
    let host = JjpHost::new();
    let (mut session, data) = register(&host, NamedValues::default(), "progress").await;
    let mut client = Client::connect(session.address()).await;
    client.hello(&token_of(&session)).await;
    for percent in 1..=50 {
        client
            .notify(
                "progress",
                json!({"percent": percent, "stage": format!("step {percent}")}),
            )
            .await;
    }
    let events = events_until_quiet(&mut session, Duration::from_millis(500)).await;
    let progress: Vec<&JjpProgress> = events
        .iter()
        .filter_map(|event| match event {
            JjpEvent::Progress(progress) => Some(progress),
            _ => None,
        })
        .collect();
    assert!(
        !progress.is_empty() && progress.len() <= 3,
        "{} progress events for 50 messages",
        progress.len()
    );
    let last = progress.last().unwrap();
    assert_eq!(last.percent, 50.0);
    assert_eq!(last.stage.as_deref(), Some("step 50"));
    assert!(!format!("{last:?}").contains("step"));
    session.finish(Duration::ZERO).await;
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn cancel_reaches_the_program_and_never_waits_for_it_to_read() {
    // Room for every answer, so the flood below only fills the socket.
    let host = JjpHost::with_config(JjpConfig {
        max_pending_output: 64 * 1024 * 1024,
        ..JjpConfig::default()
    });
    let (mut session, data) = register(&host, NamedValues::default(), "saturated").await;
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut client = Client::from_stream(socket.connect(session.address()).await.unwrap());
    client.hello(&token_of(&session)).await;
    assert_eq!(event(&mut session).await, Some(JjpEvent::Hello));

    // About 4.6 MB of requests whose 8 MB of answers the program never
    // reads: far more than both socket buffers hold.
    let mut flood = Vec::new();
    for id in 1_000..101_000 {
        flood.extend_from_slice(
            format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"nope\"}}\n").as_bytes(),
        );
    }
    tokio::time::timeout(WAIT, client.writer.write_all(&flood))
        .await
        .expect("the host keeps reading while its answers pile up")
        .unwrap();
    let asked = Instant::now();
    session.cancel();
    assert!(asked.elapsed() < Duration::from_millis(50));
    // The host still reads: a `fail` sent now is accepted.
    client
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "fail", "params": {"code": "LATE"}}))
        .await;
    assert_eq!(
        event(&mut session).await,
        Some(JjpEvent::Terminal(Terminal::Fail {
            code: "LATE".into()
        }))
    );
    let finishing = Instant::now();
    let summary = session.finish(Duration::ZERO).await;
    assert!(finishing.elapsed() < Duration::from_secs(3));
    assert!(summary.terminal.is_some());
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn a_cancel_asked_before_hello_is_sent_right_after_it() {
    let host = JjpHost::new();
    let (session, data) = register(&host, NamedValues::default(), "cancel-early").await;
    session.cancel();
    let mut client = Client::connect(session.address()).await;
    client.hello(&token_of(&session)).await;
    let cancel = client.recv().await.unwrap();
    assert_eq!(
        cancel,
        json!({"jsonrpc": "2.0", "method": "cancel", "params": {}})
    );
    session.finish(Duration::ZERO).await;
    std::fs::remove_dir_all(data).unwrap();
}

#[tokio::test]
async fn the_listener_opens_with_the_first_run_and_closes_with_the_last() {
    let host = JjpHost::new();
    assert_eq!(host.address(), None);
    let (first, first_data) = register(&host, NamedValues::default(), "listener-1").await;
    let address = first.address();
    assert!(address.ip().is_loopback());
    assert_eq!(host.address(), Some(address));
    let (second, second_data) = register(&host, NamedValues::default(), "listener-2").await;
    assert_eq!(second.address(), address);
    assert_ne!(token_of(&first), token_of(&second));
    first.finish(Duration::ZERO).await;
    assert_eq!(host.address(), Some(address));
    assert!(TcpStream::connect(address).await.is_ok());
    second.finish(Duration::ZERO).await;
    assert_eq!(host.address(), None);
    let refused = tokio::time::timeout(WAIT, async {
        while TcpStream::connect(address).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(refused.is_ok(), "the port still accepts connections");
    std::fs::remove_dir_all(first_data).unwrap();
    std::fs::remove_dir_all(second_data).unwrap();
}

#[tokio::test]
async fn tokens_inputs_and_outputs_never_reach_tracing_or_debug() {
    let (subscriber, captured) = crate::logging::capture::subscriber(tracing::Level::TRACE);
    let _default = tracing::subscriber::set_default(subscriber);
    let host = JjpHost::new();
    let inputs = NamedValues::new(vec![("address".into(), Some("input-sentinel-5521".into()))]);
    let (mut session, data) = register(&host, inputs, "secrets").await;
    let token = token_of(&session);
    let mut client = Client::connect(session.address()).await;
    let hello = client.hello(&token).await;
    assert_eq!(hello["result"]["inputs"]["address"], "input-sentinel-5521");
    client
        .notify(
            "log",
            json!({"level": "error", "message": "log-sentinel-5521"}),
        )
        .await;
    let refused = client
        .request(
            "complete",
            json!({"outputs": {"rows": "output-sentinel-5521", "note": null}}),
        )
        .await;
    assert!(!refused.to_string().contains("output-sentinel-5521"));
    client
        .request(
            "complete",
            json!({"outputs": {"rows": "5521", "note": "output-sentinel-5521"}}),
        )
        .await;
    let events = events_until_quiet(&mut session, Duration::from_millis(300)).await;
    let debug = format!(
        "{session:?} {:?} {events:?} {host:?} {:?}",
        session.token_line(),
        session.token
    );
    let summary = session.finish(Duration::ZERO).await;
    let debug = format!("{debug} {summary:?}");
    let logged = captured.text();
    assert!(logged.contains("jjp/1 hello accepted"), "the capture works");
    for secret in [
        token.as_str(),
        "input-sentinel-5521",
        "output-sentinel-5521",
        "log-sentinel-5521",
    ] {
        assert!(!logged.contains(secret), "{secret} reached tracing");
        assert!(!debug.contains(secret), "{secret} reached Debug");
    }
    std::fs::remove_dir_all(data).unwrap();
}
