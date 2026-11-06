//! Optional export of logs and traces over OTLP
//! ([ADR-0029](../../docs/adr/0029-otlp-export-of-logs-and-traces.md)).
//!
//! Off by default. **Preferences › Diagnostics › Telemetry** turns logs and
//! traces on separately and names the collector (OTLP over HTTP/protobuf or
//! gRPC); headers for its authentication are kept in the system vault.
//!
//! - **Logs** are the events of the local log, as filtered by `JYKRE_LOG`,
//!   except `trace` (SQL and payloads), the frame probe, UI-thread timings
//!   below a warning and events with a field that may hold free text
//!   ([`filter::LOCAL_ONLY_FIELDS`]).
//! - **Traces** are Jykre's spans around external calls (databases, HTTP,
//!   the Databricks CLI, the AI provider, the vault, files), with only the
//!   attributes in [`attributes::ALLOWED`]: operation, database system and
//!   name, durations, row counts and error codes. Never SQL, values,
//!   credentials, hosts, paths or request bodies.
//!
//! [`install`] adds the bridges when logging starts. Until [`apply`] runs
//! with the preferences, what the bridges capture is held in memory (a
//! bounded amount), so the start of the application can be exported; when
//! telemetry is off, it is dropped.
pub mod attributes;
pub mod export;
pub mod filter;
pub mod switch;

use crate::drivers::{CancellationToken, DriverError, DriverResult, ErrorCode, SecretString};
use crate::tr;
use filter::{ExportGate, Signal};
use jykre_secrets::{ApiKeyAccount, ApiKeyStore, SystemSecretStore};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider, SpanExporter as _};
use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;
use switch::{LogSwitch, SpanSwitch, Switch};
use tracing_subscriber::Layer;
use tracing_subscriber::registry::LookupSpan;

/// A span around a call that leaves the process (a database, a server, a
/// program, the vault, a file): `client` kind, with room for how it ended
/// (`error.type` and an error status, recorded by
/// [`attributes::record_outcome`]). Fields after the name are added as in
/// `info_span!`; only those in [`attributes::ALLOWED`] are exported.
#[macro_export]
macro_rules! client_span {
    ($name:literal $(, $($fields:tt)+)?) => {
        ::tracing::info_span!(
            $name,
            otel.kind = "client",
            error.type = ::tracing::field::Empty,
            otel.status_code = ::tracing::field::Empty
            $(, $($fields)+)?
        )
    };
}

/// The setting the preferences are kept under in `config.db`.
pub const PREFERENCES_KEY: &str = "telemetry.preferences";

/// The service name collectors see.
pub const SERVICE_NAME: &str = "jykre-studio";

/// How long the exit waits for what is still queued.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// How long **Test** waits for the collector.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How OTLP reaches the collector.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Protocol {
    /// OTLP over HTTP with protobuf bodies (usually port 4318).
    #[default]
    Http,
    /// OTLP over gRPC (usually port 4317).
    Grpc,
}

impl Protocol {
    pub const ALL: [Self; 2] = [Self::Http, Self::Grpc];

    pub fn key(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Grpc => "grpc",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|protocol| protocol.key() == key)
    }
}

/// What the user chose in **Preferences › Telemetry**. The headers are not
/// here: they live in the system vault only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetryPreferences {
    pub logs: bool,
    pub traces: bool,
    pub protocol: Protocol,
    /// The collector's address: for HTTP the base the signal paths
    /// (`/v1/traces`, `/v1/logs`) are added to; for gRPC the address itself.
    pub endpoint: String,
}

impl TelemetryPreferences {
    /// Reads what [`Self::to_value`] wrote; anything missing or unknown
    /// takes its default (off).
    pub fn from_value(value: Option<&Value>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
        Self {
            logs: flag("logs"),
            traces: flag("traces"),
            protocol: value
                .get("protocol")
                .and_then(Value::as_str)
                .and_then(Protocol::from_key)
                .unwrap_or_default(),
            endpoint: value
                .get("endpoint")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned(),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "logs": self.logs,
            "traces": self.traces,
            "protocol": self.protocol.key(),
            "endpoint": self.endpoint,
        })
    }

    /// Whether anything is sent.
    pub fn sends(&self) -> bool {
        self.logs || self.traces
    }

    /// With a signal on, the collector's address must be usable.
    pub fn validate(&self) -> DriverResult<()> {
        if self.sends() {
            validate_endpoint(&self.endpoint)?;
        }
        Ok(())
    }
}

