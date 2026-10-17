//! Health check endpoints for load balancers and orchestrators.
//!
//! [`Health::router`] serves two routes:
//!
//! - `GET /health` (liveness): `200 {"status":"ok"}` while the process can
//!   answer at all.
//! - `GET /ready` (readiness): runs every check concurrently, each with a
//!   timeout, and answers `200` if all pass or `503` with details if any
//!   fails. During graceful shutdown it answers `503` so traffic drains
//!   before the server stops.
//!
//! ```rust
//! use axeon::Server;
//! use axeon::health::Health;
//!
//! let mut app = Server::new();
//! app.mount("/", Health::new()
//!     .check("database", || async {
//!         // e.g. sqlx::query("SELECT 1").execute(&pool).await
//!         Ok::<_, std::io::Error>(())
//!     })
//!     .router());
//! ```

use crate::message::Response;
use crate::router::Router;
use futures::future::{join_all, BoxFuture};
use serde_json::json;
use std::fmt::Display;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

type Check = Arc<dyn Fn() -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// Liveness and readiness endpoints; see the [module docs](self).
#[derive(Clone)]
pub struct Health {
    checks: Vec<(String, Check)>,
    timeout: Duration,
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

impl Health {
    /// No checks, a 5-second timeout per check.
    pub fn new() -> Self {
        Self { checks: Vec::new(), timeout: Duration::from_secs(5) }
    }

    /// Adds a readiness check, e.g. pinging the database.
    pub fn check<F, Fut, E>(mut self, name: &str, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: Display,
    {
        let check: Check = Arc::new(move || {
            let future = check();
            Box::pin(async move { future.await.map_err(|e| e.to_string()) })
        });
        self.checks.push((name.to_string(), check));
        self
    }

    /// How long each check may take before it counts as failed. Default: 5s.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// A router with `GET /health` and `GET /ready`.
    pub fn router(self) -> Router {
        let health = Arc::new(self);
        let mut router = Router::new();
        router.get("/health", |_req| async { Response::json(&json!({ "status": "ok" })).with_header("Cache-Control", "no-store") });
        router.get("/ready", move |req| {
            let health = Arc::clone(&health);
            async move {
                if req.shutdown().is_shutting_down() {
                    return Response::json(&json!({ "status": "shutting_down" }))
                        .with_status(503)
                        .with_header("Cache-Control", "no-store");
                }
                health.ready().await
            }
        });
        router
    }

    async fn ready(&self) -> Response {
        let timeout = self.timeout;
        let results = join_all(self.checks.iter().map(|(name, check)| {
            let future = check();
            async move {
                let started = Instant::now();
                let result = match tokio::time::timeout(timeout, future).await {
                    Ok(result) => result,
                    Err(_) => Err(format!("timed out after {:?}", timeout)),
                };
                (name.clone(), result, started.elapsed())
            }
        }))
        .await;

        let healthy = results.iter().all(|(_, result, _)| result.is_ok());
        let checks: serde_json::Map<String, serde_json::Value> = results
            .into_iter()
            .map(|(name, result, elapsed)| {
                let detail = match result {
                    Ok(()) => json!({ "status": "up", "duration_ms": elapsed.as_millis() as u64 }),
                    Err(error) => {
                        tracing::warn!(check = %name, error = %error, "readiness check failed");
                        json!({ "status": "down", "duration_ms": elapsed.as_millis() as u64, "error": error })
                    }
                };
                (name, detail)
            })
            .collect();
        let status = if healthy { 200 } else { 503 };
        Response::json(&json!({ "status": if healthy { "ok" } else { "unavailable" }, "checks": checks }))
            .with_status(status)
            .with_header("Cache-Control", "no-store")
    }
}
