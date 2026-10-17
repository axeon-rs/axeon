use crate::error::ServerError;
use crate::message::{Method, Request, Response};
use crate::middleware::{Middleware, MiddlewareResult, Next};
use http::header::{HeaderName, HeaderValue};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Adds security headers to every response, with defaults modelled on
/// [helmet](https://helmetjs.github.io):
///
/// | Header | Default |
/// |---|---|
/// | `Strict-Transport-Security` | `max-age=31536000; includeSubDomains` |
/// | `X-Content-Type-Options` | `nosniff` |
/// | `X-Frame-Options` | `DENY` |
/// | `Referrer-Policy` | `no-referrer` |
/// | `Cross-Origin-Opener-Policy` | `same-origin` |
/// | `Cross-Origin-Resource-Policy` | `same-origin` |
/// | `X-DNS-Prefetch-Control` | `off` |
/// | `X-Permitted-Cross-Domain-Policies` | `none` |
/// | `X-XSS-Protection` | `0` (disables the obsolete, exploitable XSS auditor) |
///
/// No `Content-Security-Policy` is set unless you provide one, since a policy
/// must match your pages. Headers already set by a handler are kept.
///
/// ```rust
/// use axeon::Server;
/// use axeon::middleware::SecurityHeaders;
///
/// let mut app = Server::new();
/// app.middleware(
///     SecurityHeaders::new()
///         .content_security_policy("default-src 'self'")
///         .frame_options("SAMEORIGIN"),
/// );
/// ```
#[derive(Clone)]
pub struct SecurityHeaders {
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl Default for SecurityHeaders {
    fn default() -> Self {
        Self::new()
    }
}

impl SecurityHeaders {
    /// The defaults in the table above.
    pub fn new() -> Self {
        let defaults = [
            ("strict-transport-security", "max-age=31536000; includeSubDomains"),
            ("x-content-type-options", "nosniff"),
            ("x-frame-options", "DENY"),
            ("referrer-policy", "no-referrer"),
            ("cross-origin-opener-policy", "same-origin"),
            ("cross-origin-resource-policy", "same-origin"),
            ("x-dns-prefetch-control", "off"),
            ("x-permitted-cross-domain-policies", "none"),
            ("x-xss-protection", "0"),
        ];
        Self {
            headers: defaults
                .iter()
                .map(|(name, value)| (HeaderName::from_static(name), HeaderValue::from_static(value)))
                .collect(),
        }
    }

    /// Sets (or replaces) any header. Panics on an invalid name or value.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        let name = HeaderName::from_bytes(name.as_bytes()).expect("invalid header name");
        let value = HeaderValue::from_str(value).expect("invalid header value");
        self.headers.retain(|(existing, _)| existing != name);
        self.headers.push((name, value));
        self
    }

    fn without(mut self, name: &str) -> Self {
        self.headers.retain(|(existing, _)| existing.as_str() != name);
        self
    }

    /// Sets `Content-Security-Policy`, e.g. `default-src 'self'`.
    pub fn content_security_policy(self, policy: &str) -> Self {
        self.header("content-security-policy", policy)
    }

    /// Sets `Strict-Transport-Security` with this `max-age`.
    pub fn hsts(self, max_age: Duration, include_subdomains: bool) -> Self {
        let value = if include_subdomains {
            format!("max-age={}; includeSubDomains", max_age.as_secs())
        } else {
            format!("max-age={}", max_age.as_secs())
        };
        self.header("strict-transport-security", &value)
    }

    /// Doesn't send `Strict-Transport-Security` (e.g. a plain-HTTP service).
    pub fn no_hsts(self) -> Self {
        self.without("strict-transport-security")
    }

    /// Sets `X-Frame-Options`: `DENY` or `SAMEORIGIN`.
    pub fn frame_options(self, value: &str) -> Self {
        self.header("x-frame-options", value)
    }

    /// Sets `Referrer-Policy`, e.g. `strict-origin-when-cross-origin`.
    pub fn referrer_policy(self, value: &str) -> Self {
        self.header("referrer-policy", value)
    }

    /// Sets `Cross-Origin-Resource-Policy`; use `cross-origin` for assets
    /// other sites embed.
    pub fn cross_origin_resource_policy(self, value: &str) -> Self {
        self.header("cross-origin-resource-policy", value)
    }
}

