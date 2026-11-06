//! Rounds against a local fake of an OpenAI-compatible server: no network,
//! no credentials, synthetic text only.
use super::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the fake answers one request with.
#[derive(Clone)]
enum Reply {
    /// Server-sent events, each written and flushed in turn.
    Events(Vec<Value>),
    /// Status and body, not streamed.
    Status(u16, &'static str),
    /// Headers, one event, then silence until the client gives up.
    Stall,
}

/// A recorded request: the head (request line and headers) and the body.
#[derive(Debug, Clone)]
struct Recorded {
    head: String,
    body: String,
}

struct Fake {
    url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    /// Set when a connection was closed by the client before the reply
    /// finished.
    closed_early: Arc<Mutex<bool>>,
}

async fn fake(replies: Vec<Reply>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let closed_early = Arc::new(Mutex::new(false));
    let (recorded, closed) = (requests.clone(), closed_early.clone());
    tokio::spawn(async move {
        for reply in replies {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut bytes = Vec::new();
            let mut buf = [0u8; 8192];
            let (head, body) = loop {
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                bytes.extend_from_slice(&buf[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&bytes[..end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        let body =
                            String::from_utf8_lossy(&bytes[end + 4..end + 4 + length]).to_string();
                        break (head, body);
                    }
                }
            };
            recorded.lock().unwrap().push(Recorded { head, body });
            match reply {
                Reply::Status(status, body) => {
                    let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                }
                Reply::Events(events) => {
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                        .await;
                    for event in events {
                        let line = format!("data: {event}\n\n");
                        if stream.write_all(line.as_bytes()).await.is_err() {
                            *closed.lock().unwrap() = true;
                            break;
                        }
                        let _ = stream.flush().await;
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    let _ = stream.write_all(b"data: [DONE]\n\n").await;
                }
                Reply::Stall => {
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                        .await;
                    let _ = stream
                        .write_all(
                            format!("data: {}\n\n", chunk(json!({"content": "partial"})))
                                .as_bytes(),
                        )
                        .await;
                    let _ = stream.flush().await;
                    // Wait for the client to close the connection.
                    let mut buf = [0u8; 16];
                    let read =
                        tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await;
                    if matches!(read, Ok(Ok(0)) | Ok(Err(_))) {
                        *closed.lock().unwrap() = true;
                    }
                }
            }
        }
    });
    Fake {
        url,
        requests,
        closed_early,
    }
}

fn chunk(delta: Value) -> Value {
    json!({"id": "x", "object": "chat.completion.chunk", "model": "fake-model",
           "choices": [{"index": 0, "delta": delta}]})
}

fn finish(reason: &str) -> Value {
    json!({"id": "x", "object": "chat.completion.chunk", "model": "fake-model",
           "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

fn usage(input: u64, output: u64, cached: u64) -> Value {
    json!({"id": "x", "object": "chat.completion.chunk", "model": "fake-model", "choices": [],
           "usage": {"prompt_tokens": input, "completion_tokens": output,
                     "total_tokens": input + output,
                     "prompt_tokens_details": {"cached_tokens": cached}}})
}

fn assistant(url: &str, key: Option<&str>, timeouts: Timeouts) -> Assistant {
    Assistant::new(
        ProviderConfig {
            kind: ProviderKind::OpenAiCompatible,
            model: "fake-model".into(),
            endpoint: Some(url.into()),
        },
        key.map(|key| SecretString::from(key.to_owned())),
        timeouts,
    )
    .unwrap()
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "fake-model".into(),
        system: "synthetic-system".into(),
        messages: vec![Message::User {
            text: "synthetic-question".into(),
        }],
        tools: vec![ToolSpec {
            name: "describe_relation".into(),
            description: "Describes a table".into(),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}},
                               "required": ["path"]}),
        }],
        max_output_tokens: 256,
    }
}

#[tokio::test]
async fn a_round_streams_text_and_returns_tool_calls_and_usage() {
    let server = fake(vec![Reply::Events(vec![
        chunk(json!({"role": "assistant", "content": "Let me "})),
        chunk(json!({"content": "look."})),
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
              "function": {"name": "describe_relation", "arguments": "{\"path\":"}}]}),
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"sales.orders\"}"}}]}),
        ),
        finish("tool_calls"),
        usage(120, 30, 100),
    ])])
    .await;
    let model = assistant(&server.url, Some("synthetic-key-5d1e"), Timeouts::default());
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let result = model
        .send(&request(), &sender, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.text, "Let me look.");
    assert_eq!(
        result.tool_calls,
        [ToolCall {
            id: "call_1".into(),
            name: "describe_relation".into(),
            arguments: json!({"path": "sales.orders"}),
        }]
    );
    assert_eq!(result.usage.input, Some(120));
    assert_eq!(result.usage.output, Some(30));
    assert_eq!(result.usage.cache_read, Some(100));
    assert!(!result.truncated);
    let mut deltas = Vec::new();
    while let Ok(RoundEvent::Delta(delta)) = receiver.try_recv() {
        deltas.push(delta);
    }
    assert_eq!(deltas.concat(), "Let me look.");

    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let head = &requests[0].head;
    assert!(head.starts_with("POST /v1/chat/completions "), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("authorization: bearer synthetic-key-5d1e")
    );
    let body: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert!(!requests[0].body.contains("synthetic-key-5d1e"));
    assert_eq!(body["model"], "fake-model");
    assert_eq!(
        body["max_tokens"]
            .as_u64()
            .or(body["max_completion_tokens"].as_u64()),
        Some(256)
    );
    assert!(requests[0].body.contains("synthetic-system"));
    assert!(requests[0].body.contains("synthetic-question"));
    assert!(requests[0].body.contains("describe_relation"));
}

