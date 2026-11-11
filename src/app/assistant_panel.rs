//! The assistant panel (ADR-0028): a conversation with the configured model
//! about the tab's connection. It is its own cached entity, so a streamed
//! answer redraws the panel only (ADR-0012), and it shows everything that
//! leaves the computer: each request before it is sent (when the review
//! mode asks), each tool the model called with what it was answered, and
//! the tokens and cost of every round.
//!
//! The panel decides nothing about privacy: the workspace builds each
//! request's first message from the facts allow-list, the tools read the
//! metadata cache only (`jykre::assistant`), and a query the model asks for
//! runs only once the user approves it, its result leaving only once the
//! user sends it, through `jykre::assistant::released`.
use super::assistant_settings::{self, AssistantSettings};
use super::demo;
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::assistant::facts::SchemaScope;
use jykre::assistant::released::{self, QueryPreview};
use jykre::assistant::run::{
    self, CompactionKind, Conversation, Next, QueryDecision, Round, RoundState, ToolStep,
};
use jykre::assistant::tools::CacheTools;
use jykre::assistant::{self as library, AssistantPreferences, prompts};
use jykre::drivers::{CancellationToken, DriverError, DriverResult, ErrorCode};
use jykre::{backend, tr};
use jykre_assistant::{
    Assistant, KeyUse, Message, RoundEvent, RoundResult, Timeouts, ToolCall, sql_blocks,
};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What the panel asks of the workspace.
pub(super) enum AssistantPanelEvent {
    /// The user sent a message; the workspace builds the request's text
    /// from what it may tell and calls [`AssistantPanel::start`].
    Submit {
        text: String,
        mode: Mode,
    },
    Insert(String),
    /// Replace the failed statement with `sql`.
    Replace {
        statement: String,
        origin: Option<usize>,
        sql: String,
    },
    Notice(Feedback),
    OpenPreferences,
    Close,
    /// The user approved running `sql` for the model: run it in the
    /// assistant's own session, stop when `cancel` fires, and answer with
    /// [`AssistantPanel::query_done`] and `ticket`.
    RunQuery {
        sql: String,
        ticket: QueryTicket,
        cancel: CancellationToken,
    },
}

impl EventEmitter<AssistantPanelEvent> for AssistantPanel {}

/// Which query a result answers; a result for an older conversation or
/// card is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct QueryTicket {
    epoch: u64,
    card: usize,
}

/// A `run_query` call put to the user.
struct QueryCard {
    call: ToolCall,
    sql: String,
    purpose: Option<String>,
    risk: Option<jykre::guard::Risk>,
    state: CardState,
}

enum CardState {
    /// Waits for **Run** or **Decline**.
    Asked,
    Running {
        started: Instant,
        cancel: CancellationToken,
    },
    /// Ran; waits for **Send result** or **Don't send**. `text` is exactly
    /// what sending would put in the next request.
    Review {
        preview: Arc<QueryPreview>,
        hidden: BTreeSet<usize>,
        text: String,
    },
    Done(ToolStep),
}

/// The tool calls of a round being answered.
struct InFlight {
    calls: Vec<ToolCall>,
    steps: Vec<ToolStep>,
    /// Cards still to put to the user, in order.
    queue: VecDeque<usize>,
    continued: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Ask,
    WriteSql,
}

/// The failed statement a **Fix with AI** turn may replace.
#[derive(Clone, Debug)]
pub(super) struct FixTarget {
    pub statement: String,
    pub origin: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Idle,
    /// The pending round waits for **Send**.
    Review,
    Sending {
        started: Instant,
    },
    Tools,
    /// A query card waits for the user, or its query runs.
    Query {
        card: usize,
    },
    /// The turn used its rounds; these calls wait for **Continue**.
    Limit(Vec<ToolCall>),
}

