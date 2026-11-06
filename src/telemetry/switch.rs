//! Processors installed once, when logging starts, whose destination is
//! chosen later: the preferences are read after the window opens, and the
//! user can turn a signal on or off at any time.
//!
//! Until the preferences are read the switch is **pending**: it keeps what
//! arrives, up to [`HELD_LIMIT`] items, so the start of the application is
//! exported too. Then it is **active** (forwarding to a batch processor) or
//! **off** (dropping everything, including what it held).
use super::attributes;
use opentelemetry::InstrumentationScope;
use opentelemetry::logs::{Logger as _, LoggerProvider as _};
use opentelemetry::trace::Status;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogProcessor, SdkLogRecord, SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::trace::{Span, SpanData, SpanEvents, SpanLinks, SpanProcessor};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

/// Spans or log records kept while the preferences are not read yet.
pub const HELD_LIMIT: usize = 1024;

enum Mode<P, T> {
    Pending { held: Vec<T>, dropped: usize },
    Active(P),
    Off,
}

/// Where one signal goes now.
pub struct Switch<P, T> {
    /// Read by the signal's filter: whether anything is captured at all.
    capturing: Arc<AtomicBool>,
    mode: RwLock<Mode<P, T>>,
}

/// What a switch let go of when it changed.
pub struct Released<P, T> {
    /// The processor it used before, to be shut down off the caller's thread.
    pub previous: Option<P>,
    /// What it held while pending.
    pub held: Vec<T>,
    /// How many items it dropped while pending, past [`HELD_LIMIT`].
    pub dropped: usize,
}

impl<P, T> Switch<P, T> {
    pub fn new() -> Self {
        Self {
            capturing: Arc::new(AtomicBool::new(true)),
            mode: RwLock::new(Mode::Pending {
                held: Vec::new(),
                dropped: 0,
            }),
        }
    }

    pub fn capturing(&self) -> Arc<AtomicBool> {
        self.capturing.clone()
    }

    fn read(&self) -> RwLockReadGuard<'_, Mode<P, T>> {
        self.mode.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Mode<P, T>> {
        self.mode.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Calls `work` with the active processor, if there is one.
    pub fn with_active<R>(&self, work: impl FnOnce(&P) -> R) -> Option<R> {
        match &*self.read() {
            Mode::Active(processor) => Some(work(processor)),
            Mode::Pending { .. } | Mode::Off => None,
        }
    }

    /// Hands `item` to the active processor, keeps it while pending, or
    /// drops it when off.
    pub fn route(&self, item: impl FnOnce() -> T, forward: impl FnOnce(&P, T)) {
        if let Mode::Active(processor) = &*self.read() {
            return forward(processor, item());
        }
        match &mut *self.write() {
            Mode::Pending { held, dropped } => {
                if held.len() < HELD_LIMIT {
                    held.push(item());
                } else {
                    *dropped += 1;
                }
            }
            Mode::Active(processor) => forward(processor, item()),
            Mode::Off => {}
        }
    }

    /// Makes `next` the destination (`None` turns the signal off) and
    /// returns what the switch let go of.
    pub fn set(&self, next: Option<P>) -> Released<P, T> {
        let mut mode = self.write();
        self.capturing.store(next.is_some(), Ordering::Relaxed);
        let next = match next {
            Some(processor) => Mode::Active(processor),
            None => Mode::Off,
        };
        match std::mem::replace(&mut *mode, next) {
            Mode::Pending { held, dropped } => Released {
                previous: None,
                held,
                dropped,
            },
            Mode::Active(processor) => Released {
                previous: Some(processor),
                held: Vec::new(),
                dropped: 0,
            },
            Mode::Off => Released {
                previous: None,
                held: Vec::new(),
                dropped: 0,
            },
        }
    }

    /// Whether the switch still waits for the preferences.
    pub fn pending(&self) -> bool {
        matches!(&*self.read(), Mode::Pending { .. })
    }
}

impl<P, T> Default for Switch<P, T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Removes from a span what may not leave the computer: attributes outside
/// [`attributes::ALLOWED`], events, links and the status description.
pub fn sanitize(span: &mut SpanData) {
    span.attributes
        .retain(|pair| attributes::allowed(pair.key.as_str()));
    span.events = SpanEvents::default();
    span.links = SpanLinks::default();
    if matches!(span.status, Status::Error { .. }) {
        span.status = Status::error("");
    }
}

pub type SpanTarget = Box<dyn SpanProcessor>;

/// The tracer provider's only processor.
#[derive(Clone)]
pub struct SpanSwitch(pub Arc<Switch<SpanTarget, SpanData>>);

impl std::fmt::Debug for SpanSwitch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SpanSwitch")
    }
}

