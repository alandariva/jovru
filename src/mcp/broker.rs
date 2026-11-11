//! Pending MCP cards, one queue per token.
//!
//! The server computes a cache answer or checks a statement, then submits a
//! card. The view sends decisions. This type does not run SQL, read the
//! metadata cache, open a socket, or start a timer: the caller passes
//! `now` into [`Broker::expire`] and into every decision. It does not use a
//! UI toolkit.
//!
//! Cards of one token are answered in arrival order. A card behind another
//! is [`CardPlace::WaitingInLine`] and its 120-second deadline starts when
//! it reaches the front. Review is [`super::registry::ReviewPolicy`].

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::registry::ReviewPolicy;
use super::token::McpTokenId;

/// How long the user has to answer the card that is at the front.
pub const DECISION_TIMEOUT: Duration = Duration::from_secs(120);

/// Activity kept for this process. Older rows are dropped.
pub const ACTIVITY_LIMIT: usize = 200;

/// Normal result when the user does not send a cache answer.
pub const ANSWER_NOT_SHARED: &str = "The user chose not to share this answer.";

/// Normal result when the user declines, cancels the password, or stops.
pub const QUERY_DECLINED: &str = "The user declined to run this query.";

/// Error result when a cache card, or any other non-statement card, times out.
pub const DECISION_TIMED_OUT: &str = "The user did not answer in time; nothing was sent.";

/// Error result when a statement card times out. The query did not run.
pub const STATEMENT_TIMED_OUT: &str = "The user did not answer in time; the query did not run.";

/// Error result when a result card times out. No row count is included.
pub const RESULT_TIMED_OUT: &str =
    "The user did not answer in time; the query ran and its result was not sent.";

/// Error result when a second `run_query` arrives while one is still open.
pub const QUERY_BUSY: &str = concat!(
    "Error: another query from this client is waiting for the user. ",
    "Ask again after it is answered.",
);

/// Error result while the section is paused. No card is added.
pub const NOT_ACCEPTING: &str = "Jykre Studio is not accepting requests now.";

/// Error result when a card is closed because the application is quitting.
pub const QUITTING: &str = "Jykre Studio is quitting; nothing was sent.";

/// Shown in [`Debug`] in place of an answer, arguments, SQL, or a purpose.
const REDACTED: &str = "<redacted>";

/// The three assistant tools an MCP client can call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolName {
    SearchObjects,
    DescribeRelation,
    RunQuery,
}

impl ToolName {
    /// The plain tool name. There is no other spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SearchObjects => "search_objects",
            Self::DescribeRelation => "describe_relation",
            Self::RunQuery => "run_query",
        }
    }

    /// `search_objects` and `describe_relation`. `run_query` is not a cache tool.
    pub const fn is_cache(self) -> bool {
        matches!(self, Self::SearchObjects | Self::DescribeRelation)
    }
}

/// What the server maps onto a log field and a client result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    NotSent,
    Declined,
    Withheld,
    TimedOut,
    Cancelled,
    Busy,
    Paused,
}

impl Outcome {
    /// Stable log spelling. `rejected` and `disabled` are not broker outcomes.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::NotSent => "not_sent",
            Self::Declined => "declined",
            Self::Withheld => "withheld",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Busy => "busy",
            Self::Paused => "paused",
        }
    }
}

/// Identity of one card. Arrival order is the numeric order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CardId(u64);

impl CardId {
    /// The arrival counter. Useful only as an opaque id.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Where the card sits in its token's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardPlace {
    /// At the front, waiting for the user. [`Broker::expire`] closes it at `deadline`.
    Front { deadline: Instant },
    /// Behind another card of the same token. The deadline has not started.
    WaitingInLine,
    /// The statement was approved. The broker is not running the query.
    Running,
}

/// What the card shows. This value is for the view. It is not [`Debug`]:
/// the strings stay here so the view can render them, and they are absent
/// from [`Broker`]'s [`Debug`].
#[derive(Clone, PartialEq, Eq)]
pub enum CardContent {
    Cache {
        arguments: String,
        answer: String,
    },
    Statement {
        sql: String,
        purpose: String,
    },
    Result {
        sql: String,
        purpose: String,
        preview_text: String,
        row_count: usize,
        hidden_column_names: BTreeSet<String>,
    },
}

/// One card, as the Requests tab reads it.
///
/// The text fields are intentional. This type has no [`Debug`] impl.
#[derive(Clone, PartialEq, Eq)]
pub struct CardSnapshot {
    pub id: CardId,
    pub token: McpTokenId,
    pub tool: ToolName,
    pub place: CardPlace,
    /// Declines of this exact statement since the broker was created, not
    /// counting this card. Zero for a cache card.
    pub prior_declines: u32,
    pub content: CardContent,
}

/// Why a card left the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardEnd {
    Sent,
    NotSent,
    Declined,
    Withheld,
    /// The deadline passed. The outcome is [`Outcome::TimedOut`].
    NotAnswered,
    CancelledByClient,
    CancelledQuitting,
}

/// Returned by [`Broker::approve_statement`]. [`Broker::finish_running`]
/// accepts it once. A cancel or quit makes it stale.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[must_use]
pub struct RunPermit {
    card: CardId,
}

impl RunPermit {
    /// The statement card that was approved.
    pub const fn card(self) -> CardId {
        self.card
    }
}

/// Text the server can hand to the client.
///
/// [`Debug`] records the byte length and whether it is an error. It does
/// not record the text.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientResponse {
    pub text: String,
    /// An MCP error result. A normal tool result is `false`.
    pub is_error: bool,
}

impl fmt::Debug for ClientResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientResponse")
            .field("bytes", &self.text.len())
            .field("is_error", &self.is_error)
            .finish()
    }
}

/// The outcome of a call, and the response when the client is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct ClientResult {
    pub outcome: Outcome,
    /// `None` when the client cancelled: MCP sends no response.
    pub response: Option<ClientResponse>,
}

/// What [`Broker::submit_cache`] and [`Broker::submit_query`] return.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Arrival {
    /// The client can be answered now. No card was added.
    Ready(ClientResult),
    /// A card is waiting. The client is answered when the card closes.
    Held(CardId),
}

/// A card that just closed, plus what the client should receive.
///
/// [`Debug`] omits the token id and the response text.
#[derive(Clone, PartialEq, Eq)]
#[must_use]
pub struct Settled {
    pub card: CardId,
    pub token: McpTokenId,
    pub tool: ToolName,
    pub end: CardEnd,
    pub result: ClientResult,
}

impl fmt::Debug for Settled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Settled")
            .field("card", &self.card)
            .field("tool", &self.tool)
            .field("end", &self.end)
            .field("outcome", &self.result.outcome)
            .field("response", &self.result.response)
            .finish()
    }
}

/// One Activity row for this process. Arguments, purpose, and the answer
/// stay in memory for the session. [`Debug`] redacts them and the token id.
#[derive(Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    pub token: McpTokenId,
    pub tool: ToolName,
    pub outcome: Outcome,
    /// Byte length of the text the client received, or 0 when there was none.
    pub byte_length: usize,
    pub at: Instant,
    pub arguments: String,
    pub purpose: String,
    pub answer: String,
}