impl Middleware for SecurityHeaders {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        let headers = self.headers.clone();
        Box::pin(async move {
            let mut response = next.handle(req).await?;
            for (name, value) in headers {
                response.headers.entry(name).or_insert(value);
            }
            Ok(response)
        })
    }
}

type KeyFn = Arc<dyn Fn(&Request) -> Option<String> + Send + Sync>;

struct Bucket {
    tokens: f64,
    updated: Instant,
}

struct LimiterState {
    buckets: HashMap<String, Bucket>,
    last_sweep: Instant,
}

/// Token-bucket rate limiting, per client IP by default.
///
/// Clients may make `burst` requests at once, then `per_minute` requests a
/// minute on average. Limited requests get `429 Too Many Requests` with a
/// `Retry-After` header.
///
/// State lives in this process; behind a load balancer each instance limits
/// separately.
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
#[derive(Clone)]
pub struct RateLimiter {
    per_minute: u32,
    burst: u32,
    trust_proxy: bool,
    key: Option<KeyFn>,
    state: Arc<Mutex<LimiterState>>,
}

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

impl RateLimiter {
    /// Allows `requests` per minute per client, with bursts of the same size.
    pub fn per_minute(requests: u32) -> Self {
        Self {
            per_minute: requests,
            burst: requests.max(1),
            trust_proxy: false,
            key: None,
            state: Arc::new(Mutex::new(LimiterState { buckets: HashMap::new(), last_sweep: Instant::now() })),
        }
    }

    /// Allows `requests` per second per client.
    pub fn per_second(requests: u32) -> Self {
        Self::per_minute(requests.saturating_mul(60)).burst(requests.max(1))
    }

    /// How many requests a client can make at once before being limited.
    pub fn burst(mut self, requests: u32) -> Self {
        self.burst = requests.max(1);
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

    fn client_key(&self, req: &Request) -> String {
        if let Some(key) = self.key.as_ref().and_then(|key| key(req)) {
            return format!("k:{}", key);
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
                return format!("ip:{}", ip);
            }
        }
        match req.remote_addr {
            Some(addr) => format!("ip:{}", addr.ip()),
            None => "unknown".to_string(),
        }
    }

    /// Takes a token, or returns how long until one is available.
    fn acquire(&self, key: String) -> Result<(), Duration> {
        let now = Instant::now();
        let capacity = self.burst as f64;
        let rate = self.per_minute as f64 / 60.0;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // Drop buckets that have refilled completely: a new bucket is identical.
        if now.duration_since(state.last_sweep) >= SWEEP_INTERVAL {
            // With a zero rate buckets never refill, so they must be kept.
            state.buckets.retain(|_, b| {
                rate == 0.0 || b.tokens + now.duration_since(b.updated).as_secs_f64() * rate < capacity
            });
            state.last_sweep = now;
        }

        let bucket = state.buckets.entry(key).or_insert(Bucket { tokens: capacity, updated: now });
        let elapsed = now.duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(capacity);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else if rate > 0.0 {
            Err(Duration::from_secs_f64((1.0 - bucket.tokens) / rate))
        } else {
            Err(Duration::from_secs(60))
        }
    }
}

impl Middleware for RateLimiter {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        match self.acquire(self.client_key(&req)) {
            Ok(()) => next.handle(req),
            Err(wait) => Box::pin(async move { Err(ServerError::TooManyRequests { retry_after: Some(wait) }) }),
        }
    }
}

#[derive(Clone)]
enum Origins {
    Any,
    List(Vec<String>),
}

/// Cross-Origin Resource Sharing: lets browsers on other origins call your
/// API, and answers their preflight requests.
///
/// `Cors::new()` allows any origin without credentials. To send cookies
/// cross-origin, list the origins:
///
/// ```rust
/// use axeon::Server;
/// use axeon::middleware::Cors;
///
/// let mut app = Server::new();
/// app.middleware(
///     Cors::new()
///         .allow_origin("https://app.example.com")
///         .allow_credentials(true),
/// );
/// ```
///
/// Allowing credentials with any origin is rejected when the server is
/// built: browsers refuse that combination, and reflecting every origin
/// instead would let any website act as your logged-in users.
#[derive(Clone)]
pub struct Cors {
    origins: Origins,
    methods: Vec<String>,
    headers: Vec<String>,
    expose_headers: Vec<String>,
    credentials: bool,
    max_age: Option<Duration>,
}