impl SpanSwitch {
    /// Sends `next` the spans held while pending and makes it the
    /// destination; returns the processor it replaces.
    pub fn activate(&self, next: Option<SpanTarget>) -> Released<SpanTarget, SpanData> {
        let mut released = self.0.set(next);
        let held = std::mem::take(&mut released.held);
        if !held.is_empty() {
            self.0.with_active(|processor| {
                for span in held {
                    processor.on_end(span);
                }
            });
        }
        released
    }
}

impl SpanProcessor for SpanSwitch {
    fn on_start(&self, span: &mut Span, cx: &opentelemetry::Context) {
        self.0.with_active(|processor| processor.on_start(span, cx));
    }

    fn on_end(&self, mut span: SpanData) {
        sanitize(&mut span);
        self.0
            .route(|| span, |processor, span| processor.on_end(span));
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0
            .with_active(|processor| processor.force_flush())
            .unwrap_or(Ok(()))
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        match self.0.set(None).previous {
            Some(processor) => processor.shutdown_with_timeout(timeout),
            None => Ok(()),
        }
    }
}

pub type LogTarget = Box<dyn LogProcessor>;
pub type HeldLog = (SdkLogRecord, InstrumentationScope);

/// The logger provider's only processor. Each record is copied without what
/// may not leave ([`exportable`]) before it is held or forwarded.
#[derive(Clone)]
pub struct LogSwitch {
    pub switch: Arc<Switch<LogTarget, HeldLog>>,
    /// The provider's logger, used only to make the clean copies. It holds
    /// the provider, which holds this switch: they live as long as the
    /// process (or the test capture).
    factory: Arc<OnceLock<SdkLogger>>,
}

impl std::fmt::Debug for LogSwitch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LogSwitch")
    }
}

/// Whether a log attribute may leave: not where the code that logged lives
/// (`log.file`, `log.module_path` from the `log` crate bridge, `code.*`),
/// which for a dependency is a path on the computer that built it.
pub fn exportable(key: &str) -> bool {
    !matches!(key, "log.file" | "log.module_path") && !key.starts_with("code.")
}

/// Copies what may leave of `from` into `to`, a fresh record: times,
/// severity, body, target, trace and [`exportable`] attributes. The event
/// name is left out: the bridge names it after the source file and line.
pub fn copy_exportable(from: &SdkLogRecord, to: &mut SdkLogRecord) {
    use opentelemetry::logs::LogRecord as _;
    if let Some(timestamp) = from.timestamp() {
        to.set_timestamp(timestamp);
    }
    if let Some(timestamp) = from.observed_timestamp() {
        to.set_observed_timestamp(timestamp);
    }
    if let Some(severity) = from.severity_number() {
        to.set_severity_number(severity);
    }
    if let Some(text) = from.severity_text() {
        to.set_severity_text(text);
    }
    if let Some(body) = from.body() {
        to.set_body(body.clone());
    }
    if let Some(target) = from.target() {
        to.set_target(target.clone());
    }
    if let Some(context) = from.trace_context() {
        to.set_trace_context(context.trace_id, context.span_id, context.trace_flags);
    }
    for (key, value) in from.attributes_iter() {
        if exportable(key.as_str()) {
            to.add_attribute(key.clone(), value.clone());
        }
    }
}

impl LogSwitch {
    pub fn new() -> Self {
        Self {
            switch: Arc::new(Switch::new()),
            factory: Arc::new(OnceLock::new()),
        }
    }

    /// Gives the switch its provider's logger; until then nothing passes.
    pub fn attach(&self, provider: &SdkLoggerProvider) {
        let _ = self.factory.set(provider.logger("jykre"));
    }

    /// Sends `next` the records held while pending and makes it the
    /// destination; returns the processor it replaces.
    pub fn activate(&self, next: Option<LogTarget>) -> Released<LogTarget, HeldLog> {
        let mut released = self.switch.set(next);
        let held = std::mem::take(&mut released.held);
        if !held.is_empty() {
            self.switch.with_active(|processor| {
                for (mut record, scope) in held {
                    processor.emit(&mut record, &scope);
                }
            });
        }
        released
    }
}

impl Default for LogSwitch {
    fn default() -> Self {
        Self::new()
    }
}

impl LogProcessor for LogSwitch {
    fn emit(&self, record: &mut SdkLogRecord, scope: &InstrumentationScope) {
        let Some(factory) = self.factory.get() else {
            return;
        };
        let mut clean = factory.create_log_record();
        copy_exportable(record, &mut clean);
        if self
            .switch
            .with_active(|processor| processor.emit(&mut clean, scope))
            .is_some()
        {
            return;
        }
        self.switch.route(
            || (clean, scope.clone()),
            |processor, (mut record, scope)| processor.emit(&mut record, &scope),
        );
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.switch
            .with_active(|processor| processor.force_flush())
            .unwrap_or(Ok(()))
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        match self.switch.set(None).previous {
            Some(processor) => processor.shutdown_with_timeout(timeout),
            None => Ok(()),
        }
    }
}