impl fmt::Debug for ActivityEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActivityEntry")
            .field("tool", &self.tool)
            .field("outcome", &self.outcome)
            .field("byte_length", &self.byte_length)
            .field("at", &self.at)
            .field("arguments", &REDACTED)
            .field("purpose", &REDACTED)
            .field("answer", &REDACTED)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Deciding { deadline: Instant },
    WaitingInLine,
    Running,
    Result { deadline: Instant },
}

/// Owned by the broker. No [`Debug`]: the strings are the answer, the
/// statement, and the purpose.
struct Card {
    id: CardId,
    tool: ToolName,
    phase: Phase,
    prior_declines: u32,
    arguments: String,
    answer: String,
    sql: String,
    purpose: String,
    preview_text: String,
    row_count: usize,
    hidden: BTreeSet<String>,
}

impl Card {
    fn cache(id: CardId, tool: ToolName, arguments: &str, answer: &str) -> Self {
        Self {
            id,
            tool,
            phase: Phase::WaitingInLine,
            prior_declines: 0,
            arguments: arguments.to_owned(),
            answer: answer.to_owned(),
            sql: String::new(),
            purpose: String::new(),
            preview_text: String::new(),
            row_count: 0,
            hidden: BTreeSet::new(),
        }
    }

    fn query(id: CardId, sql: &str, purpose: &str, prior_declines: u32) -> Self {
        Self {
            id,
            tool: ToolName::RunQuery,
            phase: Phase::WaitingInLine,
            prior_declines,
            arguments: String::new(),
            answer: String::new(),
            sql: sql.to_owned(),
            purpose: purpose.to_owned(),
            preview_text: String::new(),
            row_count: 0,
            hidden: BTreeSet::new(),
        }
    }

    fn snapshot(&self, token: McpTokenId) -> CardSnapshot {
        let place = match self.phase {
            Phase::Deciding { deadline } | Phase::Result { deadline } => {
                CardPlace::Front { deadline }
            }
            Phase::WaitingInLine => CardPlace::WaitingInLine,
            Phase::Running => CardPlace::Running,
        };
        let content = match (self.tool, self.phase) {
            (ToolName::RunQuery, Phase::Result { .. }) => CardContent::Result {
                sql: self.sql.clone(),
                purpose: self.purpose.clone(),
                preview_text: self.preview_text.clone(),
                row_count: self.row_count,
                hidden_column_names: self.hidden.clone(),
            },
            (ToolName::RunQuery, _) => CardContent::Statement {
                sql: self.sql.clone(),
                purpose: self.purpose.clone(),
            },
            (ToolName::SearchObjects | ToolName::DescribeRelation, _) => CardContent::Cache {
                arguments: self.arguments.clone(),
                answer: self.answer.clone(),
            },
        };
        CardSnapshot {
            id: self.id,
            token,
            tool: self.tool,
            place,
            prior_declines: self.prior_declines,
            content,
        }
    }

    fn logged_arguments(&self) -> &str {
        if self.sql.is_empty() {
            self.arguments.as_str()
        } else {
            self.sql.as_str()
        }
    }
}

/// Per-token memory for this process. Not [`Debug`]: decline keys are SQL.
#[derive(Default)]
struct TokenState {
    cards: VecDeque<Card>,
    /// Cache answers submitted since this broker was created, not counting
    /// calls refused because the broker was paused or quitting.
    cache_answers: u32,
    /// Exact statement text to how many times it was declined or had its
    /// password cancelled. Stop does not increment this.
    declines: HashMap<String, u32>,
    hidden: BTreeSet<String>,
}

struct Record<'a> {
    token: McpTokenId,
    tool: ToolName,
    outcome: Outcome,
    arguments: &'a str,
    purpose: &'a str,
    answer: &'a str,
    response: Option<&'a str>,
    at: Instant,
}

/// The approval queue for every token.
///
/// The owner serializes calls. The counter from [`Self::changes`] is the
/// signal a view subscribes to, the same idea as the automation engine's
/// change counter.
pub struct Broker {
    changes: watch::Sender<u64>,
    paused: bool,
    quitting: bool,
    next_id: u64,
    tokens: HashMap<McpTokenId, TokenState>,
    by_id: HashMap<CardId, McpTokenId>,
    activity: Vec<ActivityEntry>,
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

impl Broker {
    pub fn new() -> Self {
        Self {
            changes: watch::Sender::new(0),
            paused: false,
            quitting: false,
            next_id: 1,
            tokens: HashMap::new(),
            by_id: HashMap::new(),
            activity: Vec::new(),
        }
    }

    /// Grows on a new card, a decision, an expiry, activity, pause, and quit.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// While paused, [`Self::submit_cache`] and [`Self::submit_query`] add
    /// no card. Cards already waiting stay until decided, expired, or quit.
    pub fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        self.changed();
    }

    /// Set by [`Self::quit`]. Later submissions get the quitting error.
    pub fn is_quitting(&self) -> bool {
        self.quitting
    }

    /// Hold or return a cache answer.
    ///
    /// [`ToolName::RunQuery`] is not a cache card: the result is
    /// [`Outcome::NotSent`] with no response and no Activity row. Submit
    /// statements with [`Self::submit_query`].
    ///
    /// [`ReviewPolicy::FirstSession`] waits only for the first cache answer
    /// submitted for `token` since this broker was created. A call refused
    /// because the broker is paused or quitting does not count as that answer.
    pub fn submit_cache(
        &mut self,
        token: McpTokenId,
        tool: ToolName,
        review: ReviewPolicy,
        arguments: &str,
        answer: &str,
        now: Instant,
    ) -> Arrival {
        if !tool.is_cache() {
            return Arrival::Ready(ClientResult {
                outcome: Outcome::NotSent,
                response: None,
            });
        }
        if let Some(arrival) = self.refuse_now(token, tool, arguments, "", answer, now) {
            return arrival;
        }
        let hold = self.note_cache(token, review);
        if !hold {
            self.record(Record {
                token,
                tool,
                outcome: Outcome::Sent,
                arguments,
                purpose: "",
                answer,
                response: Some(answer),
                at: now,
            });
            self.changed();
            return Arrival::Ready(normal_result(Outcome::Sent, answer.to_owned()));
        }
        let id = self.allocate();
        let card_id = self.enqueue(token, Card::cache(id, tool, arguments, answer), now);
        self.changed();
        Arrival::Held(card_id)
    }

    /// Always a statement card, in every review mode, unless one `run_query`
    /// card for `token` is already open or the broker is refusing calls.
    ///
    /// The open card may be waiting, in line, running, or showing a result.
    /// A second call is [`Outcome::Busy`] and adds no card.
    pub fn submit_query(
        &mut self,
        token: McpTokenId,
        sql: &str,
        purpose: &str,
        now: Instant,
    ) -> Arrival {
        if let Some(arrival) = self.refuse_now(token, ToolName::RunQuery, sql, purpose, "", now) {
            return arrival;
        }
        if self.query_open(token) {
            return self.immediate_error(Record {
                token,
                tool: ToolName::RunQuery,
                outcome: Outcome::Busy,
                arguments: sql,
                purpose,
                answer: QUERY_BUSY,
                response: Some(QUERY_BUSY),
                at: now,
            });
        }
        let prior = self.prior_declines(token, sql);
        let id = self.allocate();
        let card_id = self.enqueue(token, Card::query(id, sql, purpose, prior), now);
        self.changed();
        Arrival::Held(card_id)
    }

