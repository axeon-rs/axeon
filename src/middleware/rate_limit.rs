//! Rate limiting, in this process or shared by every instance through a
//! [`RateLimitStore`] such as Redis.

use super::{error_response, Middleware, MiddlewareResult, Next};
use crate::body::BoxError;
use crate::error::ServerError;
use crate::message::Request;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How many requests a client may make.
///
/// Two kinds:
///
/// - [`Quota::per_minute`], [`per_second`](Quota::per_second),
///   [`per_hour`](Quota::per_hour), [`per_day`](Quota::per_day) and
///   [`per`](Quota::per): a smooth rate with bursts (GCRA, equivalent to a
///   token bucket). Clients may make `burst` requests at once; after that,
///   capacity comes back steadily at the rate.
/// - [`Quota::fixed_window`] and [`daily`](Quota::daily): at most `limit`
///   requests in each window, with every window starting on a multiple of
///   its length since the Unix epoch, so a daily window runs from midnight
///   to midnight UTC. Use it for plan quotas such as 10 000 calls a day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Quota {
    kind: QuotaKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum QuotaKind {
    Gcra { requests: u32, period: Duration, burst: u32 },
    Window { limit: u32, window: Duration },
}

impl Quota {
    /// `requests` per `period`, with bursts of the same size.
    pub fn per(requests: u32, period: Duration) -> Self {
        let period = period.max(Duration::from_millis(1));
        Quota { kind: QuotaKind::Gcra { requests, period, burst: requests.max(1) } }
    }

    /// `requests` a second, with bursts of the same size.
    pub fn per_second(requests: u32) -> Self {
        Self::per(requests, Duration::from_secs(1))
    }

    /// `requests` a minute, with bursts of the same size.
    pub fn per_minute(requests: u32) -> Self {
        Self::per(requests, Duration::from_secs(60))
    }

    /// `requests` an hour, with bursts of the same size.
    pub fn per_hour(requests: u32) -> Self {
        Self::per(requests, Duration::from_secs(3600))
    }

    /// `requests` a day, with bursts of the same size. For a quota that
    /// resets at midnight UTC, use [`Quota::daily`].
    pub fn per_day(requests: u32) -> Self {
        Self::per(requests, Duration::from_secs(86_400))
    }

    /// How many requests a client can make at once. Only for rate quotas;
    /// a window's burst is its limit.
    pub fn burst(mut self, requests: u32) -> Self {
        if let QuotaKind::Gcra { burst, .. } = &mut self.kind {
            *burst = requests.max(1);
        }
        self
    }

    /// At most `limit` requests in each `window`; windows are aligned to
    /// the Unix epoch.
    pub fn fixed_window(limit: u32, window: Duration) -> Self {
        Quota { kind: QuotaKind::Window { limit, window: window.max(Duration::from_millis(1)) } }
    }

    /// At most `limit` requests a calendar day (UTC).
    pub fn daily(limit: u32) -> Self {
        Self::fixed_window(limit, Duration::from_secs(86_400))
    }

    /// The most requests allowed at once: the burst, or a window's limit.
    pub fn limit(&self) -> u32 {
        match self.kind {
            QuotaKind::Gcra { burst, .. } => burst,
            QuotaKind::Window { limit, .. } => limit,
        }
    }

    /// The rate's emission interval (the time one request's capacity takes
    /// to come back) and the burst tolerance, in microseconds. A rate of
    /// zero returns capacity once a year.
    pub fn gcra(&self) -> Option<(u64, u64)> {
        match self.kind {
            QuotaKind::Gcra { requests, period, burst } => {
                let interval = match requests {
                    0 => 365 * 86_400 * 1_000_000,
                    n => (period.as_micros() / n as u128).clamp(1, u64::MAX as u128 / 2) as u64,
                };
                Some((interval, interval.saturating_mul(burst as u64)))
            }
            QuotaKind::Window { .. } => None,
        }
    }

    /// A fixed window's limit and length.
    pub fn window(&self) -> Option<(u32, Duration)> {
        match self.kind {
            QuotaKind::Window { limit, window } => Some((limit, window)),
            QuotaKind::Gcra { .. } => None,
        }
    }

    /// A short label for this quota, used in store keys so that a client
    /// whose plan changes starts with fresh state, and in the
    /// `RateLimit-Policy` header.
    pub fn policy(&self) -> String {
        match self.kind {
            QuotaKind::Gcra { requests, period, burst } => {
                format!("{};w={};burst={}", requests, period.as_secs().max(1), burst)
            }
            QuotaKind::Window { limit, window } => format!("{};w={}", limit, window.as_secs().max(1)),
        }
    }
}

