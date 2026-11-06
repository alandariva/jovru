//! **Preferences › Assistant** (ADR-0028): which provider and model answer,
//! the API key (kept in the system vault, never shown back), when a request
//! is reviewed before it leaves, and the prices used to show costs. The
//! choices are a window global every assistant panel follows.
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::assistant::{self, AssistantPreferences, run::ReviewMode};
use jykre::drivers::{CancellationToken, DriverError, ErrorCode, SecretString};
use jykre::{backend, connection_store, tr};
use jykre_assistant::{Assistant, KeyUse, Price, ProviderKind, Timeouts};
use std::sync::Arc;
use std::time::Instant;

/// The assistant preferences of the window; the defaults (off) until read.
#[derive(Clone, Debug, Default)]
pub(super) struct AssistantSettings(pub Arc<AssistantPreferences>);

impl Global for AssistantSettings {}

pub(super) fn preferences(cx: &App) -> Arc<AssistantPreferences> {
    cx.try_global::<AssistantSettings>()
        .map(|settings| settings.0.clone())
        .unwrap_or_default()
}

/// Reads the preferences at start. A failure leaves the assistant off and
/// is logged; the rest of the application does not depend on it.
pub(super) fn load(cx: &mut App) {
    let dir = connection_store::data_dir();
    cx.spawn(async move |cx| {
        let read = backend::run(async move { assistant::load_preferences(&dir).await }).await;
        match read {
            Ok(preferences) => {
                cx.update(|cx| cx.set_global(AssistantSettings(Arc::new(preferences))));
            }
            Err(error) => {
                tracing::warn!(error_code = ?error.code, "assistant preferences unreadable");
            }
        }
    })
    .detach();
}

fn provider_name(kind: Option<ProviderKind>) -> &'static str {
    match kind {
        None => tr!("Off"),
        Some(ProviderKind::Anthropic) => "Anthropic",
        Some(ProviderKind::OpenAi) => "OpenAI",
        Some(ProviderKind::OpenAiCompatible) => tr!("OpenAI-compatible"),
        Some(ProviderKind::Ollama) => "Ollama",
    }
}

fn review_name(mode: ReviewMode) -> &'static str {
    match mode {
        ReviewMode::EveryRequest => tr!("Every request"),
        ReviewMode::FirstOnly => tr!("First request only"),
        ReviewMode::Never => tr!("Never"),
    }
}

/// The form of the assistant's preferences.
pub(super) struct AssistantSettingsView {
    provider: Option<ProviderKind>,
    review: ReviewMode,
    queries: bool,
    model: Entity<InputState>,
    endpoint: Entity<InputState>,
    key: Entity<InputState>,
    price_input: Entity<InputState>,
    price_output: Entity<InputState>,
    price_cache: Entity<InputState>,
    status: Option<Feedback>,
    testing: Option<(CancellationToken, Instant)>,
    /// Whether the form shows what the global holds; it is filled once.
    filled: bool,
    _subscriptions: Vec<Subscription>,
}