/// The collector's address is HTTPS, or plain HTTP only on this computer
/// (a local collector or agent), the same rule as the assistant's servers.
/// It never carries credentials, a query or a fragment: credentials go in
/// the headers, which the vault keeps.
pub fn validate_endpoint(endpoint: &str) -> DriverResult<()> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(DriverError::validation(tr!(
            "Enter the address of the OpenTelemetry collector"
        )));
    }
    let url = reqwest_genai::Url::parse(endpoint)
        .map_err(|_| DriverError::validation(tr!("The collector address is not a valid URL")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(DriverError::validation(tr!(
            "The collector address must not contain a user or password"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(DriverError::validation(tr!(
            "The collector address must not contain a query or fragment"
        )));
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback(&url) => Ok(()),
        "http" => Err(DriverError::validation(tr!(
            "Use HTTPS for a collector that is not on this computer"
        ))),
        _ => Err(DriverError::validation(tr!(
            "The collector address must start with https://"
        ))),
    }
}

fn is_loopback(url: &reqwest_genai::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Headers sent with every export, typically for authentication, written
/// `name=value` and separated by commas. Kept as a secret: only the vault
/// stores them and `Debug` never shows them.
#[derive(Clone)]
pub struct Headers(SecretString);

impl std::fmt::Debug for Headers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Headers(<redacted>)")
    }
}

impl PartialEq for Headers {
    fn eq(&self, other: &Self) -> bool {
        self.0.expose_secret() == other.0.expose_secret()
    }
}

impl Headers {
    /// Checks the text and keeps it. Each entry is `name=value`; the value
    /// may contain `=` but not a comma.
    pub fn parse(text: &str) -> DriverResult<Self> {
        let headers = Self(SecretString::from(text.trim().to_owned()));
        if headers.pairs().is_empty() {
            return Err(DriverError::validation(tr!(
                "Write each header as name=value, separated by commas"
            )));
        }
        for entry in text.split(',').filter(|entry| !entry.trim().is_empty()) {
            let Some((name, value)) = entry.split_once('=') else {
                return Err(DriverError::validation(tr!(
                    "Write each header as name=value, separated by commas"
                )));
            };
            let name = name.trim();
            let valid_name = !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
            if !valid_name {
                return Err(DriverError::validation(tr!("A header name is not valid")));
            }
            if value.trim().is_empty()
                || !value
                    .trim()
                    .bytes()
                    .all(|byte| (0x20..0x7f).contains(&byte))
            {
                return Err(DriverError::validation(tr!("A header value is not valid")));
            }
        }
        Ok(headers)
    }

    /// The name and value of each header. Exposes the secret: only the
    /// exporters call it.
    pub(crate) fn pairs(&self) -> Vec<(String, String)> {
        self.0
            .expose_secret()
            .split(',')
            .filter_map(|entry| entry.split_once('='))
            .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
            .filter(|(name, value)| !name.is_empty() && !value.is_empty())
            .collect()
    }

    fn secret(&self) -> SecretString {
        self.0.clone()
    }
}

/// The writes of the preferences: the last one asked for wins.
pub static PREFERENCE_WRITES: crate::connection_store::LatestWrite =
    crate::connection_store::LatestWrite::new();

/// Reads the preferences kept in the data folder `dir`.
pub async fn load_preferences(dir: &Path) -> DriverResult<TelemetryPreferences> {
    let repository = jykre_storage::ConfigRepository::open(dir).await?;
    let value = repository.setting(PREFERENCES_KEY).await;
    repository.close().await;
    Ok(TelemetryPreferences::from_value(value?.as_ref()))
}

/// Saves the preferences for the write `ticket` of [`PREFERENCE_WRITES`].
pub async fn save_preferences(
    dir: &Path,
    preferences: TelemetryPreferences,
    ticket: u64,
) -> DriverResult<()> {
    PREFERENCE_WRITES
        .run(ticket, async {
            let repository = jykre_storage::ConfigRepository::open(dir).await?;
            let result = repository
                .set_setting(PREFERENCES_KEY, &preferences.to_value())
                .await;
            repository.close().await;
            result
        })
        .await
        .unwrap_or(Ok(()))
}

const HEADERS_ACCOUNT: ApiKeyAccount = ApiKeyAccount::telemetry_headers();

/// The headers kept in the vault, if any.
pub async fn read_headers() -> DriverResult<Option<Headers>> {
    let secret = crate::connection_store::vault_within(
        crate::connection_store::VaultCall::new("telemetry_headers", "get"),
        crate::connection_store::VAULT_TIMEOUT,
        SystemSecretStore.get(HEADERS_ACCOUNT),
    )
    .await
    .map_err(vault_failure)?;
    Ok(secret.and_then(|secret| Headers::parse(secret.expose_secret()).ok()))
}

/// Keeps the headers in the vault.
pub async fn save_headers(headers: Headers) -> DriverResult<()> {
    crate::connection_store::vault_within(
        crate::connection_store::VaultCall::new("telemetry_headers", "set"),
        crate::connection_store::VAULT_TIMEOUT,
        SystemSecretStore.set(HEADERS_ACCOUNT, headers.secret()),
    )
    .await
    .map_err(vault_failure)
}

/// Removes the headers from the vault.
pub async fn remove_headers() -> DriverResult<()> {
    crate::connection_store::vault_within(
        crate::connection_store::VaultCall::new("telemetry_headers", "delete"),
        crate::connection_store::VAULT_TIMEOUT,
        SystemSecretStore.delete(HEADERS_ACCOUNT),
    )
    .await
    .map_err(vault_failure)
}

/// The vault's own messages speak of API keys and passwords; this one names
/// the headers. The code is kept.
fn vault_failure(error: DriverError) -> DriverError {
    DriverError::new(
        error.code,
        tr!("The system vault did not keep the collector headers. Try again, or leave them empty."),
    )
}

/// The `startup` span, from `main` until the main window is built.
static STARTUP: Mutex<Option<tracing::Span>> = Mutex::new(None);

/// Opens the `startup` span; [`startup_finished`] closes it.
pub fn startup_began() {
    let span = tracing::info_span!("startup");
    *STARTUP.lock().unwrap_or_else(PoisonError::into_inner) = Some(span);
}

/// Closes the `startup` span, once the main window is built.
pub fn startup_finished() {
    drop(
        STARTUP
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take(),
    );
}

/// What is sent now; [`apply`] rebuilds the exporters only when it changes.
#[derive(Debug, Clone, PartialEq)]
struct Applied {
    preferences: TelemetryPreferences,
    headers: Option<Headers>,
}

struct Installed {
    spans: SpanSwitch,
    logs: LogSwitch,
    resource: Resource,
    // Kept so the tracer and logger stay attached to their switches.
    _tracer_provider: SdkTracerProvider,
    _logger_provider: SdkLoggerProvider,
    applied: Mutex<Option<Applied>>,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// What collectors see of this process: the service, its version, an
/// identifier of this run, the system and the architecture. Never the
/// computer's or the user's name.
pub fn resource() -> Resource {
    Resource::builder_empty()
        .with_service_name(SERVICE_NAME)
        .with_attributes([
            opentelemetry::KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            opentelemetry::KeyValue::new("service.instance.id", uuid::Uuid::new_v4().to_string()),
            opentelemetry::KeyValue::new("os.type", std::env::consts::OS),
            opentelemetry::KeyValue::new("host.arch", std::env::consts::ARCH),
        ])
        .build()
}

fn installed() -> &'static Installed {
    INSTALLED.get_or_init(|| {
        let resource = resource();
        let spans = SpanSwitch(Arc::new(Switch::new()));
        let logs = LogSwitch::new();
        let tracer_provider = SdkTracerProvider::builder()
            .with_sampler(Sampler::AlwaysOn)
            .with_span_processor(spans.clone())
            .with_resource(resource.clone())
            .build();
        let logger_provider = SdkLoggerProvider::builder()
            .with_log_processor(logs.clone())
            .with_resource(resource.clone())
            .build();
        logs.attach(&logger_provider);
        Installed {
            spans,
            logs,
            resource,
            _tracer_provider: tracer_provider,
            _logger_provider: logger_provider,
            applied: Mutex::new(None),
        }
    })
}

/// The bridges from `tracing` to OpenTelemetry, each behind its
/// [`ExportGate`]: spans become traces, events become logs (filtered like
/// the local log by `local`). Added once, when logging starts.
pub fn install<S>(local: tracing_subscriber::EnvFilter) -> impl Layer<S> + Send + Sync
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    let installed = installed();
    bridges(
        &installed._tracer_provider,
        &installed._logger_provider,
        (&installed.spans, &installed.logs),
        local,
    )
}

/// The two bridges over the given providers and switches.
fn bridges<S>(
    tracer_provider: &SdkTracerProvider,
    logger_provider: &SdkLoggerProvider,
    (spans, logs): (&SpanSwitch, &LogSwitch),
    local: tracing_subscriber::EnvFilter,
) -> impl Layer<S> + Send + Sync + use<S>
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    let traces = tracing_opentelemetry::layer()
        .with_tracer(tracer_provider.tracer("jykre"))
        .with_location(false)
        .with_threads(false)
        .with_target(false)
        .with_tracked_inactivity(false)
        .with_filter(ExportGate::new(Signal::Traces, spans.0.capturing()));
    let records =
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(logger_provider)
            .with_filter(tracing_subscriber::filter::FilterExt::and(
                local,
                ExportGate::new(Signal::Logs, logs.switch.capturing()),
            ));
    traces.and_then(records)
}