/// What a [`RateLimitStore`] decided about one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// Whether the request may go ahead.
    pub allowed: bool,
    /// The quota's [limit](Quota::limit).
    pub limit: u32,
    /// Requests left right now.
    pub remaining: u32,
    /// Until the client has its full limit again (or, for a window, until
    /// the window ends).
    pub reset: Duration,
    /// For a refused request, when it may retry.
    pub retry_after: Option<Duration>,
}

impl RateLimitDecision {
    /// Applies the GCRA to the stored theoretical arrival time `tat`
    /// (microseconds, same clock as `now`). Returns the decision and the
    /// new `tat` to store, if the request was allowed. For stores that keep
    /// state outside a script.
    pub fn gcra(quota: &Quota, now: u64, tat: Option<u64>) -> (Self, Option<u64>) {
        let (interval, tolerance) = quota.gcra().expect("not a rate quota");
        let tat = tat.unwrap_or(now).max(now);
        let new_tat = tat.saturating_add(interval);
        let allow_at = new_tat.saturating_sub(tolerance);
        if now < allow_at {
            let decision = RateLimitDecision {
                allowed: false,
                limit: quota.limit(),
                remaining: 0,
                reset: Duration::from_micros(tat - now),
                retry_after: Some(Duration::from_micros(allow_at - now)),
            };
            return (decision, None);
        }
        let used = new_tat - now;
        let decision = RateLimitDecision {
            allowed: true,
            limit: quota.limit(),
            remaining: (tolerance.saturating_sub(used) / interval).min(u32::MAX as u64) as u32,
            reset: Duration::from_micros(used),
            retry_after: None,
        };
        (decision, Some(new_tat))
    }

    /// The decision for a fixed window that has seen `count` requests
    /// (counting this one) and ends in `reset`.
    pub fn window(quota: &Quota, count: u64, reset: Duration) -> Self {
        let limit = quota.limit();
        let allowed = count <= limit as u64;
        RateLimitDecision {
            allowed,
            limit,
            remaining: (limit as u64).saturating_sub(count) as u32,
            reset,
            retry_after: (!allowed).then_some(reset),
        }
    }
}

/// Where rate-limit state lives. [`MemoryRateLimitStore`] keeps it in this
/// process; `axeon_redis::RedisRateLimitStore` shares it between every
/// instance of the app, so N instances don't allow N times the limit.
pub trait RateLimitStore: Send + Sync + 'static {
    /// Counts one request from `key` against `quota`.
    fn check<'a>(&'a self, key: &'a str, quota: &'a Quota) -> BoxFuture<'a, Result<RateLimitDecision, BoxError>>;
}

/// Rate-limit state in this process's memory. Behind a load balancer each
/// instance limits separately; use a shared store for one limit across
/// instances.
#[derive(Default)]
pub struct MemoryRateLimitStore {
    state: Mutex<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    // GCRA: the theoretical arrival time. Window: the window index and count.
    entries: HashMap<String, Entry>,
    last_sweep: Option<Instant>,
}

enum Entry {
    Tat(u64),
    Window(u64, u64),
}

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

impl MemoryRateLimitStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn check_now(&self, key: &str, quota: &Quota, now: u64) -> RateLimitDecision {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Drop state that no longer matters: refilled buckets and past
        // windows behave exactly like missing ones.
        let instant = Instant::now();
        if state.last_sweep.is_none_or(|last| instant.duration_since(last) >= SWEEP_INTERVAL) {
            state.entries.retain(|_, entry| match *entry {
                Entry::Tat(tat) => tat > now,
                Entry::Window(..) => true,
            });
            state.last_sweep = Some(instant);
        }
        let key = format!("{}|{}", quota.policy(), key);
        match quota.window() {
            None => {
                let tat = match state.entries.get(&key) {
                    Some(Entry::Tat(tat)) => Some(*tat),
                    _ => None,
                };
                let (decision, new_tat) = RateLimitDecision::gcra(quota, now, tat);
                if let Some(tat) = new_tat {
                    state.entries.insert(key, Entry::Tat(tat));
                }
                decision
            }
            Some((limit, window)) => {
                let window_us = window.as_micros().max(1) as u64;
                let index = now / window_us;
                let reset = Duration::from_micros((index + 1) * window_us - now);
                let count = match state.entries.get(&key) {
                    Some(Entry::Window(i, count)) if *i == index => *count,
                    _ => 0,
                };
                if count >= limit as u64 {
                    return RateLimitDecision::window(quota, count + 1, reset);
                }
                state.entries.insert(key, Entry::Window(index, count + 1));
                RateLimitDecision::window(quota, count + 1, reset)
            }
        }
    }
}