    /// Close each token's front card when its decision deadline is `<= now`,
    /// then start the next card's deadline at `now`.
    ///
    /// A card that is running or still in line is left alone. One call closes
    /// at most the card that was already at the front: the card just promoted
    /// gets a fresh [`DECISION_TIMEOUT`].
    #[must_use]
    pub fn expire(&mut self, now: Instant) -> Vec<Settled> {
        let tokens: Vec<McpTokenId> = self.tokens.keys().copied().collect();
        let mut settled = Vec::new();
        for token in tokens {
            if let Some(one) = self.expire_token_id(token, now) {
                settled.push(one);
            }
        }
        if !settled.is_empty() {
            self.changed();
        }
        settled
    }

    /// Send the cache answer on the front card. The response text is that answer.
    #[must_use]
    pub fn send(&mut self, card: CardId, now: Instant) -> Option<Settled> {
        self.finish_cache(card, now, true)
    }

    /// Keep the cache answer. The client gets a normal [`ANSWER_NOT_SHARED`].
    #[must_use]
    pub fn dont_send(&mut self, card: CardId, now: Instant) -> Option<Settled> {
        self.finish_cache(card, now, false)
    }

    /// Decline the statement at the front. Counts toward [`CardSnapshot::prior_declines`].
    #[must_use]
    pub fn decline(&mut self, card: CardId, now: Instant) -> Option<Settled> {
        self.finish_statement(card, now, StatementStop::Decline)
    }

    /// The password dialog was cancelled. Same client result as [`Self::decline`],
    /// and it counts as a decline of that statement.
    #[must_use]
    pub fn password_cancelled(&mut self, card: CardId, now: Instant) -> Option<Settled> {
        self.finish_statement(card, now, StatementStop::PasswordCancelled)
    }

    /// Stop a running statement. Same client result as [`Self::decline`].
    /// It does not count as a prior decline: the statement was approved.
    #[must_use]
    pub fn stop(&mut self, card: CardId, now: Instant) -> Option<Settled> {
        self.finish_statement(card, now, StatementStop::Stop)
    }

    /// Move the front statement card to [`CardPlace::Running`] and return a permit.
    ///
    /// The broker does not run the query. The decision deadline stops until
    /// [`Self::finish_running`] opens the result card.
    #[must_use]
    pub fn approve_statement(&mut self, card: CardId) -> Option<RunPermit> {
        let token = self.by_id.get(&card).copied()?;
        {
            let state = self.tokens.get_mut(&token)?;
            let front = state.cards.front_mut()?;
            if front.id != card
                || front.tool != ToolName::RunQuery
                || !matches!(front.phase, Phase::Deciding { .. })
            {
                return None;
            }
            front.phase = Phase::Running;
        }
        self.changed();
        Some(RunPermit { card })
    }

    /// Open the result card for a permit from [`Self::approve_statement`].
    ///
    /// The card's hidden names are exactly `hidden_column_names`. Those names
    /// are added to the token's memory; names already remembered stay when
    /// they are not in this list. Pass [`Self::hidden_names`] for the token
    /// so the card starts from what this process has hidden. The result
    /// card's deadline starts at `now`.
    ///
    /// Returns `false` when the permit is stale. `preview_text` is stored for
    /// the view; this method does not turn a result into text.
    #[must_use]
    pub fn finish_running(
        &mut self,
        permit: RunPermit,
        preview_text: &str,
        row_count: usize,
        hidden_column_names: impl IntoIterator<Item = String>,
        now: Instant,
    ) -> bool {
        let hidden: BTreeSet<String> = hidden_column_names.into_iter().collect();
        let Some(token) = self.by_id.get(&permit.card).copied() else {
            return false;
        };
        {
            let Some(state) = self.tokens.get_mut(&token) else {
                return false;
            };
            {
                let Some(front) = state.cards.front_mut() else {
                    return false;
                };
                if front.id != permit.card || !matches!(front.phase, Phase::Running) {
                    return false;
                }
                front.phase = Phase::Result {
                    deadline: deadline_after(now),
                };
                front.preview_text = preview_text.to_owned();
                front.row_count = row_count;
                front.hidden.clone_from(&hidden);
            }
            for name in hidden {
                state.hidden.insert(name);
            }
        }
        self.changed();
        true
    }

    /// Send the result text the caller produced. Normal result, [`Outcome::Sent`].
    #[must_use]
    pub fn send_result(&mut self, card: CardId, text: &str, now: Instant) -> Option<Settled> {
        self.finish_result(card, now, Outcome::Sent, CardEnd::Sent, text)
    }

    /// Keep the result. `text` is the normal result the caller produced.
    #[must_use]
    pub fn withhold(&mut self, card: CardId, text: &str, now: Instant) -> Option<Settled> {
        self.finish_result(card, now, Outcome::Withheld, CardEnd::Withheld, text)
    }

    /// The client cancelled the call. The card closes as
    /// [`CardEnd::CancelledByClient`] and [`ClientResult::response`] is
    /// `None`, including when the card was waiting in line or running.
    #[must_use]
    pub fn cancel(&mut self, card: CardId, now: Instant) -> Option<Settled> {
        let (token, card) = self.take_card(card, now)?;
        let answer = kept(&card, "");
        self.record(Record {
            token,
            tool: card.tool,
            outcome: Outcome::Cancelled,
            arguments: card.logged_arguments(),
            purpose: card.purpose.as_str(),
            answer,
            response: None,
            at: now,
        });
        self.changed();
        Some(settled(
            token,
            &card,
            CardEnd::CancelledByClient,
            ClientResult {
                outcome: Outcome::Cancelled,
                response: None,
            },
        ))
    }

