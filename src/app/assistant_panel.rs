//! The assistant panel (ADR-0028): a conversation with the configured model
//! about the tab's connection. It is its own cached entity, so a streamed
//! answer redraws the panel only (ADR-0012), and it shows everything that
//! leaves the computer: each request before it is sent (when the review
//! mode asks), each tool the model called with what it was answered, and
//! the tokens and cost of every round.
//!
//! The panel decides nothing about privacy: the workspace builds each
//! request's first message from the facts allow-list, and the tools read
//! the metadata cache only (`jykre::assistant`).
use super::assistant_settings::{self, AssistantSettings};
use super::demo;
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::assistant::facts::SchemaScope;
use jykre::assistant::run::{self, Conversation, Next, Round, RoundState, ToolStep};
use jykre::assistant::tools::CacheTools;
use jykre::assistant::{self as library, AssistantPreferences, prompts};
use jykre::drivers::{CancellationToken, DriverError, DriverResult, ErrorCode};
use jykre::{backend, tr};
use jykre_assistant::{
    Assistant, KeyUse, Message, RoundEvent, RoundResult, Timeouts, ToolCall, sql_blocks,
};
use std::collections::{HashMap, HashSet};
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
}

impl EventEmitter<AssistantPanelEvent> for AssistantPanel {}

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
    /// The turn used its rounds; these calls wait for **Continue**.
    Limit(Vec<ToolCall>),
}

