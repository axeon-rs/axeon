//! OpenTelemetry for [Axeon](https://docs.rs/axeon) apps: a server span
//! per request, W3C trace context in and out, and export over OTLP/HTTP.
//!
//! ```no_run
//! use axeon::Server;
//! use axeon_otel::{Telemetry, TraceRequests};
//! use tracing_subscriber::layer::SubscriberExt;
//! use tracing_subscriber::util::SubscriberInitExt;
//!
//! # fn run() -> Result<(), Box<dyn std::error::Error>> {
//! // Export to a collector; the endpoint defaults to
//! // $OTEL_EXPORTER_OTLP_ENDPOINT, else http://localhost:4318.
//! let telemetry = Telemetry::builder("vs-api")
//!     .endpoint("http://otel-collector:4318")
//!     .sample_ratio(0.2)
//!     .build()?;
//! // Send `tracing` spans to it, next to your usual log output.
//! tracing_subscriber::registry().with(telemetry.layer()).init();
//!
//! let mut app = Server::new();
//! app.middleware(TraceRequests::new()); // first, so it covers everything else
//! app.get("/v1/products", |_req| async { "[]" });
//!
//! // On shutdown, after the server stops:
//! telemetry.shutdown();
//! # Ok(()) }
//! ```
//!
//! # What gets traced
//!
//! [`TraceRequests`] opens one `SERVER` span per request, named
//! `GET /v1/products/:id` (the route pattern, not the path), with the
//! OpenTelemetry HTTP attributes: `http.request.method`, `http.route`,
//! `url.path`, `http.response.status_code`, `user_agent.original`,
//! `client.address`. A `traceparent` header from the caller makes it a
//! child of the caller's span; 5xx responses mark it as an error.
//!
//! Spans and events from `tracing` inside the handler nest under it, so
//! `#[tracing::instrument]` on your functions shows up in the trace.
//!
//! The middleware also points the request's `traceparent` at the new span,
//! so [axeon-client](https://docs.rs/axeon-client)'s `from_request`
//! propagation makes outbound calls (LLM, vendor APIs) its children, and
//! one onboarding can be followed across services.

#![warn(missing_docs)]

use axeon::middleware::{Middleware, MiddlewareResult, Next};
use axeon::{MatchedRoute, Request};
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use std::collections::HashMap;
use std::time::Duration;
use tracing::field::Empty;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

pub use opentelemetry;
pub use opentelemetry_sdk;
pub use tracing_opentelemetry;

/// What can go wrong setting up export.
#[derive(Debug)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OpenTelemetry setup failed: {}", self.0)
    }
}

impl std::error::Error for Error {}

/// Settings for [`Telemetry`], from [`Telemetry::builder`].
#[derive(Clone, Debug)]
pub struct Builder {
    service: String,
    endpoint: Option<String>,
    headers: HashMap<String, String>,
    timeout: Duration,
    ratio: f64,
    attributes: Vec<(String, String)>,
}

impl Builder {
    /// The OTLP/HTTP collector, e.g. `http://otel-collector:4318`
    /// (`/v1/traces` is added). Default: `$OTEL_EXPORTER_OTLP_ENDPOINT`,
    /// else `http://localhost:4318`. `https://` needs the `tls` feature.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// A header sent with every export, e.g. an API key for a hosted
    /// backend.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// How long one export may take. Default: 10 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The share of new traces to keep, from 0.0 to 1.0. A request whose
    /// caller sampled its trace is always kept (and one whose caller didn't,
    /// never), so traces stay whole across services. Default: 1.0.
    pub fn sample_ratio(mut self, ratio: f64) -> Self {
        self.ratio = ratio.clamp(0.0, 1.0);
        self
    }

    /// A resource attribute on every span, e.g. `deployment.environment`
    /// or `cloud.region`.
    pub fn attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.push((key.into(), value.into()));
        self
    }

    fn resource(&self) -> Resource {
        let attributes = self.attributes.iter().map(|(k, v)| KeyValue::new(k.clone(), v.clone()));
        Resource::builder().with_service_name(self.service.clone()).with_attributes(attributes).build()
    }

    fn sampler(&self) -> Sampler {
        Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(self.ratio)))
    }

    /// Builds the exporter (batched, on a background thread) and installs
    /// it as the global tracer provider, with the W3C `traceparent`
    /// propagator. Must run inside a Tokio runtime or before one starts.
    pub fn build(self) -> Result<Telemetry, Error> {
        let mut exporter = opentelemetry_otlp::SpanExporter::builder().with_http().with_timeout(self.timeout);
        if let Some(endpoint) = &self.endpoint {
            exporter = exporter.with_endpoint(traces_url(endpoint));
        }
        if !self.headers.is_empty() {
            exporter = exporter.with_headers(self.headers.clone());
        }
        let exporter = exporter.build().map_err(|e| Error(e.to_string()))?;
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_sampler(self.sampler())
            .with_resource(self.resource())
            .build();
        Ok(Telemetry::from_provider(provider))
    }
}

/// The traces URL for a collector base URL: `/v1/traces` unless a path is
/// already given.
fn traces_url(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    let path = trimmed.split("://").nth(1).and_then(|rest| rest.find('/')).is_some();
    if path {
        trimmed.to_string()
    } else {
        format!("{}/v1/traces", trimmed)
    }
}

