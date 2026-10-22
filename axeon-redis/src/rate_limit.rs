//! Rate limiting shared by every instance of the app.

use axeon::middleware::rate_limit::{Quota, RateLimitDecision, RateLimitStore};
use futures::future::BoxFuture;
use redis::aio::{ConnectionLike, ConnectionManager};
use redis::Script;
use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// GCRA in one round trip. Redis' own clock is used, so instances with
// skewed clocks still agree. Times are microseconds. Returns
// {allowed, remaining, retry_after, reset}.
static GCRA: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r#"
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000000 + tonumber(t[2])
local interval = tonumber(ARGV[1])
local tolerance = tonumber(ARGV[2])
local tat = tonumber(redis.call('GET', KEYS[1]) or now)
if tat < now then tat = now end
local new_tat = tat + interval
local allow_at = new_tat - tolerance
if now < allow_at then
  return {0, 0, allow_at - now, tat - now}
end
local used = new_tat - now
redis.call('SET', KEYS[1], string.format('%.0f', new_tat), 'PX', math.ceil(used / 1000))
return {1, math.floor((tolerance - used) / interval), 0, used}
"#,
    )
});

// A fixed window aligned to the epoch, on Redis' clock. The value is
// "<window index>:<count>". Times are milliseconds. Returns {count, reset}.
static WINDOW: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r#"
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
local window = tonumber(ARGV[1])
local limit = tonumber(ARGV[2])
local index = math.floor(now / window)
local reset = (index + 1) * window - now
local count = 0
local current = redis.call('GET', KEYS[1])
if current then
  local i, c = string.match(current, '^(%d+):(%d+)$')
  if i == string.format('%.0f', index) then count = tonumber(c) end
end
if count >= limit then return {count + 1, reset} end
count = count + 1
redis.call('SET', KEYS[1], string.format('%.0f', index) .. ':' .. count, 'PX', reset)
return {count, reset}
"#,
    )
});

/// A [`RateLimitStore`] in Redis, so every instance of the app shares one
/// limit per client: with N instances, clients still get the limit, not N
/// times it.
///
/// Each check is a single Lua script call (GCRA for rate quotas, a counter
/// for [fixed windows](Quota::fixed_window)), timed by Redis' clock. Keys
/// are the [prefix](RedisRateLimitStore::prefix) plus a SHA-256 hash of the
/// client key and quota, and expire when their state no longer matters.
///
/// ```no_run
/// use axeon::middleware::rate_limit::{Quota, RateLimiter};
/// use axeon::Server;
/// use axeon_redis::RedisRateLimitStore;
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let redis = axeon_redis::connect("redis://127.0.0.1:6379").await?;
/// let mut app = Server::new();
/// app.middleware(
///     RateLimiter::new(Quota::per_minute(120).burst(20))
///         .store(RedisRateLimitStore::new(redis.clone()))
///         .trust_proxy(true)
///         .headers(true),
/// );
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct RedisRateLimitStore<C = ConnectionManager> {
    connection: C,
    prefix: Arc<str>,
}

impl<C> RedisRateLimitStore<C> {
    /// A store on `connection`, with keys prefixed `axeon:ratelimit:`.
    pub fn new(connection: C) -> Self {
        Self { connection, prefix: "axeon:ratelimit:".into() }
    }

    /// Prefixes keys with `prefix`.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into().into();
        self
    }

    fn key(&self, key: &str, quota: &Quota) -> String {
        let mut hasher = Sha256::new();
        hasher.update(quota.policy().as_bytes());
        hasher.update(b"|");
        hasher.update(key.as_bytes());
        let mut out = String::with_capacity(self.prefix.len() + 64);
        out.push_str(&self.prefix);
        for byte in hasher.finalize() {
            let _ = write!(out, "{:02x}", byte);
        }
        out
    }
}

impl<C> RateLimitStore for RedisRateLimitStore<C>
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
{
    fn check<'a>(&'a self, key: &'a str, quota: &'a Quota) -> BoxFuture<'a, Result<RateLimitDecision, BoxError>> {
        let redis_key = self.key(key, quota);
        let mut connection = self.connection.clone();
        Box::pin(async move {
            if let Some((interval, tolerance)) = quota.gcra() {
                let reply: Vec<i64> =
                    GCRA.key(&redis_key).arg(interval).arg(tolerance).invoke_async(&mut connection).await?;
                let [allowed, remaining, retry_after, reset] = reply[..] else {
                    return Err("unexpected reply from the rate limit script".into());
                };
                let micros = |v: i64| Duration::from_micros(v.max(0) as u64);
                return Ok(RateLimitDecision {
                    allowed: allowed == 1,
                    limit: quota.limit(),
                    remaining: remaining.clamp(0, u32::MAX as i64) as u32,
                    reset: micros(reset),
                    retry_after: (allowed != 1).then(|| micros(retry_after)),
                });
            }
            let (limit, window) = quota.window().expect("a quota is a rate or a window");
            let window_ms = window.as_millis().max(1) as u64;
            let reply: Vec<i64> = WINDOW.key(&redis_key).arg(window_ms).arg(limit).invoke_async(&mut connection).await?;
            let [count, reset] = reply[..] else {
                return Err("unexpected reply from the rate limit script".into());
            };
            Ok(RateLimitDecision::window(quota, count.max(0) as u64, Duration::from_millis(reset.max(0) as u64)))
        })
    }
}
