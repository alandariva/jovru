//! The agentic loop, one round at a time (ADR-0028). The application drives
//! it so each request can be shown before it leaves: a turn starts with the
//! user's message, each round is prepared, reviewed when the mode asks for
//! it, sent, and answered with tool results until the model gives a final
//! answer or the round limit stops it. Everything is kept in the trace.
use super::tools::CacheTools;
use jykre_assistant::{
    Assistant, Message, ModelRequest, Price, RoundEvent, RoundResult, ToolCall, ToolSpec, Usage,
    estimate_tokens,
};
use jykre_core::{CancellationToken, DriverError, DriverResult, ErrorCode};
use serde::Serialize;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

/// Rounds one request may take before the user is asked to continue.
pub const MAX_ROUNDS: usize = 8;
/// Earlier turns are left out of a request past this size; the current turn
/// is always sent whole.
pub const MAX_HISTORY_BYTES: usize = 48 * 1024;
pub const MAX_OUTPUT_TOKENS: u32 = 2048;

/// When the user reviews a request before it is sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReviewMode {
    /// Before every round, including those that carry tool results.
    #[default]
    EveryRequest,
    /// Before the first round of each request only.
    FirstOnly,
    Never,
}

impl ReviewMode {
    pub const ALL: [Self; 3] = [Self::EveryRequest, Self::FirstOnly, Self::Never];

    pub fn key(self) -> &'static str {
        match self {
            Self::EveryRequest => "every-request",
            Self::FirstOnly => "first-only",
            Self::Never => "never",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.key() == key)
    }

    /// Whether the round `index` of a turn (from 0) waits for the user.
    pub fn pauses(self, index: usize) -> bool {
        match self {
            Self::EveryRequest => true,
            Self::FirstOnly => index == 0,
            Self::Never => false,
        }
    }
}

/// One tool call and what the application answered.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolStep {
    pub call: ToolCall,
    pub output: String,
    pub elapsed_ms: u64,
}

/// How a round ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RoundState {
    /// Prepared, not sent: waiting for review or about to go.
    Ready,
    Answered {
        result: RoundResult,
        elapsed_ms: u64,
    },
    Failed {
        code: ErrorCode,
        message: String,
    },
    /// The user cancelled it, before or during sending.
    Cancelled,
}

/// One request and everything that followed it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Round {
    /// From 1, across the conversation.
    pub number: usize,
    pub request: ModelRequest,
    pub estimated_tokens: u64,
    /// Messages of earlier turns left out to keep the request small.
    pub omitted_messages: usize,
    pub state: RoundState,
    /// The tools this round's answer asked for, once they ran.
    pub tools: Vec<ToolStep>,
}

impl Round {
    pub fn usage(&self) -> Usage {
        match &self.state {
            RoundState::Answered { result, .. } => result.usage,
            _ => Usage::default(),
        }
    }
}

/// What to do after a round was answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// Run these tools, then call [`Conversation::answer_tools`].
    Tools(Vec<ToolCall>),
    /// The model gave its final answer.
    Done,
    /// The model wants more tools but the turn used its rounds; the user
    /// may [`Conversation::continue_turn`].
    LimitReached(Vec<ToolCall>),
}

/// A conversation with the model about one connection.
#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    provider: String,
    model: String,
    system: String,
    tools: Vec<ToolSpec>,
    messages: Vec<Message>,
    rounds: Vec<Round>,
    /// Where the current turn's user message is in `messages`.
    turn_start: usize,
    /// Rounds the current turn took since it started or was continued.
    turn_rounds: usize,
}

