//! The application's own UI-thread timings (`UiTimer`, target
//! `jykre::ui_thread`), collected in memory during a performance run so each
//! phase reports how long the production code took: a grid view rebuilt, a
//! run's status or a result published, a history page shown.
//!
//! The performance run installs [`Timings::layer`] as the only subscriber.
//! It listens to that one target and nothing else, writes no file and keeps
//! only operation names and durations, never a value.
use crate::probe::{Samples, Summary};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// The target `UiTimer` records under.
pub const TARGET: &str = "jykre::ui_thread";

/// Durations by operation, since they were last taken.
#[derive(Clone, Default)]
pub struct Timings(Arc<Mutex<BTreeMap<String, Samples>>>);

impl Timings {
    fn samples(&self) -> MutexGuard<'_, BTreeMap<String, Samples>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The layer that fills this collector.
    pub fn layer(&self) -> TimingLayer {
        TimingLayer(self.clone())
    }

    /// Makes the layer the process's only subscriber. False when another
    /// one was installed first.
    pub fn install(&self) -> bool {
        use tracing_subscriber::layer::SubscriberExt as _;
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(self.layer()))
            .is_ok()
    }

    pub fn record(&self, operation: &str, elapsed: Duration) {
        self.samples()
            .entry(operation.to_owned())
            .or_default()
            .push(elapsed);
    }

    /// Each operation's summary since the last call, and starts over.
    pub fn take(&self) -> BTreeMap<String, Summary> {
        std::mem::take(&mut *self.samples())
            .into_iter()
            .filter_map(|(operation, mut samples)| Some((operation, samples.take_summary()?)))
            .collect()
    }

    /// Drops what was collected.
    pub fn clear(&self) {
        self.samples().clear();
    }
}

/// Collects `UiTimer` events into [`Timings`].
pub struct TimingLayer(Timings);

fn ours(metadata: &Metadata<'_>) -> bool {
    metadata.target() == TARGET
}

impl<S: Subscriber> Layer<S> for TimingLayer {
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        if ours(metadata) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        ours(metadata)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::DEBUG)
    }

    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if !ours(event.metadata()) {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if let (Some(operation), Some(elapsed_ms)) = (fields.operation, fields.elapsed_ms)
            && elapsed_ms.is_finite()
            && elapsed_ms >= 0.0
        {
            self.0
                .record(&operation, Duration::from_secs_f64(elapsed_ms / 1000.0));
        }
    }
}

#[derive(Default)]
struct Fields {
    operation: Option<String>,
    elapsed_ms: Option<f64>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "operation" {
            self.operation = Some(value.to_owned());
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if field.name() == "elapsed_ms" {
            self.elapsed_ms = Some(value);
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

    #[test]
    fn ui_timer_events_are_collected_by_operation() {
        crate::logging::capture::install_global_default();
        let timings = Timings::default();
        let subscriber = tracing_subscriber::registry().with(timings.layer());
        tracing::subscriber::with_default(subscriber, || {
            drop(crate::logging::UiTimer::start("grid view"));
            drop(crate::logging::UiTimer::start("grid view"));
            drop(crate::logging::UiTimer::start("automation result"));
            // Other targets are not listened to.
            tracing::info!(operation = "grid view", elapsed_ms = 1.0, "other");
        });
        let taken = timings.take();
        assert_eq!(taken.get("grid view").map(|summary| summary.count), Some(2));
        assert_eq!(
            taken.get("automation result").map(|summary| summary.count),
            Some(1)
        );
        assert!(timings.take().is_empty(), "taking starts over");
        timings.record("x", Duration::from_millis(3));
        timings.clear();
        assert!(timings.take().is_empty());
    }
}