/// Span export: an OpenTelemetry tracer provider, installed globally, and
/// the `tracing` layer that feeds it.
#[derive(Clone, Debug)]
pub struct Telemetry {
    provider: SdkTracerProvider,
}

impl Telemetry {
    /// Settings for a service named `service` (`service.name`).
    pub fn builder(service: impl Into<String>) -> Builder {
        Builder {
            service: service.into(),
            endpoint: None,
            headers: HashMap::new(),
            timeout: Duration::from_secs(10),
            ratio: 1.0,
            attributes: Vec::new(),
        }
    }

    /// Uses a provider you built yourself (another exporter, custom
    /// processors), installing it globally with the W3C propagator.
    pub fn from_provider(provider: SdkTracerProvider) -> Self {
        opentelemetry::global::set_tracer_provider(provider.clone());
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        Telemetry { provider }
    }

    /// The `tracing` layer that turns spans into OpenTelemetry spans. Add
    /// it to your subscriber.
    pub fn layer<S>(&self) -> tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>
    where
        S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
    {
        tracing_opentelemetry::layer().with_tracer(self.provider.tracer("axeon")).with_tracked_inactivity(false)
    }

    /// The provider, e.g. to make tracers for manual spans.
    pub fn provider(&self) -> &SdkTracerProvider {
        &self.provider
    }

    /// Exports spans still waiting in the batch.
    pub fn flush(&self) {
        if let Err(error) = self.provider.force_flush() {
            tracing::warn!(%error, "couldn't flush spans");
        }
    }

    /// Exports what's left and stops the exporter. Call it once, after the
    /// server has stopped; spans ended later are dropped.
    pub fn shutdown(&self) {
        if let Err(error) = self.provider.shutdown() {
            tracing::warn!(%error, "couldn't shut the span exporter down cleanly");
        }
    }
}

/// Reads trace context from request headers.
struct HeaderExtractor<'a>(&'a axeon::http::HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Collects the headers a propagator writes.
#[derive(Default)]
struct Collect(Vec<(String, String)>);

impl Injector for Collect {
    fn set(&mut self, key: &str, value: String) {
        self.0.push((key.to_string(), value));
    }
}

/// Middleware that traces each request in a `SERVER` span; see the
/// [crate docs](crate). Works with whatever `tracing` subscriber is
/// installed; spans reach OpenTelemetry through [`Telemetry::layer`].
///
/// Add it first, so the span covers the other middleware too.
///
/// A request's `traceparent` is trusted: its trace and parent are continued,
/// and its sampling flag is followed (see
/// [`sample_ratio`](Builder::sample_ratio)). On a public endpoint that lets
/// any caller choose to have its requests traced, so strip `traceparent`
/// at your edge proxy for untrusted clients if tracing volume matters.
#[derive(Clone, Debug, Default)]
pub struct TraceRequests {
    _private: (),
}

impl TraceRequests {
    /// The middleware.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Middleware for TraceRequests {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        let propagator = TraceContextPropagator::new();
        let parent = propagator.extract(&HeaderExtractor(req.headers()));
        let method = req.method().as_str();
        let span = tracing::info_span!(
            "HTTP request",
            otel.name = method,
            otel.kind = "server",
            otel.status_code = Empty,
            http.request.method = method,
            http.route = Empty,
            url.path = req.path(),
            http.response.status_code = Empty,
            user_agent.original = req.get_header("user-agent").unwrap_or_default(),
            client.address = req.remote_addr().map(|a| a.ip().to_string()).unwrap_or_default(),
        );
        if parent.span().span_context().is_valid() {
            let _ = span.set_parent(parent);
        }
        // Outbound calls that forward this request's trace headers become
        // children of this span, not siblings of it.
        let context = span.context();
        if context.span().span_context().is_valid() {
            let mut headers = Collect::default();
            propagator.inject_context(&context, &mut headers);
            for (name, value) in headers.0 {
                if let (Ok(name), Ok(value)) =
                    (axeon::http::HeaderName::try_from(name), axeon::http::HeaderValue::try_from(value))
                {
                    req.headers_mut().insert(name, value);
                }
            }
        }
        let route = MatchedRoute::watch(&mut req);
        let response = next.handle(req).instrument(span.clone());
        Box::pin(async move {
            let result = response.await;
            if let Some(pattern) = route.get() {
                span.record("http.route", pattern);
                // The OpenTelemetry span has started (for the trace header
                // above), so rename it directly: recording `otel.name` now
                // would be ignored.
                span.context().span().update_name(format!("{} {}", method, pattern));
            }
            let status = match &result {
                Ok(response) => response.status,
                Err(error) => error.status_code(),
            };
            span.record("http.response.status_code", i64::from(status));
            if status >= 500 {
                span.record("otel.status_code", "ERROR");
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traces_urls() {
        assert_eq!(traces_url("http://collector:4318"), "http://collector:4318/v1/traces");
        assert_eq!(traces_url("http://collector:4318/"), "http://collector:4318/v1/traces");
        assert_eq!(traces_url("https://api.example.com/otlp/v1/traces"), "https://api.example.com/otlp/v1/traces");
    }
}