impl Conversation {
    pub fn new(provider: &str, model: &str, system: String, tools: Vec<ToolSpec>) -> Self {
        Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
            system,
            tools,
            messages: Vec::new(),
            rounds: Vec::new(),
            turn_start: 0,
            turn_rounds: 0,
        }
    }

    pub fn rounds(&self) -> &[Round] {
        &self.rounds
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// The round waiting to be sent, if any.
    pub fn pending(&self) -> Option<&Round> {
        self.rounds
            .last()
            .filter(|round| round.state == RoundState::Ready)
    }

    /// The index in the turn (from 0) of the pending round, for
    /// [`ReviewMode::pauses`].
    pub fn pending_index(&self) -> usize {
        self.turn_rounds.saturating_sub(1)
    }

    /// Starts a turn with the user's message and prepares its first round.
    pub fn start_turn(&mut self, text: String) -> &Round {
        self.turn_start = self.messages.len();
        self.turn_rounds = 0;
        self.messages.push(Message::User { text });
        self.prepare()
    }

    /// Lets a turn that reached [`MAX_ROUNDS`] go on, answering the tools
    /// its last round asked for.
    pub fn continue_turn(&mut self, steps: Vec<ToolStep>) -> &Round {
        self.turn_rounds = 0;
        self.answer_tools(steps)
    }

    fn prepare(&mut self) -> &Round {
        let (messages, omitted) = self.window();
        let request = ModelRequest {
            model: self.model.clone(),
            system: self.system.clone(),
            messages,
            tools: self.tools.clone(),
            max_output_tokens: MAX_OUTPUT_TOKENS,
        };
        self.turn_rounds += 1;
        let round = Round {
            number: self.rounds.len() + 1,
            estimated_tokens: estimate_tokens(&request),
            request,
            omitted_messages: omitted,
            state: RoundState::Ready,
            tools: Vec::new(),
        };
        self.rounds.push(round);
        let last = self.rounds.len() - 1;
        &self.rounds[last]
    }

    /// The messages a request carries: the current turn whole, and earlier
    /// turns from the newest back while they fit.
    fn window(&self) -> (Vec<Message>, usize) {
        let current: usize = self.messages[self.turn_start..]
            .iter()
            .map(Message::len)
            .sum();
        let mut budget = MAX_HISTORY_BYTES.saturating_sub(current);
        // Turn boundaries before the current one: each user message.
        let starts: Vec<usize> = self.messages[..self.turn_start]
            .iter()
            .enumerate()
            .filter(|(_, message)| matches!(message, Message::User { .. }))
            .map(|(at, _)| at)
            .collect();
        let mut keep_from = self.turn_start;
        for (index, start) in starts.iter().enumerate().rev() {
            let end = starts.get(index + 1).copied().unwrap_or(self.turn_start);
            let size: usize = self.messages[*start..end].iter().map(Message::len).sum();
            if size > budget {
                break;
            }
            budget -= size;
            keep_from = *start;
        }
        (self.messages[keep_from..].to_vec(), keep_from)
    }

    /// Records the answer to the pending round.
    pub fn record(&mut self, result: RoundResult, elapsed_ms: u64) -> Next {
        tracing::info!(
            provider = %self.provider,
            elapsed_ms,
            input_tokens = ?result.usage.input,
            output_tokens = ?result.usage.output,
            tool_calls = result.tool_calls.len(),
            truncated = result.truncated,
            "assistant round"
        );
        self.messages.push(Message::Assistant {
            text: result.text.clone(),
            tool_calls: result.tool_calls.clone(),
        });
        let calls = result.tool_calls.clone();
        if let Some(round) = self.rounds.last_mut() {
            round.state = RoundState::Answered { result, elapsed_ms };
        }
        if calls.is_empty() {
            Next::Done
        } else if self.turn_rounds >= MAX_ROUNDS {
            Next::LimitReached(calls)
        } else {
            Next::Tools(calls)
        }
    }

    /// Records that the pending round failed or was cancelled. The turn is
    /// taken back out of the history, so the next request starts clean; the
    /// round stays in the trace.
    pub fn record_error(&mut self, error: &DriverError) {
        tracing::info!(provider = %self.provider, error_code = ?error.code, "assistant round failed");
        if let Some(round) = self.rounds.last_mut() {
            round.state = if error.code == ErrorCode::Cancelled {
                RoundState::Cancelled
            } else {
                RoundState::Failed {
                    code: error.code,
                    message: error.message.clone(),
                }
            };
        }
        self.messages.truncate(self.turn_start);
        self.turn_rounds = 0;
    }

    /// Answers the tools of the last round and prepares the next one.
    pub fn answer_tools(&mut self, steps: Vec<ToolStep>) -> &Round {
        for step in &steps {
            self.messages.push(Message::Tool {
                call_id: step.call.id.clone(),
                name: step.call.name.clone(),
                content: step.output.clone(),
            });
        }
        if let Some(round) = self.rounds.last_mut() {
            round.tools = steps;
        }
        self.prepare()
    }

    /// All tokens the conversation used.
    pub fn usage(&self) -> Usage {
        self.rounds
            .iter()
            .fold(Usage::default(), |sum, round| sum.plus(round.usage()))
    }

    /// What the conversation cost at `price`, when every answered round
    /// reported its tokens.
    pub fn cost(&self, price: &Price) -> Option<f64> {
        self.rounds
            .iter()
            .filter(|round| matches!(round.state, RoundState::Answered { .. }))
            .map(|round| price.cost(&round.usage()))
            .sum()
    }

    /// The whole trace as JSON, for **Copy trace**: every request as it was
    /// sent, every answer, tool step, token count and time.
    pub fn trace_json(&self) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "provider": self.provider,
            "model": self.model,
            "rounds": self.rounds,
        }))
        .unwrap_or_default()
    }
}

