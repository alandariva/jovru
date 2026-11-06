//! Which callsites reach each OpenTelemetry bridge. The decision per
//! callsite is static (level, target, field names), so a callsite neither
//! bridge wants costs nothing; the decision per event only reads whether the
//! signal is still captured.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{Level, Metadata, subscriber::Interest};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Filter};

/// Fields that may hold free text naming a statement, a file, a server or
/// what an error said. An event with any of them stays in the local log only.
pub const LOCAL_ONLY_FIELDS: &[&str] = &["sql", "file", "path", "url", "error", "account"];

/// Targets never exported as logs: the exporter's own stack (exporting what
/// it logs would feed itself) and the frame probe, which logs every 5 s.
const LOCAL_ONLY_TARGETS: &[&str] = &[
    "opentelemetry",
    "tracing_opentelemetry",
    "tonic",
    "h2",
    "hyper",
    "tower",
    "reqwest",
    "rustls",
    "jykre::probe",
];

/// The target of UI-thread timings (`logging::UiTimer`): one per publish,
/// grid rebuild or copy. Only its warnings (slow work) are exported.
pub const UI_THREAD_TARGET: &str = "jykre::ui_thread";

/// The signal a filter feeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Logs,
    Traces,
}

/// The per-layer filter in front of a bridge.
#[derive(Clone, Debug)]
pub struct ExportGate {
    signal: Signal,
    capturing: Arc<AtomicBool>,
}

impl ExportGate {
    pub fn new(signal: Signal, capturing: Arc<AtomicBool>) -> Self {
        Self { signal, capturing }
    }

    /// The static part of the decision: never `trace` (it carries SQL and
    /// payloads), spans only from Jykre's crates, and events without the
    /// fields and targets that stay local.
    pub fn admits(&self, metadata: &Metadata<'_>) -> bool {
        if *metadata.level() == Level::TRACE {
            return false;
        }
        let target = metadata.target();
        match self.signal {
            Signal::Traces => metadata.is_span() && target.starts_with("jykre"),
            Signal::Logs => {
                metadata.is_event()
                    && !LOCAL_ONLY_TARGETS
                        .iter()
                        .any(|local| target.starts_with(local))
                    && (target != UI_THREAD_TARGET || *metadata.level() <= Level::WARN)
                    && !metadata
                        .fields()
                        .iter()
                        .any(|field| LOCAL_ONLY_FIELDS.contains(&field.name()))
            }
        }
    }
}

impl<S> Filter<S> for ExportGate {
    fn enabled(&self, metadata: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        self.admits(metadata) && self.capturing.load(Ordering::Relaxed)
    }

    fn callsite_enabled(&self, metadata: &'static Metadata<'static>) -> Interest {
        if self.admits(metadata) {
            Interest::sometimes()
        } else {
            Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::DEBUG)
    }
}
