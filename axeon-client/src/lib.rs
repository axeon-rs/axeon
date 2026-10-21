//! An outbound HTTP client for [Axeon](https://docs.rs/axeon) apps, on
//! [reqwest](https://docs.rs/reqwest), for calling vendor APIs and fetching
//! URLs users provide:
//!
//! - **Egress control:** private, loopback and cloud-metadata addresses are
//!   refused by default, checked on resolved addresses, IP-literal URLs and
//!   every redirect, so user-supplied URLs can't reach internal services
//!   (SSRF). Vendor clients can be limited to their hosts.
//! - **Timeouts** on every request (10 s by default).
//! - **Retries** with exponential backoff and jitter, only when safe:
//!   connection failures for any request; timeouts, 429, 502, 503 and 504
//!   for idempotent ones (`GET`, `PUT`, `DELETE`..., or marked
//!   [`idempotent`](RequestBuilder::idempotent), or carrying an
//!   `Idempotency-Key`). `Retry-After` in seconds is respected.
//! - **A circuit breaker** per service (host and port): after 5 failures in a row, calls fail
//!   at once for 30 s instead of piling up.
//! - **Request IDs:** [`context`](RequestBuilder::context) passes the
//!   incoming request's `X-Request-Id` and `traceparent` on.
//! - **Metrics** per client on Axeon's [`Metrics`](axeon::metrics::Metrics).
//!
//! ```no_run
//! use axeon_client::{Client, Egress};
//! use std::time::Duration;
//!
//! # async fn run(req: axeon::Request) -> Result<(), axeon_client::Error> {
//! let kyb = Client::builder("kyb")
//!     .egress(Egress::hosts(["api.kyb-vendor.com"]))
//!     .timeout(Duration::from_secs(5))
//!     .build()?;
//!
//! let report: serde_json::Value = kyb
//!     .get("https://api.kyb-vendor.com/v1/companies/91310000MA1FL")
//!     .bearer_auth("token")
//!     .context(&req)
//!     .send()
//!     .await?
//!     .error_for_status()?
//!     .json()
//!     .await?;
//!
//! // For URLs from users: any public host, nothing internal.
//! let crawler = Client::builder("crawler").egress(axeon_client::Egress::public()).max_redirects(5).build()?;
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

mod breaker;
mod egress;

pub use egress::{Blocked, Egress};
pub use reqwest;
pub use reqwest::{Method, Response, StatusCode, Url};

use breaker::{Breaker, Permit};
use reqwest::header::{HeaderName, HeaderValue};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What can go wrong sending a request.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The [`Egress`] policy refused the destination.
    Blocked(Blocked),
    /// The service's circuit is open after repeated failures; try again in
    /// this long.
    CircuitOpen {
        /// The service: `host:port`.
        host: String,
        /// When it may be tried again.
        retry_in: Duration,
    },
    /// A request error from reqwest: connection, timeout, invalid URL,
    /// body or decoding errors, or an `error_for_status`.
    Request(reqwest::Error),
}

impl Error {
    /// Whether the request timed out.
    pub fn is_timeout(&self) -> bool {
        matches!(self, Error::Request(e) if e.is_timeout())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Blocked(blocked) => blocked.fmt(f),
            Error::CircuitOpen { host, retry_in } => {
                write!(f, "circuit open for {} after repeated failures; retry in {:?}", host, retry_in)
            }
            Error::Request(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Blocked(blocked) => Some(blocked),
            Error::CircuitOpen { .. } => None,
            Error::Request(error) => Some(error),
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        // A refusal from the resolver or the redirect policy arrives
        // wrapped in a reqwest error.
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
        while let Some(err) = source {
            if let Some(blocked) = err.downcast_ref::<Blocked>() {
                return Error::Blocked(blocked.clone());
            }
            source = err.source();
        }
        Error::Request(error)
    }
}

impl From<Error> for axeon::ServerError {
    /// A failed call to another service: 504 for timeouts, 503 for an open
    /// circuit, 502 otherwise. The details are logged, not sent.
    fn from(error: Error) -> Self {
        tracing::warn!(%error, "outbound request failed");
        let status = match &error {
            Error::CircuitOpen { .. } => 503,
            error if error.is_timeout() => 504,
            _ => 502,
        };
        axeon::ServerError::Status(status, "upstream service unavailable".into())
    }
}

/// Configures a [`Client`]; from [`Client::builder`].
pub struct ClientBuilder {
    name: String,
    egress: Egress,
    timeout: Duration,
    connect_timeout: Duration,
    attempts: u32,
    backoff: Duration,
    max_backoff: Duration,
    breaker: Option<(u32, Duration)>,
    max_redirects: usize,
    user_agent: Option<String>,
    #[cfg(feature = "metrics")]
    metrics: Option<axeon::metrics::Metrics>,
}

impl ClientBuilder {
    /// Which destinations are allowed. Default: [`Egress::public`].
    pub fn egress(mut self, egress: Egress) -> Self {
        self.egress = egress;
        self
    }

