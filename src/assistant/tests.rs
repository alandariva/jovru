//! The privacy boundary end to end: the three requests run against a local
//! fake provider, and what the fake received is checked against what the
//! user would have reviewed and against values that must never leave.
use super::facts::{ErrorFacts, RunFacts, SchemaScope};
use super::run::{Conversation, DeclineQueries, Next, QueryDecision, QueryGate, RoundState, drive};
use super::tools::CacheTools;
use super::*;
use crate::backend::cache_config;
use crate::error_report::ErrorReport;
use crate::result::{ResultColumn, ResultData};
use crate::result_tabs::{Completed, Placement, ResultTabs};
use jykre_assistant::{Assistant, Message, Timeouts};
use jykre_core::{
    CancellationToken, ConnectionConfig, ConnectionId, DriverKind, IntrospectionReport,
    MetadataObject, ObjectKind, ObjectPath, ParameterKind, QueryParameter, SessionContext,
    SqlDialect,
};
use jykre_storage::MetadataRepository;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CELL: &str = "synthetic-cell-7f3a";
const PARAMETER: &str = "synthetic-param-91c2";
const COMMENT: &str = "synthetic-comment-ok";
const KEY: &str = "synthetic-key-c41d";

/// A fake OpenAI-compatible server answering with `replies` in turn and
/// recording each request's head and body.
async fn fake(replies: Vec<Vec<serde_json::Value>>) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/", listener.local_addr().unwrap());
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let requests = recorded.clone();
    tokio::spawn(async move {
        for events in replies {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut bytes = Vec::new();
            let mut buf = [0u8; 16384];
            loop {
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
                        requests.lock().unwrap().push((head, body));
                        break;
                    }
                }
            }
            let mut reply = String::from(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            for event in events {
                reply.push_str(&format!("data: {event}\n\n"));
            }
            reply.push_str("data: [DONE]\n\n");
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    });
    (url, recorded)
}

fn chunk(delta: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"id": "x", "object": "chat.completion.chunk", "model": "fake",
                       "choices": [{"index": 0, "delta": delta}]})
}