impl RateLimitStore for MemoryRateLimitStore {
    fn check<'a>(&'a self, key: &'a str, quota: &'a Quota) -> BoxFuture<'a, Result<RateLimitDecision, BoxError>> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
        let decision = self.check_now(key, quota, now);
        Box::pin(async move { Ok(decision) })
    }
}

type KeyFn = Arc<dyn Fn(&Request) -> Option<String> + Send + Sync>;
type QuotaFn = Arc<dyn Fn(&Request) -> Option<Quota> + Send + Sync>;

/// Rate limiting, per client IP by default.
///
/// Clients may make `burst` requests at once, then `per_minute` requests a
/// minute on average. Limited requests get `429 Too Many Requests` with a
/// `Retry-After` header.
///
/// By default state lives in this process, so behind a load balancer each
/// instance limits separately. Pass a shared [`store`](RateLimiter::store)
/// (such as `axeon_redis::RedisRateLimitStore`) for one limit across every
/// instance.
///
/// ```rust
/// use axeon::Server;
/// use axeon::middleware::RateLimiter;
///
/// let mut app = Server::new();
/// app.middleware(RateLimiter::per_minute(120).burst(20));
/// // Per API key instead of per IP (requests without one fall back to IP):
/// app.middleware(RateLimiter::per_minute(1000).key_by(|req| req.get_header("x-api-key").map(String::from)));
/// ```
///
/// Quotas can depend on the request, e.g. on the plan of the API key that
/// an earlier authentication middleware attached:
///
/// ```rust
/// use axeon::Server;
/// use axeon::middleware::rate_limit::{Quota, RateLimiter};
///
/// #[derive(Clone)]
/// struct ApiKey { id: String, daily_quota: u32 }
///
/// let mut app = Server::new();
/// app.middleware(
///     RateLimiter::new(Quota::per_minute(60))
///         .key_by(|req| req.extension::<ApiKey>().map(|k| k.id.clone()))
///         .quota_by(|req| req.extension::<ApiKey>().map(|k| Quota::daily(k.daily_quota)))
///         .headers(true),
/// );
/// ```
#[derive(Clone)]
pub struct RateLimiter {
    quota: Quota,
    trust_proxy: bool,
    key: Option<KeyFn>,
    quota_by: Option<QuotaFn>,
    store: Arc<dyn RateLimitStore>,
    prefix: String,
    headers: bool,
    fail_open: bool,
}

impl RateLimiter {
    /// Limits each client to `quota`, in this process's memory.
    pub fn new(quota: Quota) -> Self {
        Self {
            quota,
            trust_proxy: false,
            key: None,
            quota_by: None,
            store: Arc::new(MemoryRateLimitStore::new()),
            prefix: String::new(),
            headers: false,
            fail_open: true,
        }
    }

    /// Allows `requests` per minute per client, with bursts of the same size.
    pub fn per_minute(requests: u32) -> Self {
        Self::new(Quota::per_minute(requests))
    }

    /// Allows `requests` per second per client.
    pub fn per_second(requests: u32) -> Self {
        Self::new(Quota::per_second(requests))
    }

    /// How many requests a client can make at once before being limited.
    pub fn burst(mut self, requests: u32) -> Self {
        self.quota = self.quota.burst(requests);
        self
    }

    /// Identify clients by `X-Forwarded-For` / `X-Real-IP`. Enable only
    /// behind a reverse proxy that sets these headers; otherwise clients can
    /// spoof them to dodge the limit. Default: the TCP peer address.
    pub fn trust_proxy(mut self, trust: bool) -> Self {
        self.trust_proxy = trust;
        self
    }

    /// Limits per custom key, e.g. an API key or user ID. Requests for which
    /// the function returns `None` are limited by IP.
    pub fn key_by<F>(mut self, key: F) -> Self
    where
        F: Fn(&Request) -> Option<String> + Send + Sync + 'static,
    {
        self.key = Some(Arc::new(key));
        self
    }

    /// Picks the quota per request, e.g. from the caller's plan. Requests
    /// for which the function returns `None` get the default quota.
    pub fn quota_by<F>(mut self, quota: F) -> Self
    where
        F: Fn(&Request) -> Option<Quota> + Send + Sync + 'static,
    {
        self.quota_by = Some(Arc::new(quota));
        self
    }

    /// Where the state lives. The default keeps it in this process.
    pub fn store<S: RateLimitStore>(mut self, store: S) -> Self {
        self.store = Arc::new(store);
        self
    }

    /// Prepended to every key, so several limiters can share one store
    /// (e.g. `"api:"` and `"login:"`).
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Sends `RateLimit-Limit`, `RateLimit-Remaining`, `RateLimit-Reset`
    /// and `RateLimit-Policy` on every response, so clients can pace
    /// themselves. Off by default.
    pub fn headers(mut self, send: bool) -> Self {
        self.headers = send;
        self
    }

