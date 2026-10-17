//! Production setup example for Axeon
//!
//! This example demonstrates:
//! - Structured logs with `tracing` and a request ID on every line
//! - Security headers, compression and rate limiting
//! - Limits and timeouts
//! - Graceful shutdown on Ctrl+C / SIGTERM (built into `listen`)
//!
//! Run it, make a few requests, then press Ctrl+C:
//!   curl -i http://127.0.0.1:3000/
//!   curl -i http://127.0.0.1:3000/slow   # then Ctrl+C: this request still completes

use axeon::middleware::{
    CompressionConfig, CompressionMiddleware, RateLimitConfig, RateLimiter, RequestLogger, SecurityConfig,
    SecurityHeaders, SetRequestId,
};
use axeon::{json, Json, Server};
use std::time::Duration;

fn main() {
    // Print logs to stdout. Use `.json()` here for JSON logs in production.
    tracing_subscriber::fmt().with_target(false).init();

    let mut app = Server::new();
    app.max_connections(10_000)
        .keep_alive(Duration::from_secs(15))
        .request_timeout(Duration::from_secs(10))
        .shutdown_timeout(Duration::from_secs(20))
        .max_body_size(1024 * 1024);

    // Order matters: the request ID is set before the logger reads it.
    app.middleware(SetRequestId::new());
    app.middleware(RequestLogger::new());
    app.middleware(SecurityHeaders::new(SecurityConfig::default()));
    app.middleware(CompressionMiddleware::new(CompressionConfig::default()));
    app.middleware(RateLimiter::new(RateLimitConfig {
        requests_per_minute: 600,
        burst_size: 50,
        // Set to true only behind a proxy that sets X-Forwarded-For.
        trust_proxy: false,
    }));

    app.get("/", |_req| async { Json(json!({ "service": "axeon-demo", "status": "ok" })) });
    app.get("/health", |_req| async { "ok" });
    app.get("/slow", |_req| async {
        tracing::info!("working on a slow request");
        tokio::time::sleep(Duration::from_secs(5)).await;
        "done"
    });

    app.listen("127.0.0.1:3000")
        .expect("Server failed to start");
}
