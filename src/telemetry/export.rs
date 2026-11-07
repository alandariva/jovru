//! The OTLP exporters and the batch processors around them.
//!
//! The SDK's batch processors run on threads of their own and drive each
//! export with a plain executor. The HTTP client and tonic need Tokio for
//! their I/O and timers, so every export is polled inside the context of the
//! application's only runtime ([`OnRuntime`]); no second runtime is created.
use super::{Headers, Protocol, TelemetryPreferences};
use crate::drivers::{DriverError, DriverResult, ErrorCode};
use crate::tr;
use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _, WithTonicConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::logs::{BatchLogProcessor, LogBatch, LogExporter};
use opentelemetry_sdk::trace::{BatchConfigBuilder, BatchSpanProcessor, SpanData, SpanExporter};
use reqwest_genai::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

/// How long one export (a batch of spans or records) may take.
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a batch is sent while it is not full.
const SCHEDULED_DELAY: Duration = Duration::from_secs(5);

/// Spans or records waiting to be sent; past it, new ones are dropped.
const QUEUE_SIZE: usize = 2048;

/// Spans or records sent in one request.
const BATCH_SIZE: usize = 512;

/// Exports polled inside the runtime's context.
#[derive(Debug)]
pub struct OnRuntime<E> {
    inner: E,
    runtime: tokio::runtime::Handle,
}

impl<E> OnRuntime<E> {
    pub fn new(inner: E, runtime: tokio::runtime::Handle) -> Self {
        Self { inner, runtime }
    }
}

/// A future that enters the runtime's context each time it is polled.
struct Entered<F> {
    runtime: tokio::runtime::Handle,
    future: Pin<Box<F>>,
}

impl<F: Future> Future for Entered<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let _context = this.runtime.enter();
        this.future.as_mut().poll(cx)
    }
}

