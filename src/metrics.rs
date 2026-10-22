//! Prometheus metrics for HTTP traffic and the application.
//!
//! [`Metrics`] is middleware that records, per method, route pattern and
//! status:
//!
//! - `http_requests_total` (counter)
//! - `http_request_duration_seconds` (histogram)
//! - `http_requests_in_flight` (gauge)
//!
//! Routes are labelled by pattern (`/users/:id`), not by raw path, so the
//! number of series stays bounded; unmatched requests are labelled
//! `unmatched`. Serve the metrics with [`Metrics::router`]:
//!
//! ```rust
//! use axeon::Server;
//! use axeon::metrics::Metrics;
//!
//! let metrics = Metrics::new();
//! let mut app = Server::new();
//! app.middleware(metrics.clone());
//! app.mount("/", metrics.router()); // GET /metrics
//! ```
//!
//! # Application metrics
//!
//! Register counters, gauges and histograms on the same registry, and they
//! are served on the same endpoint:
//!
//! ```rust
//! use axeon::metrics::{Metrics, DEFAULT_BUCKETS};
//!
//! let metrics = Metrics::new();
//! let tokens = metrics.counter_vec("llm_tokens_total", "LLM tokens used.", &["model"]);
//! let depth = metrics.gauge_vec("queue_depth", "Jobs waiting.", &["queue"]);
//! let vendor = metrics.histogram_vec("vendor_seconds", "Vendor call duration.", &["vendor"], &DEFAULT_BUCKETS);
//!
//! tokens.with(&["qwen-max"]).inc_by(812.0);
//! depth.with(&["onboarding"]).set(17.0);
//! let timer = vendor.with(&["kyb"]).start_timer();
//! // ... call the vendor ...
//! timer.observe();
//!
//! let text = metrics.render();
//! assert!(text.contains(r#"llm_tokens_total{model="qwen-max"} 812"#));
//! assert!(text.contains(r#"vendor_seconds_count{vendor="kyb"} 1"#));
//! ```
//!
//! Handles are cheap to clone and update with atomics, so keep them in
//! application state. Each distinct set of label values is a series kept
//! for the life of the process: use labels from a small, fixed set.
//!
//! Keep `/metrics` off the public internet, e.g. by mounting it on an
//! internal port or protecting it with [`BasicAuth`](crate::auth::BasicAuth).

use crate::message::{Method, Request, Response};
use crate::middleware::{Middleware, MiddlewareResult, Next};
use crate::router::{MatchedRoute, Router};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

mod custom;

pub use custom::{Counter, CounterVec, Gauge, GaugeVec, Histogram, HistogramVec, Timer};

/// Histogram bucket upper bounds in seconds, from 5 ms to 10 s, used for
/// HTTP request durations and suited to other request-like durations.
pub const DEFAULT_BUCKETS: [f64; 11] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];
const BUCKETS: [f64; 11] = DEFAULT_BUCKETS;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    method: Method,
    route: Arc<str>,
    status: u16,
}

#[derive(Default)]
struct Series {
    count: AtomicU64,
    /// Sum of durations, in microseconds.
    sum_micros: AtomicU64,
    buckets: [AtomicU64; BUCKETS.len()],
}

#[derive(Default)]
struct Registry {
    series: RwLock<HashMap<Key, Arc<Series>>>,
    in_flight: AtomicI64,
    /// Application metrics, in registration order.
    custom: RwLock<Vec<Arc<custom::Family>>>,
}

/// HTTP metrics middleware and its `/metrics` endpoint. Clones share data.
#[derive(Clone, Default)]
pub struct Metrics {
    registry: Arc<Registry>,
}