/// One row of the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    User {
        turn: usize,
    },
    Request {
        round: usize,
    },
    Answer {
        round: usize,
        turn: usize,
    },
    Tool {
        round: usize,
        step: usize,
    },
    Query {
        card: usize,
    },
    /// The conversation's compaction `index`.
    Compaction {
        index: usize,
    },
    Demo {
        index: usize,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Section {
    #[default]
    Messages,
    Tools,
    Json,
}

struct Turn {
    text: String,
    fix: Option<FixTarget>,
    /// The turn asks the model to summarize the conversation.
    summary: bool,
}

pub(super) struct AssistantPanel {
    prompt: Entity<InputState>,
    mode: Mode,
    demo: bool,
    /// The tab's connection, as its cache: `None` without a connection.
    target: Option<SchemaScope>,
    label: SharedString,
    conversation: Option<Conversation>,
    tools: Option<Arc<CacheTools>>,
    assistant: Option<Arc<Assistant>>,
    turns: Vec<Turn>,
    items: Vec<Item>,
    list: ListState,
    expanded: HashSet<usize>,
    sections: HashMap<usize, Section>,
    phase: Phase,
    /// The text of the round being received.
    streaming: String,
    demo_answers: Vec<(String, Option<&'static str>)>,
    /// Bumped whenever the conversation is reset; late results of an older
    /// one are dropped.
    epoch: u64,
    cancel: CancellationToken,
    clock: bool,
    /// The conversation offers `run_query`.
    queries: bool,
    cards: Vec<QueryCard>,
    in_flight: Option<InFlight>,
    /// Columns the user hid in this conversation, lowercase: they start
    /// hidden in the next result.
    hidden_names: BTreeSet<String>,
    /// The tab's connection is marked production.
    production: bool,
    /// Request cards (by item) showing every message, not only what is new
    /// since the last request sent.
    whole_requests: HashSet<usize>,
    /// The session summary shows its tiles; folded, it is one line. It
    /// stays as the user left it across conversations.
    summary_open: bool,
    _subscriptions: Vec<Subscription>,
}

impl AssistantPanel {
    pub(super) fn new(demo: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let prompt =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr!("Ask about your database…")));
        let submit = cx.subscribe_in(
            &prompt,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit(window, cx);
                }
            },
        );
        let settings = cx.observe_global::<AssistantSettings>(|this, cx| {
            // Another provider, model or key: the next round builds its
            // client again.
            this.assistant = None;
            cx.notify();
        });
        let list = ListState::new(0, ListAlignment::Top, px(400.));
        list.set_follow_mode(FollowMode::Tail);
        Self {
            prompt,
            mode: Mode::Ask,
            demo,
            target: None,
            label: SharedString::default(),
            conversation: None,
            tools: None,
            assistant: None,
            turns: Vec::new(),
            items: Vec::new(),
            list,
            expanded: HashSet::new(),
            sections: HashMap::new(),
            phase: Phase::Idle,
            streaming: String::new(),
            demo_answers: Vec::new(),
            epoch: 0,
            cancel: CancellationToken::new(),
            clock: false,
            queries: false,
            cards: Vec::new(),
            in_flight: None,
            hidden_names: BTreeSet::new(),
            production: false,
            whole_requests: HashSet::new(),
            summary_open: true,
            _subscriptions: vec![submit, settings],
        }
    }

    /// The tab's connection changed (or went away): another server, so the
    /// conversation starts over.
    pub(super) fn set_target(
        &mut self,
        target: Option<SchemaScope>,
        label: SharedString,
        cx: &mut Context<Self>,
    ) {
        let same = match (&self.target, &target) {
            (Some(old), Some(new)) => old.connection == new.connection && old.config == new.config,
            (None, None) => true,
            _ => false,
        };
        self.label = label;
        if same {
            if let (Some(old), Some(new)) = (&mut self.target, target) {
                old.context = new.context;
            }
        } else {
            self.target = target;
            self.clear(cx);
        }
        cx.notify();
    }

    /// Where unqualified names resolve now; the conversation goes on.
    pub(super) fn set_context(&mut self, context: jykre::drivers::SessionContext) {
        if let Some(target) = &mut self.target {
            target.context = context;
        }
    }

    /// Whether the tab's connection is marked production, which a query
    /// card says before **Run**.
    pub(super) fn set_production(&mut self, production: bool, cx: &mut Context<Self>) {
        if self.production != production {
            self.production = production;
            cx.notify();
        }
    }

    /// Stops what runs and forgets the conversation. A query on its way is
    /// cancelled; its result, if any, is dropped.
    pub(super) fn clear(&mut self, cx: &mut Context<Self>) {
        self.cancel.cancel();
        self.cancel_queries();
        self.epoch += 1;
        self.conversation = None;
        self.tools = None;
        self.cards.clear();
        self.in_flight = None;
        self.hidden_names.clear();
        self.whole_requests.clear();
        self.turns.clear();
        self.items.clear();
        self.expanded.clear();
        self.sections.clear();
        self.demo_answers.clear();
        self.streaming.clear();
        self.phase = Phase::Idle;
        self.list.reset(0);
        cx.notify();
    }

    pub(super) fn stop(&mut self, cx: &mut Context<Self>) {
        match self.phase {
            Phase::Review => self.cancel_review(cx),
            Phase::Sending { .. } | Phase::Tools => {
                self.cancel.cancel();
                cx.notify();
            }
            // The turn ends: the query on its way is cancelled and nothing
            // of it, or of the queries after it, is sent.
            Phase::Query { .. } => self.abandon_tools(&DriverError::cancelled(), cx),
            Phase::Limit(_) => {
                self.phase = Phase::Idle;
                cx.notify();
            }
            Phase::Idle => {}
        }
    }

    pub(super) fn busy(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// The conversation can be compacted now.
    fn can_compact(&self) -> bool {
        !self.busy()
            && self
                .conversation
                .as_ref()
                .is_some_and(Conversation::can_compact)
    }

    /// **Prune tool results**: long tool answers become one line each.
    fn compact_prune(&mut self, cx: &mut Context<Self>) {
        if !self.can_compact() {
            return;
        }
        let Some(conversation) = &mut self.conversation else {
            return;
        };
        match conversation.prune_tool_results() {
            Some(_) => {
                let index = conversation.compactions().len() - 1;
                self.push(Item::Compaction { index });
            }
            None => cx.emit(AssistantPanelEvent::Notice(Feedback::new(
                Tone::Info,
                tr!("There are no long tool results to prune."),
            ))),
        }
        cx.notify();
    }

    /// **Summarize with the model**: a turn that asks for a summary, which
    /// then replaces the conversation. It is reviewed like any request.
    fn compact_summary(&mut self, cx: &mut Context<Self>) {
        if !self.can_compact() {
            return;
        }
        let Some(conversation) = &mut self.conversation else {
            return;
        };
        let Some(round) = conversation.start_summary().map(|round| round.number - 1) else {
            return;
        };
        self.turns.push(Turn {
            text: tr!("Summarize the conversation to compact the context").to_owned(),
            fix: None,
            summary: true,
        });
        self.push(Item::User {
            turn: self.turns.len() - 1,
        });
        self.push(Item::Request { round });
        self.expanded.insert(self.items.len() - 1);
        self.advance(cx);
    }

    /// The demo's opening exchange.
    pub(super) fn seed_demo(
        &mut self,
        question: String,
        answer: String,
        sql: Option<&'static str>,
        cx: &mut Context<Self>,
    ) {
        self.turns.push(Turn {
            text: question,
            fix: None,
            summary: false,
        });
        self.push(Item::User {
            turn: self.turns.len() - 1,
        });
        self.demo_answers.push((answer, sql));
        self.push(Item::Demo {
            index: self.demo_answers.len() - 1,
        });
        cx.notify();
    }

    fn preferences(cx: &App) -> Arc<AssistantPreferences> {
        assistant_settings::preferences(cx)
    }

    fn configured(&self, cx: &App) -> bool {
        self.demo || Self::preferences(cx).config().is_some()
    }

    fn push(&mut self, item: Item) {
        let at = self.items.len();
        self.items.push(item);
        self.list.splice(at..at, 1);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.prompt.read(cx).value().trim().to_owned();
        if text.is_empty() || self.busy() {
            return;
        }
        self.prompt
            .update(cx, |input, cx| input.set_value("", window, cx));
        if self.demo {
            let (answer, sql) = demo::assistant::answer(&text);
            self.seed_demo(text, answer, sql, cx);
            return;
        }
        if !self.configured(cx) {
            cx.emit(AssistantPanelEvent::OpenPreferences);
            return;
        }
        cx.emit(AssistantPanelEvent::Submit {
            text,
            mode: self.mode,
        });
    }

    /// Starts a turn: `display` is what the user sees as their message,
    /// `prompt` the full text the model receives (shown in the request).
    pub(super) fn start(
        &mut self,
        display: String,
        prompt: String,
        fix: Option<FixTarget>,
        cx: &mut Context<Self>,
    ) {
        if self.busy() {
            cx.emit(AssistantPanelEvent::Notice(Feedback::new(
                Tone::Warning,
                tr!("The assistant is still answering; stop it first."),
            )));
            return;
        }
        let Some(target) = self.target.clone() else {
            cx.emit(AssistantPanelEvent::Notice(Feedback::new(
                Tone::Warning,
                tr!("Choose a connection for the tab first."),
            )));
            return;
        };
        let preferences = Self::preferences(cx);
        let Some(config) = preferences.config() else {
            cx.emit(AssistantPanelEvent::OpenPreferences);
            return;
        };
        if self.conversation.is_none() {
            let language = if jykre::i18n::locale().starts_with("pt") {
                "Brazilian Portuguese"
            } else {
                "English"
            };
            // Whether the model may ask to run queries is fixed for the
            // conversation, so every request offers the same tools.
            self.queries = preferences.queries && !self.demo;
            self.conversation = Some(Conversation::new(
                config.kind.key(),
                &config.model,
                prompts::system_prompt(&target.engine, language, self.queries),
                CacheTools::specs(self.queries),
            ));
            self.tools = Some(Arc::new(CacheTools::new(target.clone())));
        }
        self.turns.push(Turn {
            text: display,
            fix,
            summary: false,
        });
        let turn = self.turns.len() - 1;
        self.push(Item::User { turn });
        let Some(conversation) = &mut self.conversation else {
            return;
        };
        let round = conversation.start_turn(prompt).number - 1;
        self.push(Item::Request { round });
        self.expanded.insert(self.items.len() - 1);
        self.advance(cx);
    }

    /// Sends the pending round, or waits for review.
    fn advance(&mut self, cx: &mut Context<Self>) {
        let Some(conversation) = &self.conversation else {
            return;
        };
        if conversation.pending().is_none() {
            self.phase = Phase::Idle;
            cx.notify();
            return;
        }
        let review = Self::preferences(cx).review;
        if review.pauses(conversation.pending_index()) {
            self.phase = Phase::Review;
            cx.notify();
        } else {
            self.send(cx);
        }
    }

    fn cancel_review(&mut self, cx: &mut Context<Self>) {
        if let Some(conversation) = &mut self.conversation {
            conversation.record_error(&DriverError::cancelled());
        }
        self.phase = Phase::Idle;
        cx.notify();
    }

    fn current_turn(&self) -> usize {
        self.turns.len().saturating_sub(1)
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let Some(round) = self.conversation.as_ref().and_then(Conversation::pending) else {
            return;
        };
        let request = round.request.clone();
        let index = round.number - 1;
        let turn = self.current_turn();
        // The request card closes once it is sent; its answer follows.
        if let Some(at) = self
            .items
            .iter()
            .position(|item| *item == Item::Request { round: index })
        {
            self.expanded.remove(&at);
        }
        self.push(Item::Answer { round: index, turn });
        self.streaming.clear();
        self.cancel = CancellationToken::new();
        self.phase = Phase::Sending {
            started: Instant::now(),
        };
        self.start_clock(cx);
        cx.notify();

        let epoch = self.epoch;
        let cancel = self.cancel.clone();
        let cached = self.assistant.clone();
        let preferences = Self::preferences(cx);
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel::<RoundEvent>();
        // Pieces of text arrive often; the panel redraws at most ~30 times
        // a second, with everything that came meanwhile.
        cx.spawn(async move |weak, cx| {
            while let Some(RoundEvent::Delta(first)) = received.recv().await {
                let mut text = first;
                while let Ok(RoundEvent::Delta(more)) = received.try_recv() {
                    text.push_str(&more);
                }
                let alive = weak.update(cx, |this, cx| {
                    if this.epoch == epoch && matches!(this.phase, Phase::Sending { .. }) {
                        this.streaming.push_str(&text);
                        let last = this.items.len().saturating_sub(1);
                        this.list.remeasure_items(last..last + 1);
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
            }
        })
        .detach();
        cx.spawn(async move |weak, cx| {
            let outcome = backend::run(async move {
                let assistant = match cached {
                    Some(assistant) => assistant,
                    None => {
                        let Some(config) = preferences.config() else {
                            return Err(DriverError::validation("The assistant is not set up"));
                        };
                        let key = if config.kind.key_use() == KeyUse::None {
                            None
                        } else {
                            library::read_key(config.kind).await?
                        };
                        Arc::new(Assistant::new(config, key, Timeouts::default())?)
                    }
                };
                let (result, elapsed) = run::send(&assistant, &request, &events, &cancel).await;
                Ok((assistant, result, elapsed))
            })
            .await;
            let _ = weak.update(cx, |this, cx| match outcome {
                Ok((assistant, result, elapsed)) => {
                    this.assistant = Some(assistant);
                    this.finish(epoch, result, elapsed, cx);
                }
                Err(error) => this.finish(epoch, Err(error), 0, cx),
            });
        })
        .detach();
    }

    fn finish(
        &mut self,
        epoch: u64,
        result: DriverResult<RoundResult>,
        elapsed: u64,
        cx: &mut Context<Self>,
    ) {
        if self.epoch != epoch {
            return;
        }
        let Some(conversation) = &mut self.conversation else {
            return;
        };
        self.streaming.clear();
        match result {
            Ok(result) => {
                let next = conversation.record(result, elapsed);
                // A summary turn ends with its first answer, which takes
                // the conversation's place.
                let summary = conversation.summarizing().then(|| {
                    conversation
                        .apply_summary()
                        .map(|_| conversation.compactions().len() - 1)
                });
                match (summary, next) {
                    (Some(Ok(index)), _) => {
                        self.phase = Phase::Idle;
                        self.push(Item::Compaction { index });
                    }
                    (Some(Err(_)), _) => {
                        self.phase = Phase::Idle;
                        cx.emit(AssistantPanelEvent::Notice(Feedback::new(
                            Tone::Warning,
                            tr!("The model did not answer with a summary; the context was not compacted."),
                        )));
                    }
                    (None, Next::Done) => self.phase = Phase::Idle,
                    (None, Next::LimitReached(calls)) => self.phase = Phase::Limit(calls),
                    (None, Next::Tools(calls)) => self.run_tools(calls, false, cx),
                }
            }
            Err(error) => {
                if error.code == ErrorCode::Authentication {
                    self.assistant = None;
                }
                conversation.record_error(&error);
                self.phase = Phase::Idle;
            }
        }
        self.list.remeasure();
        cx.notify();
    }

    /// Answers the tools of the last round: the cache's on the runtime,
    /// then each `run_query` put to the user in turn; then shows each step
    /// and prepares the next round.
    fn run_tools(&mut self, calls: Vec<ToolCall>, continued: bool, cx: &mut Context<Self>) {
        let Some(tools) = self.tools.clone() else {
            return;
        };
        let (cache, asked) = run::split_calls(calls.clone());
        let dialect = self.target.as_ref().and_then(|target| target.dialect);
        let mut steps = Vec::new();
        let mut queue = VecDeque::new();
        for call in asked {
            let raw = call
                .arguments
                .get("sql")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let checked = if self.queries {
                run::query_sql(&call, dialect)
            } else {
                Err(format!("Error: there is no tool named {}.", call.name))
            };
            match checked {
                Ok(sql) => {
                    self.cards.push(QueryCard {
                        purpose: run::query_purpose(&call).map(str::to_owned),
                        risk: jykre::guard::risk(&sql, dialect),
                        call,
                        sql,
                        state: CardState::Asked,
                    });
                    queue.push_back(self.cards.len() - 1);
                }
                Err(reason) => steps.push(released::rejected(call, raw, reason)),
            }
        }
        self.in_flight = Some(InFlight {
            calls,
            steps,
            queue,
            continued,
        });
        self.phase = Phase::Tools;
        let epoch = self.epoch;
        let cancel = self.cancel.clone();
        cx.spawn(async move |weak, cx| {
            let steps = backend::run(async move {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(DriverError::cancelled()),
                    steps = run::run_tools(&tools, &cache) => Ok(steps),
                }
            })
            .await;
            let _ = weak.update(cx, |this, cx| this.tools_done(epoch, steps, cx));
        })
        .detach();
        cx.notify();
    }

    fn tools_done(
        &mut self,
        epoch: u64,
        steps: DriverResult<Vec<ToolStep>>,
        cx: &mut Context<Self>,
    ) {
        if self.epoch != epoch {
            return;
        }
        match steps {
            Ok(steps) => {
                if let Some(flight) = &mut self.in_flight {
                    flight.steps.extend(steps);
                }
                self.next_query(cx);
            }
            Err(error) => self.abandon_tools(&error, cx),
        }
    }

    /// Puts the next query to the user, or ends the round's tools.
    fn next_query(&mut self, cx: &mut Context<Self>) {
        match self
            .in_flight
            .as_mut()
            .and_then(|flight| flight.queue.pop_front())
        {
            Some(card) => {
                self.phase = Phase::Query { card };
                self.push(Item::Query { card });
                cx.notify();
            }
            None => self.finish_tools(cx),
        }
    }

    /// Every tool of the round is answered: the steps go to the model in
    /// the order it asked for them, and the next round is prepared.
    fn finish_tools(&mut self, cx: &mut Context<Self>) {
        let Some(flight) = self.in_flight.take() else {
            return;
        };
        let Some(conversation) = &mut self.conversation else {
            return;
        };
        let steps = run::in_call_order(&flight.calls, flight.steps);
        // A query the user saw already has its card.
        let shown: Vec<usize> = steps
            .iter()
            .enumerate()
            .filter(|(_, step)| {
                step.query
                    .as_ref()
                    .is_none_or(|query| query.decision == QueryDecision::Rejected)
            })
            .map(|(at, _)| at)
            .collect();
        let answered = conversation.rounds().len() - 1;
        let next = if flight.continued {
            conversation.continue_turn(steps).number - 1
        } else {
            conversation.answer_tools(steps).number - 1
        };
        for step in shown {
            self.push(Item::Tool {
                round: answered,
                step,
            });
        }
        self.push(Item::Request { round: next });
        self.expanded.insert(self.items.len() - 1);
        self.advance(cx);
    }

    /// The round's tools stop: queries on their way are cancelled, those
    /// waiting are declined, and the turn ends with nothing more sent.
    fn abandon_tools(&mut self, error: &DriverError, cx: &mut Context<Self>) {
        self.cancel_queries();
        for card in &mut self.cards {
            if !matches!(card.state, CardState::Done(_)) {
                card.state =
                    CardState::Done(released::declined(card.call.clone(), card.sql.clone()));
            }
        }
        self.in_flight = None;
        if let Some(conversation) = &mut self.conversation {
            conversation.record_error(error);
        }
        self.phase = Phase::Idle;
        self.list.remeasure();
        cx.notify();
    }

    fn cancel_queries(&self) {
        for card in &self.cards {
            if let CardState::Running { cancel, .. } = &card.state {
                cancel.cancel();
            }
        }
    }

    fn remeasure_card(&mut self, card: usize) {
        if let Some(at) = self
            .items
            .iter()
            .position(|item| *item == Item::Query { card })
        {
            self.list.remeasure_items(at..at + 1);
        }
    }

    /// The user's answer to a card: it goes to the model with the round's
    /// other steps, and the next card comes up.
    fn decide(&mut self, card: usize, step: ToolStep, cx: &mut Context<Self>) {
        if let Some(data) = self.cards.get_mut(card) {
            data.state = CardState::Done(step.clone());
        }
        if let Some(flight) = &mut self.in_flight {
            flight.steps.push(step);
        }
        self.remeasure_card(card);
        self.next_query(cx);
    }

    /// **Run**: the workspace runs the statement in the assistant's session.
    fn run_card(&mut self, card: usize, cx: &mut Context<Self>) {
        let epoch = self.epoch;
        let cancel = self.cancel.child_token();
        let Some(data) = self.cards.get_mut(card) else {
            return;
        };
        if !matches!(data.state, CardState::Asked) {
            return;
        }
        data.state = CardState::Running {
            started: Instant::now(),
            cancel: cancel.clone(),
        };
        let sql = data.sql.clone();
        cx.emit(AssistantPanelEvent::RunQuery {
            sql,
            ticket: QueryTicket { epoch, card },
            cancel,
        });
        self.start_clock(cx);
        self.remeasure_card(card);
        cx.notify();
    }

    /// **Decline**: nothing runs.
    fn decline_card(&mut self, card: usize, cx: &mut Context<Self>) {
        let Some(data) = self.cards.get(card) else {
            return;
        };
        if !matches!(data.state, CardState::Asked) {
            return;
        }
        let step = released::declined(data.call.clone(), data.sql.clone());
        self.decide(card, step, cx);
    }

    /// The workspace ran the query of `ticket`: `None` when it did not run
    /// (no password was given) or was stopped.
    pub(super) fn query_done(
        &mut self,
        ticket: QueryTicket,
        preview: Option<QueryPreview>,
        cx: &mut Context<Self>,
    ) {
        if ticket.epoch != self.epoch {
            return;
        }
        let Some(data) = self.cards.get_mut(ticket.card) else {
            return;
        };
        if !matches!(data.state, CardState::Running { .. }) {
            return;
        }
        match preview {
            None => {
                let step = released::declined(data.call.clone(), data.sql.clone());
                self.decide(ticket.card, step, cx);
            }
            Some(preview) => {
                let hidden = preview.hidden_by_name(&self.hidden_names);
                let text = released::release(&preview, &hidden);
                data.state = CardState::Review {
                    preview: Arc::new(preview),
                    hidden,
                    text,
                };
                self.remeasure_card(ticket.card);
                cx.notify();
            }
        }
    }

    /// Hides or shows the values of `column` in what would be sent.
    fn toggle_column(&mut self, card: usize, column: usize, cx: &mut Context<Self>) {
        let Some(QueryCard {
            state:
                CardState::Review {
                    preview,
                    hidden,
                    text,
                },
            ..
        }) = self.cards.get_mut(card)
        else {
            return;
        };
        if !hidden.remove(&column) {
            hidden.insert(column);
        }
        *text = released::release(preview, hidden);
        self.remeasure_card(card);
        cx.notify();
    }

    /// **Send result** (`send`) or **Don't send**.
    fn settle_card(&mut self, card: usize, send: bool, cx: &mut Context<Self>) {
        let Some(QueryCard {
            call,
            sql,
            state: CardState::Review {
                preview, hidden, ..
            },
            ..
        }) = self.cards.get(card)
        else {
            return;
        };
        let step = if send {
            // What the user hid or showed again holds for the next results.
            for (at, column) in preview.columns.iter().enumerate() {
                let name = column.name.to_lowercase();
                if hidden.contains(&at) {
                    self.hidden_names.insert(name);
                } else {
                    self.hidden_names.remove(&name);
                }
            }
            released::sent(call.clone(), sql.clone(), preview, hidden)
        } else {
            released::withheld(call.clone(), sql.clone(), preview)
        };
        self.decide(card, step, cx);
    }

    fn start_clock(&mut self, cx: &mut Context<Self>) {
        if self.clock {
            return;
        }
        self.clock = true;
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let running = weak
                    .update(cx, |this, cx| {
                        let running = match this.phase {
                            Phase::Sending { .. } | Phase::Tools => true,
                            Phase::Query { card } => this.cards.get(card).is_some_and(|data| {
                                matches!(data.state, CardState::Running { .. })
                            }),
                            _ => false,
                        };
                        if let Phase::Query { card } = this.phase
                            && running
                        {
                            this.remeasure_card(card);
                        }
                        if running {
                            cx.notify();
                        } else {
                            this.clock = false;
                        }
                        running
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
    }

    /// The conversation at a glance: what it cost, how big the next request
    /// is, and how much of the input the provider served from its cache.
    fn summary_view(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let stats = self
            .conversation
            .as_ref()
            .map(Conversation::stats)
            .unwrap_or_default();
        let usage = stats.usage;
        let cost = self.conversation.as_ref().and_then(|conversation| {
            Self::preferences(cx)
                .price()
                .and_then(|price| conversation.cost(&price))
        });
        let tile = |value: String, caption: &'static str| {
            column()
                .flex_1()
                .min_w(px(84.))
                .gap(px(2.))
                .px_2()
                .py(px(6.))
                .rounded(px(6.))
                .bg(rgb(colors().surface))
                .child(
                    div()
                        .text_size(px(15.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(colors().text))
                        .child(value),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(colors().faint))
                        .child(caption),
                )
        };
        let spent = match cost {
            Some(cost) => format!("${cost:.4}"),
            None => "—".to_owned(),
        };
        let context = match stats.context_tokens {
            Some(tokens) if stats.context_estimated => format!("~{}", count_short(tokens)),
            Some(tokens) => count_short(tokens),
            None => "—".to_owned(),
        };
        // Hits are input tokens read from the provider's prompt cache; the
        // rest of the input was a miss.
        let input = usage.input.unwrap_or(0);
        let hits = usage.cache_read.unwrap_or(0).min(input);
        let known = usage.cache_read.is_some() && input > 0;
        let fraction = if known {
            hits as f32 / input as f32
        } else {
            0.0
        };
        let mut details = vec![tr!("{0} rounds", stats.rounds)];
        if let (Some(input), Some(output)) = (usage.input, usage.output) {
            details.push(tr!(
                "{0} in · {1} out",
                count_short(input),
                count_short(output)
            ));
        }
        if known {
            details.push(tr!(
                "cache: {0} hit · {1} missed",
                count_short(hits),
                count_short(input - hits)
            ));
        } else if input > 0 {
            details.push(tr!("the provider reported no cache").to_owned());
        }
        let open = self.summary_open;
        let cache = if known {
            format!("{:.0}%", fraction * 100.0)
        } else {
            "—".to_owned()
        };
        // Folded, the three numbers stay in one line.
        let folded = tr!(
            "{0} · {1} tokens · {2} cache",
            spent.clone(),
            context.clone(),
            cache.clone()
        );
        let toggle = row()
            .id("assistant-summary-toggle")
            .role(Role::Button)
            .aria_label(if open {
                tr!("Fold the session summary")
            } else {
                tr!("Unfold the session summary")
            })
            .aria_toggled(if open { Toggled::True } else { Toggled::False })
            .focusable()
            .tab_index(0)
            .gap_1()
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(colors().hover)))
            .focus_visible(|s| s.bg(rgb(colors().hover)))
            .child(icon(
                if open {
                    I::ChevronDown
                } else {
                    I::ChevronRight
                },
                colors().faint,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(rgb(colors().muted))
                    .child(if open {
                        tr!("Session").to_owned()
                    } else {
                        folded
                    }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.summary_open = !this.summary_open;
                cx.notify();
            }));
        column()
            .id("assistant-summary")
            .role(Role::Status)
            .flex_none()
            .px_4()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(rgb(colors().border))
            .child(toggle)
            .when(open, |s| {
                s.child(
                    row()
                        .flex_wrap()
                        .items_stretch()
                        .gap_2()
                        .child(tile(
                            spent,
                            if cost.is_some() || self.conversation.is_none() {
                                tr!("spent in the session")
                            } else {
                                tr!("no price for the model")
                            },
                        ))
                        .child(tile(context, tr!("context tokens")))
                        .child(
                            tile(cache, tr!("cache hits")).child(
                                div()
                                    .h(px(3.))
                                    .w_full()
                                    .rounded(px(2.))
                                    .bg(rgb(colors().border))
                                    .child(
                                        div()
                                            .h_full()
                                            .w(relative(fraction))
                                            .rounded(px(2.))
                                            .bg(rgb(colors().mint)),
                                    ),
                            ),
                        ),
                )
                .child(label(details.join(" · ")))
            })
    }

    /// The gear of the panel: what is sent, and compacting the context.
    /// More options will join it.
    fn options_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        let can_compact = self.can_compact();
        // What leaves the computer, in two short lines a menu can hold.
        let values = if Self::preferences(cx).queries {
            tr!("Values: only query results you send")
        } else {
            tr!("Values: never")
        };
        Button::new("assistant-options")
            .ghost()
            .small()
            .icon(I::Settings)
            .accessibility_label(tr!("Assistant options"))
            .tooltip(tr!("Assistant options"))
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                let prune = panel.clone();
                let summarize = panel.clone();
                menu.max_w(px(320.))
                    .label(tr!("Sends: SQL, errors, row counts, structure"))
                    .label(values)
                    .separator()
                    .label(tr!("Compact the context"))
                    .item(
                        PopupMenuItem::new(tr!("Prune long tool results"))
                            .icon(I::Scissors)
                            .disabled(!can_compact)
                            .on_click(move |_, _, cx| {
                                let _ = prune.update(cx, |this, cx| this.compact_prune(cx));
                            }),
                    )
                    .item(
                        PopupMenuItem::new(tr!("Summarize with the model"))
                            .icon(I::Sparkles)
                            .disabled(!can_compact)
                            .on_click(move |_, _, cx| {
                                let _ = summarize.update(cx, |this, cx| this.compact_summary(cx));
                            }),
                    )
            })
    }

    fn compaction_item(&self, index: usize) -> Div {
        let Some(compaction) = self
            .conversation
            .as_ref()
            .and_then(|conversation| conversation.compactions().get(index))
        else {
            return div();
        };
        let (glyph, what) = match compaction.kind {
            CompactionKind::Pruned => (
                I::Scissors,
                tr!(
                    "Context compacted: {0} tool results pruned",
                    compaction.count
                ),
            ),
            CompactionKind::Summarized => (
                I::Sparkles,
                tr!(
                    "Context compacted: a summary replaced {0} messages",
                    compaction.count
                ),
            ),
        };
        div().child(
            column()
                .id(("assistant-compaction", index))
                .role(Role::Status)
                .gap_1()
                .px_3()
                .py_2()
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(colors().border))
                .child(
                    row().items_start().child(icon(glyph, colors().teal)).child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(rgb(colors().text))
                            .child(what),
                    ),
                )
                .child(label(tr!(
                    "~{0} → ~{1} tokens. The next request starts the provider's cache again.",
                    count_short(compaction.before_tokens),
                    count_short(compaction.after_tokens)
                ))),
        )
    }

    fn render_item(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.items.get(index).copied() else {
            return div().into_any_element();
        };
        let body = match item {
            Item::User { turn } => self.user_item(turn),
            Item::Request { round } => self.request_item(index, round, cx),
            Item::Answer { round, turn } => self.answer_item(round, turn, cx),
            Item::Tool { round, step } => self.tool_item(index, round, step, cx),
            Item::Query { card } => self.query_item(index, card, cx),
            Item::Compaction { index: compaction } => self.compaction_item(compaction),
            Item::Demo { index: answer } => self.demo_item(answer, cx),
        };
        div().px_4().py_2().child(body).into_any_element()
    }

    fn user_item(&self, turn: usize) -> Div {
        let text = self
            .turns
            .get(turn)
            .map(|turn| turn.text.clone())
            .unwrap_or_default();
        column()
            .gap_1()
            .child(
                row()
                    .child(icon(I::UserRound, colors().muted))
                    .child(label(tr!("You"))),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(rgb(colors().text))
                    .child(text),
            )
    }

    fn round(&self, round: usize) -> Option<&Round> {
        self.conversation.as_ref()?.rounds().get(round)
    }

    fn request_item(&self, index: usize, round: usize, cx: &mut Context<Self>) -> Div {
        let Some(data) = self.round(round) else {
            return div();
        };
        let open = self.expanded.contains(&index);
        let reviewing = self.phase == Phase::Review
            && self
                .conversation
                .as_ref()
                .and_then(Conversation::pending)
                .is_some_and(|p| p.number == data.number);
        let price = Self::preferences(cx).price();
        let estimate = data.estimated_tokens;
        // What is new since the last request sent; the rest went before.
        let repeated = data.repeated_messages.min(data.request.messages.len());
        let skip_system = data.repeated_system;
        let new_bytes: usize = data.request.messages[repeated..]
            .iter()
            .map(Message::len)
            .sum::<usize>()
            + if skip_system {
                0
            } else {
                data.request.system.len()
            };
        let mut summary = tr!(
            "Request {0} · {1} · ~{2} tokens",
            data.number,
            data.request.model.clone(),
            estimate
        );
        if repeated > 0 || skip_system {
            summary.push_str(&format!(" · {}", tr!("~{0} new", new_bytes.div_ceil(4))));
        }
        if let Some(price) = price {
            let cost = price.input_per_mtok * estimate as f64 / 1_000_000.0;
            summary.push_str(&format!(" · ~${cost:.4}"));
        }
        let section = self.sections.get(&index).copied().unwrap_or_default();
        let request = data.request.clone();
        let whole = self.whole_requests.contains(&index);
        let (skip, skip_system) = if whole {
            (0, false)
        } else {
            (repeated, skip_system)
        };
        // Unfolds what the last request already carried.
        let earlier = (skip > 0 || skip_system).then(|| {
            let mut what = Vec::new();
            if skip_system {
                what.push(tr!("the system prompt").to_owned());
            }
            match skip {
                0 => {}
                1 => what.push(tr!("1 message").to_owned()),
                _ => what.push(tr!("{0} messages", skip)),
            }
            row()
                .id(("assistant-request-earlier", index))
                .role(Role::Button)
                .aria_label(tr!("Show the whole request"))
                .focusable()
                .tab_index(0)
                .gap_2()
                .px_2()
                .py_1()
                .rounded(px(4.))
                .bg(rgb(colors().surface))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(colors().hover)))
                .focus_visible(|s| s.bg(rgb(colors().hover)))
                .child(
                    div()
                        .text_size(px(14.))
                        .text_color(rgb(colors().text))
                        .child("…"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.))
                        .text_color(rgb(colors().faint))
                        .child(tr!(
                            "{0} as in the last request sent. Show everything",
                            what.join(tr!(" and "))
                        )),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.whole_requests.insert(index);
                    this.list.remeasure_items(index..index + 1);
                    cx.notify();
                }))
        });
        column()
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(if reviewing { colors().amber } else { colors().border }))
            .bg(rgb(colors().bg))
            .child(
                row()
                    .id(("assistant-request", index))
                    .role(Role::Button)
                    .aria_label(tr!("Show or hide the request"))
                    .focusable()
                    .tab_index(0)
                    .focus_visible(|s| s.bg(rgb(colors().hover)))
                    .px_3()
                    .h(px(30.))
                    .cursor_pointer()
                    .child(icon(if open { I::ChevronDown } else { I::ChevronRight }, colors().faint))
                    .child(icon(I::Send, if reviewing { colors().amber } else { colors().blue }))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(rgb(colors().muted)).child(summary))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded.remove(&index) {
                            this.expanded.insert(index);
                        }
                        this.list.remeasure_items(index..index + 1);
                        cx.notify();
                    })),
            )
            .when(open, |s| {
                s.child(
                    row()
                        .px_3()
                        .gap_1()
                        .children(
                            [(Section::Messages, tr!("Messages")), (Section::Tools, tr!("Tools")), (Section::Json, "JSON")]
                                .into_iter()
                                .enumerate()
                                .map(|(order, (kind, title))| {
                                    div()
                                        .id(("assistant-request-section", index * 4 + order))
                                        .role(Role::Tab)
                                        .aria_label(title)
                                        .focusable()
                                        .tab_index(0)
                                        .px_2()
                                        .py(px(3.))
                                        .rounded(px(4.))
                                        .text_size(px(12.))
                                        .cursor_pointer()
                                        .text_color(rgb(if section == kind { colors().text } else { colors().faint }))
                                        .when(section == kind, |s| s.bg(rgb(colors().surface)))
                                        .focus_visible(|s| s.bg(rgb(colors().hover)))
                                        .child(title)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.sections.insert(index, kind);
                                            this.list.remeasure_items(index..index + 1);
                                            cx.notify();
                                        }))
                                }),
                        )
                        .child(div().flex_1())
                        .child(icon_button(("assistant-request-copy", index), I::Copy, tr!("Copy the request as JSON")).on_click(
                            cx.listener(move |this, _, _, cx| {
                                if let Some(round) = this.round(round) {
                                    let json = serde_json::to_string_pretty(&round.request).unwrap_or_default();
                                    cx.write_to_clipboard(ClipboardItem::new_string(json));
                                    cx.emit(AssistantPanelEvent::Notice(Feedback::new(Tone::Success, tr!("Request copied as JSON."))));
                                }
                            }),
                        )),
                )
                .child(column().px_3().pb_3().gap_2().child(match section {
                    Section::Messages => column()
                        .gap_2()
                        .children(earlier)
                        .child(request_messages(&request, skip, skip_system)),
                    Section::Tools => column().gap_2().children(request.tools.iter().map(|tool| {
                        column()
                            .gap_1()
                            .child(div().text_size(px(12.)).text_color(rgb(colors().text)).child(tool.name.clone()))
                            .child(div().text_size(px(12.)).text_color(rgb(colors().muted)).child(tool.description.clone()))
                            .child(mono(serde_json::to_string_pretty(&tool.input_schema).unwrap_or_default()))
                    })),
                    Section::Json => column().child(mono(serde_json::to_string_pretty(&request).unwrap_or_default())),
                }))
            })
            .when(reviewing, |s| {
                s.child(
                    column()
                        .px_3()
                        .pb_3()
                        .gap_2()
                        .child(
                            div()
                                .id("assistant-review-status")
                                .role(Role::Status)
                                .text_size(px(12.))
                                .text_color(rgb(colors().amber))
                                .child(tr!(
                                    "Nothing has been sent yet. Review what will leave this computer."
                                )),
                        )
                        .child(
                            row()
                                .justify_end()
                                .gap_1()
                                .child(button("assistant-review-cancel", I::X, tr!("Cancel")).on_click(
                                    cx.listener(|this, _, _, cx| this.cancel_review(cx)),
                                ))
                                .child(primary_button("assistant-review-send", I::Send, tr!("Send")).on_click(
                                    cx.listener(|this, _, _, cx| this.send(cx)),
                                )),
                        ),
                )
            })
    }

    fn answer_item(&self, round: usize, turn: usize, cx: &mut Context<Self>) -> Div {
        let Some(data) = self.round(round) else {
            return div();
        };
        let header = row()
            .child(icon(I::Sparkles, colors().mint))
            .child(label(tr!("Assistant")));
        match &data.state {
            RoundState::Ready => {
                let elapsed = match &self.phase {
                    Phase::Sending { started } if started.elapsed() >= Duration::from_secs(2) => {
                        format!(" {:.1} s", started.elapsed().as_secs_f32())
                    }
                    _ => String::new(),
                };
                column()
                    .gap_1()
                    .child(header)
                    .child(answer_text(&self.streaming, None, round, cx))
                    .child(
                        row()
                            .id(("assistant-thinking", round))
                            .role(Role::Status)
                            .child(spinning_icon(
                                ("assistant-spinner", round),
                                I::LoaderCircle,
                                colors().amber,
                            ))
                            .child(label(format!("{}{elapsed}", tr!("Thinking…"))))
                            .child(
                                button(("assistant-stop", round), I::Square, tr!("Stop"))
                                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                            ),
                    )
            }
            RoundState::Answered { result, elapsed_ms } => {
                let fix = self.turns.get(turn).and_then(|turn| turn.fix.clone());
                let mut usage = Vec::new();
                if let Some(input) = result.usage.input {
                    usage.push(tr!("{0} in", input));
                }
                if let Some(output) = result.usage.output {
                    usage.push(tr!("{0} out", output));
                }
                if let Some(cached) = result.usage.cache_read.filter(|c| *c > 0) {
                    usage.push(tr!("{0} cached", cached));
                }
                if let Some(cost) = Self::preferences(cx)
                    .price()
                    .and_then(|price| price.cost(&result.usage))
                {
                    usage.push(format!("${cost:.4}"));
                }
                usage.push(format!("{:.1} s", *elapsed_ms as f32 / 1000.0));
                column()
                    .gap_1()
                    .child(header)
                    .when(!result.text.is_empty(), |s| {
                        s.child(answer_text(&result.text, fix, round, cx))
                    })
                    .when(
                        !result.tool_calls.is_empty() && result.text.is_empty(),
                        |s| s.child(label(tr!("Asked the application for information."))),
                    )
                    .when(result.truncated, |s| {
                        s.child(feedback_line(&Feedback::new(
                            Tone::Warning,
                            tr!("The answer stopped at the output limit."),
                        )))
                    })
                    .child(label(usage.join(" · ")))
            }
            RoundState::Failed { code, message } => {
                let title = match code {
                    ErrorCode::Authentication => tr!("The provider refused the API key"),
                    ErrorCode::Timeout => tr!("The provider did not answer in time"),
                    ErrorCode::Validation => tr!("The provider rejected the request"),
                    _ => tr!("The assistant could not answer"),
                };
                column()
                    .gap_1()
                    .child(header)
                    .child(
                        div()
                            .id(("assistant-failed", round))
                            .role(Role::Alert)
                            .child(feedback_line(&Feedback::new(
                                Tone::Error,
                                format!("{title}: {message}"),
                            ))),
                    )
                    // A request past the model's context is rejected; the
                    // user may compact it.
                    .when(*code == ErrorCode::Validation, |s| {
                        s.child(label(tr!(
                            "If the conversation is too long for the model, compact it in the assistant's options (gear)."
                        )))
                    })
                    .child(
                        button(("assistant-retry", round), I::RotateCcw, tr!("Try again"))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.retry(turn, round, cx)),
                            ),
                    )
            }
            RoundState::Cancelled => column()
                .gap_1()
                .child(header)
                .child(label(tr!("Stopped. Nothing more was sent."))),
        }
    }

    /// Starts the failed turn again, with the same first message; a failed
    /// summary asks for a summary again.
    fn retry(&mut self, turn: usize, round: usize, cx: &mut Context<Self>) {
        if self.turns.get(turn).is_some_and(|turn| turn.summary) {
            self.compact_summary(cx);
            return;
        }
        let Some(prompt) = self.round(round).and_then(|round| {
            round
                .request
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::User { text } => Some(text.clone()),
                    _ => None,
                })
        }) else {
            return;
        };
        let display = self
            .turns
            .get(turn)
            .map(|turn| turn.text.clone())
            .unwrap_or_default();
        let fix = self.turns.get(turn).and_then(|turn| turn.fix.clone());
        self.start(display, prompt, fix, cx);
    }

    fn tool_item(&self, index: usize, round: usize, step: usize, cx: &mut Context<Self>) -> Div {
        let Some(step) = self.round(round).and_then(|round| round.tools.get(step)) else {
            return div();
        };
        let open = self.expanded.contains(&index);
        let arguments = step.call.arguments.to_string();
        let size = step.output.len();
        column()
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(colors().border))
            .child(
                row()
                    .id(("assistant-tool", index))
                    .role(Role::Button)
                    .aria_label(tr!("Show or hide the tool step"))
                    .focusable()
                    .tab_index(0)
                    .focus_visible(|s| s.bg(rgb(colors().hover)))
                    .px_3()
                    .h(px(28.))
                    .cursor_pointer()
                    .child(icon(
                        if open {
                            I::ChevronDown
                        } else {
                            I::ChevronRight
                        },
                        colors().faint,
                    ))
                    .child(icon(I::Wrench, colors().teal))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(rgb(colors().muted))
                            .child(format!(
                                "{} {} · {} B · {} ms",
                                step.call.name, arguments, size, step.elapsed_ms
                            )),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded.remove(&index) {
                            this.expanded.insert(index);
                        }
                        this.list.remeasure_items(index..index + 1);
                        cx.notify();
                    })),
            )
            .when(open, |s| {
                s.child(
                    column()
                        .px_3()
                        .pb_3()
                        .gap_1()
                        .child(label(tr!("Answer sent to the model with the next request")))
                        .child(mono(step.output.clone())),
                )
            })
    }

    /// A query the model asked for: the statement to approve, the query
    /// running, its result to send with columns hidden, or what was decided.
    fn query_item(&self, index: usize, card: usize, cx: &mut Context<Self>) -> Div {
        let Some(data) = self.cards.get(card) else {
            return div();
        };
        let title = |text: String, color: u32| {
            row().child(icon(I::Database, color)).child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(rgb(colors().muted))
                    .child(text),
            )
        };
        let frame = |color: u32| {
            column()
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(color))
                .bg(rgb(colors().bg))
                .px_3()
                .py_2()
                .gap_2()
        };
        match &data.state {
            CardState::Asked => {
                let dangerous = self.production && data.risk.is_some();
                frame(colors().amber)
                    .child(title(tr!("The assistant asks to run a query").to_owned(), colors().amber))
                    .when_some(data.purpose.clone(), |s, purpose| {
                        s.child(prose(&super::mcp_section::show_controls(&purpose)))
                    })
                    .child(sql_text(data.sql.clone()))
                    .when(data.risk.is_some(), |s| {
                        s.child(badge(tr!("Changes or deletes data"), colors().amber))
                    })
                    .when(self.production, |s| {
                        s.child(feedback_line(&Feedback::new(
                            Tone::Warning,
                            tr!("This is a production connection."),
                        )))
                    })
                    .child(
                        div()
                            .id(("assistant-query-status", card))
                            .role(Role::Status)
                            .child(label(tr!(
                                "It runs on {0}, in a session of the assistant's own. Nothing of the result is sent until you approve it.",
                                self.label.clone()
                            ))),
                    )
                    .child(
                        row()
                            .justify_end()
                            .gap_1()
                            .child(button(("assistant-query-decline", card), I::X, tr!("Decline")).on_click(
                                cx.listener(move |this, _, _, cx| this.decline_card(card, cx)),
                            ))
                            .child(
                                primary_button(
                                    ("assistant-query-run", card),
                                    I::Play,
                                    if dangerous { tr!("Run on production") } else { tr!("Run") },
                                )
                                .on_click(cx.listener(move |this, _, _, cx| this.run_card(card, cx))),
                            ),
                    )
            }
            CardState::Running { started, .. } => {
                let elapsed = started.elapsed();
                let mut status = tr!("Running…").to_owned();
                if elapsed >= Duration::from_secs(2) {
                    status.push_str(&format!(" {:.1} s", elapsed.as_secs_f32()));
                }
                frame(colors().border)
                    .child(title(tr!("Query").to_owned(), colors().blue))
                    .child(sql_text(data.sql.clone()))
                    .child(
                        row()
                            .id(("assistant-query-running", card))
                            .role(Role::Status)
                            .child(spinning_icon(
                                ("assistant-query-spinner", card),
                                I::LoaderCircle,
                                colors().amber,
                            ))
                            .child(label(status))
                            .child(div().flex_1())
                            .child(
                                button(("assistant-query-stop", card), I::Square, tr!("Stop"))
                                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                            ),
                    )
            }
            CardState::Review {
                preview,
                hidden,
                text,
            } => {
                let mut counts = if preview.more {
                    tr!("First {0} rows; there are more", preview.rows.len())
                } else {
                    tr!("{0} rows", preview.rows.len())
                };
                if let Some(affected) = preview.rows_affected {
                    counts.push_str(&format!(" · {}", tr!("{0} affected", affected)));
                }
                if !hidden.is_empty() {
                    counts.push_str(&format!(" · {}", tr!("hidden columns: {0}", hidden.len())));
                }
                counts.push_str(&format!(" · {} ms", preview.elapsed_ms));
                let result =
                    if let Some(error) = &preview.error {
                        column().child(feedback_line(&Feedback::new(
                            Tone::Error,
                            format!("{}: {}", error.title, error.message),
                        )))
                    } else if preview.columns.is_empty() {
                        column().child(label(tr!("The statement ran and returned no rows.")))
                    } else {
                        // One toggle per column, wrapping, so every column can
                        // be hidden however narrow the panel is.
                        let toggles = row().flex_wrap().gap_1().children(
                            preview.columns.iter().enumerate().map(|(at, info)| {
                                let off = hidden.contains(&at);
                                row()
                                    .id(("assistant-query-hide", card * 4096 + at))
                                    .role(Role::Button)
                                    .aria_label(if off {
                                        tr!("Send the values of column {0}", info.name.clone())
                                    } else {
                                        tr!("Hide the values of column {0}", info.name.clone())
                                    })
                                    .aria_toggled(if off { Toggled::True } else { Toggled::False })
                                    .focusable()
                                    .tab_index(0)
                                    .gap_1()
                                    .px_2()
                                    .py(px(2.))
                                    .rounded(px(4.))
                                    .border_1()
                                    .border_color(rgb(if off {
                                        colors().amber
                                    } else {
                                        colors().border
                                    }))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(colors().hover)))
                                    .focus_visible(|s| s.bg(rgb(colors().hover)))
                                    .child(icon(
                                        if off { I::EyeOff } else { I::Eye },
                                        if off { colors().amber } else { colors().muted },
                                    ))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(if off {
                                                colors().faint
                                            } else {
                                                colors().text
                                            }))
                                            .when(off, |s| s.line_through())
                                            .child(info.name.clone()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_column(card, at, cx)
                                    }))
                            }),
                        );
                        let head = row()
                            .gap_0()
                            .children(preview.columns.iter().enumerate().map(|(at, info)| {
                                let off = hidden.contains(&at);
                                column()
                                    .w(RESULT_COLUMN)
                                    .flex_none()
                                    .px_1()
                                    .border_b_1()
                                    .border_color(rgb(colors().border))
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(12.))
                                            .text_color(rgb(if off {
                                                colors().faint
                                            } else {
                                                colors().text
                                            }))
                                            .child(info.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(px(11.))
                                            .text_color(rgb(colors().faint))
                                            .child(info.data_type.clone()),
                                    )
                            }));
                        let rows = preview.rows.iter().map(|cells| {
                            row()
                                .gap_0()
                                .children(cells.iter().enumerate().map(|(at, cell)| {
                                    let off = hidden.contains(&at);
                                    let (shown, color) = match (off, cell) {
                                        (true, _) => (released::HIDDEN.to_owned(), colors().faint),
                                        (false, Some(text)) => {
                                            (text.replace(['\r', '\n'], " "), colors().muted)
                                        }
                                        (false, None) => ("NULL".to_owned(), colors().faint),
                                    };
                                    div()
                                        .w(RESULT_COLUMN)
                                        .flex_none()
                                        .px_1()
                                        .truncate()
                                        .font_family(fonts().mono.clone())
                                        .text_size(px(12.))
                                        .text_color(rgb(color))
                                        .child(shown)
                                }))
                        });
                        column()
                            .gap_2()
                            .child(label(tr!("Columns to send; select one to hide its values")))
                            .child(toggles)
                            .child(
                                div()
                                    .id(("assistant-query-grid", card))
                                    .overflow_x_scroll()
                                    .child(column().child(head).children(rows)),
                            )
                    };
                frame(colors().amber)
                    .child(title(
                        tr!("Result of the query: review what is sent").to_owned(),
                        colors().amber,
                    ))
                    .child(sql_text(data.sql.clone()))
                    .child(result)
                    .child(label(counts))
                    .child(label(tr!("What the model receives if you send it")))
                    .child(mono(text.clone()))
                    .child(
                        div()
                            .id(("assistant-query-review", card))
                            .role(Role::Status)
                            .text_size(px(12.))
                            .text_color(rgb(colors().amber))
                            .child(tr!("Nothing of the result has been sent yet.")),
                    )
                    .child(
                        row()
                            .justify_end()
                            .gap_1()
                            .child(
                                button(
                                    ("assistant-query-withhold", card),
                                    I::Ban,
                                    tr!("Don't send"),
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.settle_card(card, false, cx),
                                )),
                            )
                            .child(
                                primary_button(
                                    ("assistant-query-send", card),
                                    I::Send,
                                    tr!("Send result"),
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.settle_card(card, true, cx),
                                )),
                            ),
                    )
            }
            CardState::Done(step) => {
                let open = self.expanded.contains(&index);
                let record = step.query.as_ref();
                let mut summary = match record.map(|record| record.decision) {
                    Some(QueryDecision::Sent) => tr!("Query · result sent").to_owned(),
                    Some(QueryDecision::Withheld) => tr!("Query · ran, result not sent").to_owned(),
                    _ => tr!("Query · declined, nothing ran").to_owned(),
                };
                if let Some(record) = record.filter(|record| record.decision == QueryDecision::Sent)
                {
                    summary.push_str(&format!(" · {}", tr!("{0} rows", record.rows_sent)));
                    if !record.hidden.is_empty() {
                        summary.push_str(&format!(
                            " · {}",
                            tr!("hidden: {0}", record.hidden.join(", "))
                        ));
                    }
                }
                column()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(colors().border))
                    .child(
                        row()
                            .id(("assistant-query-done", index))
                            .role(Role::Button)
                            .aria_label(tr!("Show or hide the query"))
                            .focusable()
                            .tab_index(0)
                            .focus_visible(|s| s.bg(rgb(colors().hover)))
                            .px_3()
                            .h(px(28.))
                            .cursor_pointer()
                            .child(icon(
                                if open {
                                    I::ChevronDown
                                } else {
                                    I::ChevronRight
                                },
                                colors().faint,
                            ))
                            .child(icon(I::Database, colors().teal))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors().muted))
                                    .child(summary),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.expanded.remove(&index) {
                                    this.expanded.insert(index);
                                }
                                this.list.remeasure_items(index..index + 1);
                                cx.notify();
                            })),
                    )
                    .when(open, |s| {
                        s.child(
                            column()
                                .px_3()
                                .pb_3()
                                .gap_1()
                                .child(sql_text(data.sql.clone()))
                                .child(label(tr!("Answer sent to the model with the next request")))
                                .child(mono(step.output.clone())),
                        )
                    })
            }
        }
    }

    fn demo_item(&self, index: usize, cx: &mut Context<Self>) -> Div {
        let Some((text, sql)) = self.demo_answers.get(index) else {
            return div();
        };
        let mut shown = text.clone();
        if let Some(sql) = sql {
            shown.push_str(&format!("\n\n```sql\n{sql}\n```"));
        }
        column()
            .gap_1()
            .child(
                row()
                    .child(icon(I::Sparkles, colors().mint))
                    .child(label(tr!("Assistant")))
                    .child(badge(tr!("Simulated"), colors().amber)),
            )
            .child(answer_text(&shown, None, 10_000 + index, cx))
    }
}