impl<E: SpanExporter> SpanExporter for OnRuntime<E> {
    fn export(&self, batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
        Entered {
            runtime: self.runtime.clone(),
            future: Box::pin(self.inner.export(batch)),
        }
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        let _context = self.runtime.enter();
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        let _context = self.runtime.enter();
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: LogExporter> LogExporter for OnRuntime<E> {
    fn export(&self, batch: LogBatch<'_>) -> impl Future<Output = OTelSdkResult> + Send {
        Entered {
            runtime: self.runtime.clone(),
            future: Box::pin(self.inner.export(batch)),
        }
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        let _context = self.runtime.enter();
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// The headers as the HTTP client takes them, validated.
fn header_map(headers: Option<&Headers>) -> DriverResult<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers.map(Headers::pairs).unwrap_or_default() {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| DriverError::validation(tr!("A header name is not valid")))?;
        let value = HeaderValue::from_str(&value)
            .map_err(|_| DriverError::validation(tr!("A header value is not valid")))?;
        map.append(name, value);
    }
    Ok(map)
}

fn build_failure(error: opentelemetry_otlp::ExporterBuildError) -> DriverError {
    tracing::debug!(error = %error, "telemetry exporter not built");
    DriverError::validation(tr!("The collector address cannot be used"))
}

/// The HTTP client for OTLP over HTTP: TLS on ring with the system's trust
/// store, no redirects, a bounded connection time.
fn http_client() -> DriverResult<reqwest_genai::Client> {
    // The rest of the application already uses ring; another part of the
    // process may have installed it first.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest_genai::Client::builder()
        .connect_timeout(EXPORT_TIMEOUT)
        .timeout(EXPORT_TIMEOUT)
        .redirect(reqwest_genai::redirect::Policy::none())
        .build()
        .map_err(|_| {
            DriverError::new(
                ErrorCode::Internal,
                tr!("Could not prepare the telemetry HTTP client"),
            )
        })
}

/// The address of one signal: OTLP over HTTP adds the signal's path to the
/// collector's address; gRPC uses the address as it is.
fn signal_endpoint(preferences: &TelemetryPreferences, path: &str) -> String {
    let base = preferences.endpoint.trim().trim_end_matches('/');
    match preferences.protocol {
        Protocol::Http => format!("{base}{path}"),
        Protocol::Grpc => base.to_owned(),
    }
}

fn tls(endpoint: &str) -> Option<opentelemetry_otlp::tonic_types::transport::ClientTlsConfig> {
    endpoint.starts_with("https://").then(|| {
        opentelemetry_otlp::tonic_types::transport::ClientTlsConfig::new().with_native_roots()
    })
}

fn http_headers(map: &HeaderMap) -> HashMap<String, String> {
    map.iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

/// A span exporter for the preferences. gRPC needs the runtime's context
/// while it is built.
pub fn span_exporter(
    preferences: &TelemetryPreferences,
    headers: Option<&Headers>,
    retry: opentelemetry_otlp::RetryPolicy,
) -> DriverResult<opentelemetry_otlp::SpanExporter> {
    let map = header_map(headers)?;
    let endpoint = signal_endpoint(preferences, "/v1/traces");
    match preferences.protocol {
        Protocol::Http => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(endpoint)
            .with_timeout(EXPORT_TIMEOUT)
            .with_headers(http_headers(&map))
            .with_http_client(http_client()?)
            .with_retry_policy(retry)
            .build(),
        Protocol::Grpc => {
            let tls = tls(&endpoint);
            let builder = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_protocol(opentelemetry_otlp::Protocol::Grpc)
                .with_endpoint(endpoint)
                .with_timeout(EXPORT_TIMEOUT)
                .with_metadata(
                    opentelemetry_otlp::tonic_types::metadata::MetadataMap::from_headers(map),
                )
                .with_retry_policy(retry);
            match tls {
                Some(tls) => builder.with_tls_config(tls).build(),
                None => builder.build(),
            }
        }
    }
    .map_err(build_failure)
}

/// A log exporter for the preferences, as [`span_exporter`].
pub fn log_exporter(
    preferences: &TelemetryPreferences,
    headers: Option<&Headers>,
) -> DriverResult<opentelemetry_otlp::LogExporter> {
    let map = header_map(headers)?;
    let endpoint = signal_endpoint(preferences, "/v1/logs");
    let retry = opentelemetry_otlp::RetryPolicy::recommended();
    match preferences.protocol {
        Protocol::Http => opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .with_endpoint(endpoint)
            .with_timeout(EXPORT_TIMEOUT)
            .with_headers(http_headers(&map))
            .with_http_client(http_client()?)
            .with_retry_policy(retry)
            .build(),
        Protocol::Grpc => {
            let tls = tls(&endpoint);
            let builder = opentelemetry_otlp::LogExporter::builder()
                .with_tonic()
                .with_protocol(opentelemetry_otlp::Protocol::Grpc)
                .with_endpoint(endpoint)
                .with_timeout(EXPORT_TIMEOUT)
                .with_metadata(
                    opentelemetry_otlp::tonic_types::metadata::MetadataMap::from_headers(map),
                )
                .with_retry_policy(retry);
            match tls {
                Some(tls) => builder.with_tls_config(tls).build(),
                None => builder.build(),
            }
        }
    }
    .map_err(build_failure)
}

/// A batch span processor that exports on the runtime.
pub fn span_processor(
    preferences: &TelemetryPreferences,
    headers: Option<&Headers>,
    runtime: tokio::runtime::Handle,
    resource: &Resource,
) -> DriverResult<BatchSpanProcessor> {
    let _context = runtime.enter();
    let exporter = span_exporter(
        preferences,
        headers,
        opentelemetry_otlp::RetryPolicy::recommended(),
    )?;
    let mut processor = BatchSpanProcessor::builder(OnRuntime::new(exporter, runtime.clone()))
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(QUEUE_SIZE)
                .with_max_export_batch_size(BATCH_SIZE)
                .with_scheduled_delay(SCHEDULED_DELAY)
                .build(),
        )
        .build();
    opentelemetry_sdk::trace::SpanProcessor::set_resource(&mut processor, resource);
    Ok(processor)
}

/// A batch log processor that exports on the runtime.
pub fn log_processor(
    preferences: &TelemetryPreferences,
    headers: Option<&Headers>,
    runtime: tokio::runtime::Handle,
    resource: &Resource,
) -> DriverResult<BatchLogProcessor> {
    let _context = runtime.enter();
    let exporter = log_exporter(preferences, headers)?;
    let mut processor = BatchLogProcessor::builder(OnRuntime::new(exporter, runtime.clone()))
        .with_batch_config(
            opentelemetry_sdk::logs::BatchConfigBuilder::default()
                .with_max_queue_size(QUEUE_SIZE)
                .with_max_export_batch_size(BATCH_SIZE)
                .with_scheduled_delay(SCHEDULED_DELAY)
                .build(),
        )
        .build();
    opentelemetry_sdk::logs::LogProcessor::set_resource(&mut processor, resource);
    Ok(processor)
}

/// Turns an export failure into a classified error. The library reports
/// only text, so the HTTP status or gRPC code is read from it; the text
/// itself goes to the local log at `debug`, never to the user.
pub fn export_failure(error: &OTelSdkError) -> DriverError {
    tracing::debug!(error = %error, "telemetry export failed");
    let text = match error {
        OTelSdkError::Timeout(_) => {
            return DriverError::new(
                ErrorCode::Timeout,
                tr!("The collector did not answer in time."),
            );
        }
        OTelSdkError::InternalFailure(text) => text.as_str(),
        _ => "",
    };
    let has = |needle: &str| text.contains(needle);
    if has("status code: 401")
        || has("status code: 403")
        || has("Unauthenticated")
        || has("PermissionDenied")
    {
        DriverError::new(
            ErrorCode::Authentication,
            tr!("The collector refused the credentials. Check the headers."),
        )
    } else if has("DeadlineExceeded") || has("timed out") {
        DriverError::new(
            ErrorCode::Timeout,
            tr!("The collector did not answer in time."),
        )
    } else if has("status code: 404") || has("Unimplemented") {
        DriverError::new(
            ErrorCode::NotFound,
            tr!("The collector does not accept OTLP at this address and protocol."),
        )
    } else if has("status code:") || (has("gRPC code:") && !has("Unavailable")) {
        DriverError::new(ErrorCode::Protocol, tr!("The collector rejected the data."))
    } else {
        DriverError::new(
            ErrorCode::Connection,
            tr!("The collector could not be reached."),
        )
    }
}