impl Metrics {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, key: Key, seconds: f64) {
        // Fast path: an existing series under a read lock, updated with atomics.
        let existing = self.registry.series.read().unwrap_or_else(|e| e.into_inner()).get(&key).cloned();
        let series = match existing {
            Some(series) => series,
            None => Arc::clone(
                self.registry.series.write().unwrap_or_else(|e| e.into_inner()).entry(key).or_default(),
            ),
        };
        series.count.fetch_add(1, Ordering::Relaxed);
        series.sum_micros.fetch_add((seconds * 1e6) as u64, Ordering::Relaxed);
        if let Some(bucket) = BUCKETS.iter().position(|bound| seconds <= *bound) {
            series.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The metrics in the Prometheus text format.
    pub fn render(&self) -> String {
        let series = self.registry.series.read().unwrap_or_else(|e| e.into_inner());
        let mut entries: Vec<(&Key, &Arc<Series>)> = series.iter().collect();
        entries.sort_by(|a, b| (&a.0.route, a.0.method.as_str(), a.0.status).cmp(&(&b.0.route, b.0.method.as_str(), b.0.status)));

        let mut out = String::new();
        out.push_str("# HELP http_requests_total Total HTTP requests.\n# TYPE http_requests_total counter\n");
        for (key, series) in &entries {
            let _ = writeln!(out, "http_requests_total{{{}}} {}", labels(key), series.count.load(Ordering::Relaxed));
        }
        out.push_str("# HELP http_request_duration_seconds HTTP request duration.\n# TYPE http_request_duration_seconds histogram\n");
        for (key, series) in &entries {
            let labels = labels(key);
            let mut cumulative = 0;
            for (bound, bucket) in BUCKETS.iter().zip(&series.buckets) {
                cumulative += bucket.load(Ordering::Relaxed);
                let _ = writeln!(out, "http_request_duration_seconds_bucket{{{},le=\"{}\"}} {}", labels, bound, cumulative);
            }
            let count = series.count.load(Ordering::Relaxed);
            let _ = writeln!(out, "http_request_duration_seconds_bucket{{{},le=\"+Inf\"}} {}", labels, count);
            let sum = series.sum_micros.load(Ordering::Relaxed) as f64 / 1e6;
            let _ = writeln!(out, "http_request_duration_seconds_sum{{{}}} {}", labels, sum);
            let _ = writeln!(out, "http_request_duration_seconds_count{{{}}} {}", labels, count);
        }
        out.push_str("# HELP http_requests_in_flight HTTP requests being handled.\n# TYPE http_requests_in_flight gauge\n");
        let _ = writeln!(out, "http_requests_in_flight {}", self.registry.in_flight.load(Ordering::Relaxed));
        for family in self.registry.custom.read().unwrap_or_else(|e| e.into_inner()).iter() {
            family.render(&mut out);
        }
        out
    }

    /// A router with `GET /metrics`.
    pub fn router(&self) -> Router {
        let metrics = self.clone();
        let mut router = Router::new();
        router.get("/metrics", move |_req| {
            let body = metrics.render();
            async move { Response::new(200).with_header("Content-Type", "text/plain; version=0.0.4; charset=utf-8").with_body(body) }
        });
        router
    }
}

/// Prometheus label values escape backslash, double quote and newline.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn labels(key: &Key) -> String {
    format!("method=\"{}\",route=\"{}\",status=\"{}\"", key.method, escape(&key.route), key.status)
}

/// Decrements the in-flight gauge even if the request is cancelled.
struct InFlight(Arc<Registry>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Middleware for Metrics {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        let slot = MatchedRoute::watch(&mut req);
        let method = req.method();
        let metrics = self.clone();
        self.registry.in_flight.fetch_add(1, Ordering::Relaxed);
        let in_flight = InFlight(Arc::clone(&self.registry));
        let started = Instant::now();
        let response = next.handle(req);
        Box::pin(async move {
            let _in_flight = in_flight;
            let result = response.await;
            let status = match &result {
                Ok(response) => response.status,
                Err(error) => error.status_code(),
            };
            let route = slot.0.get().cloned().unwrap_or_else(|| Arc::from("unmatched"));
            metrics.record(Key { method, route, status }, started.elapsed().as_secs_f64());
            result
        })
    }
}
