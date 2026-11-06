//! Structured logs and timings ([ADR-0009](../docs/adr/0009-structured-logging-and-timings.md)).
//!
//! The application calls [`init`] once at startup. Logs go to
//! `<data folder>/logs/` as JSON lines, one file per day (`jykre.<date>.log`),
//! keeping the last seven; debug builds also print readable lines to the
//! console. The default level is `info` for Jykre's crates and `warn` for
//! dependencies; `JYKRE_LOG` overrides it with `EnvFilter` syntax.
//!
//! Never log passwords, tokens, headers, connection URLs, cell values, query
//! parameters or SQL above `trace` ([Reliability](../docs/reliability.md#never-logged)).
//!
//! The same events, and Jykre's spans, can also be sent to an OpenTelemetry
//! collector when the user turns it on ([`crate::telemetry`], ADR-0029).
use crate::drivers::{DriverError, DriverResult, ErrorCode};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{Builder, Rotation},
};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

/// The environment variable that overrides the log filter.
pub const FILTER_VARIABLE: &str = "JYKRE_LOG";

/// `info` for Jykre's crates, `warn` for everything else, and nothing from
/// the AI provider library: some of its warnings carry streamed answers,
/// which are payloads (ADR-0028). The assistant logs its own rounds.
pub const DEFAULT_FILTER: &str = "warn,jykre=info,jykre_core=info,jykre_storage=info,\
jykre_secrets=info,jykre_driver_postgres=info,jykre_driver_mysql=info,\
jykre_driver_databricks=info,jykre_assistant=info,genai=off";

/// UI-thread work longer than this is logged as a warning.
pub const UI_THREAD_WARNING: Duration = Duration::from_millis(100);

/// Days of log files kept.
const KEPT_FILES: usize = 7;

/// Keeps the background log writer alive; dropping it sends what telemetry
/// still has queued (within [`crate::telemetry::SHUTDOWN_TIMEOUT`]) and
/// flushes pending lines.
#[must_use = "dropping the guard stops writing logs"]
pub struct LogGuard {
    _writer: WorkerGuard,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        crate::telemetry::shutdown(crate::telemetry::SHUTDOWN_TIMEOUT);
    }
}

/// The folder that holds the log files.
pub fn log_dir(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("logs")
}

/// The filter in effect: `JYKRE_LOG` when it is set and valid, otherwise
/// [`DEFAULT_FILTER`].
pub fn filter() -> EnvFilter {
    EnvFilter::try_from_env(FILTER_VARIABLE).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// Starts logging to `<data_dir>/logs` and records panics. Fails when the
/// folder cannot be created or another subscriber is already installed; the
/// application then runs without logs.
pub fn init(data_dir: &Path) -> DriverResult<LogGuard> {
    let appender = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("jykre")
        .filename_suffix("log")
        .max_log_files(KEPT_FILES)
        .build(log_dir(data_dir))
        .map_err(|_| DriverError::new(ErrorCode::Internal, "Could not open the log folder."))?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_writer(writer);
    // Debug builds keep a console; release builds on Windows have none.
    let console = cfg!(debug_assertions).then(|| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::io::stderr)
    });
    tracing_subscriber::registry()
        .with(file.and_then(console).with_filter(filter()))
        .with(crate::telemetry::install(filter()))
        .try_init()
        .map_err(|_| DriverError::new(ErrorCode::Internal, "Logging is already initialised."))?;
    install_panic_hook();
    Ok(LogGuard { _writer: guard })
}

/// Records every panic, with its location and a backtrace, before the
/// default hook runs. Panic messages come from the application's own code,
/// which never puts secrets in them.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_default();
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(%message, %location, %backtrace, "panic");
        previous(info);
    }));
}

/// Measures work on the UI thread and warns when it exceeds
/// [`UI_THREAD_WARNING`]. The time is also recorded at `debug`.
pub struct UiTimer {
    operation: &'static str,
    started: Instant,
}

impl UiTimer {
    pub fn start(operation: &'static str) -> Self {
        Self {
            operation,
            started: Instant::now(),
        }
    }
}

impl Drop for UiTimer {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        // A target of their own: telemetry exports only the warnings
        // (crate::telemetry::filter::UI_THREAD_TARGET).
        if elapsed > UI_THREAD_WARNING {
            tracing::warn!(
                target: "jykre::ui_thread",
                operation = self.operation,
                elapsed_ms,
                "slow UI-thread work"
            );
        } else {
            tracing::debug!(
                target: "jykre::ui_thread",
                operation = self.operation,
                elapsed_ms,
                "UI-thread work"
            );
        }
    }
}