impl Drop for AssistantPanel {
    /// Closing the tab stops a round or a query on its way; nothing more
    /// is sent.
    fn drop(&mut self) {
        self.cancel.cancel();
        self.cancel_queries();
    }
}

/// Width of a column in a query's result card.
const RESULT_COLUMN: Pixels = px(150.);

/// A token count in few characters: `850`, `12.4k`, `1.2M` (a decimal comma
/// in Portuguese).
fn count_short(count: u64) -> String {
    let text = match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{:.1}k", count as f64 / 1_000.0),
        _ => format!("{:.1}M", count as f64 / 1_000_000.0),
    };
    if jykre::i18n::locale().starts_with("pt") {
        text.replace('.', ",")
    } else {
        text
    }
}

/// A query's statement, as the user approves it.
fn sql_text(sql: String) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded(px(4.))
        .bg(rgb(colors().surface))
        .font_family(fonts().mono.clone())
        .text_size(px(12.))
        .text_color(rgb(colors().blue))
        .whitespace_normal()
        .child(super::mcp_section::show_controls(&sql))
}

fn mono(text: String) -> Div {
    div()
        .p_2()
        .rounded(px(4.))
        .bg(rgb(colors().surface))
        .font_family(fonts().mono.clone())
        .text_size(px(12.))
        .text_color(rgb(colors().muted))
        .whitespace_normal()
        .child(text)
}