impl AssistantSettingsView {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let model = input(tr!("Model name, as the provider calls it"), window, cx);
        let endpoint = input(tr!("Server address (optional)"), window, cx);
        let key = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(tr!("API key; empty keeps the saved one"))
        });
        let price_input = input(tr!("Input"), window, cx);
        let price_output = input(tr!("Output"), window, cx);
        let price_cache = input(tr!("Cached input"), window, cx);
        let observation = cx.observe_global_in::<AssistantSettings>(window, |this, window, cx| {
            if !this.filled {
                this.fill(window, cx);
            }
        });
        let mut view = Self {
            provider: None,
            review: ReviewMode::default(),
            queries: true,
            model,
            endpoint,
            key,
            price_input,
            price_output,
            price_cache,
            status: None,
            testing: None,
            filled: false,
            _subscriptions: vec![observation],
        };
        view.fill(window, cx);
        view
    }

    fn fill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = cx.try_global::<AssistantSettings>().map(|s| s.0.clone()) else {
            return;
        };
        self.filled = true;
        self.provider = settings.provider;
        self.review = settings.review;
        self.queries = settings.queries;
        let set = |state: &Entity<InputState>,
                   text: String,
                   window: &mut Window,
                   cx: &mut Context<Self>| {
            state.update(cx, |input, cx| input.set_value(text, window, cx));
        };
        set(&self.model, settings.model.clone(), window, cx);
        set(
            &self.endpoint,
            settings.endpoint.clone().unwrap_or_default(),
            window,
            cx,
        );
        let price = settings.price().unwrap_or_default();
        let number = |value: f64| {
            if value == 0.0 {
                String::new()
            } else {
                value.to_string()
            }
        };
        set(&self.price_input, number(price.input_per_mtok), window, cx);
        set(
            &self.price_output,
            number(price.output_per_mtok),
            window,
            cx,
        );
        set(
            &self.price_cache,
            price
                .cache_read_per_mtok
                .map(|v| v.to_string())
                .unwrap_or_default(),
            window,
            cx,
        );
        cx.notify();
    }

    /// The preferences the form holds, or what is wrong with them.
    fn read(&self, cx: &App) -> Result<AssistantPreferences, String> {
        let text = |state: &Entity<InputState>| state.read(cx).value().trim().to_owned();
        let mut preferences = (*preferences(cx)).clone();
        preferences.provider = self.provider;
        preferences.review = self.review;
        preferences.queries = self.queries;
        preferences.model = text(&self.model);
        let endpoint = text(&self.endpoint);
        preferences.endpoint = (!endpoint.is_empty()).then_some(endpoint);
        let number = |state: &Entity<InputState>| -> Result<Option<f64>, String> {
            let value = text(state).replace(',', ".");
            if value.is_empty() {
                return Ok(None);
            }
            value
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(Some)
                .ok_or_else(|| tr!("Prices are positive numbers per million tokens").to_owned())
        };
        let (input, output, cache) = (
            number(&self.price_input)?,
            number(&self.price_output)?,
            number(&self.price_cache)?,
        );
        if !preferences.model.is_empty() {
            if input.is_some() || output.is_some() {
                preferences.prices.insert(
                    preferences.model.clone(),
                    Price {
                        input_per_mtok: input.unwrap_or(0.0),
                        output_per_mtok: output.unwrap_or(0.0),
                        cache_read_per_mtok: cache,
                        cache_write_per_mtok: None,
                    },
                );
            } else {
                preferences.prices.remove(&preferences.model);
            }
        }
        if let Some(config) = preferences.config() {
            config.validate().map_err(|error| error.message)?;
        } else if preferences.provider.is_some() {
            return Err(tr!("Enter the model name").to_owned());
        }
        Ok(preferences)
    }

    fn typed_key(&self, cx: &App) -> Option<SecretString> {
        let key = self.key.read(cx).value().trim().to_owned();
        (!key.is_empty()).then(|| SecretString::from(key))
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let preferences = match self.read(cx) {
            Ok(preferences) => preferences,
            Err(problem) => {
                self.status = Some(Feedback::new(Tone::Error, problem));
                cx.notify();
                return;
            }
        };
        let key = self.typed_key(cx).zip(preferences.provider);
        cx.set_global(AssistantSettings(Arc::new(preferences.clone())));
        self.key
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.status = Some(Feedback::new(Tone::Info, tr!("Saving…")));
        cx.notify();
        let dir = connection_store::data_dir();
        let ticket = assistant::PREFERENCE_WRITES.ask();
        cx.spawn(async move |weak, cx| {
            let saved = backend::run(async move {
                if let Some((key, kind)) = key {
                    assistant::save_key(kind, key).await?;
                }
                assistant::save_preferences(&dir, preferences, ticket).await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                this.status = Some(match saved {
                    Ok(()) => Feedback::new(Tone::Success, tr!("Assistant preferences saved.")),
                    Err(error) => Feedback::new(
                        Tone::Error,
                        tr!("Could not save the assistant preferences: {0}", error),
                    ),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn remove_key(&mut self, cx: &mut Context<Self>) {
        let Some(kind) = self.provider else {
            return;
        };
        cx.spawn(async move |weak, cx| {
            let removed = backend::run(async move { assistant::remove_key(kind).await }).await;
            let _ = weak.update(cx, |this, cx| {
                this.status = Some(match removed {
                    Ok(()) => {
                        Feedback::new(Tone::Success, tr!("API key removed from the system vault."))
                    }
                    Err(error) => {
                        Feedback::new(Tone::Error, tr!("Could not remove the API key: {0}", error))
                    }
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Asks the provider a tiny question with the form's settings.
    fn test(&mut self, cx: &mut Context<Self>) {
        if let Some((cancel, _)) = self.testing.take() {
            cancel.cancel();
            self.status = Some(Feedback::new(Tone::Info, tr!("Test cancelled.")));
            cx.notify();
            return;
        }
        let config = match self.read(cx).map(|preferences| preferences.config()) {
            Ok(Some(config)) => config,
            Ok(None) => {
                self.status = Some(Feedback::new(
                    Tone::Warning,
                    tr!("Choose a provider and a model first."),
                ));
                cx.notify();
                return;
            }
            Err(problem) => {
                self.status = Some(Feedback::new(Tone::Error, problem));
                cx.notify();
                return;
            }
        };
        let typed = self.typed_key(cx);
        let cancel = CancellationToken::new();
        self.testing = Some((cancel.clone(), Instant::now()));
        self.status = Some(Feedback::new(Tone::Info, tr!("Testing the assistant…")));
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(async move {
                let key = match typed {
                    Some(key) => Some(key),
                    None if config.kind.key_use() != KeyUse::None => {
                        assistant::read_key(config.kind).await?
                    }
                    None => None,
                };
                let model = Assistant::new(config, key, Timeouts::default())?;
                model.check(&cancel).await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                let Some((_, started)) = this.testing.take() else {
                    return;
                };
                let seconds = started.elapsed().as_secs_f32();
                this.status = Some(match result {
                    Ok(()) => Feedback::new(
                        Tone::Success,
                        tr!(
                            "The assistant answered in {0} s, with tools.",
                            format!("{seconds:.1}")
                        ),
                    ),
                    Err(error) if error.code == ErrorCode::Cancelled => {
                        Feedback::new(Tone::Info, tr!("Test cancelled."))
                    }
                    Err(error) => Feedback::new(Tone::Error, test_failure(&error)),
                });
                cx.notify();
            });
        })
        .detach();
    }
}

fn test_failure(error: &DriverError) -> String {
    match error.code {
        ErrorCode::Validation => tr!(
            "The provider rejected the request. The model may not exist or may not support tools: {0}",
            error
        ),
        _ => tr!("The assistant could not be reached: {0}", error),
    }
}

/// A segmented choice of the Preferences forms.
pub(super) fn choice(
    id: impl Into<ElementId>,
    text: &'static str,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(text)
        .focusable()
        .tab_index(0)
        .px_2()
        .h(px(28.))
        .flex()
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(BORDER))
        .text_size(px(12.))
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(HOVER)))
        .focus_visible(|s| s.border_color(rgb(MINT)))
        .when(selected, |s| {
            s.bg(rgb(0x263b37))
                .border_color(rgb(MINT))
                .text_color(rgb(TEXT))
        })
        .child(text)
}

impl Render for AssistantSettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let provider = self.provider;
        let needs_endpoint = provider == Some(ProviderKind::OpenAiCompatible);
        let key_use = provider.map(ProviderKind::key_use).unwrap_or(KeyUse::None);
        let field = |title: &'static str, state: &Entity<InputState>| {
            column()
                .gap_1()
                .child(label(title))
                .child(Input::new(state).small().aria_label(title))
        };
        // Its own height inside the scrolling Preferences column: an
        // embedded view would otherwise shrink and overlap what follows.
        column()
            .flex_none()
            .w_full()
            .gap_2()
            .child(label(tr!("Provider")))
            .child(row().flex_wrap().gap_1().children(
                std::iter::once(None)
                    .chain(ProviderKind::ALL.into_iter().map(Some))
                    .enumerate()
                    .map(|(index, kind)| {
                        choice(("assistant-provider", index), provider_name(kind), provider == kind)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.provider = kind;
                                cx.notify();
                            }))
                    }),
            ))
            .when(provider.is_some(), |s| {
                s.child(field(tr!("Model"), &self.model))
                    .child(field(
                        if needs_endpoint { tr!("Server address") } else { tr!("Server address (optional)") },
                        &self.endpoint,
                    ))
                    .when(key_use != KeyUse::None, |s| {
                        s.child(field(tr!("API key"), &self.key)).child(label(tr!(
                            "The key is kept in the system vault and is never shown again."
                        )))
                    })
                    .child(label(tr!("Review before sending")))
                    .child(row().flex_wrap().gap_1().children(ReviewMode::ALL.into_iter().enumerate().map(
                        |(index, mode)| {
                            choice(("assistant-review", index), review_name(mode), self.review == mode)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.review = mode;
                                    cx.notify();
                                }))
                        },
                    )))
                    .child(label(tr!("Queries the assistant asks to run")))
                    .child(row().flex_wrap().gap_1().children(
                        [(true, tr!("Ask me each time")), (false, tr!("Off"))].into_iter().enumerate().map(
                            |(index, (allowed, title))| {
                                choice(("assistant-queries", index), title, self.queries == allowed)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.queries = allowed;
                                        cx.notify();
                                    }))
                            },
                        ),
                    ))
                    .child(label(tr!(
                        "You approve each query and each result. At most 50 rows are sent, and you can hide columns."
                    )))
                    .child(label(tr!("Price per million tokens (optional, for the cost shown)")))
                    .child(
                        row()
                            .gap_1()
                            .child(div().flex_1().child(Input::new(&self.price_input).small().aria_label(tr!("Input price"))))
                            .child(div().flex_1().child(Input::new(&self.price_output).small().aria_label(tr!("Output price"))))
                            .child(div().flex_1().child(Input::new(&self.price_cache).small().aria_label(tr!("Cached input price")))),
                    )
            })
            .child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(primary_button("assistant-save", I::Check, tr!("Save")).on_click(
                        cx.listener(|this, _, window, cx| this.save(window, cx)),
                    ))
                    .when(provider.is_some(), |s| {
                        s.child(
                            button(
                                "assistant-test",
                                I::Activity,
                                if self.testing.is_some() { tr!("Cancel test") } else { tr!("Test") },
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                        )
                    })
                    .when(key_use != KeyUse::None, |s| {
                        s.child(
                            button("assistant-remove-key", I::Trash, tr!("Remove key"))
                                .on_click(cx.listener(|this, _, _, cx| this.remove_key(cx))),
                        )
                    }),
            )
            .when_some(self.status.as_ref(), |s, status| {
                s.child(div().id("assistant-settings-status").role(Role::Status).child(feedback_line(status)))
            })
            .child(label(tr!(
                "Sent to the provider: your SQL, error messages, row counts and the structure read from the local cache (names, types, keys, defaults, comments). Never sent: values from results or parameters."
            )))
    }
}
