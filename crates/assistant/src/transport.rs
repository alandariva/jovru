//! One round against the provider, streamed. The provider library (genai)
//! is used only here.
use crate::error::{classify, protocol, timeout};
use crate::{
    KeyUse, Message, ModelRequest, ProviderConfig, ProviderKind, ToolCall, ToolSpec, Usage,
};
use futures_util::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart, MessageContent,
    StopReason, Tool, ToolResponse,
};
use genai::resolver::{AuthData, Endpoint};
use genai::{ModelIden, ServiceTarget};
use jykre_core::{CancellationToken, DriverError, DriverResult, ErrorCode, SecretString};
use secrecy::ExposeSecret;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// Limits of one round ([reliability](../../../docs/reliability.md)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Opening the connection.
    pub connect: Duration,
    /// From the request to the first byte of the answer.
    pub first_byte: Duration,
    /// The longest silence between two pieces of a streamed answer.
    pub between_chunks: Duration,
    /// The whole round.
    pub round: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(15),
            first_byte: Duration::from_secs(120),
            between_chunks: Duration::from_secs(60),
            round: Duration::from_secs(300),
        }
    }
}

/// What arrives while a round streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundEvent {
    /// The next piece of the answer's text.
    Delta(String),
}

/// What a round returned.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RoundResult {
    pub text: String,
    /// Tools the model asked for; empty when the answer is final.
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    /// The answer stopped at the output limit.
    pub truncated: bool,
}

/// A configured provider, ready to run rounds.
pub struct Assistant {
    config: ProviderConfig,
    endpoint: String,
    key: Option<SecretString>,
    client: genai::Client,
    timeouts: Timeouts,
}

impl std::fmt::Debug for Assistant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assistant")
            .field("config", &self.config)
            .field("key", &self.key.as_ref().map(|_| "REDACTED"))
            .finish_non_exhaustive()
    }
}

impl Assistant {
    pub fn new(
        config: ProviderConfig,
        key: Option<SecretString>,
        timeouts: Timeouts,
    ) -> DriverResult<Self> {
        config.validate()?;
        let key = key.filter(|key| !key.expose_secret().trim().is_empty());
        if config.kind.key_use() == KeyUse::Required && key.is_none() {
            return Err(DriverError::validation("Enter the API key"));
        }
        let endpoint = config.endpoint()?;
        // TLS uses ring, which the rest of the application already uses;
        // the HTTP client panics when no provider is installed. Another
        // part of the process may have installed it first.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(timeouts.connect)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                DriverError::new(ErrorCode::Internal, "Could not prepare the HTTP client")
            })?;
        let client = genai::Client::builder()
            .with_reqwest(http)
            .build()
            .map_err(|_| {
                DriverError::new(ErrorCode::Internal, "Could not prepare the AI client")
            })?;
        Ok(Self {
            config,
            endpoint,
            key,
            client,
            timeouts,
        })
    }

    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    /// Sends one round and streams its text to `events`. Cancelling drops
    /// the request, which closes the connection.
    pub async fn send(
        &self,
        request: &ModelRequest,
        events: &UnboundedSender<RoundEvent>,
        cancel: &CancellationToken,
    ) -> DriverResult<RoundResult> {
        self.bounded(self.timeouts.round, cancel, self.stream(request, events))
            .await
    }

    /// Checks the provider, model and key with a tiny request that also
    /// declares one tool, so a model or server without tool calling fails
    /// here instead of in the first real question.
    pub async fn check(&self, cancel: &CancellationToken) -> DriverResult<()> {
        let request = ModelRequest {
            model: self.config.model.clone(),
            system: "Reply with the single word OK.".into(),
            messages: vec![Message::User { text: "OK?".into() }],
            tools: vec![ToolSpec {
                name: "noop".into(),
                description: "Does nothing. Do not call it.".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            }],
            max_output_tokens: 16,
        };
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        self.bounded(
            Duration::from_secs(30),
            cancel,
            self.stream(&request, &events),
        )
        .await
        .map(|_| ())
    }

    async fn bounded<T>(
        &self,
        limit: Duration,
        cancel: &CancellationToken,
        work: impl Future<Output = DriverResult<T>>,
    ) -> DriverResult<T> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(DriverError::cancelled()),
            result = tokio::time::timeout(limit, work) => result.map_err(|_| timeout())?,
        }
    }

    fn target(&self) -> ServiceTarget {
        let adapter = match self.config.kind {
            ProviderKind::Anthropic => AdapterKind::Anthropic,
            ProviderKind::OpenAi | ProviderKind::OpenAiCompatible => AdapterKind::OpenAI,
            ProviderKind::Ollama => AdapterKind::Ollama,
        };
        let auth = match (&self.key, self.config.kind.key_use()) {
            (_, KeyUse::None) => AuthData::None,
            (Some(key), _) => AuthData::from_single(key.expose_secret()),
            // A local OpenAI-compatible server usually ignores the header,
            // but the protocol requires one.
            (None, _) => AuthData::from_single("none"),
        };
        ServiceTarget {
            endpoint: Endpoint::from_owned(self.endpoint.clone()),
            auth,
            model: ModelIden::new(adapter, request_model(&self.config)),
        }
    }

    async fn stream(
        &self,
        request: &ModelRequest,
        events: &UnboundedSender<RoundEvent>,
    ) -> DriverResult<RoundResult> {
        let options = ChatOptions::default()
            .with_capture_usage(true)
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_max_tokens(request.max_output_tokens)
            .with_stream_header_timeout(self.timeouts.first_byte)
            .with_stream_read_timeout(self.timeouts.between_chunks);
        let response = self
            .client
            .exec_chat_stream(self.target(), chat_request(request), Some(&options))
            .await
            .map_err(|error| classify(&error))?;
        let mut stream = response.stream;
        let mut text = String::new();
        let mut end = None;
        while let Some(event) = stream.next().await {
            match event.map_err(|error| classify(&error))? {
                ChatStreamEvent::Chunk(chunk) => {
                    text.push_str(&chunk.content);
                    // The receiver may be gone; the round goes on.
                    let _ = events.send(RoundEvent::Delta(chunk.content));
                }
                ChatStreamEvent::End(stream_end) => end = Some(stream_end),
                _ => {}
            }
        }
        let end = end.ok_or_else(protocol)?;
        if text.is_empty()
            && let Some(texts) = end.captured_texts()
        {
            text = texts.concat();
        }
        let tool_calls = end
            .captured_tool_calls()
            .unwrap_or_default()
            .into_iter()
            .map(|call| ToolCall {
                id: call.call_id.clone(),
                name: call.fn_name.clone(),
                arguments: call.fn_arguments.clone(),
            })
            .collect();
        let usage = end
            .captured_usage
            .as_ref()
            .map(|usage| {
                let count = |value: Option<i32>| value.and_then(|v| u64::try_from(v).ok());
                let details = usage.prompt_tokens_details.as_ref();
                Usage {
                    input: count(usage.prompt_tokens),
                    output: count(usage.completion_tokens),
                    cache_read: count(details.and_then(|d| d.cached_tokens)),
                    cache_write: count(details.and_then(|d| d.cache_creation_tokens)),
                }
            })
            .unwrap_or_default();
        Ok(RoundResult {
            text,
            tool_calls,
            usage,
            truncated: matches!(end.captured_stop_reason, Some(StopReason::MaxTokens(_))),
        })
    }
}