    /// Close every card as [`CardEnd::CancelledQuitting`]. Each client gets
    /// an error result [`QUITTING`]. Later submissions get the same error
    /// and no card.
    #[must_use]
    pub fn quit(&mut self, now: Instant) -> Vec<Settled> {
        let mut ids: Vec<CardId> = self.by_id.keys().copied().collect();
        ids.sort();
        let mut settled_cards = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(one) = self.close_quitting(id, now) {
                settled_cards.push(one);
            }
        }
        let changed = !settled_cards.is_empty() || !self.quitting;
        self.quitting = true;
        if changed {
            self.changed();
        }
        settled_cards
    }

    /// Column names hidden for `token` since this broker was created.
    pub fn hidden_names(&self, token: McpTokenId) -> BTreeSet<String> {
        self.tokens
            .get(&token)
            .map(|state| state.hidden.clone())
            .unwrap_or_default()
    }

    /// Replace the result card's hidden names and update the token's memory.
    ///
    /// A name removed from the card is forgotten. A name added is remembered.
    /// A remembered name that this card was not showing stays remembered.
    /// Returns `false` when `card` is not a result card.
    #[must_use]
    pub fn set_hidden(&mut self, card: CardId, names: impl IntoIterator<Item = String>) -> bool {
        let names: BTreeSet<String> = names.into_iter().collect();
        let Some(token) = self.by_id.get(&card).copied() else {
            return false;
        };
        let Some(state) = self.tokens.get_mut(&token) else {
            return false;
        };
        let Some(position) = state
            .cards
            .iter()
            .position(|candidate| candidate.id == card)
        else {
            return false;
        };
        let removed = {
            let Some(current) = state.cards.get_mut(position) else {
                return false;
            };
            if !matches!(current.phase, Phase::Result { .. }) {
                return false;
            }
            if current.hidden == names {
                return true;
            }
            std::mem::replace(&mut current.hidden, names.clone())
        };
        for name in &removed {
            if !names.contains(name) {
                state.hidden.remove(name);
            }
        }
        for name in names {
            state.hidden.insert(name);
        }
        self.changed();
        true
    }

    /// Every open card, in arrival order.
    pub fn cards(&self) -> Vec<CardSnapshot> {
        let mut snapshots = Vec::new();
        for (token, state) in &self.tokens {
            for card in &state.cards {
                snapshots.push(card.snapshot(*token));
            }
        }
        snapshots.sort_by_key(|card| card.id);
        snapshots
    }

    /// The newest rows are at the end. At most [`ACTIVITY_LIMIT`].
    pub fn activity(&self) -> &[ActivityEntry] {
        &self.activity
    }

    fn refuse_now(
        &mut self,
        token: McpTokenId,
        tool: ToolName,
        arguments: &str,
        purpose: &str,
        answer: &str,
        now: Instant,
    ) -> Option<Arrival> {
        let (outcome, text) = if self.quitting {
            (Outcome::Cancelled, QUITTING)
        } else if self.paused {
            (Outcome::Paused, NOT_ACCEPTING)
        } else {
            return None;
        };
        let retained = if answer.is_empty() { text } else { answer };
        Some(self.immediate_error(Record {
            token,
            tool,
            outcome,
            arguments,
            purpose,
            answer: retained,
            response: Some(text),
            at: now,
        }))
    }

    fn immediate_error(&mut self, call: Record<'_>) -> Arrival {
        let outcome = call.outcome;
        let text = call.response.unwrap_or("").to_owned();
        self.record(call);
        self.changed();
        Arrival::Ready(error_result(outcome, &text))
    }

    fn note_cache(&mut self, token: McpTokenId, review: ReviewPolicy) -> bool {
        let state = self.tokens.entry(token).or_default();
        let first = state.cache_answers == 0;
        state.cache_answers = state.cache_answers.saturating_add(1);
        match review {
            ReviewPolicy::EveryCall => true,
            ReviewPolicy::FirstSession => first,
            ReviewPolicy::Never => false,
        }
    }

    fn prior_declines(&self, token: McpTokenId, sql: &str) -> u32 {
        self.tokens
            .get(&token)
            .and_then(|state| state.declines.get(sql))
            .copied()
            .unwrap_or(0)
    }

    fn note_decline(&mut self, token: McpTokenId, sql: &str) {
        let state = self.tokens.entry(token).or_default();
        let count = state.declines.entry(sql.to_owned()).or_insert(0);
        *count = count.saturating_add(1);
    }

    fn query_open(&self, token: McpTokenId) -> bool {
        self.tokens.get(&token).is_some_and(|state| {
            state
                .cards
                .iter()
                .any(|card| card.tool == ToolName::RunQuery)
        })
    }

    fn allocate(&mut self) -> CardId {
        let id = CardId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    fn enqueue(&mut self, token: McpTokenId, mut card: Card, now: Instant) -> CardId {
        let id = card.id;
        {
            let state = self.tokens.entry(token).or_default();
            card.phase = if state.cards.is_empty() {
                Phase::Deciding {
                    deadline: deadline_after(now),
                }
            } else {
                Phase::WaitingInLine
            };
            state.cards.push_back(card);
        }
        self.by_id.insert(id, token);
        id
    }

    fn expire_token_id(&mut self, token: McpTokenId, now: Instant) -> Option<Settled> {
        let (id, text) = {
            let front = self.tokens.get(&token)?.cards.front()?;
            if !decision_due(front, now) {
                return None;
            }
            (front.id, timeout_text(front))
        };
        let (token, card) = self.take_card(id, now)?;
        let answer = kept(&card, text);
        self.record(Record {
            token,
            tool: card.tool,
            outcome: Outcome::TimedOut,
            arguments: card.logged_arguments(),
            purpose: card.purpose.as_str(),
            answer,
            response: Some(text),
            at: now,
        });
        Some(settled(
            token,
            &card,
            CardEnd::NotAnswered,
            error_result(Outcome::TimedOut, text),
        ))
    }

    fn finish_cache(&mut self, id: CardId, now: Instant, send: bool) -> Option<Settled> {
        let (token, card) = self.take_front_if(id, now, |card| {
            card.tool.is_cache() && matches!(card.phase, Phase::Deciding { .. })
        })?;
        let (outcome, end, response) = if send {
            (Outcome::Sent, CardEnd::Sent, card.answer.clone())
        } else {
            (
                Outcome::NotSent,
                CardEnd::NotSent,
                ANSWER_NOT_SHARED.to_owned(),
            )
        };
        self.record(Record {
            token,
            tool: card.tool,
            outcome,
            arguments: card.arguments.as_str(),
            purpose: "",
            answer: card.answer.as_str(),
            response: Some(response.as_str()),
            at: now,
        });
        self.changed();
        Some(settled(token, &card, end, normal_result(outcome, response)))
    }

    fn finish_statement(
        &mut self,
        id: CardId,
        now: Instant,
        stop: StatementStop,
    ) -> Option<Settled> {
        let running = matches!(stop, StatementStop::Stop);
        let (token, card) = self.take_front_if(id, now, |card| {
            if card.tool != ToolName::RunQuery {
                return false;
            }
            if running {
                matches!(card.phase, Phase::Running)
            } else {
                matches!(card.phase, Phase::Deciding { .. })
            }
        })?;
        if !running {
            self.note_decline(token, card.sql.as_str());
        }
        self.record(Record {
            token,
            tool: card.tool,
            outcome: Outcome::Declined,
            arguments: card.sql.as_str(),
            purpose: card.purpose.as_str(),
            answer: QUERY_DECLINED,
            response: Some(QUERY_DECLINED),
            at: now,
        });
        self.changed();
        Some(settled(
            token,
            &card,
            CardEnd::Declined,
            normal_result(Outcome::Declined, QUERY_DECLINED.to_owned()),
        ))
    }

    fn finish_result(
        &mut self,
        id: CardId,
        now: Instant,
        outcome: Outcome,
        end: CardEnd,
        text: &str,
    ) -> Option<Settled> {
        let (token, card) =
            self.take_front_if(id, now, |card| matches!(card.phase, Phase::Result { .. }))?;
        self.record(Record {
            token,
            tool: card.tool,
            outcome,
            arguments: card.sql.as_str(),
            purpose: card.purpose.as_str(),
            answer: text,
            response: Some(text),
            at: now,
        });
        self.changed();
        Some(settled(
            token,
            &card,
            end,
            normal_result(outcome, text.to_owned()),
        ))
    }

    fn close_quitting(&mut self, id: CardId, now: Instant) -> Option<Settled> {
        let (token, card) = self.take_card(id, now)?;
        let answer = kept(&card, QUITTING);
        self.record(Record {
            token,
            tool: card.tool,
            outcome: Outcome::Cancelled,
            arguments: card.logged_arguments(),
            purpose: card.purpose.as_str(),
            answer,
            response: Some(QUITTING),
            at: now,
        });
        Some(settled(
            token,
            &card,
            CardEnd::CancelledQuitting,
            error_result(Outcome::Cancelled, QUITTING),
        ))
    }

    fn take_front_if(
        &mut self,
        id: CardId,
        now: Instant,
        allow: impl FnOnce(&Card) -> bool,
    ) -> Option<(McpTokenId, Card)> {
        let token = self.by_id.get(&id).copied()?;
        let allowed = self
            .tokens
            .get(&token)?
            .cards
            .front()
            .is_some_and(|card| card.id == id && allow(card));
        if !allowed {
            return None;
        }
        self.take_card(id, now)
    }

    fn take_card(&mut self, id: CardId, now: Instant) -> Option<(McpTokenId, Card)> {
        let token = self.by_id.get(&id).copied()?;
        let card = {
            let state = self.tokens.get_mut(&token)?;
            let position = state.cards.iter().position(|card| card.id == id)?;
            let card = state.cards.remove(position)?;
            if position == 0 {
                start_front(&mut state.cards, now);
            }
            card
        };
        self.by_id.remove(&id);
        Some((token, card))
    }

    fn record(&mut self, call: Record<'_>) {
        self.activity.push(ActivityEntry {
            token: call.token,
            tool: call.tool,
            outcome: call.outcome,
            byte_length: call.response.map(str::len).unwrap_or(0),
            at: call.at,
            arguments: call.arguments.to_owned(),
            purpose: call.purpose.to_owned(),
            answer: call.answer.to_owned(),
        });
        if self.activity.len() > ACTIVITY_LIMIT {
            let overflow = self.activity.len() - ACTIVITY_LIMIT;
            self.activity.drain(..overflow);
        }
    }

    fn changed(&mut self) {
        self.changes
            .send_modify(|count| *count = count.wrapping_add(1));
    }
}