    /// The limit for each attempt, from connecting to the end of the
    /// response headers... and body, when read through the response.
    /// Default: 10 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The limit for connecting. Default: 3 seconds.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// How many times a request may be sent in all, when retrying is safe.
    /// Default: 3; `1` turns retries off.
    pub fn attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts.max(1);
        self
    }

    /// The first retry's delay, doubling each time up to `max`, with full
    /// jitter. Default: 200 ms up to 5 s.
    pub fn backoff(mut self, first: Duration, max: Duration) -> Self {
        self.backoff = first;
        self.max_backoff = max.max(first);
        self
    }

    /// Opens a host's circuit after `failures` failures in a row, for
    /// `cooldown`. Default: 5 failures, 30 seconds.
    pub fn circuit_breaker(mut self, failures: u32, cooldown: Duration) -> Self {
        self.breaker = Some((failures, cooldown));
        self
    }

    /// No circuit breaker.
    pub fn no_circuit_breaker(mut self) -> Self {
        self.breaker = None;
        self
    }

    /// How many redirects to follow, each checked against the egress
    /// policy. Default: 5; `0` returns redirects as responses.
    pub fn max_redirects(mut self, max: usize) -> Self {
        self.max_redirects = max;
        self
    }

    /// The `User-Agent` header.
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = Some(user_agent.into());
        self
    }

    /// Records metrics labelled with the client's name:
    /// `http_client_requests_total` (by `outcome`: `2xx`... `5xx`,
    /// `error`, `blocked`, `circuit_open`), `http_client_request_duration_seconds`,
    /// `http_client_retries_total` and `http_client_circuit_opened_total`.
    #[cfg(feature = "metrics")]
    pub fn metrics(mut self, metrics: &axeon::metrics::Metrics) -> Self {
        self.metrics = Some(metrics.clone());
        self
    }

    /// Builds the client.
    pub fn build(self) -> Result<Client, Error> {
        let egress = self.egress.clone();
        let max_redirects = self.max_redirects;
        let redirects = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > max_redirects {
                return attempt.error(Blocked(format!("more than {} redirects", max_redirects)));
            }
            match attempt.url().host_str().map(|host| egress.check_host(host)) {
                Some(Ok(())) => attempt.follow(),
                Some(Err(blocked)) => attempt.error(blocked),
                None => attempt.error(Blocked("redirect without a host".into())),
            }
        });
        let redirects = if self.max_redirects == 0 { reqwest::redirect::Policy::none() } else { redirects };
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(self.egress.resolver())
            .redirect(redirects)
            .timeout(self.timeout)
            .connect_timeout(self.connect_timeout);
        if let Some(user_agent) = &self.user_agent {
            builder = builder.user_agent(user_agent);
        }
        let inner = builder.build()?;
        Ok(Client {
            inner: Arc::new(Inner {
                http: inner,
                egress: self.egress,
                attempts: self.attempts,
                backoff: self.backoff,
                max_backoff: self.max_backoff,
                breaker: self.breaker.map(|(failures, cooldown)| Breaker::new(failures, cooldown)),
                #[cfg(feature = "metrics")]
                metrics: self.metrics.map(|m| Metrics::new(&m, &self.name)),
                name: self.name,
            }),
        })
    }
}

/// An HTTP client; cheap to clone, so make one per service and share it.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    name: String,
    egress: Egress,
    attempts: u32,
    backoff: Duration,
    max_backoff: Duration,
    breaker: Option<Breaker>,
    #[cfg(feature = "metrics")]
    metrics: Option<Metrics>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").field("name", &self.inner.name).field("egress", &self.inner.egress).finish_non_exhaustive()
    }
}

impl Client {
    /// Starts configuring a client named `name`, used in logs and metrics
    /// (e.g. `"qwen"`, `"kyb"`, `"crawler"`).
    pub fn builder(name: impl Into<String>) -> ClientBuilder {
        ClientBuilder {
            name: name.into(),
            egress: Egress::public(),
            timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(3),
            attempts: 3,
            backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(5),
            breaker: Some((5, Duration::from_secs(30))),
            max_redirects: 5,
            user_agent: None,
            #[cfg(feature = "metrics")]
            metrics: None,
        }
    }

    /// A request with `method` to `url`.
    pub fn request(&self, method: Method, url: impl reqwest::IntoUrl) -> RequestBuilder {
        RequestBuilder { client: self.clone(), builder: self.inner.http.request(method, url), idempotent: None }
    }