/// A request's messages after the first `skip`, with the system prompt
/// unless `skip_system`: what is new since the last request sent.
fn request_messages(
    request: &jykre_assistant::ModelRequest,
    skip: usize,
    skip_system: bool,
) -> Div {
    let role = |title: &'static str, size: usize| label(format!("{title} · {size} B"));
    column()
        .gap_2()
        .when(!skip_system, |s| {
            s.child(role(tr!("System"), request.system.len()))
                .child(mono(request.system.clone()))
        })
        .children(request.messages.iter().skip(skip).map(|message| {
            match message {
                Message::User { text } => column()
                    .gap_1()
                    .child(role(tr!("User"), text.len()))
                    .child(mono(text.clone())),
                Message::Assistant { text, tool_calls } => {
                    let mut shown = text.clone();
                    for call in tool_calls {
                        shown.push_str(&format!("\n→ {} {}", call.name, call.arguments));
                    }
                    column()
                        .gap_1()
                        .child(role(tr!("Assistant"), message.len()))
                        .child(mono(shown.trim().to_owned()))
                }
                Message::Tool { name, content, .. } => column()
                    .gap_1()
                    .child(role(tr!("Tool result"), content.len()))
                    .child(label(name.clone()))
                    .child(mono(content.clone())),
            }
        }))
}

