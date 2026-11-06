//! **Preferences › Diagnostics › Telemetry** (ADR-0029): whether logs and
//! traces go to an OpenTelemetry collector, which protocol reaches it, its
//! address, and headers for its authentication (kept in the system vault,
//! never shown back). Saving applies the choice at once.
use super::assistant_settings::choice;
use super::ui::*;
use gpui_kit::assets::IconName as I;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::{prelude::FluentBuilder as _, *};
use jykre::drivers::{CancellationToken, DriverError, ErrorCode};
use jykre::telemetry::{self, Headers, Protocol, TelemetryPreferences};
use jykre::{backend, connection_store, tr};
use std::sync::Arc;
use std::time::Instant;

/// The telemetry preferences of the window; the defaults (off) until read.
#[derive(Clone, Debug, Default)]
pub(super) struct TelemetrySettings(pub Arc<TelemetryPreferences>);

impl Global for TelemetrySettings {}

/// Reads the preferences at start and applies them, which also ends what
/// telemetry held while it waited. A failure leaves telemetry off and is
/// logged; the rest of the application does not depend on it.
pub(super) fn load(cx: &mut App) {
    let dir = connection_store::data_dir();
    cx.spawn(async move |cx| {
        let read = backend::run(async move {
            let preferences = match telemetry::load_preferences(&dir).await {
                Ok(preferences) => preferences,
                Err(error) => {
                    // Off, so what was held is dropped.
                    let _ = telemetry::apply(TelemetryPreferences::default(), None).await;
                    return Err(error);
                }
            };
            let headers = if preferences.sends() {
                telemetry::read_headers().await.unwrap_or_else(|error| {
                    tracing::warn!(error_code = ?error.code, "telemetry headers unreadable");
                    None
                })
            } else {
                None
            };
            if let Err(error) = telemetry::apply(preferences.clone(), headers).await {
                tracing::warn!(error_code = ?error.code, "telemetry not started");
            }
            Ok(preferences)
        })
        .await;
        match read {
            Ok(preferences) => {
                cx.update(|cx| cx.set_global(TelemetrySettings(Arc::new(preferences))));
            }
            Err(error) => {
                tracing::warn!(error_code = ?error.code, "telemetry preferences unreadable");
            }
        }
    })
    .detach();
}

fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Http => tr!("HTTP/protobuf"),
        Protocol::Grpc => "gRPC",
    }
}

/// A check box of the form.
fn toggle(id: &'static str, on: bool, title: &'static str) -> Stateful<Div> {
    button(id, if on { I::SquareCheck } else { I::Square }, title)
        .role(Role::CheckBox)
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .when(on, |s| s.text_color(rgb(MINT)))
}

/// The form of the telemetry preferences.
pub(super) struct TelemetrySettingsView {
    logs: bool,
    traces: bool,
    protocol: Protocol,
    endpoint: Entity<InputState>,
    headers: Entity<InputState>,
    status: Option<Feedback>,
    testing: Option<(CancellationToken, Instant)>,
    /// Whether the form shows what the global holds; it is filled once.
    filled: bool,
    _subscriptions: Vec<Subscription>,
}