/// Logs a finished operation: `debug` with its time, `warn` when it took
/// longer than `expected`. For operations that run off the UI thread.
pub fn timing(operation: &'static str, started: Instant, expected: Duration) {
    let elapsed = started.elapsed();
    let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
    if elapsed > expected {
        tracing::warn!(operation, elapsed_ms, "operation slower than expected");
    } else {
        tracing::debug!(operation, elapsed_ms, "operation finished");
    }
}

#[cfg(test)]
pub(crate) mod capture {
    //! Collects formatted log output in memory for tests.
    use std::sync::{Arc, Mutex, Once};
    use tracing::span::{Attributes, Id, Record};
    use tracing::subscriber::Interest;
    use tracing::{Event, Metadata};
    use tracing_subscriber::fmt::MakeWriter;

    /// The global default while tests run: it records nothing and answers
    /// `Interest::sometimes` for every callsite.
    ///
    /// `tracing-core` caches one interest per callsite for the whole process.
    /// While a single dispatcher is registered it asks the default of the
    /// thread that first reaches the callsite (`Dispatchers::rebuilder` in
    /// `tracing-core` 0.1.36 `callsite.rs`), so a parallel test without a
    /// capture would cache `never` from the empty global default, and the
    /// capturing test would lose that event. With this subscriber the answer
    /// is `sometimes`, and every event asks its own thread's dispatcher.
    struct AskEveryEvent;

    impl tracing::Subscriber for AskEveryEvent {
        fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
            Interest::sometimes()
        }
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            false
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, _: &Event<'_>) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    static GLOBAL_DEFAULT: Once = Once::new();

    #[derive(Clone, Default)]
    pub struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        pub fn text(&self) -> String {
            let bytes = self.0.lock().map(|bytes| bytes.clone()).unwrap_or_default();
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut bytes) = self.0.lock() {
                bytes.extend_from_slice(buf);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Installs [`AskEveryEvent`] as the global default, once.
    pub fn install_global_default() {
        GLOBAL_DEFAULT.call_once(|| {
            tracing::subscriber::set_global_default(AskEveryEvent)
                .expect("tests install no other global subscriber");
        });
    }

    /// A JSON subscriber at `level` writing into the returned buffer.
    pub fn subscriber(level: tracing::Level) -> (impl tracing::Subscriber + Send + Sync, Captured) {
        install_global_default();
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(level)
            .with_writer(captured.clone())
            .finish();
        (subscriber, captured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_filter_parses_and_names_every_jykre_crate() {
        assert!(EnvFilter::try_new(DEFAULT_FILTER).is_ok());
        for krate in [
            "jykre",
            "jykre_core",
            "jykre_storage",
            "jykre_secrets",
            "jykre_driver_postgres",
            "jykre_driver_mysql",
            "jykre_driver_databricks",
            "jykre_assistant",
        ] {
            assert!(DEFAULT_FILTER.contains(&format!("{krate}=info")), "{krate}");
        }
        assert!(DEFAULT_FILTER.contains("genai=off"));
    }

    #[test]
    fn slow_ui_work_is_a_warning() {
        let (subscriber, captured) = capture::subscriber(tracing::Level::DEBUG);
        tracing::subscriber::with_default(subscriber, || {
            drop(UiTimer::start("fast"));
            let mut slow = UiTimer::start("slow");
            slow.started -= Duration::from_millis(150);
            drop(slow);
            timing("cache read", Instant::now(), Duration::from_secs(1));
        });
        let text = captured.text();
        let line = |operation: &str| {
            text.lines()
                .find(|line| line.contains(&format!("\"operation\":\"{operation}\"")))
                .map(str::to_owned)
                .unwrap_or_default()
        };
        assert!(line("fast").contains("\"DEBUG\""), "{text}");
        assert!(line("slow").contains("\"WARN\""), "{text}");
        assert!(line("cache read").contains("\"DEBUG\""), "{text}");
        assert!(line("slow").contains("elapsed_ms"), "{text}");
    }

    #[test]
    fn a_capture_sees_events_another_thread_reached_first() {
        fn emit() {
            tracing::info!("reached first by another thread");
        }
        let (subscriber, captured) = capture::subscriber(tracing::Level::INFO);
        tracing::subscriber::with_default(subscriber, || {
            std::thread::spawn(emit).join().unwrap();
            emit();
        });
        let text = captured.text();
        assert_eq!(
            text.matches("reached first by another thread").count(),
            1,
            "{text}"
        );
    }
}