/// An answer as plain text with its SQL blocks set apart, each with Copy,
/// Insert at the cursor and, for a fix, Replace the failed statement.
fn answer_text(
    text: &str,
    fix: Option<FixTarget>,
    round: usize,
    cx: &mut Context<AssistantPanel>,
) -> Div {
    let blocks = sql_blocks(text);
    let mut parts = column().gap_2();
    let mut at = 0;
    let fence_start = |range_start: usize| text[..range_start].rfind("```").unwrap_or(range_start);
    for (order, block) in blocks.into_iter().enumerate() {
        let start = fence_start(block.range.start);
        let before = text[at..start].trim();
        if !before.is_empty() {
            parts = parts.child(prose(before));
        }
        let sql = text[block.range.clone()].trim_end().to_owned();
        let id = round * 100 + order;
        let mut card = column()
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(colors().border))
            .bg(rgb(colors().bg))
            .overflow_hidden()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .font_family(fonts().mono.clone())
                    .text_size(px(12.))
                    .text_color(rgb(colors().blue))
                    .child(sql.clone()),
            );
        if block.closed {
            let risk = jykre::guard::risk(&sql, None);
            let insert = sql.clone();
            let copy = sql.clone();
            card = card
                .when_some(risk, |s, _| {
                    s.child(div().px_3().pb_1().child(badge(
                        tr!("Changes or deletes data · review before running"),
                        colors().amber,
                    )))
                })
                .child(
                    row()
                        .px_2()
                        .py_1()
                        .flex_wrap()
                        .gap_1()
                        .border_t_1()
                        .border_color(rgb(colors().border))
                        .child(
                            icon_button(
                                ("assistant-copy-sql", id),
                                I::Copy,
                                tr!("Copy suggested SQL"),
                            )
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                            }),
                        )
                        .child(
                            button(
                                ("assistant-insert-sql", id),
                                I::CornerDownLeft,
                                tr!("Insert at the cursor"),
                            )
                            .on_click(cx.listener(
                                move |_, _, _, cx| {
                                    cx.emit(AssistantPanelEvent::Insert(insert.clone()))
                                },
                            )),
                        )
                        .when_some(fix.clone(), |s, fix| {
                            let replacement = sql.clone();
                            s.child(
                                button(
                                    ("assistant-replace-sql", id),
                                    I::Replace,
                                    tr!("Replace the failed statement"),
                                )
                                .on_click(cx.listener(
                                    move |_, _, _, cx| {
                                        cx.emit(AssistantPanelEvent::Replace {
                                            statement: fix.statement.clone(),
                                            origin: fix.origin,
                                            sql: replacement.clone(),
                                        })
                                    },
                                )),
                            )
                        }),
                );
        }
        parts = parts.child(card);
        at = (block.range.end
            + if block.closed {
                text[block.range.end..]
                    .find('\n')
                    .map_or(text.len() - block.range.end, |n| n + 1)
            } else {
                0
            })
        .min(text.len());
    }
    let rest = text[at..].trim();
    if !rest.is_empty() {
        parts = parts.child(prose(rest));
    }
    parts
}