impl Default for Cors {
    fn default() -> Self {
        Self::new()
    }
}

impl Cors {
    /// Any origin; methods GET, HEAD, POST, PUT, PATCH, DELETE; headers
    /// `Content-Type` and `Authorization`; preflights cached for a day.
    pub fn new() -> Self {
        Self {
            origins: Origins::Any,
            methods: ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"].iter().map(|m| m.to_string()).collect(),
            headers: vec!["Content-Type".to_string(), "Authorization".to_string()],
            expose_headers: Vec::new(),
            credentials: false,
            max_age: Some(Duration::from_secs(86400)),
        }
    }

    /// Allows this origin (e.g. `https://app.example.com`). The first call
    /// replaces "any origin" with a list.
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        let origin = origin.into();
        match &mut self.origins {
            Origins::List(list) => list.push(origin),
            Origins::Any => self.origins = Origins::List(vec![origin]),
        }
        self
    }

    /// Allows each of these origins.
    pub fn allow_origins<I, S>(mut self, origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for origin in origins {
            self = self.allow_origin(origin);
        }
        self
    }

    /// Allows any origin (the default).
    pub fn allow_any_origin(mut self) -> Self {
        self.origins = Origins::Any;
        self
    }

    /// The methods allowed in cross-origin requests.
    pub fn allow_methods<I, S>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.methods = methods.into_iter().map(Into::into).collect();
        self
    }

    /// The request headers allowed in cross-origin requests.
    pub fn allow_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.headers = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Response headers that browser code may read (beyond the safe defaults).
    pub fn expose_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.expose_headers = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Allows cookies and `Authorization` in cross-origin requests. Requires
    /// a list of origins.
    pub fn allow_credentials(mut self, allow: bool) -> Self {
        self.credentials = allow;
        self
    }

    /// How long browsers may cache a preflight response; `None` to not say.
    pub fn max_age(mut self, max_age: Option<Duration>) -> Self {
        self.max_age = max_age;
        self
    }

    /// The `Access-Control-Allow-Origin` value for this request, if allowed.
    fn allowed_origin(&self, origin: Option<&str>) -> Option<String> {
        match &self.origins {
            Origins::Any => Some("*".to_string()),
            Origins::List(list) => {
                let origin = origin?;
                list.iter().any(|o| o.eq_ignore_ascii_case(origin)).then(|| origin.to_string())
            }
        }
    }

    fn apply(&self, response: &mut Response, allow_origin: Option<&str>) {
        if matches!(self.origins, Origins::List(_)) {
            // The response differs per Origin, so caches must key on it.
            response.append_vary("Origin");
        }
        if let Some(allow_origin) = allow_origin {
            response.header("Access-Control-Allow-Origin", allow_origin);
            if self.credentials {
                response.header("Access-Control-Allow-Credentials", "true");
            }
            if !self.expose_headers.is_empty() {
                response.header("Access-Control-Expose-Headers", self.expose_headers.join(", "));
            }
        }
    }
}

impl Middleware for Cors {
    fn validate(&self) -> Result<(), String> {
        if self.credentials && matches!(self.origins, Origins::Any) {
            return Err("Cors: allow_credentials(true) needs explicit origins (allow_origin); \
                        browsers reject credentials with any origin"
                .to_string());
        }
        Ok(())
    }

    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        let cors = self.clone();
        Box::pin(async move {
            let origin = req.get_header("origin").map(str::to_string);
            let allow_origin = cors.allowed_origin(origin.as_deref());

            let is_preflight = req.method == Method::OPTIONS
                && origin.is_some()
                && req.get_header("access-control-request-method").is_some();
            if is_preflight {
                let mut response = Response::new(204);
                cors.apply(&mut response, allow_origin.as_deref());
                if allow_origin.is_some() {
                    response.header("Access-Control-Allow-Methods", cors.methods.join(", "));
                    response.header("Access-Control-Allow-Headers", cors.headers.join(", "));
                    if let Some(max_age) = cors.max_age {
                        response.header("Access-Control-Max-Age", max_age.as_secs().to_string());
                    }
                }
                return Ok(response);
            }

            let mut response = next.handle(req).await?;
            if origin.is_some() || matches!(cors.origins, Origins::List(_)) {
                cors.apply(&mut response, allow_origin.as_deref());
            }
            Ok(response)
        })
    }
}