/// Shuts `processor` down on the blocking pool, so a collector that does not
/// answer never holds the caller.
fn retire<P: Send + 'static>(processor: Option<P>, shutdown: fn(P)) {
    if let Some(processor) = processor {
        drop(tokio::task::spawn_blocking(move || shutdown(processor)));
    }
}

/// Sends what the preferences ask for, from now on. Exporters are rebuilt
/// only when the preferences or the headers changed; a signal turned off
/// stops being captured at once (what was queued is still sent). Runs on
/// the database runtime. Without [`install`] (tests, the performance run)
/// it does nothing.
pub async fn apply(
    preferences: TelemetryPreferences,
    headers: Option<Headers>,
) -> DriverResult<()> {
    let Some(installed) = INSTALLED.get() else {
        return Ok(());
    };
    let wanted = Applied {
        preferences: preferences.clone(),
        headers: headers.clone(),
    };
    let mut applied = installed
        .applied
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if applied.as_ref() == Some(&wanted) && !installed.spans.0.pending() {
        return Ok(());
    }
    let runtime = tokio::runtime::Handle::current();
    let built = preferences.validate().and_then(|()| {
        let traces = preferences
            .traces
            .then(|| {
                export::span_processor(
                    &preferences,
                    headers.as_ref(),
                    runtime.clone(),
                    &installed.resource,
                )
            })
            .transpose()?;
        let logs = preferences
            .logs
            .then(|| {
                export::log_processor(
                    &preferences,
                    headers.as_ref(),
                    runtime.clone(),
                    &installed.resource,
                )
            })
            .transpose()?;
        Ok((traces, logs))
    });
    let (traces, logs, result) = match built {
        Ok((traces, logs)) => (traces, logs, Ok(())),
        Err(error) => (None, None, Err(error)),
    };
    let spans = installed
        .spans
        .activate(traces.map(|processor| Box::new(processor) as switch::SpanTarget));
    let records = installed
        .logs
        .activate(logs.map(|processor| Box::new(processor) as switch::LogTarget));
    let dropped = spans.dropped + records.dropped;
    retire(spans.previous, |processor| {
        let _ = processor.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
    });
    retire(records.previous, |processor| {
        let _ = processor.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
    });
    *applied = result.is_ok().then_some(wanted);
    drop(applied);
    tracing::info!(
        logs = preferences.logs && result.is_ok(),
        traces = preferences.traces && result.is_ok(),
        protocol = preferences.protocol.key(),
        dropped,
        "telemetry applied"
    );
    result
}