#[tokio::test]
async fn tool_results_go_back_in_the_next_round_with_the_calls() {
    let server = fake(vec![Reply::Events(vec![
        chunk(json!({"content": "Done."})),
        finish("stop"),
    ])])
    .await;
    let model = assistant(&server.url, None, Timeouts::default());
    let mut next = request();
    next.messages.push(Message::Assistant {
        text: String::new(),
        tool_calls: vec![
            ToolCall {
                id: "a".into(),
                name: "describe_relation".into(),
                arguments: json!({"path": "x"}),
            },
            ToolCall {
                id: "b".into(),
                name: "describe_relation".into(),
                arguments: json!({"path": "y"}),
            },
        ],
    });
    for (id, content) in [("a", "synthetic-result-x"), ("b", "synthetic-result-y")] {
        next.messages.push(Message::Tool {
            call_id: id.into(),
            name: "describe_relation".into(),
            content: content.into(),
        });
    }
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let result = model
        .send(&next, &sender, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.text, "Done.");
    assert!(result.tool_calls.is_empty());
    let body: Value = serde_json::from_str(&server.requests.lock().unwrap()[0].body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool", "tool"]);
    assert_eq!(messages[2]["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(messages[3]["tool_call_id"], "a");
    assert_eq!(messages[4]["content"], "synthetic-result-y");
}

#[tokio::test]
async fn statuses_are_classified_without_the_response_body() {
    for (status, code) in [
        (401, ErrorCode::Authentication),
        (404, ErrorCode::NotFound),
        (400, ErrorCode::Validation),
        (429, ErrorCode::Connection),
        (500, ErrorCode::Connection),
    ] {
        let server = fake(vec![Reply::Status(
            status,
            r#"{"error":{"message":"synthetic-body-echo"}}"#,
        )])
        .await;
        let model = assistant(&server.url, Some("k"), Timeouts::default());
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let error = model
            .send(&request(), &sender, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, code, "{status}");
        assert!(!error.message.contains("synthetic-body-echo"));
        assert!(!format!("{error:?}").contains("synthetic-body-echo"));
    }
}

#[tokio::test]
async fn cancelling_mid_stream_closes_the_connection() {
    let server = fake(vec![Reply::Stall]).await;
    let model = assistant(&server.url, None, Timeouts::default());
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        // Cancel once the first piece of text has arrived.
        if receiver.recv().await.is_some() {
            trigger.cancel();
        }
    });
    let error = model.send(&request(), &sender, &cancel).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Cancelled);
    for _ in 0..100 {
        if *server.closed_early.lock().unwrap() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    unreachable!("the server never saw the connection close");
}

#[tokio::test]
async fn a_silent_stream_times_out() {
    let server = fake(vec![Reply::Stall]).await;
    let timeouts = Timeouts {
        between_chunks: Duration::from_millis(200),
        ..Timeouts::default()
    };
    let model = assistant(&server.url, None, timeouts);
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let error = model
        .send(&request(), &sender, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Timeout);
}

#[tokio::test]
async fn the_round_limit_bounds_the_whole_answer() {
    let server = fake(vec![Reply::Stall]).await;
    let timeouts = Timeouts {
        round: Duration::from_millis(300),
        ..Timeouts::default()
    };
    let model = assistant(&server.url, None, timeouts);
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let error = model
        .send(&request(), &sender, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Timeout);
}

#[test]
fn a_required_key_is_checked_and_never_printed() {
    let config = ProviderConfig {
        kind: ProviderKind::OpenAi,
        model: "m".into(),
        endpoint: None,
    };
    let missing = Assistant::new(config.clone(), None, Timeouts::default()).unwrap_err();
    assert_eq!(missing.code, ErrorCode::Validation);
    let blank = Assistant::new(
        config.clone(),
        Some(SecretString::from(" ".to_owned())),
        Timeouts::default(),
    );
    assert!(blank.is_err());
    let model = Assistant::new(
        config,
        Some(SecretString::from("synthetic-key-0b7c".to_owned())),
        Timeouts::default(),
    )
    .unwrap();
    assert!(!format!("{model:?}").contains("synthetic-key-0b7c"));
}

#[tokio::test]
async fn the_output_limit_is_reported() {
    let server = fake(vec![Reply::Events(vec![
        chunk(json!({"content": "SELECT"})),
        finish("length"),
    ])])
    .await;
    let model = assistant(&server.url, None, Timeouts::default());
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let result = model
        .send(&request(), &sender, &CancellationToken::new())
        .await
        .unwrap();
    assert!(result.truncated);
}