fn prose(text: &str) -> Div {
    div()
        .text_size(px(14.))
        .line_height(relative(1.55))
        .text_color(rgb(colors().muted))
        .child(text.to_owned())
}

impl Render for AssistantPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::probe::rendered("AssistantPanel");
        let configured = self.configured(cx);
        let preferences = Self::preferences(cx);
        let provider = preferences
            .config()
            .map(|config| format!("{} · {}", config.kind.key(), config.model))
            .unwrap_or_default();
        let busy = self.busy();
        let limit = matches!(self.phase, Phase::Limit(_));
        let entity = cx.entity();
        column()
            .size_full()
            .bg(rgb(colors().panel))
            .border_l_1()
            .border_color(rgb(colors().border))
            .child(
                row()
                    .h(px(48.))
                    .flex_none()
                    .px_4()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(colors().border))
                    .child(
                        row()
                            .min_w_0()
                            .child(icon(I::Sparkles, colors().mint))
                            .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(tr!("Assistant")))
                            .when(self.demo, |s| s.child(badge(tr!("Simulated"), colors().amber)))
                            .when(!self.demo && !provider.is_empty(), |s| {
                                s.child(div().min_w_0().truncate().child(badge(provider.clone(), colors().mint)))
                            }),
                    )
                    .child(
                        row()
                            .gap_0()
                            .when(!self.demo, |s| s.child(self.options_menu(cx)))
                            .when(self.conversation.is_some(), |s| {
                                s.child(icon_button("assistant-copy-trace", I::ClipboardCopy, tr!("Copy the whole trace as JSON")).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        if let Some(conversation) = &this.conversation {
                                            cx.write_to_clipboard(ClipboardItem::new_string(conversation.trace_json()));
                                            cx.emit(AssistantPanelEvent::Notice(Feedback::new(Tone::Success, tr!("Trace copied as JSON."))));
                                        }
                                    }),
                                ))
                            })
                            .child(icon_button("reset-chat", I::SquarePen, tr!("New conversation")).on_click(
                                cx.listener(|this, _, _, cx| this.clear(cx)),
                            ))
                            .child(icon_button("close-ai", I::PanelRightClose, tr!("Collapse assistant")).on_click(
                                cx.listener(|_, _, _, cx| cx.emit(AssistantPanelEvent::Close)),
                            )),
                    ),
            )
            .when(configured && !self.demo, |s| s.child(self.summary_view(cx)))
            .child(if !configured {
                column()
                    .flex_1()
                    .p_4()
                    .gap_3()
                    .child(icon(I::Sparkles, colors().mint))
                    .child(div().text_size(px(16.)).child(tr!("Set up the assistant")))
                    .child(label(tr!(
                        "Choose a provider and a model in Preferences. Your SQL, error messages, row counts and the structure of the database are sent; values never are."
                    )))
                    .child(primary_button("assistant-setup", I::Settings2, tr!("Open Preferences")).on_click(
                        cx.listener(|_, _, _, cx| cx.emit(AssistantPanelEvent::OpenPreferences)),
                    ))
                    .into_any_element()
            } else if self.items.is_empty() {
                column()
                    .flex_1()
                    .p_4()
                    .gap_3()
                    .child(icon(I::Sparkles, colors().mint))
                    .child(div().text_size(px(16.)).child(tr!("One question, a new path.")))
                    .child(label(tr!(
                        "Ask about the structure of the database, describe a query to write, or use Fix with AI on a failed statement."
                    )))
                    .into_any_element()
            } else {
                list(self.list.clone(), {
                    let entity = entity.clone();
                    move |index, window, cx| entity.update(cx, |this, cx| this.render_item(index, window, cx))
                })
                .flex_1()
                .size_full()
                .into_any_element()
            })
            .when(limit, |s| {
                s.child(
                    row()
                        .flex_none()
                        .px_4()
                        .py_2()
                        .border_t_1()
                        .border_color(rgb(colors().border))
                        .child(div().flex_1().child(label(tr!("The assistant used {0} rounds for this request.", run::MAX_ROUNDS))))
                        .child(button("assistant-limit-stop", I::Square, tr!("Stop")).on_click(cx.listener(|this, _, _, cx| this.stop(cx))))
                        .child(primary_button("assistant-limit-continue", I::Play, tr!("Continue")).on_click(cx.listener(
                            |this, _, _, cx| {
                                if let Phase::Limit(calls) = std::mem::replace(&mut this.phase, Phase::Idle) {
                                    this.run_tools(calls, true, cx);
                                }
                            },
                        ))),
                )
            })
            .child(
                column()
                    .flex_none()
                    .p_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(rgb(colors().border))
                    .child(
                        row()
                            .gap_1()
                            .children([(Mode::Ask, tr!("Ask")), (Mode::WriteSql, tr!("Write SQL"))].into_iter().map(|(mode, title)| {
                                div()
                                    .id(match mode {
                                        Mode::Ask => "assistant-mode-ask",
                                        Mode::WriteSql => "assistant-mode-write",
                                    })
                                    .role(Role::Tab)
                                    .aria_label(title)
                                    .focusable()
                                    .tab_index(0)
                                    .px_2()
                                    .py(px(4.))
                                    .rounded(px(4.))
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .text_color(rgb(if self.mode == mode { colors().text } else { colors().faint }))
                                    .when(self.mode == mode, |s| s.bg(rgb(colors().surface)))
                                    .focus_visible(|s| s.bg(rgb(colors().hover)))
                                    .child(title)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.mode = mode;
                                        cx.notify();
                                    }))
                            })),
                    )
                    .child(
                        row()
                            .p_2()
                            .rounded(px(7.))
                            .border_1()
                            .border_color(rgb(colors().border_strong))
                            .bg(rgb(colors().bg))
                            .child(
                                div().flex_1().child(
                                    Input::new(&self.prompt)
                                        .appearance(false)
                                        .bordered(false)
                                        .small()
                                        .w_full()
                                        .aria_label(tr!("Message to the AI assistant")),
                                ),
                            )
                            .child(if busy && !limit {
                                icon_button("assistant-stop-composer", I::Square, tr!("Stop"))
                                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                            } else {
                                icon_button("send-ai", I::ArrowUp, tr!("Send message"))
                                    .bg(rgb(colors().accent_bg))
                                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                            }),
                    )
                    .child(div().text_center().text_size(px(12.)).text_color(rgb(colors().faint)).child(if self.demo {
                        tr!("Simulated AI · Always review the suggestions.")
                    } else {
                        tr!("Nothing runs on its own · Review the suggestions before running them.")
                    })),
            )
    }
}