/// Runs every tool call of a round against the cache, in order.
pub async fn run_tools(tools: &CacheTools, calls: &[ToolCall]) -> Vec<ToolStep> {
    use tracing::Instrument as _;
    let mut steps = Vec::with_capacity(calls.len());
    for call in calls {
        let started = Instant::now();
        // The name comes from the model: only a plain tool name is recorded.
        let plain = !call.name.is_empty()
            && call.name.len() <= 40
            && call
                .name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_');
        let span = tracing::info_span!(
            "assistant_tool",
            jykre.assistant.tool = if plain { call.name.as_str() } else { "other" }
        );
        let output = tools.call(call).instrument(span).await;
        steps.push(ToolStep {
            call: call.clone(),
            output,
            elapsed_ms: started.elapsed().as_millis() as u64,
        });
    }
    steps
}

/// Sends a request and times it.
pub async fn send(
    assistant: &Assistant,
    request: &ModelRequest,
    events: &UnboundedSender<RoundEvent>,
    cancel: &CancellationToken,
) -> (DriverResult<RoundResult>, u64) {
    use tracing::Instrument as _;
    let config = assistant.config();
    let span = crate::client_span!(
        "chat",
        otel.name = format!("chat {}", config.model),
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = config.kind.key(),
        gen_ai.request.model = config.model.as_str(),
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        jykre.assistant.tool_calls = tracing::field::Empty
    );
    let started = Instant::now();
    let result = assistant
        .send(request, events, cancel)
        .instrument(span.clone())
        .await;
    match &result {
        Ok(round) => {
            if let Some(input) = round.usage.input {
                span.record(
                    "gen_ai.usage.input_tokens",
                    crate::telemetry::attributes::count(input),
                );
            }
            if let Some(output) = round.usage.output {
                span.record(
                    "gen_ai.usage.output_tokens",
                    crate::telemetry::attributes::count(output),
                );
            }
            span.record(
                "jykre.assistant.tool_calls",
                crate::telemetry::attributes::count(round.tool_calls.len()),
            );
        }
        Err(error) => crate::telemetry::attributes::record_error(&span, error),
    }
    (result, started.elapsed().as_millis() as u64)
}