/// A round where the model asks for every tool, listing a schema too.
fn calls_every_tool() -> Vec<serde_json::Value> {
    let call = |index: u32, id: &str, name: &str, arguments: &str| {
        chunk(
            serde_json::json!({"tool_calls": [{"index": index, "id": id, "type": "function",
              "function": {"name": name, "arguments": arguments}}]}),
        )
    };
    vec![
        call(
            0,
            "s",
            "search_objects",
            r#"{"pattern":"*ORD*","kinds":["table","column"]}"#,
        ),
        call(
            1,
            "l",
            "search_objects",
            r#"{"pattern":"*","kinds":["table","view"],"namespace":"shop"}"#,
        ),
        call(2, "d", "describe_relation", r#"{"path":"orders"}"#),
        serde_json::json!({"id": "x", "object": "chat.completion.chunk", "model": "fake",
                           "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ]
}

fn answers(text: &str) -> Vec<serde_json::Value> {
    vec![
        chunk(serde_json::json!({"content": text})),
        serde_json::json!({"id": "x", "object": "chat.completion.chunk", "model": "fake",
                           "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ]
}

async fn cache() -> SchemaScope {
    let dir = std::env::temp_dir().join(format!("jykre-assistant-{}", ConnectionId::new()));
    let id = ConnectionId::new();
    let config = ConnectionConfig {
        driver: DriverKind::MySql,
        options: serde_json::json!({"host":"unreachable.invalid","user":"fixture","sslMode":"disabled"}),
    };
    let repository = MetadataRepository::open(&dir, id, &cache_config(&config).unwrap())
        .await
        .unwrap();
    let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "shop");
    let orders = MetadataObject::new(ObjectKind::Table, &db.path, "orders");
    let mut key = MetadataObject::new(ObjectKind::Column, &orders.path, "id");
    key.ordinal = Some(1);
    key.data_type = Some("bigint".into());
    key.primary_key = Some(1);
    let mut status = MetadataObject::new(ObjectKind::Column, &orders.path, "status");
    status.ordinal = Some(2);
    status.data_type = Some("varchar(20)".into());
    status.default_value = Some("'pending'".into());
    status.comment = Some(COMMENT.into());
    let mut pending = repository.begin(&ObjectPath::default()).await.unwrap();
    pending.batch(vec![db, orders, key, status]).await.unwrap();
    pending
        .activate(&IntrospectionReport::default(), &CancellationToken::new())
        .await
        .unwrap();
    drop(pending);
    repository.close().await;
    SchemaScope {
        dir,
        connection: id,
        config,
        engine: "MySQL".into(),
        dialect: Some(SqlDialect::MySql),
        context: SessionContext {
            catalog: None,
            schema: Some("shop".into()),
        },
    }
}

/// Every text a request carries, as it appears inside a JSON body.
fn shown_texts(request: &jykre_assistant::ModelRequest) -> Vec<String> {
    let mut texts = vec![request.system.clone()];
    for message in &request.messages {
        match message {
            Message::User { text } => texts.push(text.clone()),
            Message::Assistant { text, .. } if !text.is_empty() => texts.push(text.clone()),
            Message::Assistant { .. } => {}
            Message::Tool { content, .. } => texts.push(content.clone()),
        }
    }
    texts
        .into_iter()
        .map(|text| {
            let quoted = serde_json::to_string(&text).unwrap();
            quoted[1..quoted.len() - 1].to_owned()
        })
        .collect()
}

#[tokio::test]
async fn no_result_value_or_bound_parameter_reaches_the_provider() {
    let scope = cache().await;
    // A finished result holding a value, a statement with a bound value,
    // and a failure, as the result tabs keep them.
    let mut tabs = ResultTabs::new(usize::MAX);
    let ids = tabs.begin(
        &[
            "SELECT status FROM orders WHERE id = :id".into(),
            "SELECT stauts FROM orders".into(),
        ],
        Placement::New,
    );
    tabs.set_parameters(
        ids[0],
        vec![QueryParameter {
            name: "id".into(),
            kind: ParameterKind::Text,
            value: Some(PARAMETER.into()),
            markers: Vec::new(),
        }],
    );
    tabs.complete(
        ids[0],
        Completed {
            data: ResultData::from_rows(
                vec![ResultColumn::new("status", "varchar")],
                [[Some(CELL)]],
            ),
            ..Completed::default()
        },
    );
    tabs.fail(
        ids[1],
        ErrorReport::database("Unknown column 'stauts' in 'field list'"),
    );
    let done = tabs.get(ids[0]).unwrap();
    let failed = tabs.get(ids[1]).unwrap();
    let ResultState::Failed(report) = &failed.state else {
        unreachable!("the second tab failed");
    };

    let first_messages = [
        prompts::fix_message(
            &failed.sql,
            &ErrorFacts::new(report, &failed.sql),
            &scope.context,
        ),
        prompts::write_message("orders per status", Some(&done.sql), &scope.context),
        prompts::chat_message(
            "How many orders came back?",
            RunFacts::of(done),
            &scope.context,
        ),
    ];
    assert!(first_messages[2].contains("returned 1 rows"));

    let (url, recorded) = fake(vec![
        calls_every_tool(),
        answers("```sql\nSELECT status FROM orders;\n```"),
        calls_every_tool(),
        answers("```sql\nSELECT status, COUNT(*) FROM orders GROUP BY status;\n```"),
        calls_every_tool(),
        answers("One row."),
    ])
    .await;
    let assistant = Assistant::new(
        jykre_assistant::ProviderConfig {
            kind: jykre_assistant::ProviderKind::OpenAiCompatible,
            model: "fake".into(),
            endpoint: Some(url),
        },
        Some(SecretString::from(KEY.to_owned())),
        Timeouts::default(),
    )
    .unwrap();
    let tools = CacheTools::new(scope.clone());
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut shown = Vec::new();
    for message in first_messages {
        let mut conversation = Conversation::new(
            "openai-compatible",
            "fake",
            prompts::system_prompt(&scope.engine, "English", false),
            CacheTools::specs(false),
        );
        conversation.start_turn(message);
        let next = drive(
            &mut conversation,
            &assistant,
            &tools,
            &mut DeclineQueries,
            &events,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(next, Next::Done);
        assert_eq!(conversation.rounds().len(), 2);
        assert!(matches!(
            conversation.rounds()[1].state,
            RoundState::Answered { .. }
        ));
        shown.extend(
            conversation
                .rounds()
                .iter()
                .map(|round| round.request.clone()),
        );
    }

    let recorded = recorded.lock().unwrap().clone();
    assert_eq!(recorded.len(), 6);
    for ((head, body), request) in recorded.iter().zip(&shown) {
        // What left is what was shown: every text of the reviewed request
        // is in the body.
        for text in shown_texts(request) {
            assert!(
                body.contains(&text),
                "missing from the request sent: {text}"
            );
        }
        assert!(!body.contains(CELL), "a result value left the computer");
        assert!(!body.contains(PARAMETER), "a bound value left the computer");
        assert!(!body.contains(KEY));
        assert!(head.to_ascii_lowercase().contains(&format!("bearer {KEY}")));
    }
    // The tools answered from the cache: the comment and the default
    // reached the model, as decided.
    for (_, body) in recorded.iter().skip(1).step_by(2) {
        assert!(body.contains(COMMENT));
        assert!(body.contains("DEFAULT 'pending'"));
        assert!(body.contains("shop.orders (table)"));
    }
    assert!(recorded[1].1.contains("Unknown column 'stauts'"));
}

const VISIBLE: &str = "synthetic-visible-5b1e";
const HIDDEN_VALUE: &str = "synthetic-hidden-c4d9";

/// What the user does with each `run_query` call in the test.
#[derive(Clone, Copy, PartialEq)]
enum Choice {
    /// Run it and send the result, hiding `secret`.
    SendHidingSecret,
    /// Run it and keep the result.
    Withhold,
    /// Do not run it.
    Decline,
}

/// A user answering `run_query` as the panel and the workspace would: the
/// statement runs in a session of its own on a synthetic SQLite file.
struct User {
    input: jykre_core::ResolvedConnection,
    session: Option<Box<dyn jykre_core::Session>>,
    choice: Choice,
    runs: usize,
}

impl QueryGate for User {
    async fn answer(&mut self, call: jykre_assistant::ToolCall) -> run::ToolStep {
        let sql = match run::query_sql(&call, Some(SqlDialect::Sqlite)) {
            Ok(sql) => sql,
            Err(reason) => return released::rejected(call, String::new(), reason),
        };
        if self.choice == Choice::Decline {
            return released::declined(call, sql);
        }
        self.runs += 1;
        let run = crate::backend::query_in_context(
            None,
            self.input.clone(),
            SessionContext::default(),
            self.session.take(),
            vec![jykre_core::QueryRequest::new(
                sql.clone(),
                jykre_core::ExecutionOptions {
                    max_rows: released::READ_ROWS,
                    ..jykre_core::ExecutionOptions::default()
                },
            )],
            4 << 20,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        self.session = run.session;
        let preview = match &run.batch {
            Ok(batch) => released::QueryPreview::from_batch(batch, &sql),
            Err(error) => released::QueryPreview::from_error(error, &sql),
        };
        match self.choice {
            Choice::SendHidingSecret => {
                let hidden = preview.hidden_by_name(&["secret".to_owned()].into());
                released::sent(call, sql, &preview, &hidden)
            }
            _ => released::withheld(call, sql, &preview),
        }
    }
}

/// A synthetic SQLite file with 60 rows holding a visible and a secret value.
async fn people() -> jykre_core::ResolvedConnection {
    let path = std::env::temp_dir().join(format!("jykre-assistant-{}.sqlite", ConnectionId::new()));
    std::fs::write(&path, b"").unwrap();
    let input = jykre_core::ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::Sqlite,
            options: serde_json::json!({"path": path.to_string_lossy()}),
        },
        credentials: jykre_core::Credentials::default(),
    };
    let setup = [
        "CREATE TABLE people (id INTEGER, visible TEXT, secret TEXT)".to_owned(),
        format!(
            "INSERT INTO people WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 60) \
             SELECT i, '{VISIBLE}', '{HIDDEN_VALUE}' FROM n"
        ),
    ];
    let run = crate::backend::query_in_context(
        None,
        input.clone(),
        SessionContext::default(),
        None,
        setup
            .into_iter()
            .map(|sql| jykre_core::QueryRequest::new(sql, jykre_core::ExecutionOptions::default()))
            .collect(),
        4 << 20,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let batch = run.batch.unwrap();
    assert!(batch.error.is_none(), "{:?}", batch.error);
    crate::backend::close_session(run.session).await;
    input
}

/// A round asking for one good query and one with two statements.
fn asks_for_queries() -> Vec<serde_json::Value> {
    let call = |index: u32, id: &str, arguments: serde_json::Value| {
        chunk(
            serde_json::json!({"tool_calls": [{"index": index, "id": id, "type": "function",
          "function": {"name": "run_query", "arguments": arguments.to_string()}}]}),
        )
    };
    vec![
        call(
            0,
            "q",
            serde_json::json!({"sql": "SELECT id, visible, secret FROM people", "purpose": "See the values"}),
        ),
        call(
            1,
            "r",
            serde_json::json!({"sql": "SELECT 1; SELECT 2", "purpose": "Two at once"}),
        ),
        serde_json::json!({"id": "x", "object": "chat.completion.chunk", "model": "fake",
                           "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ]
}

#[tokio::test]
async fn query_values_leave_only_when_sent_and_never_from_a_hidden_column() {
    let input = people().await;
    let scope = cache().await;
    let tools = CacheTools::new(scope.clone());
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    for choice in [Choice::SendHidingSecret, Choice::Withhold, Choice::Decline] {
        let (url, recorded) = fake(vec![asks_for_queries(), answers("Done.")]).await;
        let assistant = Assistant::new(
            jykre_assistant::ProviderConfig {
                kind: jykre_assistant::ProviderKind::OpenAiCompatible,
                model: "fake".into(),
                endpoint: Some(url),
            },
            Some(SecretString::from(KEY.to_owned())),
            Timeouts::default(),
        )
        .unwrap();
        let mut conversation = Conversation::new(
            "openai-compatible",
            "fake",
            prompts::system_prompt(&scope.engine, "English", true),
            CacheTools::specs(true),
        );
        conversation.start_turn("What is in people?".into());
        let mut user = User {
            input: input.clone(),
            session: None,
            choice,
            runs: 0,
        };
        let next = drive(
            &mut conversation,
            &assistant,
            &tools,
            &mut user,
            &events,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        crate::backend::close_session(user.session.take()).await;
        assert_eq!(next, Next::Done);
        let recorded = recorded.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2);
        assert!(recorded[0].1.contains("run_query"));
        let body = &recorded[1].1;
        for (request, (_, sent)) in conversation.rounds().iter().zip(&recorded) {
            for text in shown_texts(&request.request) {
                assert!(
                    sent.contains(&text),
                    "missing from the request sent: {text}"
                );
            }
        }
        // The statement with two queries never reached the user.
        assert!(body.contains("exactly one statement"));
        let steps = &conversation.rounds()[0].tools;
        assert_eq!(steps.len(), 2);
        assert_eq!(
            steps[1].query.as_ref().unwrap().decision,
            QueryDecision::Rejected
        );
        assert!(
            !body.contains(HIDDEN_VALUE),
            "a hidden value left the computer"
        );
        match choice {
            Choice::SendHidingSecret => {
                assert_eq!(user.runs, 1);
                assert!(body.contains(VISIBLE));
                assert!(body.contains("hidden by the user"));
                assert!(body.contains(released::HIDDEN));
                assert!(body.contains("more than 50 rows"));
                let record = steps[0].query.as_ref().unwrap();
                assert_eq!(record.decision, QueryDecision::Sent);
                assert_eq!(record.hidden, ["secret"]);
                assert_eq!(record.rows_sent, 50);
            }
            Choice::Withhold => {
                assert_eq!(user.runs, 1);
                assert!(
                    !body.contains(VISIBLE),
                    "a withheld value left the computer"
                );
                assert!(body.contains("chose not to share"));
            }
            Choice::Decline => {
                assert_eq!(user.runs, 0, "a declined query ran");
                assert!(!body.contains(VISIBLE));
                assert!(body.contains("declined to run"));
            }
        }
    }
}

#[test]
fn preferences_round_trip_and_ignore_what_they_do_not_know() {
    let mut preferences = AssistantPreferences {
        provider: Some(jykre_assistant::ProviderKind::Ollama),
        model: "llama".into(),
        endpoint: Some("http://localhost:11434/".into()),
        review: run::ReviewMode::FirstOnly,
        queries: false,
        prices: BTreeMap::new(),
    };
    preferences.prices.insert(
        "llama".into(),
        Price {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        },
    );
    let value = preferences.to_value();
    assert_eq!(AssistantPreferences::from_value(Some(&value)), preferences);
    assert!(preferences.config().is_some());
    assert!(preferences.price().is_some());
    let odd = serde_json::json!({"provider": "nope", "review": "sometimes", "prices": {"m": {"input_per_mtok": -1.0, "output_per_mtok": 1.0}}});
    let read = AssistantPreferences::from_value(Some(&odd));
    assert_eq!(read, AssistantPreferences::default());
    assert!(read.config().is_none());
    assert_eq!(key_account(jykre_assistant::ProviderKind::Ollama), None);
}

use crate::result_tabs::ResultState;

#[tokio::test]
async fn assistant_turns_never_log_prompts_answers_schema_or_keys_above_trace() {
    let (subscriber, captured) = crate::logging::capture::subscriber(tracing::Level::DEBUG);
    let _default = tracing::subscriber::set_default(subscriber);
    let scope = cache().await;
    let (url, _recorded) = fake(vec![calls_every_tool(), answers("synthetic-answer-marker")]).await;
    let assistant = Assistant::new(
        jykre_assistant::ProviderConfig {
            kind: jykre_assistant::ProviderKind::OpenAiCompatible,
            model: "fake".into(),
            endpoint: Some(url),
        },
        Some(SecretString::from(KEY.to_owned())),
        Timeouts::default(),
    )
    .unwrap();
    let tools = CacheTools::new(scope.clone());
    let mut conversation = Conversation::new(
        "openai-compatible",
        "fake",
        prompts::system_prompt(&scope.engine, "English", false),
        CacheTools::specs(false),
    );
    conversation.start_turn("synthetic-question-marker".into());
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    drive(
        &mut conversation,
        &assistant,
        &tools,
        &mut DeclineQueries,
        &events,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let text = captured.text();
    assert!(text.contains("assistant round"), "{text}");
    assert!(
        text.contains("input_tokens") || text.contains("tool_calls"),
        "{text}"
    );
    for secret in [
        KEY,
        COMMENT,
        "synthetic-question-marker",
        "synthetic-answer-marker",
        "'pending'",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
}
