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
pub const MAX_OUTPUT_TOKENS: u32 = 2048;
/// Tool answers shorter than this are kept when results are pruned: they
/// say little ("declined", an error) and cost almost nothing.
pub const PRUNE_FROM_BYTES: usize = 256;
/// How a pruned tool answer begins, so pruning twice changes nothing.
const PRUNED: &str = "[Omitted to keep the context small:";

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
    /// For `run_query`: what the user decided. Built only by
    /// [`super::released`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<QueryRecord>,
}

/// What became of a `run_query` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryDecision {
    /// Not one plain statement: refused before asking the user.
    Rejected,
    /// The user did not run it.
    Declined,
    /// The user ran it and kept the result.
    Withheld,
    /// The user ran it and sent the result.
    Sent,
}

/// A `run_query` call as the trace keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueryRecord {
    pub sql: String,
    pub decision: QueryDecision,
    /// The columns whose values the user hid.
    pub hidden: Vec<String>,
    pub rows_sent: usize,
}

/// The calls of a round the cache answers, and the `run_query` calls that
/// wait for the user, each in its original order.
pub fn split_calls(calls: Vec<ToolCall>) -> (Vec<ToolCall>, Vec<ToolCall>) {
    calls
        .into_iter()
        .partition(|call| call.name != super::tools::RUN_QUERY)
}

/// The statement of a `run_query` call, when it may be put to the user:
/// exactly one statement and no `:name` marker. Otherwise the answer the
/// model gets instead.
pub fn query_sql(
    call: &ToolCall,
    dialect: Option<jykre_core::SqlDialect>,
) -> Result<String, String> {
    let sql = call
        .arguments
        .get("sql")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if sql.is_empty() {
        return Err("Error: `sql` is required.".into());
    }
    if crate::sql::statements(sql, dialect).len() != 1 {
        return Err("Error: run_query takes exactly one statement. Ask for one at a time.".into());
    }
    if !crate::sql::named_markers(sql, dialect).is_empty() {
        return Err(
            "Error: run_query cannot bind :name parameters. Write the values in the SQL.".into(),
        );
    }
    Ok(sql.to_owned())
}

/// The short reason the model gave for a `run_query` call, if any.
pub fn query_purpose(call: &ToolCall) -> Option<&str> {
    call.arguments
        .get("purpose")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|purpose| !purpose.is_empty())
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
    /// The first messages the last sent request carried too, unchanged:
    /// the review shows what is new after them.
    pub repeated_messages: usize,
    /// The system prompt is the one the last sent request carried.
    pub repeated_system: bool,
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

/// How the context was made smaller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionKind {
    /// Long tool answers were replaced by a line saying what they were.
    Pruned,
    /// The model summarized the conversation, and the summary replaced it.
    Summarized,
}

/// One compaction, as the trace and the panel show it. Token counts are
/// estimates of the next request's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Compaction {
    pub kind: CompactionKind,
    /// Tool answers pruned, or messages a summary replaced.
    pub count: usize,
    pub before_tokens: u64,
    pub after_tokens: u64,
}

/// What the panel's summary shows about the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextStats {
    pub rounds: usize,
    /// Tokens of every answered round, as the provider reported them.
    pub usage: Usage,
    /// Tokens the next request carries: the last request's real input and
    /// output, or an estimate when a request waits or the context was
    /// compacted since.
    pub context_tokens: Option<u64>,
    pub context_estimated: bool,
}

/// A conversation with the model about one connection. Every request
/// carries the whole conversation; it only gets smaller when the user
/// compacts it ([`Conversation::prune_tool_results`],
/// [`Conversation::start_summary`]).
#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    provider: String,
    model: String,
    /// The system prompt the conversation started with.
    base_system: String,
    /// What requests carry: the base prompt, with the last summary.
    system: String,
    tools: Vec<ToolSpec>,
    messages: Vec<Message>,
    rounds: Vec<Round>,
    /// Where the current turn's user message is in `messages`.
    turn_start: usize,
    /// Rounds the current turn took since it started or was continued.
    turn_rounds: usize,
    /// A summary was asked for: the estimated context before it.
    summarizing: Option<u64>,
    compactions: Vec<Compaction>,
    /// The context changed since the last answer reported its size.
    compacted_since_answer: bool,
}