/// Runs a turn to its end without pausing for review: every round is sent
/// as soon as it is prepared. The panel uses the same steps one by one.
pub async fn drive(
    conversation: &mut Conversation,
    assistant: &Assistant,
    tools: &CacheTools,
    events: &UnboundedSender<RoundEvent>,
    cancel: &CancellationToken,
) -> DriverResult<Next> {
    loop {
        let Some(round) = conversation.pending() else {
            return Ok(Next::Done);
        };
        let request = round.request.clone();
        let (result, elapsed_ms) = send(assistant, &request, events, cancel).await;
        let next = match result {
            Ok(result) => conversation.record(result, elapsed_ms),
            Err(error) => {
                conversation.record_error(&error);
                return Err(error);
            }
        };
        match next {
            Next::Tools(calls) => {
                let steps = run_tools(tools, &calls).await;
                conversation.answer_tools(steps);
            }
            other => return Ok(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answered(text: &str, calls: Vec<ToolCall>) -> RoundResult {
        RoundResult {
            text: text.into(),
            tool_calls: calls,
            usage: Usage {
                input: Some(100),
                output: Some(10),
                ..Usage::default()
            },
            truncated: false,
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "describe_relation".into(),
            arguments: json!({"path": "orders"}),
        }
    }

    #[test]
    fn a_turn_goes_through_tools_and_keeps_the_trace() {
        let mut conversation = Conversation::new("openai", "m", "system".into(), Vec::new());
        assert_eq!(conversation.start_turn("question".into()).number, 1);
        assert_eq!(conversation.pending_index(), 0);
        let next = conversation.record(answered("", vec![call("a")]), 5);
        assert_eq!(next, Next::Tools(vec![call("a")]));
        let round = conversation.answer_tools(vec![ToolStep {
            call: call("a"),
            output: "orders (table)".into(),
            elapsed_ms: 1,
        }]);
        assert_eq!(round.number, 2);
        assert_eq!(round.request.messages.len(), 3);
        assert_eq!(conversation.pending_index(), 1);
        assert_eq!(
            conversation.record(answered("SELECT 1", Vec::new()), 5),
            Next::Done
        );
        assert!(conversation.pending().is_none());
        assert_eq!(conversation.usage().input, Some(200));
        let price = Price {
            input_per_mtok: 1.0,
            output_per_mtok: 10.0,
            ..Price::default()
        };
        assert!((conversation.cost(&price).unwrap() - 0.0004).abs() < 1e-12);
        let trace = conversation.trace_json();
        assert!(trace.contains("orders (table)") && trace.contains("\"number\": 2"));
    }

    #[test]
    fn review_modes_pause_where_they_say() {
        assert!(ReviewMode::EveryRequest.pauses(3));
        assert!(ReviewMode::FirstOnly.pauses(0) && !ReviewMode::FirstOnly.pauses(1));
        assert!(!ReviewMode::Never.pauses(0));
        for mode in ReviewMode::ALL {
            assert_eq!(ReviewMode::from_key(mode.key()), Some(mode));
        }
    }

    #[test]
    fn the_round_limit_stops_a_turn_until_it_is_continued() {
        let mut conversation = Conversation::new("openai", "m", "s".into(), Vec::new());
        conversation.start_turn("q".into());
        for round in 1..=MAX_ROUNDS {
            let next = conversation.record(answered("", vec![call("x")]), 1);
            if round < MAX_ROUNDS {
                assert!(matches!(next, Next::Tools(_)));
                conversation.answer_tools(Vec::new());
            } else {
                assert!(matches!(next, Next::LimitReached(_)));
            }
        }
        conversation.continue_turn(Vec::new());
        assert_eq!(conversation.pending_index(), 0);
    }

    #[test]
    fn a_failed_turn_leaves_the_history_and_old_turns_drop_first() {
        let mut conversation = Conversation::new("openai", "m", "s".into(), Vec::new());
        conversation.start_turn("first".into());
        conversation.record(answered("one", Vec::new()), 1);
        conversation.start_turn("second".into());
        conversation.record_error(&DriverError::cancelled());
        assert_eq!(conversation.rounds()[1].state, RoundState::Cancelled);
        let round = conversation.start_turn("third".into());
        let texts: Vec<_> = round
            .request
            .messages
            .iter()
            .map(|m| match m {
                Message::User { text } | Message::Assistant { text, .. } => text.as_str(),
                Message::Tool { content, .. } => content.as_str(),
            })
            .collect();
        assert_eq!(texts, ["first", "one", "third"]);

        let mut long = Conversation::new("openai", "m", "s".into(), Vec::new());
        long.start_turn("x".repeat(MAX_HISTORY_BYTES));
        long.record(answered("ok", Vec::new()), 1);
        let round = long.start_turn("now".into());
        assert_eq!(round.request.messages.len(), 1);
        assert_eq!(round.omitted_messages, 2);
    }
}
