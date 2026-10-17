use crate::http::Request;
use crate::middleware::{Middleware, MiddlewareResult, Next};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tracing::Instrument;

const REQUEST_ID_HEADER: &str = "x-request-id";

/// The ID of the current request, set by [`SetRequestId`]. Read it with
/// `req.extension::<RequestId>()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestId(pub String);

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Gives every request an ID: the incoming `X-Request-Id` when present and
/// well-formed, otherwise a new one. The ID is stored as a [`RequestId`]
/// extension and echoed in the response's `X-Request-Id` header.
///
/// Register it before [`RequestLogger`] so log lines include the ID.
pub struct SetRequestId {
    trust_incoming: bool,
}

impl SetRequestId {
    pub fn new() -> Self {
        Self { trust_incoming: true }
    }

    /// Whether to reuse an incoming `X-Request-Id` (default: true). Turn
    /// this off when clients, rather than your proxy, can set the header.
    pub fn trust_incoming(mut self, trust: bool) -> Self {
        self.trust_incoming = trust;
        self
    }
}

impl Default for SetRequestId {
    fn default() -> Self {
        Self::new()
    }
}

/// Accepts IDs that are safe to log and echo: short, printable ASCII, no spaces.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_graphic())
}

/// A process-unique ID: a per-process random-ish prefix plus a counter.
fn generate_id() -> String {
    static PREFIX: OnceLock<u64> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let prefix = *PREFIX.get_or_init(|| {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
        nanos ^ ((std::process::id() as u64) << 32)
    });
    format!("{:016x}-{:08x}", prefix, COUNTER.fetch_add(1, Ordering::Relaxed))
}

impl Middleware for SetRequestId {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        let id = req
            .get_header(REQUEST_ID_HEADER)
            .filter(|id| self.trust_incoming && valid_id(id))
            .map(str::to_string)
            .unwrap_or_else(generate_id);
        req.insert_extension(RequestId(id.clone()));
        let response = next.handle(req);
        Box::pin(async move {
            let mut res = response.await?;
            res.header(REQUEST_ID_HEADER, &id);
            Ok(res)
        })
    }
}

/// Logs each request with [`tracing`]: a `request` span carrying the method,
/// path and request ID, and one event when it completes with the status and
/// latency (`error` level for 5xx, `info` otherwise).
///
/// Events logged inside handlers belong to the request's span, so a
/// subscriber such as `tracing_subscriber::fmt` prints them with the same
/// request ID.
#[derive(Default)]
pub struct RequestLogger;

impl RequestLogger {
    pub fn new() -> Self {
        Self
    }
}

impl Middleware for RequestLogger {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        let request_id = req.extension::<RequestId>().map(|id| id.0.clone());
        let span = tracing::info_span!(
            "request",
            method = %req.method,
            path = %req.path,
            request_id = %request_id.as_deref().unwrap_or("-"),
        );
        let start = Instant::now();
        let response = span.in_scope(|| next.handle(req));
        Box::pin(
            async move {
                let res = response.await;
                let status = match &res {
                    Ok(res) => res.status,
                    Err(err) => err.status_code(),
                };
                let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
                if status >= 500 {
                    tracing::error!(status, latency_ms, "request failed");
                } else {
                    tracing::info!(status, latency_ms, "request completed");
                }
                res
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_generates_ids() {
        assert!(valid_id("abc-123"));
        assert!(!valid_id(""));
        assert!(!valid_id("has space"));
        assert!(!valid_id("new\nline"));
        assert!(!valid_id(&"x".repeat(129)));
        let (a, b) = (generate_id(), generate_id());
        assert_ne!(a, b);
        assert!(valid_id(&a));
    }
}