/// One row of the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    User { turn: usize },
    Request { round: usize },
    Answer { round: usize, turn: usize },
    Tool { round: usize, step: usize },
    Demo { index: usize },
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

    /// Stops what runs and forgets the conversation.
    pub(super) fn clear(&mut self, cx: &mut Context<Self>) {
        self.cancel.cancel();
        self.epoch += 1;
        self.conversation = None;
        self.tools = None;
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
            self.conversation = Some(Conversation::new(
                config.kind.key(),
                &config.model,
                prompts::system_prompt(&target.engine, language),
                CacheTools::specs(),
            ));
            self.tools = Some(Arc::new(CacheTools::new(target.clone())));
        }
        self.turns.push(Turn { text: display, fix });
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
            Ok(result) => match conversation.record(result, elapsed) {
                Next::Done => self.phase = Phase::Idle,
                Next::LimitReached(calls) => self.phase = Phase::Limit(calls),
                Next::Tools(calls) => self.run_tools(calls, false, cx),
            },
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

    /// Runs the tools of the last round on the cache, shows each step and
    /// prepares the next round.
    fn run_tools(&mut self, calls: Vec<ToolCall>, continued: bool, cx: &mut Context<Self>) {
        let Some(tools) = self.tools.clone() else {
            return;
        };
        self.phase = Phase::Tools;
        let epoch = self.epoch;
        let cancel = self.cancel.clone();
        cx.spawn(async move |weak, cx| {
            let steps = backend::run(async move {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(DriverError::cancelled()),
                    steps = run::run_tools(&tools, &calls) => Ok(steps),
                }
            })
            .await;
            let _ = weak.update(cx, |this, cx| this.tools_done(epoch, steps, continued, cx));
        })
        .detach();
        cx.notify();
    }

    fn tools_done(
        &mut self,
        epoch: u64,
        steps: DriverResult<Vec<ToolStep>>,
        continued: bool,
        cx: &mut Context<Self>,
    ) {
        if self.epoch != epoch {
            return;
        }
        let Some(conversation) = &mut self.conversation else {
            return;
        };
        let steps = match steps {
            Ok(steps) => steps,
            Err(error) => {
                conversation.record_error(&error);
                self.phase = Phase::Idle;
                cx.notify();
                return;
            }
        };
        let answered = conversation.rounds().len() - 1;
        let count = steps.len();
        let next = if continued {
            conversation.continue_turn(steps).number - 1
        } else {
            conversation.answer_tools(steps).number - 1
        };
        for step in 0..count {
            self.push(Item::Tool {
                round: answered,
                step,
            });
        }
        self.push(Item::Request { round: next });
        self.expanded.insert(self.items.len() - 1);
        self.advance(cx);
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
                        let running = matches!(this.phase, Phase::Sending { .. } | Phase::Tools);
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

    fn total_line(&self, cx: &App) -> Option<String> {
        let conversation = self.conversation.as_ref()?;
        let usage = conversation.usage();
        let (input, output) = (usage.input?, usage.output?);
        let mut line = tr!("{0} in · {1} out", input, output);
        if let Some(cost) = Self::preferences(cx)
            .price()
            .and_then(|price| conversation.cost(&price))
        {
            line.push_str(&format!(" · ${cost:.4}"));
        }
        Some(line)
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
                    .child(icon(I::UserRound, MUTED))
                    .child(label(tr!("You"))),
            )
            .child(div().text_size(px(14.)).text_color(rgb(TEXT)).child(text))
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
        let mut summary = tr!(
            "Request {0} · {1} · ~{2} tokens",
            data.number,
            data.request.model.clone(),
            estimate
        );
        if let Some(price) = price {
            let cost = price.input_per_mtok * estimate as f64 / 1_000_000.0;
            summary.push_str(&format!(" · ~${cost:.4}"));
        }
        let section = self.sections.get(&index).copied().unwrap_or_default();
        let request = data.request.clone();
        let omitted = data.omitted_messages;
        column()
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(if reviewing { AMBER } else { BORDER }))
            .bg(rgb(BG))
            .child(
                row()
                    .id(("assistant-request", index))
                    .role(Role::Button)
                    .aria_label(tr!("Show or hide the request"))
                    .focusable()
                    .tab_index(0)
                    .focus_visible(|s| s.bg(rgb(HOVER)))
                    .px_3()
                    .h(px(30.))
                    .cursor_pointer()
                    .child(icon(if open { I::ChevronDown } else { I::ChevronRight }, FAINT))
                    .child(icon(I::Send, if reviewing { AMBER } else { BLUE }))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(rgb(MUTED)).child(summary))
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
                                        .text_color(rgb(if section == kind { TEXT } else { FAINT }))
                                        .when(section == kind, |s| s.bg(rgb(SURFACE)))
                                        .focus_visible(|s| s.bg(rgb(HOVER)))
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
                    Section::Messages => request_messages(&request, omitted),
                    Section::Tools => column().gap_2().children(request.tools.iter().map(|tool| {
                        column()
                            .gap_1()
                            .child(div().text_size(px(12.)).text_color(rgb(TEXT)).child(tool.name.clone()))
                            .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child(tool.description.clone()))
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
                                .text_color(rgb(AMBER))
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
            .child(icon(I::Sparkles, MINT))
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
                                AMBER,
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

    /// Starts the failed turn again, with the same first message.
    fn retry(&mut self, turn: usize, round: usize, cx: &mut Context<Self>) {
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
            .border_color(rgb(BORDER))
            .child(
                row()
                    .id(("assistant-tool", index))
                    .role(Role::Button)
                    .aria_label(tr!("Show or hide the tool step"))
                    .focusable()
                    .tab_index(0)
                    .focus_visible(|s| s.bg(rgb(HOVER)))
                    .px_3()
                    .h(px(28.))
                    .cursor_pointer()
                    .child(icon(
                        if open {
                            I::ChevronDown
                        } else {
                            I::ChevronRight
                        },
                        FAINT,
                    ))
                    .child(icon(I::Wrench, TEAL))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
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
                    .child(icon(I::Sparkles, MINT))
                    .child(label(tr!("Assistant")))
                    .child(badge(tr!("Simulated"), AMBER)),
            )
            .child(answer_text(&shown, None, 10_000 + index, cx))
    }
}

impl Drop for AssistantPanel {
    /// Closing the tab stops a round on its way; nothing more is sent.
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn mono(text: String) -> Div {
    div()
        .p_2()
        .rounded(px(4.))
        .bg(rgb(SURFACE))
        .font_family("Cascadia Mono")
        .text_size(px(12.))
        .text_color(rgb(MUTED))
        .whitespace_normal()
        .child(text)
}

fn request_messages(request: &jykre_assistant::ModelRequest, omitted: usize) -> Div {
    let role = |title: &'static str, size: usize| label(format!("{title} · {size} B"));
    column()
        .gap_2()
        .child(role(tr!("System"), request.system.len()))
        .child(mono(request.system.clone()))
        .when(omitted > 0, |s| {
            s.child(label(tr!(
                "{0} earlier messages left out to keep the request small",
                omitted
            )))
        })
        .children(request.messages.iter().map(|message| {
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
            .border_color(rgb(BORDER))
            .bg(rgb(BG))
            .overflow_hidden()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .font_family("Cascadia Mono")
                    .text_size(px(12.))
                    .text_color(rgb(BLUE))
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
                        AMBER,
                    )))
                })
                .child(
                    row()
                        .px_2()
                        .py_1()
                        .flex_wrap()
                        .gap_1()
                        .border_t_1()
                        .border_color(rgb(BORDER))
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
        .text_color(rgb(0xaeb8c5))
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
        let total = self.total_line(cx);
        let busy = self.busy();
        let limit = matches!(self.phase, Phase::Limit(_));
        let entity = cx.entity();
        column()
            .size_full()
            .bg(rgb(0x191d23))
            .border_l_1()
            .border_color(rgb(BORDER))
            .child(
                row()
                    .h(px(48.))
                    .flex_none()
                    .px_4()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        row()
                            .min_w_0()
                            .child(icon(I::Sparkles, MINT))
                            .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(tr!("Assistant")))
                            .when(self.demo, |s| s.child(badge(tr!("Simulated"), AMBER)))
                            .when(!self.demo && !provider.is_empty(), |s| {
                                s.child(div().min_w_0().truncate().child(badge(provider.clone(), MINT)))
                            }),
                    )
                    .child(
                        row()
                            .gap_0()
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
            .child(
                column()
                    .flex_none()
                    .px_4()
                    .py_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        row()
                            .child(icon(I::Layers, FAINT))
                            .child(label(if self.label.is_empty() {
                                SharedString::from(tr!("No connection"))
                            } else {
                                self.label.clone()
                            }))
                            .when_some(total, |s, total| s.child(div().flex_1()).child(label(total))),
                    )
                    .child(label(tr!("Sends SQL, errors, row counts and structure. Never values."))),
            )
            .child(if !configured {
                column()
                    .flex_1()
                    .p_4()
                    .gap_3()
                    .child(icon(I::Sparkles, MINT))
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
                    .child(icon(I::Sparkles, MINT))
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
                        .border_color(rgb(BORDER))
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
                    .border_color(rgb(BORDER))
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
                                    .text_color(rgb(if self.mode == mode { TEXT } else { FAINT }))
                                    .when(self.mode == mode, |s| s.bg(rgb(SURFACE)))
                                    .focus_visible(|s| s.bg(rgb(HOVER)))
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
                            .border_color(rgb(0x3b4945))
                            .bg(rgb(BG))
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
                                    .bg(rgb(0x304a40))
                                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                            }),
                    )
                    .child(div().text_center().text_size(px(12.)).text_color(rgb(FAINT)).child(if self.demo {
                        tr!("Simulated AI · Always review the suggestions.")
                    } else {
                        tr!("Nothing runs on its own · Review the suggestions before running them.")
                    })),
            )
    }
}