fn request_model(config: &ProviderConfig) -> String {
    config.model.trim().to_owned()
}

/// The provider library's request for a canonical one. Nothing is added
/// but the wire format: every text here is in the canonical request.
fn chat_request(request: &ModelRequest) -> ChatRequest {
    let mut messages = Vec::new();
    let mut responses: Vec<ToolResponse> = Vec::new();
    for message in &request.messages {
        if !matches!(message, Message::Tool { .. }) && !responses.is_empty() {
            messages.push(ChatMessage::from(std::mem::take(&mut responses)));
        }
        match message {
            Message::User { text } => messages.push(ChatMessage::user(text.as_str())),
            Message::Assistant { text, tool_calls } => {
                let mut parts = Vec::new();
                if !text.is_empty() {
                    parts.push(ContentPart::Text(text.clone()));
                }
                for call in tool_calls {
                    parts.push(ContentPart::ToolCall(genai::chat::ToolCall {
                        call_id: call.id.clone(),
                        fn_name: call.name.clone(),
                        fn_arguments: call.arguments.clone(),
                        thought_signatures: None,
                    }));
                }
                messages.push(ChatMessage::assistant(MessageContent::from_parts(parts)));
            }
            // Consecutive results travel together: some providers require
            // every answer to one round's calls in a single turn.
            Message::Tool {
                call_id,
                name,
                content,
            } => responses.push(
                ToolResponse::new(call_id.as_str(), content.as_str()).with_fn_name(name.as_str()),
            ),
        }
    }
    if !responses.is_empty() {
        messages.push(ChatMessage::from(responses));
    }
    let mut chat = ChatRequest::new(messages).with_system(request.system.as_str());
    if !request.tools.is_empty() {
        chat = chat.with_tools(request.tools.iter().map(|tool| {
            Tool::new(tool.name.as_str())
                .with_description(tool.description.as_str())
                .with_schema(tool.input_schema.clone())
        }));
    }
    chat
}

#[cfg(test)]
mod tests;