impl Conversation {
    pub fn new(provider: &str, model: &str, system: String, tools: Vec<ToolSpec>) -> Self {
        Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
            base_system: system.clone(),
            system,
            tools,
            messages: Vec::new(),
            rounds: Vec::new(),
            turn_start: 0,
            turn_rounds: 0,
            summarizing: None,
            compactions: Vec::new(),
            compacted_since_answer: false,
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

    /// The request the conversation would send now: the whole conversation.
    /// Nothing is left out on its own, so each request begins as the one
    /// before it did and providers can serve that beginning from their
    /// prompt cache.
    fn request(&self) -> ModelRequest {
        ModelRequest {
            model: self.model.clone(),
            system: self.system.clone(),
            messages: self.messages.clone(),
            tools: self.tools.clone(),
            max_output_tokens: MAX_OUTPUT_TOKENS,
        }
    }

    fn prepare(&mut self) -> &Round {
        let request = self.request();
        // What the last request that left carried as well.
        let sent = self.rounds.iter().rev().find(|round| {
            matches!(
                round.state,
                RoundState::Answered { .. } | RoundState::Failed { .. }
            )
        });
        let (repeated_messages, repeated_system) = sent.map_or((0, false), |sent| {
            (
                sent.request
                    .messages
                    .iter()
                    .zip(&request.messages)
                    .take_while(|(before, now)| before == now)
                    .count(),
                sent.request.system == request.system,
            )
        });
        self.turn_rounds += 1;
        let round = Round {
            number: self.rounds.len() + 1,
            estimated_tokens: estimate_tokens(&request),
            request,
            repeated_messages,
            repeated_system,
            state: RoundState::Ready,
            tools: Vec::new(),
        };
        self.rounds.push(round);
        let last = self.rounds.len() - 1;
        &self.rounds[last]
    }

    /// The estimated size of the next request.
    pub fn context_estimate(&self) -> u64 {
        estimate_tokens(&self.request())
    }

    /// Sizes and tokens for the panel's summary.
    pub fn stats(&self) -> ContextStats {
        let answered = self
            .rounds
            .iter()
            .filter(|round| matches!(round.state, RoundState::Answered { .. }));
        let last = answered.clone().next_back();
        let (context_tokens, context_estimated) = if let Some(pending) = self.pending() {
            (Some(pending.estimated_tokens), true)
        } else if self.compacted_since_answer {
            (Some(self.context_estimate()), true)
        } else {
            // The last answer joins the context of the next request.
            let usage = last.map(Round::usage);
            match usage.and_then(|usage| Some(usage.input? + usage.output.unwrap_or(0))) {
                Some(tokens) => (Some(tokens), false),
                None if self.messages.is_empty() => (None, false),
                None => (Some(self.context_estimate()), true),
            }
        };
        ContextStats {
            rounds: answered.count(),
            usage: self.usage(),
            context_tokens,
            context_estimated,
        }
    }

    /// The compactions so far, in order.
    pub fn compactions(&self) -> &[Compaction] {
        &self.compactions
    }

    /// Nothing is on its way and there is something to compact.
    pub fn can_compact(&self) -> bool {
        self.pending().is_none() && self.summarizing.is_none() && !self.messages.is_empty()
    }

    /// Replaces every tool answer of [`PRUNE_FROM_BYTES`] or more with a
    /// line naming the call, so the model can ask again. Questions, answers
    /// and the calls themselves stay, and so the values a query sent are not
    /// sent again. `None` when nothing was long enough.
    pub fn prune_tool_results(&mut self) -> Option<Compaction> {
        if !self.can_compact() {
            return None;
        }
        let before = self.context_estimate();
        let arguments: std::collections::HashMap<String, String> = self
            .messages
            .iter()
            .flat_map(|message| match message {
                Message::Assistant { tool_calls, .. } => tool_calls.as_slice(),
                _ => &[],
            })
            .map(|call| (call.id.clone(), clip(&call.arguments.to_string(), 160)))
            .collect();
        let mut count = 0;
        for message in &mut self.messages {
            if let Message::Tool {
                call_id,
                name,
                content,
            } = message
                && content.len() >= PRUNE_FROM_BYTES
                && !content.starts_with(PRUNED)
            {
                let called = arguments.get(call_id.as_str()).map_or("", String::as_str);
                *content = format!(
                    "{PRUNED} the answer of {name} {called}. Call the tool again if you need it.]"
                );
                count += 1;
            }
        }
        if count == 0 {
            return None;
        }
        Some(self.compacted(CompactionKind::Pruned, count, before))
    }

    fn compacted(&mut self, kind: CompactionKind, count: usize, before: u64) -> Compaction {
        let compaction = Compaction {
            kind,
            count,
            before_tokens: before,
            after_tokens: self.context_estimate(),
        };
        tracing::info!(
            kind = ?kind,
            count,
            before_tokens = compaction.before_tokens,
            after_tokens = compaction.after_tokens,
            "assistant context compacted"
        );
        self.compactions.push(compaction);
        self.compacted_since_answer = true;
        compaction
    }

    /// Asks the model for a summary that will replace the conversation: a
    /// turn of its own, reviewed like any other request.
    pub fn start_summary(&mut self) -> Option<&Round> {
        if !self.can_compact() {
            return None;
        }
        self.summarizing = Some(self.context_estimate());
        Some(self.start_turn(super::prompts::summary_request()))
    }

    /// A summary was asked for and not applied yet.
    pub fn summarizing(&self) -> bool {
        self.summarizing.is_some()
    }

    /// Puts the model's summary in place of the conversation: it becomes
    /// part of the system prompt and every message goes. An answer that
    /// called tools or said nothing changes nothing.
    pub fn apply_summary(&mut self) -> Result<Compaction, String> {
        let Some(before) = self.summarizing.take() else {
            return Err("No summary was asked for.".into());
        };
        let summary = match self.messages.last() {
            Some(Message::Assistant { text, tool_calls })
                if tool_calls.is_empty() && !text.trim().is_empty() =>
            {
                text.trim().to_owned()
            }
            _ => {
                self.messages.truncate(self.turn_start);
                self.turn_rounds = 0;
                return Err("The model did not answer with a summary.".into());
            }
        };
        let count = self.turn_start;
        self.system = super::prompts::with_summary(&self.base_system, &summary);
        self.messages.clear();
        self.turn_start = 0;
        self.turn_rounds = 0;
        Ok(self.compacted(CompactionKind::Summarized, count, before))
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
        self.compacted_since_answer = false;
        // A summary turn ends with its first answer; the caller applies it.
        if calls.is_empty() || self.summarizing.is_some() {
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
        self.summarizing = None;
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
            "compactions": self.compactions,
        }))
        .unwrap_or_default()
    }
}