impl fmt::Debug for Broker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Counts only. Answer text, SQL, purpose, arguments, and token ids
        // stay out of this formatting so a log of the broker cannot carry them.
        formatter
            .debug_struct("Broker")
            .field("paused", &self.paused)
            .field("quitting", &self.quitting)
            .field("cards", &self.by_id.len())
            .field("activity", &self.activity.len())
            .field("arguments", &REDACTED)
            .field("answer", &REDACTED)
            .field("sql", &REDACTED)
            .field("purpose", &REDACTED)
            .finish()
    }
}

enum StatementStop {
    Decline,
    PasswordCancelled,
    Stop,
}

fn deadline_after(now: Instant) -> Instant {
    // `Instant + Duration` aborts on overflow. A clock that cannot add two
    // minutes is treated as already due on the next `expire`.
    now.checked_add(DECISION_TIMEOUT).unwrap_or(now)
}

fn decision_due(card: &Card, now: Instant) -> bool {
    match card.phase {
        Phase::Deciding { deadline } | Phase::Result { deadline } => deadline <= now,
        Phase::WaitingInLine | Phase::Running => false,
    }
}

fn timeout_text(card: &Card) -> &'static str {
    if matches!(card.phase, Phase::Result { .. }) {
        return RESULT_TIMED_OUT;
    }
    if card.tool == ToolName::RunQuery && matches!(card.phase, Phase::Deciding { .. }) {
        return STATEMENT_TIMED_OUT;
    }
    DECISION_TIMED_OUT
}

fn start_front(cards: &mut VecDeque<Card>, now: Instant) {
    let Some(front) = cards.front_mut() else {
        return;
    };
    if matches!(front.phase, Phase::WaitingInLine) {
        front.phase = Phase::Deciding {
            deadline: deadline_after(now),
        };
    }
}

/// Activity keeps the computed answer or the preview when the client
/// receives a different sentence. `fallback` covers a statement that
/// produced neither.
fn kept<'a>(card: &'a Card, fallback: &'a str) -> &'a str {
    if !card.answer.is_empty() {
        card.answer.as_str()
    } else if !card.preview_text.is_empty() {
        card.preview_text.as_str()
    } else {
        fallback
    }
}

fn settled(token: McpTokenId, card: &Card, end: CardEnd, result: ClientResult) -> Settled {
    Settled {
        card: card.id,
        token,
        tool: card.tool,
        end,
        result,
    }
}

fn normal_result(outcome: Outcome, text: String) -> ClientResult {
    ClientResult {
        outcome,
        response: Some(ClientResponse {
            text,
            is_error: false,
        }),
    }
}