impl TelemetrySettingsView {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let endpoint = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr!("https://collector.example.com:4318"))
        });
        let headers = cx.new(|cx| {
            InputState::new(window, cx).masked(true).placeholder(tr!(
                "name=value, separated by commas; empty keeps the saved ones"
            ))
        });
        let observation = cx.observe_global_in::<TelemetrySettings>(window, |this, window, cx| {
            if !this.filled {
                this.fill(window, cx);
            }
        });
        let mut view = Self {
            logs: false,
            traces: false,
            protocol: Protocol::default(),
            endpoint,
            headers,
            status: None,
            testing: None,
            filled: false,
            _subscriptions: vec![observation],
        };
        view.fill(window, cx);
        view
    }

    fn fill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = cx.try_global::<TelemetrySettings>().map(|s| s.0.clone()) else {
            return;
        };
        self.filled = true;
        self.logs = settings.logs;
        self.traces = settings.traces;
        self.protocol = settings.protocol;
        let endpoint = settings.endpoint.clone();
        self.endpoint
            .update(cx, |input, cx| input.set_value(endpoint, window, cx));
        cx.notify();
    }

    /// The preferences the form holds, or what is wrong with them.
    fn read(&self, cx: &App) -> Result<TelemetryPreferences, String> {
        let preferences = TelemetryPreferences {
            logs: self.logs,
            traces: self.traces,
            protocol: self.protocol,
            endpoint: self.endpoint.read(cx).value().trim().to_owned(),
        };
        preferences.validate().map_err(|error| error.message)?;
        Ok(preferences)
    }

    /// The headers typed in the form, checked; `None` when the field is empty.
    fn typed_headers(&self, cx: &App) -> Result<Option<Headers>, String> {
        let text = self.headers.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return Ok(None);
        }
        Headers::parse(&text)
            .map(Some)
            .map_err(|error| error.message)
    }

    fn problem(&mut self, problem: String, cx: &mut Context<Self>) {
        self.status = Some(Feedback::new(Tone::Error, problem));
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (preferences, typed) = match self
            .read(cx)
            .and_then(|preferences| Ok((preferences, self.typed_headers(cx)?)))
        {
            Ok(read) => read,
            Err(problem) => return self.problem(problem, cx),
        };
        cx.set_global(TelemetrySettings(Arc::new(preferences.clone())));
        self.headers
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.status = Some(Feedback::new(Tone::Info, tr!("Saving…")));
        cx.notify();
        let dir = connection_store::data_dir();
        let ticket = telemetry::PREFERENCE_WRITES.ask();
        cx.spawn(async move |weak, cx| {
            let saved = backend::run(async move {
                let headers = match typed {
                    Some(headers) => {
                        telemetry::save_headers(headers.clone()).await?;
                        Some(headers)
                    }
                    None if preferences.sends() => telemetry::read_headers().await?,
                    None => None,
                };
                telemetry::save_preferences(&dir, preferences.clone(), ticket).await?;
                telemetry::apply(preferences.clone(), headers).await?;
                Ok(preferences.sends())
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                this.status = Some(match saved {
                    Ok(true) => Feedback::new(
                        Tone::Success,
                        tr!("Telemetry preferences saved. Sending to the collector."),
                    ),
                    Ok(false) => Feedback::new(
                        Tone::Success,
                        tr!("Telemetry preferences saved. Nothing is sent."),
                    ),
                    Err(error) => Feedback::new(
                        Tone::Error,
                        tr!(
                            "Could not save or apply the telemetry preferences: {0}",
                            error
                        ),
                    ),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn remove_headers(&mut self, cx: &mut Context<Self>) {
        let preferences = (*cx
            .try_global::<TelemetrySettings>()
            .map(|settings| settings.0.clone())
            .unwrap_or_default())
        .clone();
        cx.spawn(async move |weak, cx| {
            let removed = backend::run(async move {
                telemetry::remove_headers().await?;
                telemetry::apply(preferences, None).await
            })
            .await;
            let _ = weak.update(cx, |this, cx| {
                this.status = Some(match removed {
                    Ok(()) => {
                        Feedback::new(Tone::Success, tr!("Headers removed from the system vault."))
                    }
                    Err(error) => {
                        Feedback::new(Tone::Error, tr!("Could not remove the headers: {0}", error))
                    }
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Sends one test span to the collector with the form's settings.
    fn test(&mut self, cx: &mut Context<Self>) {
        if let Some((cancel, _)) = self.testing.take() {
            cancel.cancel();
            self.status = Some(Feedback::new(Tone::Info, tr!("Test cancelled.")));
            cx.notify();
            return;
        }
        let preferences = TelemetryPreferences {
            logs: self.logs,
            traces: self.traces,
            protocol: self.protocol,
            endpoint: self.endpoint.read(cx).value().trim().to_owned(),
        };
        if let Err(error) = telemetry::validate_endpoint(&preferences.endpoint) {
            return self.problem(error.message, cx);
        }
        let typed = match self.typed_headers(cx) {
            Ok(typed) => typed,
            Err(problem) => return self.problem(problem, cx),
        };
        let cancel = CancellationToken::new();
        self.testing = Some((cancel.clone(), Instant::now()));
        self.status = Some(Feedback::new(Tone::Info, tr!("Testing the collector…")));
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let result = backend::run(async move {
                let headers = match typed {
                    Some(headers) => Some(headers),
                    None => telemetry::read_headers().await?,
                };
                telemetry::test(preferences, headers, cancel).await
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
                            "The collector accepted a test span in {0} s.",
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
    tr!("The collector test failed: {0}", error)
}

impl Render for TelemetrySettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let field = |title: &'static str, state: &Entity<InputState>| {
            column()
                .gap_1()
                .child(label(title))
                .child(Input::new(state).small().aria_label(title))
        };
        let sends = self.logs || self.traces;
        // Its own height inside the scrolling Preferences column: an
        // embedded view would otherwise shrink and overlap what follows.
        column()
            .flex_none()
            .w_full()
            .gap_2()
            .child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(
                        toggle("telemetry-logs", self.logs, tr!("Send logs")).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.logs = !this.logs;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        toggle("telemetry-traces", self.traces, tr!("Send traces")).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.traces = !this.traces;
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(label(tr!("Protocol")))
            .child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .children(Protocol::ALL.into_iter().enumerate().map(|(index, protocol)| {
                        choice(
                            ("telemetry-protocol", index),
                            protocol_name(protocol),
                            self.protocol == protocol,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.protocol = protocol;
                            cx.notify();
                        }))
                    })),
            )
            .child(field(tr!("Collector address"), &self.endpoint))
            .child(field(tr!("Headers"), &self.headers))
            .child(label(tr!(
                "The headers are kept in the system vault and are never shown again."
            )))
            .child(
                row()
                    .flex_wrap()
                    .gap_1()
                    .child(
                        primary_button("telemetry-save", I::Check, tr!("Save"))
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    )
                    .child(
                        button(
                            "telemetry-test",
                            I::Activity,
                            if self.testing.is_some() {
                                tr!("Cancel test")
                            } else {
                                tr!("Test")
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                    )
                    .child(
                        button("telemetry-remove-headers", I::Trash, tr!("Remove headers"))
                            .on_click(cx.listener(|this, _, _, cx| this.remove_headers(cx))),
                    ),
            )
            .when_some(self.status.as_ref(), |s, status| {
                s.child(
                    div()
                        .id("telemetry-settings-status")
                        .role(Role::Status)
                        .child(feedback_line(status)),
                )
            })
            .when(sends, |s| {
                s.child(label(tr!(
                    "Sent: operation names, durations, row counts, database names and error codes, and the log without SQL. Never sent: SQL, values, credentials, file paths or connection settings."
                )))
            })
    }
}
