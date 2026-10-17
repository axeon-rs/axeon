//! Production setup example for Axeon
//!
//! This example demonstrates:
//! - Structured logs with `tracing` and a request ID on every line
//! - Security headers, compression and rate limiting
//! - Health checks and Prometheus metrics
//! - Limits, timeouts and graceful shutdown with a drain period
//!
//! Run it, make a few requests, then press Ctrl+C:
//!   curl -i http://127.0.0.1:3000/
//!   curl http://127.0.0.1:3000/ready
//!   curl http://127.0.0.1:3000/metrics
//!   curl -i http://127.0.0.1:3000/slow   # then Ctrl+C: this request still completes

use axeon::health::Health;
use axeon::metrics::Metrics;
use axeon::middleware::{Compression, RateLimiter, RequestLogger, SecurityHeaders, SetRequestId};
use axeon::{json, Json, Server};
use std::time::Duration;

fn main() {
    // Print logs to stdout. Use `.json()` here for JSON logs in production.
    tracing_subscriber::fmt().with_target(false).init();

    let metrics = Metrics::new();
    let mut app = Server::new();
    app.max_connections(10_000)
        .keep_alive(Duration::from_secs(15))
        .request_timeout(Duration::from_secs(10))
        .shutdown_delay(Duration::from_secs(2)) // report not-ready, keep serving
        .shutdown_timeout(Duration::from_secs(20)) // then finish in-flight requests
        .max_body_size(1024 * 1024);

    // Order matters: the request ID is set before the logger reads it.
    app.middleware(SetRequestId::new())
        .middleware(RequestLogger::new())
        .middleware(metrics.clone())
        .middleware(SecurityHeaders::new())
        .middleware(Compression::new())
        // Set trust_proxy(true) only behind a proxy that sets X-Forwarded-For.
        .middleware(RateLimiter::per_minute(600).burst(50));

    app.mount("/", Health::new().check("self", || async { Ok::<_, std::io::Error>(()) }).router())
        .mount("/", metrics.router());

    app.get("/", |_req| async { Json(json!({ "service": "axeon-demo", "status": "ok" })) });
    app.get("/slow", |_req| async {
        tracing::info!("working on a slow request");
        tokio::time::sleep(Duration::from_secs(5)).await;
        "done"
    });

    app.listen("127.0.0.1:3000").expect("server failed to start");
}