fn error_result(outcome: Outcome, text: &str) -> ClientResult {
    ClientResult {
        outcome,
        response: Some(ClientResponse {
            text: text.to_owned(),
            is_error: true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    use super::*;

    fn token_id(raw: &str) -> McpTokenId {
        McpTokenId::parse(raw).unwrap()
    }

    fn held(arrival: Arrival) -> CardId {
        match arrival {
            Arrival::Held(card) => Some(card),
            Arrival::Ready(_) => None,
        }
        .unwrap()
    }

    fn ready(arrival: Arrival) -> ClientResult {
        match arrival {
            Arrival::Ready(result) => Some(result),
            Arrival::Held(_) => None,
        }
        .unwrap()
    }

    fn response(result: &ClientResult) -> &ClientResponse {
        result.response.as_ref().unwrap()
    }

    fn front_deadline(place: CardPlace) -> Instant {
        match place {
            CardPlace::Front { deadline } => Some(deadline),
            CardPlace::WaitingInLine | CardPlace::Running => None,
        }
        .unwrap()
    }

    fn prior(broker: &Broker, card: CardId) -> u32 {
        broker
            .cards()
            .into_iter()
            .find(|snapshot| snapshot.id == card)
            .unwrap()
            .prior_declines
    }

    fn result_fields(snapshot: &CardSnapshot) -> Option<(&str, usize, &BTreeSet<String>)> {
        match &snapshot.content {
            CardContent::Result {
                preview_text,
                row_count,
                hidden_column_names,
                ..
            } => Some((preview_text.as_str(), *row_count, hidden_column_names)),
            CardContent::Cache { .. } | CardContent::Statement { .. } => None,
        }
    }

    #[test]
    fn every_call_holds_then_send_and_dont_send() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let token = token_id("abcdefghij");
        let changes = broker.changes();
        assert_eq!(*changes.borrow(), 0);

        let first = held(broker.submit_cache(
            token,
            ToolName::SearchObjects,
            ReviewPolicy::EveryCall,
            "pattern *order*",
            "sales.orders (table)",
            now,
        ));
        assert_eq!(*changes.borrow(), 1);
        let second = held(broker.submit_cache(
            token,
            ToolName::DescribeRelation,
            ReviewPolicy::EveryCall,
            "sales.orders",
            "id integer",
            now,
        ));
        assert!(broker.send(second, now).is_none());
        assert!(broker.approve_statement(first).is_none());

        let cards = broker.cards();
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].id, first);
        assert_eq!(front_deadline(cards[0].place), now + DECISION_TIMEOUT);
        assert_eq!(cards[1].id, second);
        assert_eq!(cards[1].place, CardPlace::WaitingInLine);
        let (arguments, answer) = match &cards[0].content {
            CardContent::Cache { arguments, answer } => (arguments.as_str(), answer.as_str()),
            CardContent::Statement { .. } | CardContent::Result { .. } => ("", ""),
        };
        assert_eq!(arguments, "pattern *order*");
        assert_eq!(answer, "sales.orders (table)");

        let sent = broker.send(first, now).unwrap();
        assert_eq!(sent.end, CardEnd::Sent);
        assert_eq!(sent.result.outcome, Outcome::Sent);
        let body = response(&sent.result);
        assert!(!body.is_error);
        assert_eq!(body.text, "sales.orders (table)");
        let cards = broker.cards();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id, second);
        assert_eq!(
            cards[0].place,
            CardPlace::Front {
                deadline: now + DECISION_TIMEOUT
            }
        );

        let kept = broker.dont_send(second, now).unwrap();
        assert_eq!(kept.end, CardEnd::NotSent);
        assert_eq!(kept.result.outcome, Outcome::NotSent);
        let body = response(&kept.result);
        assert!(!body.is_error);
        assert_eq!(body.text, ANSWER_NOT_SHARED);
        assert!(broker.cards().is_empty());

        assert_eq!(broker.activity().len(), 2);
        assert_eq!(broker.activity()[0].outcome, Outcome::Sent);
        assert_eq!(
            broker.activity()[0].byte_length,
            "sales.orders (table)".len()
        );
        assert_eq!(broker.activity()[0].answer, "sales.orders (table)");
        assert_eq!(broker.activity()[1].outcome, Outcome::NotSent);
        assert_eq!(broker.activity()[1].byte_length, ANSWER_NOT_SHARED.len());
        assert_eq!(broker.activity()[1].answer, "id integer");
        assert_eq!(*changes.borrow(), 4);
    }

    #[test]
    fn never_returns_immediately() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let answer = "sales.orders (table)";
        let result = ready(broker.submit_cache(
            token_id("abcdefghij"),
            ToolName::DescribeRelation,
            ReviewPolicy::Never,
            "pattern *order*",
            answer,
            now,
        ));
        assert!(broker.cards().is_empty());
        assert_eq!(result.outcome, Outcome::Sent);
        let body = response(&result);
        assert!(!body.is_error);
        assert_eq!(body.text, answer);
        assert_eq!(broker.activity().len(), 1);
        assert_eq!(broker.activity()[0].tool, ToolName::DescribeRelation);
        assert_eq!(broker.activity()[0].tool.as_str(), "describe_relation");
        assert_eq!(broker.activity()[0].outcome, Outcome::Sent);
        assert_eq!(broker.activity()[0].byte_length, answer.len());
        assert_eq!(broker.activity()[0].arguments, "pattern *order*");
        assert_eq!(broker.activity()[0].answer, answer);
        assert_eq!(broker.activity()[0].at, now);
    }

    #[test]
    fn first_session_holds_once() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let first_token = token_id("abcdefghij");
        let other_token = token_id("klmnopqrst");
        let held_id = held(broker.submit_cache(
            first_token,
            ToolName::SearchObjects,
            ReviewPolicy::FirstSession,
            "args-1",
            "first-answer",
            now,
        ));
        let second = ready(broker.submit_cache(
            first_token,
            ToolName::DescribeRelation,
            ReviewPolicy::FirstSession,
            "args-2",
            "second-answer",
            now,
        ));
        assert_eq!(second.outcome, Outcome::Sent);
        assert!(!response(&second).is_error);
        assert_eq!(response(&second).text, "second-answer");
        assert_eq!(broker.cards().len(), 1);
        assert_eq!(broker.cards()[0].id, held_id);

        let third = ready(broker.submit_cache(
            first_token,
            ToolName::SearchObjects,
            ReviewPolicy::FirstSession,
            "args-3",
            "third-answer",
            now,
        ));
        assert_eq!(third.outcome, Outcome::Sent);
        assert_eq!(response(&third).text, "third-answer");

        let other = held(broker.submit_cache(
            other_token,
            ToolName::SearchObjects,
            ReviewPolicy::FirstSession,
            "args",
            "other-answer",
            now,
        ));
        assert_eq!(broker.cards().len(), 2);
        assert!(broker.cards().iter().any(|card| card.id == other));
        assert_eq!(broker.activity().len(), 2);
        assert_eq!(broker.activity()[0].answer, "second-answer");
        assert_eq!(broker.activity()[1].answer, "third-answer");
    }

    #[test]
    fn second_run_query_is_busy() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let token = token_id("abcdefghij");
        let other = token_id("klmnopqrst");
        let first = held(broker.submit_query(token, "select 1", "count rows", now));
        let _other = held(broker.submit_query(other, "select 1", "", now));
        let busy = ready(broker.submit_query(token, "select 2", "again", now));
        assert_eq!(busy.outcome, Outcome::Busy);
        assert_eq!(busy.outcome.as_str(), "busy");
        let body = response(&busy);
        assert!(body.is_error);
        assert_eq!(body.text, QUERY_BUSY);
        assert_eq!(broker.cards().len(), 2);

        let permit = broker.approve_statement(first).unwrap();
        let busy = ready(broker.submit_query(token, "select 2", "again", now));
        assert_eq!(busy.outcome, Outcome::Busy);
        assert!(broker.finish_running(permit, "preview", 1, Vec::<String>::new(), now,));
        let busy = ready(broker.submit_query(token, "select 2", "again", now));
        assert_eq!(busy.outcome, Outcome::Busy);

        let sent = broker.send_result(first, "released-text", now).unwrap();
        assert_eq!(sent.result.outcome, Outcome::Sent);
        assert!(!response(&sent.result).is_error);
        assert_eq!(response(&sent.result).text, "released-text");
        let next = held(broker.submit_query(token, "select 3", "later", now));
        assert!(broker.cards().iter().any(|card| card.id == next));
    }

    #[test]
    fn deadline_does_not_start_while_waiting_in_line() {
        let mut broker = Broker::new();
        let start = Instant::now();
        let token = token_id("abcdefghij");
        let front = held(broker.submit_cache(
            token,
            ToolName::SearchObjects,
            ReviewPolicy::EveryCall,
            "a",
            "A",
            start,
        ));
        let behind = held(broker.submit_cache(
            token,
            ToolName::DescribeRelation,
            ReviewPolicy::EveryCall,
            "b",
            "B",
            start,
        ));
        let cards = broker.cards();
        assert_eq!(front_deadline(cards[0].place), start + DECISION_TIMEOUT);
        assert_eq!(cards[1].id, behind);
        assert_eq!(cards[1].place, CardPlace::WaitingInLine);

        assert!(
            broker
                .expire(start + DECISION_TIMEOUT - Duration::from_secs(1))
                .is_empty()
        );
        let closed = broker.expire(start + DECISION_TIMEOUT);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].card, front);
        assert_eq!(closed[0].end, CardEnd::NotAnswered);
        assert!(broker.expire(start + DECISION_TIMEOUT).is_empty());

        let cards = broker.cards();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id, behind);
        let opened = start + DECISION_TIMEOUT;
        assert_eq!(front_deadline(cards[0].place), opened + DECISION_TIMEOUT);
        assert!(
            broker
                .expire(opened + DECISION_TIMEOUT - Duration::from_secs(1))
                .is_empty()
        );
        let closed = broker.expire(opened + DECISION_TIMEOUT);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].card, behind);
        assert_eq!(response(&closed[0].result).text, DECISION_TIMED_OUT);
    }

    #[test]
    fn deadline_on_the_front_card() {
        let start = Instant::now();
        let token = token_id("abcdefghij");

        let mut cache = Broker::new();
        let card = held(cache.submit_cache(
            token,
            ToolName::SearchObjects,
            ReviewPolicy::EveryCall,
            "pattern",
            "answer",
            start,
        ));
        assert!(
            cache
                .expire(start + DECISION_TIMEOUT - Duration::from_secs(1))
                .is_empty()
        );
        let closed = cache.expire(start + DECISION_TIMEOUT);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].card, card);
        assert_eq!(closed[0].result.outcome, Outcome::TimedOut);
        assert_eq!(closed[0].end, CardEnd::NotAnswered);
        let body = response(&closed[0].result);
        assert!(body.is_error);
        assert_eq!(body.text, DECISION_TIMED_OUT);
        assert!(cache.cards().is_empty());

        let mut statement = Broker::new();
        let _card = held(statement.submit_query(token, "select 1", "count", start));
        assert!(
            statement
                .expire(start + Duration::from_secs(119))
                .is_empty()
        );
        let closed = statement.expire(start + DECISION_TIMEOUT);
        assert_eq!(response(&closed[0].result).text, STATEMENT_TIMED_OUT);
        assert!(response(&closed[0].result).is_error);

        let mut running = Broker::new();
        let card = held(running.submit_query(token, "select 1", "count", start));
        let permit = running.approve_statement(card).unwrap();
        assert!(running.expire(start + DECISION_TIMEOUT).is_empty());
        assert_eq!(running.cards()[0].place, CardPlace::Running);
        let finished = start + Duration::from_secs(10);
        assert!(running.finish_running(
            permit,
            "preview-not-in-the-error",
            42,
            Vec::<String>::new(),
            finished,
        ));
        assert!(
            running
                .expire(finished + DECISION_TIMEOUT - Duration::from_secs(1))
                .is_empty()
        );
        let closed = running.expire(finished + DECISION_TIMEOUT);
        let body = response(&closed[0].result);
        assert!(body.is_error);
        assert_eq!(body.text, RESULT_TIMED_OUT);
        assert!(!body.text.contains("42"));
        assert!(!body.text.contains("preview"));
        assert_eq!(
            running.activity().last().unwrap().answer,
            "preview-not-in-the-error"
        );
    }

    #[test]
    fn decline_count_increments() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let token = token_id("abcdefghij");
        let other = token_id("klmnopqrst");
        let sql = "select 1";

        let first = held(broker.submit_query(token, sql, "why", now));
        assert_eq!(prior(&broker, first), 0);
        assert!(broker.send(first, now).is_none());
        assert!(broker.withhold(first, "no", now).is_none());
        let declined = broker.decline(first, now).unwrap();
        assert_eq!(declined.result.outcome, Outcome::Declined);
        assert!(!response(&declined.result).is_error);
        assert_eq!(response(&declined.result).text, QUERY_DECLINED);

        let second = held(broker.submit_query(token, sql, "why", now));
        assert_eq!(prior(&broker, second), 1);
        let _ = broker.decline(second, now).unwrap();

        let third = held(broker.submit_query(token, sql, "why", now));
        assert_eq!(prior(&broker, third), 2);
        let _ = broker.decline(third, now).unwrap();

        let different = held(broker.submit_query(token, "select 2", "", now));
        assert_eq!(prior(&broker, different), 0);
        let other_card = held(broker.submit_query(other, sql, "", now));
        assert_eq!(prior(&broker, other_card), 0);
    }

    #[test]
    fn stop_and_cancelled_password_use_the_decline_text() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let token = token_id("abcdefghij");
        let sql = "select 9";

        let card = held(broker.submit_query(token, sql, "why", now));
        let settled = broker.password_cancelled(card, now).unwrap();
        assert_eq!(settled.result.outcome, Outcome::Declined);
        assert!(!response(&settled.result).is_error);
        assert_eq!(response(&settled.result).text, QUERY_DECLINED);

        let card = held(broker.submit_query(token, sql, "why", now));
        assert_eq!(prior(&broker, card), 1);
        let permit = broker.approve_statement(card).unwrap();
        let settled = broker.stop(card, now).unwrap();
        assert_eq!(settled.result.outcome, Outcome::Declined);
        assert_eq!(response(&settled.result).text, QUERY_DECLINED);
        assert!(!broker.finish_running(permit, "x", 0, Vec::<String>::new(), now));

        let card = held(broker.submit_query(token, sql, "why", now));
        assert_eq!(prior(&broker, card), 1);
    }

    #[test]
    fn pause_rejects_a_new_call_and_keeps_the_waiting_card() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let token = token_id("abcdefghij");
        let waiting = held(broker.submit_cache(
            token,
            ToolName::SearchObjects,
            ReviewPolicy::EveryCall,
            "pattern",
            "answer",
            now,
        ));
        broker.set_paused(true);
        assert!(broker.is_paused());
        assert_eq!(broker.cards().len(), 1);

        let rejected = ready(broker.submit_cache(
            token,
            ToolName::DescribeRelation,
            ReviewPolicy::EveryCall,
            "sales.orders",
            "columns",
            now,
        ));
        assert_eq!(rejected.outcome, Outcome::Paused);
        let body = response(&rejected);
        assert!(body.is_error);
        assert_eq!(body.text, NOT_ACCEPTING);
        assert_eq!(broker.cards().len(), 1);
        assert_eq!(broker.cards()[0].id, waiting);

        let rejected = ready(broker.submit_query(token, "select 1", "why", now));
        assert_eq!(rejected.outcome, Outcome::Paused);
        assert_eq!(response(&rejected).text, NOT_ACCEPTING);
        assert_eq!(broker.cards().len(), 1);

        let sent = broker.send(waiting, now).unwrap();
        assert_eq!(sent.result.outcome, Outcome::Sent);

        let fresh = token_id("klmnopqrst");
        broker.set_paused(true);
        let paused = ready(broker.submit_cache(
            fresh,
            ToolName::SearchObjects,
            ReviewPolicy::FirstSession,
            "a",
            "paused-answer",
            now,
        ));
        assert_eq!(paused.outcome, Outcome::Paused);
        assert_eq!(broker.activity().last().unwrap().answer, "paused-answer");
        assert_eq!(
            broker.activity().last().unwrap().byte_length,
            NOT_ACCEPTING.len()
        );
        broker.set_paused(false);
        let card = held(broker.submit_cache(
            fresh,
            ToolName::SearchObjects,
            ReviewPolicy::FirstSession,
            "a",
            "first-real",
            now,
        ));
        assert_eq!(broker.cards()[0].id, card);
    }

    #[test]
    fn client_cancel_has_no_response() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let token = token_id("abcdefghij");
        let front = held(broker.submit_cache(
            token,
            ToolName::SearchObjects,
            ReviewPolicy::EveryCall,
            "a",
            "A",
            now,
        ));
        let behind = held(broker.submit_cache(
            token,
            ToolName::DescribeRelation,
            ReviewPolicy::EveryCall,
            "b",
            "B",
            now,
        ));
        let settled = broker.cancel(behind, now).unwrap();
        assert_eq!(settled.end, CardEnd::CancelledByClient);
        assert_eq!(settled.result.outcome, Outcome::Cancelled);
        assert!(settled.result.response.is_none());
        assert_eq!(broker.cards().len(), 1);
        assert_eq!(broker.cards()[0].id, front);
        assert_eq!(broker.activity().last().unwrap().byte_length, 0);
        assert_eq!(broker.activity().last().unwrap().answer, "B");
        let settled = broker.cancel(front, now).unwrap();
        assert!(settled.result.response.is_none());
        assert!(broker.cards().is_empty());

        let card = held(broker.submit_query(token, "select 1", "why", now));
        let permit = broker.approve_statement(card).unwrap();
        let settled = broker.cancel(card, now).unwrap();
        assert!(settled.result.response.is_none());
        assert!(!broker.finish_running(permit, "x", 1, Vec::<String>::new(), now));
    }

    #[test]
    fn quit_closes_cards_with_the_quitting_error() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let token = token_id("abcdefghij");
        let _cache = held(broker.submit_cache(
            token,
            ToolName::SearchObjects,
            ReviewPolicy::EveryCall,
            "a",
            "A",
            now,
        ));
        let _query = held(broker.submit_query(token, "select 1", "why", now));
        let closed = broker.quit(now);
        assert_eq!(closed.len(), 2);
        assert!(broker.is_quitting());
        assert!(broker.cards().is_empty());
        for item in &closed {
            assert_eq!(item.end, CardEnd::CancelledQuitting);
            assert_eq!(item.result.outcome, Outcome::Cancelled);
            let body = response(&item.result);
            assert!(body.is_error);
            assert_eq!(body.text, QUITTING);
        }
        let later = ready(broker.submit_query(token, "select 2", "later", now));
        assert_eq!(later.outcome, Outcome::Cancelled);
        assert_eq!(response(&later).text, QUITTING);
        assert!(response(&later).is_error);
        assert!(broker.cards().is_empty());
    }

    #[test]
    fn hidden_names_are_remembered_per_token_id() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let token = token_id("abcdefghij");
        let other = token_id("klmnopqrst");
        assert!(broker.hidden_names(token).is_empty());

        let card = held(broker.submit_query(token, "select 1", "why", now));
        assert!(!broker.set_hidden(card, ["secret".to_owned()]));
        let permit = broker.approve_statement(card).unwrap();
        assert!(broker.finish_running(
            permit,
            "preview",
            3,
            ["secret".to_owned(), "note".to_owned()],
            now,
        ));
        let cards = broker.cards();
        let (preview_text, row_count, hidden_column_names) = result_fields(&cards[0]).unwrap();
        assert_eq!(
            hidden_column_names,
            &BTreeSet::from(["note".to_owned(), "secret".to_owned()])
        );
        assert_eq!(preview_text, "preview");
        assert_eq!(row_count, 3);
        assert_eq!(
            broker.hidden_names(token),
            BTreeSet::from(["note".to_owned(), "secret".to_owned()])
        );
        assert!(broker.hidden_names(other).is_empty());

        assert!(broker.set_hidden(card, ["note".to_owned()]));
        assert_eq!(
            broker.hidden_names(token),
            BTreeSet::from(["note".to_owned()])
        );
        let withheld_text =
            "The user ran the query but chose not to share its result. It returned 3 rows.";
        let settled = broker.withhold(card, withheld_text, now).unwrap();
        assert_eq!(settled.result.outcome, Outcome::Withheld);
        assert_eq!(settled.end, CardEnd::Withheld);
        assert!(!response(&settled.result).is_error);
        assert_eq!(response(&settled.result).text, withheld_text);

        let card = held(broker.submit_query(token, "select 2", "next", now));
        let permit = broker.approve_statement(card).unwrap();
        let remembered = broker.hidden_names(token);
        assert!(broker.finish_running(permit, "next", 1, remembered, now));
        let cards = broker.cards();
        let (_, _, hidden_column_names) = result_fields(&cards[0]).unwrap();
        assert_eq!(hidden_column_names, &BTreeSet::from(["note".to_owned()]));
        assert_eq!(cards[0].id, card);
    }

    #[test]
    fn debug_redacts_sql() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let mark = "synthetic_secret_9f3a2c";
        let token = token_id("zzzzzzzzzz");
        let arguments = format!("pattern {mark}");
        let answer = format!("answer {mark}");
        let sql = format!("SELECT {mark} FROM accounts");
        let purpose = format!("purpose {mark}");
        let _sent = ready(broker.submit_cache(
            token,
            ToolName::SearchObjects,
            ReviewPolicy::Never,
            &arguments,
            &answer,
            now,
        ));
        let _card = held(broker.submit_query(token, &sql, &purpose, now));

        let rendered = format!("{broker:?}");
        assert!(!rendered.contains(mark), "{rendered}");
        assert!(!rendered.contains("SELECT"), "{rendered}");
        assert!(!rendered.contains("zzzzzzzzzz"), "{rendered}");
        let activity_debug = format!("{:?}", broker.activity());
        assert!(!activity_debug.contains(mark), "{activity_debug}");
        assert!(!activity_debug.contains("zzzzzzzzzz"), "{activity_debug}");

        assert!(
            broker
                .activity()
                .iter()
                .any(|entry| { entry.answer.contains(mark) && entry.arguments.contains(mark) })
        );
        assert!(broker.cards().iter().any(|card| match &card.content {
            CardContent::Statement { sql, purpose } => {
                sql.contains(mark) && purpose.contains(mark)
            }
            CardContent::Cache { .. } | CardContent::Result { .. } => false,
        }));
    }

    #[test]
    fn activity_caps_at_200() {
        let mut broker = Broker::new();
        let now = Instant::now();
        let token = token_id("abcdefghij");
        for index in 0..205 {
            let arguments = format!("arg-{index}");
            let answer = format!("answer-{index}");
            let result = ready(broker.submit_cache(
                token,
                ToolName::SearchObjects,
                ReviewPolicy::Never,
                &arguments,
                &answer,
                now,
            ));
            assert_eq!(result.outcome, Outcome::Sent);
        }
        assert_eq!(broker.activity().len(), 200);
        assert_eq!(ACTIVITY_LIMIT, 200);
        assert_eq!(broker.activity()[0].arguments, "arg-5");
        assert_eq!(broker.activity()[0].answer, "answer-5");
        assert_eq!(broker.activity().last().unwrap().arguments, "arg-204");
        assert_eq!(broker.activity().last().unwrap().answer, "answer-204");
        assert!(
            broker
                .activity()
                .iter()
                .all(|entry| entry.arguments != "arg-0" && entry.arguments != "arg-4")
        );
    }
}