    /// A `GET` request.
    pub fn get(&self, url: impl reqwest::IntoUrl) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    /// A `POST` request.
    pub fn post(&self, url: impl reqwest::IntoUrl) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    /// A `PUT` request.
    pub fn put(&self, url: impl reqwest::IntoUrl) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    /// A `PATCH` request.
    pub fn patch(&self, url: impl reqwest::IntoUrl) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }

    /// A `DELETE` request.
    pub fn delete(&self, url: impl reqwest::IntoUrl) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }
}

/// A request being built; [`send`](RequestBuilder::send) it.
pub struct RequestBuilder {
    client: Client,
    builder: reqwest::RequestBuilder,
    idempotent: Option<bool>,
}

impl RequestBuilder {
    /// Adds a header.
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<axeon::http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<axeon::http::Error>,
    {
        self.builder = self.builder.header(key, value);
        self
    }

    /// `Authorization: Bearer <token>`.
    pub fn bearer_auth(mut self, token: impl fmt::Display) -> Self {
        self.builder = self.builder.bearer_auth(token);
        self
    }

    /// `Authorization: Basic ...`.
    pub fn basic_auth(mut self, user: impl fmt::Display, password: Option<impl fmt::Display>) -> Self {
        self.builder = self.builder.basic_auth(user, password);
        self
    }

    /// A JSON body.
    pub fn json<T: serde::Serialize + ?Sized>(mut self, body: &T) -> Self {
        self.builder = self.builder.json(body);
        self
    }

    /// A URL-encoded form body.
    pub fn form<T: serde::Serialize + ?Sized>(mut self, form: &T) -> Self {
        self.builder = self.builder.form(form);
        self
    }

    /// Adds query parameters.
    pub fn query<T: serde::Serialize + ?Sized>(mut self, query: &T) -> Self {
        self.builder = self.builder.query(query);
        self
    }

    /// A raw body.
    pub fn body(mut self, body: impl Into<reqwest::Body>) -> Self {
        self.builder = self.builder.body(body);
        self
    }

    /// Overrides the client's per-attempt timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.builder = self.builder.timeout(timeout);
        self
    }

    /// Anything else reqwest's builder offers.
    pub fn with(mut self, f: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder) -> Self {
        self.builder = f(self.builder);
        self
    }

    /// Whether repeating this request is harmless, which allows retrying
    /// it after timeouts and 5xx responses. Default: true for `GET`,
    /// `HEAD`, `OPTIONS`, `PUT` and `DELETE`, and for requests with an
    /// `Idempotency-Key` header.
    pub fn idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = Some(idempotent);
        self
    }

    /// Passes on the incoming request's ID (from
    /// [`SetRequestId`](axeon::middleware::SetRequestId) or its
    /// `X-Request-Id` header) and its `traceparent` / `tracestate`, so the
    /// call can be followed across services.
    pub fn context(mut self, request: &axeon::Request) -> Self {
        let id = request
            .extension::<axeon::middleware::RequestId>()
            .map(|id| id.0.clone())
            .or_else(|| request.get_header("x-request-id").map(str::to_string));
        if let Some(id) = id {
            self = self.request_id(&id);
        }
        for name in ["traceparent", "tracestate"] {
            if let Some(value) = request.get_header(name) {
                self.builder = self.builder.header(name, value);
            }
        }
        self
    }

    /// Sets `X-Request-Id`, e.g. from a background job.
    pub fn request_id(mut self, id: &str) -> Self {
        self.builder = self.builder.header("x-request-id", id);
        self
    }

    /// Sends the request, retrying when safe. Any response is `Ok`, 4xx and
    /// 5xx included (after retries); use `error_for_status` to turn those
    /// into errors.
    pub async fn send(self) -> Result<Response, Error> {
        let RequestBuilder { client, builder, idempotent } = self;
        let inner = &client.inner;
        let request = builder.build()?;
        let host = request.url().host_str().unwrap_or_default().to_string();
        // Circuits are per service: host and port.
        let service = match request.url().port_or_known_default() {
            Some(port) => format!("{}:{}", host, port),
            None => host.clone(),
        };
        let started = Instant::now();
        let result = send(inner, request, &host, &service, idempotent).await;
        #[cfg(feature = "metrics")]
        if let Some(metrics) = &inner.metrics {
            metrics.finish(&result, started.elapsed());
        }
        #[cfg(not(feature = "metrics"))]
        let _ = started;
        result
    }
}

fn is_idempotent(request: &reqwest::Request, idempotent: Option<bool>) -> bool {
    idempotent.unwrap_or_else(|| {
        matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS | Method::PUT | Method::DELETE | Method::TRACE)
            || request.headers().contains_key("idempotency-key")
    })
}

