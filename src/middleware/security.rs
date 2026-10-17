use crate::http::{Method, Request};
use crate::middleware::{Middleware, MiddlewareResult, Next};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use crate::error::ServerError;
use crate::http::Response;

#[derive(Clone)]
pub struct SecurityConfig {
    pub hsts: bool,
    pub xss_protection: bool,
    pub content_type_options: bool,
    pub frame_options: Option<String>,
    pub content_security_policy: Option<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            hsts: true,
            xss_protection: true,
            content_type_options: true,
            frame_options: Some("DENY".to_string()),
            content_security_policy: None,
        }
    }
}

pub struct SecurityHeaders {
    config: SecurityConfig,
}

impl SecurityHeaders {
    pub fn new(config: SecurityConfig) -> Self {
        Self { config }
    }
}

impl Middleware for SecurityHeaders {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        let config = self.config.clone();
        Box::pin(async move {
            let mut response = next.handle(req).await?;
            if config.hsts {
                response.header("Strict-Transport-Security", "max-age=31536000");
            }
            if config.xss_protection {
                response.header("X-XSS-Protection", "1; mode=block");
            }
            if config.content_type_options {
                response.header("X-Content-Type-Options", "nosniff");
            }
            if let Some(ref frame_options) = config.frame_options {
                response.header("X-Frame-Options", frame_options);
            }
            if let Some(ref content_security_policy) = config.content_security_policy {
                response.header("Content-Security-Policy", content_security_policy);
            }

            Ok(response)
        })
    }

    fn clone_box(&self) -> Box<dyn Middleware> {
        Box::new(Self::new(self.config.clone()))
    }
}

#[derive(Clone)]
pub struct RateLimitConfig {
    /// Sustained rate: tokens refill at this many per minute.
    pub requests_per_minute: u32,
    /// Bucket size: how many requests a client can make in a quick burst.
    pub burst_size: u32,
    /// Take the client IP from `X-Forwarded-For` / `X-Real-IP`. Enable only
    /// behind a reverse proxy that sets these headers; otherwise clients can
    /// spoof them to dodge the limit. Default: false (use the TCP peer address).
    pub trust_proxy: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 60,
            burst_size: 10,
            trust_proxy: false,
        }
    }
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

struct LimiterState {
    buckets: HashMap<Option<IpAddr>, Bucket>,
    last_sweep: Instant,
}

/// Token-bucket rate limiter, keyed by client IP.
///
/// State is per instance and shared by its clones, so each `RateLimiter`
/// you create is an independent limit. State lives in process memory; with
/// several server instances each one limits separately.
#[derive(Clone)]
pub struct RateLimiter {
    config: RateLimitConfig,
    state: Arc<Mutex<LimiterState>>,
}

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(LimiterState {
                buckets: HashMap::new(),
                last_sweep: Instant::now(),
            })),
        }
    }

    fn capacity(&self) -> f64 {
        self.config.burst_size.max(1) as f64
    }

    fn refill_per_sec(&self) -> f64 {
        self.config.requests_per_minute as f64 / 60.0
    }

    fn client_key(&self, req: &Request) -> Option<IpAddr> {
        if self.config.trust_proxy {
            // The right-most X-Forwarded-For entry is the one our proxy added.
            let forwarded = req
                .get_header("x-forwarded-for")
                .and_then(|v| v.rsplit(',').next())
                .or_else(|| req.get_header("x-real-ip"))
                .and_then(|ip| ip.trim().parse().ok());
            if forwarded.is_some() {
                return forwarded;
            }
        }
        req.remote_addr.map(|addr| addr.ip())
    }

    fn is_allowed(&self, key: Option<IpAddr>) -> bool {
        let now = Instant::now();
        let capacity = self.capacity();
        let rate = self.refill_per_sec();
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
            true
        } else {
            false
        }
    }
}