/// `text` cut to `max` characters, with `…` when cut.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
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
        let answer = tools.call(call).instrument(span).await;
        steps.push(ToolStep {
            call: call.clone(),
            output: answer.text,
            elapsed_ms: started.elapsed().as_millis() as u64,
            query: None,
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

/// Who answers the `run_query` calls of a turn driven by [`drive`]: the
/// panel asks the user instead.
pub trait QueryGate {
    fn answer(&mut self, call: ToolCall) -> impl std::future::Future<Output = ToolStep>;
}

/// Declines every query, as a user who never approves would.
pub struct DeclineQueries;

impl QueryGate for DeclineQueries {
    async fn answer(&mut self, call: ToolCall) -> ToolStep {
        match query_sql(&call, None) {
            Ok(sql) => super::released::declined(call, sql),
            Err(reason) => super::released::rejected(call, String::new(), reason),
        }
    }
}

/// Puts the steps back in the order the model asked for them.
pub fn in_call_order(calls: &[ToolCall], mut steps: Vec<ToolStep>) -> Vec<ToolStep> {
    steps.sort_by_key(|step| {
        calls
            .iter()
            .position(|call| call.id == step.call.id)
            .unwrap_or(usize::MAX)
    });
    steps
}

/// Runs a turn to its end without pausing for review: every round is sent
/// as soon as it is prepared, and `queries` answers the `run_query` calls.
/// The panel uses the same steps one by one.
pub async fn drive(
    conversation: &mut Conversation,
    assistant: &Assistant,
    tools: &CacheTools,
    queries: &mut impl QueryGate,
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
                let (cache, asked) = split_calls(calls.clone());
                let mut steps = run_tools(tools, &cache).await;
                for call in asked {
                    steps.push(queries.answer(call).await);
                }
                conversation.answer_tools(in_call_order(&calls, steps));
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
            query: None,
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

    fn texts(round: &Round) -> Vec<&str> {
        round
            .request
            .messages
            .iter()
            .map(|m| match m {
                Message::User { text } | Message::Assistant { text, .. } => text.as_str(),
                Message::Tool { content, .. } => content.as_str(),
            })
            .collect()
    }

    #[test]
    fn a_failed_turn_leaves_the_history_and_nothing_is_left_out_on_its_own() {
        let mut conversation = Conversation::new("openai", "m", "s".into(), Vec::new());
        conversation.start_turn("x".repeat(200 * 1024));
        conversation.record(answered("one", Vec::new()), 1);
        conversation.start_turn("second".into());
        conversation.record_error(&DriverError::cancelled());
        assert_eq!(conversation.rounds()[1].state, RoundState::Cancelled);
        let round = conversation.start_turn("third".into());
        // However long, the history goes whole until the user compacts it.
        assert_eq!(texts(round)[1..], ["one", "third"]);
        assert_eq!(texts(round)[0].len(), 200 * 1024);
    }

    #[test]
    fn each_request_knows_what_the_last_sent_one_carried() {
        let mut conversation = Conversation::new("openai", "m", "s".into(), Vec::new());
        let first = conversation.start_turn("q".into());
        assert_eq!((first.repeated_messages, first.repeated_system), (0, false));
        conversation.record(answered("", vec![call("a")]), 1);
        let second = conversation.answer_tools(vec![ToolStep {
            call: call("a"),
            output: "orders (table)".into(),
            elapsed_ms: 1,
            query: None,
        }]);
        // The question went before; the call and its answer are new.
        assert_eq!(
            (second.repeated_messages, second.repeated_system),
            (1, true)
        );
        assert_eq!(second.request.messages.len(), 3);
        conversation.record(answered("done", Vec::new()), 1);
        let third = conversation.start_turn("r".into());
        assert_eq!(third.repeated_messages, 3);
        // A failed request left too: the next turn repeats what it carried
        // up to the failed question.
        conversation.record_error(&DriverError::new(ErrorCode::Timeout, "slow"));
        let fourth = conversation.start_turn("s".into());
        assert_eq!(fourth.repeated_messages, 4);
        assert_eq!(fourth.request.messages.len(), 5);
    }

    fn with_tool_answers() -> Conversation {
        let mut conversation = Conversation::new("openai", "m", "base".into(), Vec::new());
        conversation.start_turn("question".into());
        conversation.record(answered("", vec![call("a"), call("b")]), 1);
        conversation.answer_tools(vec![
            ToolStep {
                call: call("a"),
                output: format!("synthetic-visible-5b1e {}", "x".repeat(PRUNE_FROM_BYTES)),
                elapsed_ms: 1,
                query: None,
            },
            ToolStep {
                call: call("b"),
                output: "The user declined to run this query.".into(),
                elapsed_ms: 1,
                query: None,
            },
        ]);
        conversation.record(answered("SELECT 1", Vec::new()), 1);
        conversation
    }

    #[test]
    fn pruning_replaces_long_tool_answers_and_keeps_the_calls() {
        let mut conversation = with_tool_answers();
        let before = conversation.context_estimate();
        let pruned = conversation.prune_tool_results().unwrap();
        assert_eq!(pruned.kind, CompactionKind::Pruned);
        assert_eq!(pruned.count, 1);
        assert_eq!(pruned.before_tokens, before);
        assert!(pruned.after_tokens < before);
        let round = conversation.start_turn("next".into());
        let texts = texts(round);
        assert!(texts[2].starts_with(PRUNED) && texts[2].contains("describe_relation"));
        assert!(texts[2].contains("orders"));
        // A short answer stays; so do questions, answers and the calls.
        assert_eq!(texts[3], "The user declined to run this query.");
        assert_eq!(texts[0], "question");
        assert_eq!(texts[4], "SELECT 1");
        // A value a query sent is not sent again.
        let body = serde_json::to_string(&round.request).unwrap();
        assert!(!body.contains("synthetic-visible-5b1e"));
        conversation.record(answered("ok", Vec::new()), 1);
        assert!(
            conversation.prune_tool_results().is_none(),
            "pruning twice changes nothing"
        );
        assert_eq!(conversation.compactions().len(), 1);
        assert!(conversation.trace_json().contains("\"pruned\""));
    }

    #[test]
    fn a_summary_replaces_the_conversation_and_joins_the_system_prompt() {
        let mut conversation = with_tool_answers();
        let round = conversation.start_summary().unwrap();
        assert_eq!(texts(round).last(), Some(&prompts_summary().as_str()));
        assert!(conversation.summarizing());
        assert!(!conversation.can_compact(), "nothing else while it waits");
        conversation.record(answered("Orders has id and status.", Vec::new()), 1);
        let summarized = conversation.apply_summary().unwrap();
        assert_eq!(summarized.kind, CompactionKind::Summarized);
        assert_eq!(summarized.count, 5);
        let round = conversation.start_turn("next".into());
        assert_eq!(texts(round), ["next"]);
        assert!(round.request.system.starts_with("base"));
        assert!(round.request.system.contains("Orders has id and status."));
        assert!(!round.repeated_system);
        conversation.record(answered("ok", Vec::new()), 1);

        // A second summary replaces the first one in the system prompt.
        conversation.start_summary().unwrap();
        conversation.record(answered("Second summary.", Vec::new()), 1);
        conversation.apply_summary().unwrap();
        let round = conversation.start_turn("more".into());
        assert!(round.request.system.contains("Second summary."));
        assert!(!round.request.system.contains("Orders has id and status."));
    }

    #[test]
    fn a_summary_that_calls_tools_or_fails_changes_nothing() {
        let mut conversation = with_tool_answers();
        conversation.start_summary().unwrap();
        // The turn ends with the first answer, tools or not.
        assert_eq!(
            conversation.record(answered("", vec![call("x")]), 1),
            Next::Done
        );
        assert!(conversation.apply_summary().is_err());
        assert!(!conversation.summarizing());
        let round = conversation.start_turn("next".into());
        assert_eq!(texts(round).len(), 6);
        conversation.record(answered("ok", Vec::new()), 1);

        conversation.start_summary().unwrap();
        conversation.record_error(&DriverError::new(ErrorCode::Connection, "down"));
        assert!(!conversation.summarizing());
        assert!(conversation.can_compact());
    }

    #[test]
    fn the_context_size_is_the_last_reported_or_an_estimate() {
        let mut conversation = Conversation::new("openai", "m", "s".into(), Vec::new());
        assert_eq!(conversation.stats().context_tokens, None);
        let pending = conversation.start_turn("q".into()).estimated_tokens;
        assert_eq!(conversation.stats().context_tokens, Some(pending));
        assert!(conversation.stats().context_estimated);
        conversation.record(answered("a", Vec::new()), 1);
        let stats = conversation.stats();
        assert_eq!(
            (stats.context_tokens, stats.context_estimated),
            (Some(110), false)
        );
        assert_eq!(stats.rounds, 1);
        let mut pruned = with_tool_answers();
        pruned.prune_tool_results().unwrap();
        assert!(pruned.stats().context_estimated);
    }

    fn prompts_summary() -> String {
        super::super::prompts::summary_request()
    }
}