    /// What to do when the store fails (e.g. Redis is down): let requests
    /// through (`true`, the default, logging the error) or refuse them with
    /// 503 (`false`).
    pub fn fail_open(mut self, open: bool) -> Self {
        self.fail_open = open;
        self
    }

    fn client_key(&self, req: &Request) -> String {
        if let Some(key) = self.key.as_ref().and_then(|key| key(req)) {
            return format!("{}k:{}", self.prefix, key);
        }
        if self.trust_proxy {
            // The right-most X-Forwarded-For entry is the one our proxy added.
            let forwarded = req
                .headers
                .get_all("x-forwarded-for")
                .iter()
                .next_back()
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit(',').next())
                .or_else(|| req.get_header("x-real-ip"))
                .and_then(|ip| ip.trim().parse::<std::net::IpAddr>().ok());
            if let Some(ip) = forwarded {
                return format!("{}ip:{}", self.prefix, ip);
            }
        }
        match req.remote_addr {
            Some(addr) => format!("{}ip:{}", self.prefix, addr.ip()),
            None => format!("{}unknown", self.prefix),
        }
    }
}

fn set_headers(response: &mut crate::Response, decision: &RateLimitDecision, quota: &Quota) {
    let seconds = |d: Duration| d.as_secs() + u64::from(d.subsec_nanos() > 0);
    response.header("RateLimit-Limit", decision.limit.to_string());
    response.header("RateLimit-Remaining", decision.remaining.to_string());
    response.header("RateLimit-Reset", seconds(decision.reset).to_string());
    response.header("RateLimit-Policy", quota.policy());
}

impl Middleware for RateLimiter {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        let key = self.client_key(&req);
        let quota = self.quota_by.as_ref().and_then(|f| f(&req)).unwrap_or(self.quota);
        let store = Arc::clone(&self.store);
        let (headers, fail_open) = (self.headers, self.fail_open);
        Box::pin(async move {
            let decision = match store.check(&key, &quota).await {
                Ok(decision) => decision,
                Err(error) if fail_open => {
                    tracing::warn!(%error, "rate limit store failed; letting the request through");
                    return next.handle(req).await;
                }
                Err(error) => {
                    tracing::error!(%error, "rate limit store failed");
                    return Err(ServerError::Status(503, "rate limiting unavailable".into()));
                }
            };
            if !decision.allowed {
                let error = ServerError::TooManyRequests { retry_after: decision.retry_after };
                if !headers {
                    return Err(error);
                }
                let mut response = error_response(next.chain.on_error.as_ref(), error);
                set_headers(&mut response, &decision, &quota);
                return Ok(response);
            }
            let mut response = next.handle(req).await?;
            if headers {
                set_headers(&mut response, &decision, &quota);
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcra_allows_the_burst_then_one_per_interval() {
        let store = MemoryRateLimitStore::new();
        let quota = Quota::per_second(2).burst(3);
        let t0 = 1_000_000_000;
        let allowed = |now| store.check_now("k", &quota, now);
        let first: Vec<_> = (0..4).map(|_| allowed(t0)).collect();
        assert_eq!(first.iter().map(|d| d.allowed).collect::<Vec<_>>(), [true, true, true, false]);
        assert_eq!(first.iter().map(|d| d.remaining).collect::<Vec<_>>(), [2, 1, 0, 0]);
        assert_eq!(first[3].retry_after, Some(Duration::from_millis(500)));
        assert!(!allowed(t0 + 499_999).allowed);
        assert!(allowed(t0 + 500_000).allowed);
        assert!(!allowed(t0 + 500_000).allowed);
    }

    #[test]
    fn windows_reset_on_their_boundary() {
        let store = MemoryRateLimitStore::new();
        let quota = Quota::fixed_window(2, Duration::from_secs(10));
        let start = 10_000_000 * 7; // A window boundary.
        let check = |now| store.check_now("k", &quota, now);
        assert_eq!(check(start + 1).remaining, 1);
        assert_eq!(check(start + 2).remaining, 0);
        let refused = check(start + 3_000_000);
        assert!(!refused.allowed);
        assert_eq!(refused.retry_after, Some(Duration::from_micros(7_000_000)));
        assert!(check(start + 10_000_000).allowed);
    }

    #[test]
    fn plans_have_separate_state() {
        let store = MemoryRateLimitStore::new();
        let free = Quota::daily(1);
        let paid = Quota::daily(5);
        assert!(store.check_now("k", &free, 1).allowed);
        assert!(!store.check_now("k", &free, 2).allowed);
        assert!(store.check_now("k", &paid, 3).allowed);
    }
}