async fn send(inner: &Inner, request: reqwest::Request, host: &str, service: &str, idempotent: Option<bool>) -> Result<Response, Error> {
    inner.egress.check_host(host).map_err(Error::Blocked)?;
    let idempotent = is_idempotent(&request, idempotent);
    let mut pending = Some(request);
    let mut attempt = 1;
    loop {
        if let Some(breaker) = &inner.breaker {
            if let Permit::Open(retry_in) = breaker.before(service) {
                return Err(Error::CircuitOpen { host: service.to_string(), retry_in });
            }
        }
        let current = pending.take().expect("a request to send");
        // Keep a copy for a retry; streaming bodies can't be copied.
        let spare = if attempt < inner.attempts { current.try_clone() } else { None };
        let result = inner.http.execute(current).await.map_err(Error::from);

        let failed = match &result {
            Ok(response) => response.status().is_server_error(),
            Err(Error::Blocked(_)) => return result,
            Err(_) => true,
        };
        if let Some(breaker) = &inner.breaker {
            if breaker.after(service, failed) {
                tracing::warn!(client = %inner.name, service, "circuit opened after repeated failures");
                #[cfg(feature = "metrics")]
                if let Some(metrics) = &inner.metrics {
                    metrics.circuit_opened.inc();
                }
            }
        }

        let retry = match &result {
            Ok(response) => {
                let status = response.status().as_u16();
                (matches!(status, 429 | 502 | 503 | 504) && idempotent).then(|| retry_after(response))
            }
            Err(Error::Request(error)) if error.is_connect() => Some(None),
            Err(Error::Request(error)) if error.is_timeout() && idempotent => Some(None),
            Err(_) => None,
        };
        let (Some(retry_after), Some(spare)) = (retry, spare) else { return result };
        // The circuit just opened: give back what happened, not a retry
        // that would only fail with `CircuitOpen`.
        if inner.breaker.as_ref().is_some_and(|breaker| breaker.is_open(service)) {
            return result;
        }
        let delay = match retry_after {
            // The server says when; give up if that's longer than our backoff allows.
            Some(after) if after > inner.max_backoff => return result,
            Some(after) => after,
            None => jitter(inner.backoff.saturating_mul(1 << (attempt - 1).min(16)).min(inner.max_backoff)),
        };
        tracing::debug!(client = %inner.name, host, attempt, ?delay, "retrying request");
        #[cfg(feature = "metrics")]
        if let Some(metrics) = &inner.metrics {
            metrics.retries.inc();
        }
        tokio::time::sleep(delay).await;
        pending = Some(spare);
        attempt += 1;
    }
}

/// `Retry-After` in seconds, if given.
fn retry_after(response: &Response) -> Option<Duration> {
    let value = response.headers().get("retry-after")?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// A random delay between 0 and `max` ("full jitter").
fn jitter(max: Duration) -> Duration {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    max.mul_f64(f64::from(nanos % 10_000) / 10_000.0)
}

#[cfg(feature = "metrics")]
struct Metrics {
    client: String,
    requests: axeon::metrics::CounterVec,
    duration: axeon::metrics::Histogram,
    retries: axeon::metrics::Counter,
    circuit_opened: axeon::metrics::Counter,
}

#[cfg(feature = "metrics")]
impl Metrics {
    fn new(metrics: &axeon::metrics::Metrics, client: &str) -> Self {
        let requests = metrics.counter_vec("http_client_requests_total", "Outbound HTTP requests, by outcome.", &["client", "outcome"]);
        let duration = metrics.histogram_vec(
            "http_client_request_duration_seconds",
            "Outbound HTTP request duration, including retries.",
            &["client"],
            &axeon::metrics::DEFAULT_BUCKETS,
        );
        let retries = metrics.counter_vec("http_client_retries_total", "Outbound HTTP retries.", &["client"]);
        let opened = metrics.counter_vec("http_client_circuit_opened_total", "Times a circuit opened.", &["client"]);
        Self {
            client: client.to_string(),
            requests,
            duration: duration.with(&[client]),
            retries: retries.with(&[client]),
            circuit_opened: opened.with(&[client]),
        }
    }

    fn finish(&self, result: &Result<Response, Error>, elapsed: Duration) {
        let outcome = match result {
            Ok(response) => match response.status().as_u16() {
                100..=199 => "1xx",
                200..=299 => "2xx",
                300..=399 => "3xx",
                400..=499 => "4xx",
                _ => "5xx",
            },
            Err(Error::Blocked(_)) => "blocked",
            Err(Error::CircuitOpen { .. }) => "circuit_open",
            Err(Error::Request(_)) => "error",
        };
        self.requests.with(&[&self.client, outcome]).inc();
        self.duration.observe(elapsed.as_secs_f64());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_bounds() {
        for _ in 0..100 {
            assert!(jitter(Duration::from_millis(100)) <= Duration::from_millis(100));
        }
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }
}