/// Sends what is still queued, within `timeout`, and stops capturing. Called
/// once, when the application exits.
pub fn shutdown(timeout: Duration) {
    let Some(installed) = INSTALLED.get() else {
        return;
    };
    if let Some(processor) = installed.spans.activate(None).previous {
        let _ = processor.shutdown_with_timeout(timeout);
    }
    if let Some(processor) = installed.logs.activate(None).previous {
        let _ = processor.shutdown_with_timeout(timeout);
    }
}

/// Sends one span named `telemetry.test` with the form's settings and
/// reports how the collector answered. Runs on the database runtime.
pub async fn test(
    preferences: TelemetryPreferences,
    headers: Option<Headers>,
    cancel: CancellationToken,
) -> DriverResult<()> {
    validate_endpoint(&preferences.endpoint)?;
    let mut exporter = export::span_exporter(
        &preferences,
        headers.as_ref(),
        opentelemetry_otlp::RetryPolicy::disabled(),
    )?;
    exporter.set_resource(&resource());
    let span = test_span();
    let sent = tokio::select! {
        sent = tokio::time::timeout(TEST_TIMEOUT + Duration::from_secs(2), exporter.export(vec![span])) => sent,
        () = cancel.cancelled() => return Err(DriverError::cancelled()),
    };
    match sent {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(export::export_failure(&error)),
        Err(_) => Err(DriverError::new(
            ErrorCode::Timeout,
            tr!("The collector did not answer in time."),
        )),
    }
}