impl Middleware for RateLimiter {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        let allowed = self.is_allowed(self.client_key(&req));
        Box::pin(async move {
            if allowed {
                next.handle(req).await
            } else {
                Err(ServerError::TooManyRequests)
            }
        })
    }

    fn clone_box(&self) -> Box<dyn Middleware> {
        // Clones share the bucket state.
        Box::new(self.clone())
    }
}

#[derive(Clone)]
pub struct CorsConfig {
    /// Allowed origins, e.g. `https://app.example.com`, or `"*"` for any origin.
    pub allow_origins: Vec<String>,
    pub allow_methods: Vec<String>,
    pub allow_headers: Vec<String>,
    /// Cannot be combined with `"*"` in `allow_origins`.
    pub allow_credentials: bool,
    pub max_age: Option<u32>,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allow_origins: vec!["*".to_string()],
            allow_methods: vec!["GET".to_string(), "POST".to_string(), "PUT".to_string(), "DELETE".to_string(), "OPTIONS".to_string()],
            allow_headers: vec!["Content-Type".to_string(), "Authorization".to_string()],
            allow_credentials: false,
            max_age: Some(86400),
        }
    }
}

pub struct Cors {
    config: Arc<CorsConfig>,
    any_origin: bool,
}

impl Cors {
    /// # Panics
    ///
    /// If `allow_credentials` is set together with a `"*"` origin. Browsers
    /// reject that combination, and reflecting every origin instead would let
    /// any site make credentialed requests to your API.
    pub fn new(config: CorsConfig) -> Self {
        let any_origin = config.allow_origins.iter().any(|o| o == "*");
        assert!(
            !(any_origin && config.allow_credentials),
            "Invalid CORS configuration: allow_credentials cannot be used with a \"*\" origin; list the allowed origins explicitly"
        );
        Self { config: Arc::new(config), any_origin }
    }

    /// The `Access-Control-Allow-Origin` value for this request, if allowed.
    fn allow_origin(&self, origin: Option<&str>) -> Option<String> {
        if self.any_origin {
            return Some("*".to_string());
        }
        let origin = origin?;
        self.config
            .allow_origins
            .iter()
            .any(|o| o.eq_ignore_ascii_case(origin))
            .then(|| origin.to_string())
    }

    fn apply(&self, response: &mut Response, allow_origin: Option<&str>) {
        if !self.any_origin {
            // The response differs per Origin, so caches must key on it.
            response.append_vary("Origin");
        }
        if let Some(allow_origin) = allow_origin {
            response.header("Access-Control-Allow-Origin", allow_origin);
            if self.config.allow_credentials {
                response.header("Access-Control-Allow-Credentials", "true");
            }
        }
    }
}

impl Middleware for Cors {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        let cors = Cors { config: Arc::clone(&self.config), any_origin: self.any_origin };
        Box::pin(async move {
            let origin = req.get_header("origin").map(str::to_string);
            let allow_origin = cors.allow_origin(origin.as_deref());

            let is_preflight = req.method == Method::OPTIONS
                && origin.is_some()
                && req.get_header("access-control-request-method").is_some();
            if is_preflight {
                let mut response = Response::new(204);
                cors.apply(&mut response, allow_origin.as_deref());
                if allow_origin.is_some() {
                    response.header("Access-Control-Allow-Methods", cors.config.allow_methods.join(", "));
                    response.header("Access-Control-Allow-Headers", cors.config.allow_headers.join(", "));
                    if let Some(max_age) = cors.config.max_age {
                        response.header("Access-Control-Max-Age", max_age.to_string());
                    }
                }
                return Ok(response);
            }

            let mut response = next.handle(req).await?;
            if origin.is_some() || !cors.any_origin {
                cors.apply(&mut response, allow_origin.as_deref());
            }
            Ok(response)
        })
    }

    fn clone_box(&self) -> Box<dyn Middleware> {
        Box::new(Cors { config: Arc::clone(&self.config), any_origin: self.any_origin })
    }
}