fn test_span() -> opentelemetry_sdk::trace::SpanData {
    use opentelemetry::trace::{
        SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
    };
    let ids = uuid::Uuid::new_v4();
    let bytes = ids.as_bytes();
    let mut span_id = [0u8; 8];
    span_id.copy_from_slice(&bytes[..8]);
    let now = std::time::SystemTime::now();
    opentelemetry_sdk::trace::SpanData {
        span_context: SpanContext::new(
            TraceId::from_bytes(*bytes),
            SpanId::from_bytes(span_id),
            TraceFlags::SAMPLED,
            false,
            TraceState::default(),
        ),
        parent_span_id: SpanId::INVALID,
        parent_span_is_remote: false,
        span_kind: SpanKind::Internal,
        name: "telemetry.test".into(),
        start_time: now,
        end_time: now,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        events: opentelemetry_sdk::trace::SpanEvents::default(),
        links: opentelemetry_sdk::trace::SpanLinks::default(),
        status: Status::Unset,
        instrumentation_scope: opentelemetry::InstrumentationScope::builder("jykre").build(),
    }
}

/// Telemetry captured in memory, for tests: the same bridges, switches and
/// sanitizing as the application, with in-memory exporters behind them.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SimpleLogProcessor};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SimpleSpanProcessor};

    pub struct Capture {
        pub spans: InMemorySpanExporter,
        pub logs: InMemoryLogExporter,
        pub span_switch: SpanSwitch,
        pub log_switch: LogSwitch,
        // Dropping the last provider shuts the exporters down, which empties
        // them; the capture keeps them for as long as it is read.
        _providers: (SdkTracerProvider, SdkLoggerProvider),
    }

    impl Capture {
        /// Every attribute key and value exported, spans and logs, as text.
        pub fn exported_text(&self) -> String {
            let mut text = String::new();
            for span in self.spans.get_finished_spans().unwrap_or_default() {
                text.push_str(&format!("{} {:?} ", span.name, span.status));
                for pair in &span.attributes {
                    text.push_str(&format!("{}={} ", pair.key, pair.value));
                }
                text.push('\n');
            }
            for log in self.logs.get_emitted_logs().unwrap_or_default() {
                text.push_str(&format!("{:?} ", log.record.body()));
                for (key, value) in log.record.attributes_iter() {
                    text.push_str(&format!("{key}={value:?} "));
                }
                text.push('\n');
            }
            text
        }
    }

    /// A subscriber with both bridges at `local` (an `EnvFilter` directive),
    /// both signals active.
    pub fn capture(local: &str) -> (impl tracing::Subscriber + Send + Sync, Capture) {
        crate::logging::capture::install_global_default();
        let spans = InMemorySpanExporter::default();
        let logs = InMemoryLogExporter::default();
        let span_switch = SpanSwitch(Arc::new(Switch::new()));
        let log_switch = LogSwitch::new();
        span_switch.activate(Some(Box::new(SimpleSpanProcessor::new(spans.clone()))));
        log_switch.activate(Some(Box::new(SimpleLogProcessor::new(logs.clone()))));
        let tracer_provider = SdkTracerProvider::builder()
            .with_span_processor(span_switch.clone())
            .build();
        let logger_provider = SdkLoggerProvider::builder()
            .with_log_processor(log_switch.clone())
            .build();
        log_switch.attach(&logger_provider);
        let layer = bridges(
            &tracer_provider,
            &logger_provider,
            (&span_switch, &log_switch),
            tracing_subscriber::EnvFilter::new(local),
        );
        use tracing_subscriber::layer::SubscriberExt as _;
        let subscriber = tracing_subscriber::registry().with(layer);
        (
            subscriber,
            Capture {
                spans,
                logs,
                span_switch,
                log_switch,
                _providers: (tracer_provider, logger_provider),
            },
        )
    }
}

#[cfg(test)]
mod tests;
