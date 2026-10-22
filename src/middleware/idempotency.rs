//! `Idempotency-Key` support (feature `idempotency`): retried requests get
//! the first response instead of running twice.

use super::{Middleware, MiddlewareResult, Next};
use crate::body::BoxError;
use crate::error::ServerError;
use crate::message::{Method, Request, Response};
use base64::Engine;
use bytes::Bytes;
use futures::future::BoxFuture;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A response kept for replaying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredResponse {
    /// The status code.
    pub status: u16,
    /// The headers, without per-response ones (`Date`, `Set-Cookie`,
    /// `Content-Length`, `X-Request-Id`).
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Bytes,
}

/// Headers that belong to one response and are not replayed.
const NOT_REPLAYED: [&str; 4] = ["date", "set-cookie", "content-length", "x-request-id"];

impl StoredResponse {
    fn capture(response: &Response) -> Self {
        let headers = response
            .headers
            .iter()
            .filter(|(name, _)| !NOT_REPLAYED.contains(&name.as_str()))
            .filter_map(|(name, value)| Some((name.as_str().to_string(), value.to_str().ok()?.to_string())))
            .collect();
        StoredResponse { status: response.status, headers, body: response.body.clone() }
    }

    fn replay(&self) -> Response {
        let mut response = Response::new(self.status);
        response.body = self.body.clone();
        for (name, value) in &self.headers {
            response.header(name, value);
        }
        response.header("Idempotent-Replayed", "true");
        response
    }

    /// A compact serialization (JSON, body in base64), for stores that
    /// keep bytes.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::json!({
            "s": self.status,
            "h": self.headers,
            "b": base64::engine::general_purpose::STANDARD.encode(&self.body),
        })
        .to_string()
        .into_bytes()
    }

    /// Reads [`encode`](StoredResponse::encode) output.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let doc: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        let status = u16::try_from(doc.get("s")?.as_u64()?).ok()?;
        let headers = serde_json::from_value(doc.get("h")?.clone()).ok()?;
        let body = base64::engine::general_purpose::STANDARD.decode(doc.get("b")?.as_str()?).ok()?;
        Some(StoredResponse { status, headers, body: body.into() })
    }
}

/// What [`IdempotencyStore::begin`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Begin {
    /// No live record: this request now holds the key and runs.
    Started,
    /// The first request with this key and fingerprint is still running.
    InProgress,
    /// It finished; replay its response.
    Completed(StoredResponse),
    /// The key was used with a different request.
    Mismatch,
}

/// Where idempotency records live. [`MemoryIdempotencyStore`] keeps them
/// in this process; `axeon_redis::RedisIdempotencyStore` and
/// `axeon_orm::idempotency::PgIdempotencyStore` share them between
/// instances, which retries behind a load balancer need.
///
/// Keys arrive already hashed with the caller's scope.
pub trait IdempotencyStore: Send + Sync + 'static {
    /// Claims `key` for a request with `fingerprint`, unless a live record
    /// exists. A claim expires after `lock_ttl` if never completed, so a
    /// crashed instance doesn't block retries forever.
    fn begin<'a>(&'a self, key: &'a str, fingerprint: &'a str, lock_ttl: Duration) -> BoxFuture<'a, Result<Begin, BoxError>>;

    /// Stores the response for the claim, kept for `ttl`.
    fn complete<'a>(
        &'a self,
        key: &'a str,
        fingerprint: &'a str,
        response: &'a StoredResponse,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<(), BoxError>>;

    /// Drops an unfinished claim, so the request can be retried (after a
    /// server error, or a response that can't be stored).
    fn release<'a>(&'a self, key: &'a str, fingerprint: &'a str) -> BoxFuture<'a, Result<(), BoxError>>;
}

/// Idempotency records in this process's memory. Expired records are
/// dropped as new ones arrive.
#[derive(Default)]
pub struct MemoryIdempotencyStore {
    records: Mutex<HashMap<String, Record>>,
}

struct Record {
    fingerprint: String,
    response: Option<StoredResponse>,
    expires: Instant,
}

impl MemoryIdempotencyStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdempotencyStore for MemoryIdempotencyStore {
    fn begin<'a>(&'a self, key: &'a str, fingerprint: &'a str, lock_ttl: Duration) -> BoxFuture<'a, Result<Begin, BoxError>> {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        records.retain(|_, r| r.expires > now);
        let begin = match records.get(key) {
            Some(r) if r.fingerprint != fingerprint => Begin::Mismatch,
            Some(Record { response: Some(response), .. }) => Begin::Completed(response.clone()),
            Some(_) => Begin::InProgress,
            None => {
                let record = Record { fingerprint: fingerprint.to_string(), response: None, expires: now + lock_ttl };
                records.insert(key.to_string(), record);
                Begin::Started
            }
        };
        Box::pin(async move { Ok(begin) })
    }

    fn complete<'a>(
        &'a self,
        key: &'a str,
        fingerprint: &'a str,
        response: &'a StoredResponse,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<(), BoxError>> {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = records.get_mut(key).filter(|r| r.fingerprint == fingerprint && r.response.is_none()) {
            r.response = Some(response.clone());
            r.expires = Instant::now() + ttl;
        }
        Box::pin(async { Ok(()) })
    }

    fn release<'a>(&'a self, key: &'a str, fingerprint: &'a str) -> BoxFuture<'a, Result<(), BoxError>> {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if records.get(key).is_some_and(|r| r.fingerprint == fingerprint && r.response.is_none()) {
            records.remove(key);
        }
        Box::pin(async { Ok(()) })
    }
}

type ScopeFn = Arc<dyn Fn(&Request) -> Option<String> + Send + Sync>;

/// Makes unsafe requests safe to retry: a request with an
/// `Idempotency-Key` header that repeats an earlier one gets the earlier
/// response (with `Idempotent-Replayed: true`) instead of running again.
///
/// ```rust
/// use axeon::Server;
/// use axeon::middleware::idempotency::Idempotency;
///
/// let mut app = Server::new();
/// // After authentication, before Transactional: the response is stored
/// // once the transaction has committed.
/// app.middleware(Idempotency::new());
/// app.post("/v1/selections", |_req| async { (201, "created") });
/// ```
///
/// - Only `POST` and `PATCH` by default ([`methods`](Idempotency::methods));
///   requests without the header run normally unless
///   [`required`](Idempotency::required).
/// - Keys are per caller: hashed together with the `Authorization` and
///   `X-API-Key` headers, or with [`scope_by`](Idempotency::scope_by), so
///   one client can never receive another's response.
/// - The same key with a different method, path, query or body gets
///   `422`. While the first request runs, repeats get `409` with
///   `Retry-After`.
/// - Responses below 500 are kept for 24 hours ([`ttl`](Idempotency::ttl)).
///   Server errors and streamed responses aren't kept, so the client can
///   retry.
/// - With [`MemoryIdempotencyStore`] (the default) records live in this
///   process; use a shared [`store`](Idempotency::store) behind a load
///   balancer.
/// - If the store fails, requests with a key get `503`, since running them
///   unguarded could do the work twice; see
///   [`fail_open`](Idempotency::fail_open).
#[derive(Clone)]
pub struct Idempotency {
    store: Arc<dyn IdempotencyStore>,
    methods: Vec<Method>,
    ttl: Duration,
    lock_ttl: Duration,
    required: bool,
    fail_open: bool,
    scope: Option<ScopeFn>,
}

impl Default for Idempotency {
    fn default() -> Self {
        Self::new()
    }
}

impl Idempotency {
    /// `POST` and `PATCH`, records in memory for 24 hours.
    pub fn new() -> Self {
        Self {
            store: Arc::new(MemoryIdempotencyStore::new()),
            methods: vec![Method::POST, Method::PATCH],
            ttl: Duration::from_secs(24 * 3600),
            lock_ttl: Duration::from_secs(60),
            required: false,
            fail_open: false,
            scope: None,
        }
    }

    /// Where records live.
    pub fn store<S: IdempotencyStore>(mut self, store: S) -> Self {
        self.store = Arc::new(store);
        self
    }

    /// The methods it applies to.
    pub fn methods(mut self, methods: impl IntoIterator<Item = Method>) -> Self {
        self.methods = methods.into_iter().collect();
        self
    }

    /// How long a completed response is replayed. Default: 24 hours.
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// How long a running request holds its key if it never finishes (the
    /// instance crashed). Set it above your longest handler timeout.
    /// Default: 60 seconds.
    pub fn lock_ttl(mut self, ttl: Duration) -> Self {
        self.lock_ttl = ttl;
        self
    }

    /// Rejects requests without `Idempotency-Key` with `400`.
    pub fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// Lets requests run without protection when the store fails, instead
    /// of answering `503`.
    pub fn fail_open(mut self, open: bool) -> Self {
        self.fail_open = open;
        self
    }

    /// Scopes keys to the caller with your own identifier (e.g. the API key
    /// or user ID from an earlier middleware), instead of the
    /// `Authorization` and `X-API-Key` headers. `None` falls back to them.
    pub fn scope_by<F>(mut self, scope: F) -> Self
    where
        F: Fn(&Request) -> Option<String> + Send + Sync + 'static,
    {
        self.scope = Some(Arc::new(scope));
        self
    }

    /// The store key: the client's key hashed with the caller's scope.
    fn key(&self, req: &Request, key: &str) -> String {
        let mut hasher = Sha256::new();
        match self.scope.as_ref().and_then(|scope| scope(req)) {
            Some(scope) => {
                hasher.update(b"scope\0");
                hasher.update(scope.as_bytes());
            }
            None => {
                for header in ["authorization", "x-api-key"] {
                    hasher.update(header.as_bytes());
                    hasher.update(b"\0");
                    hasher.update(req.get_header(header).unwrap_or_default().as_bytes());
                    hasher.update(b"\0");
                }
            }
        }
        hasher.update(b"\0key\0");
        hasher.update(key.as_bytes());
        hex(&hasher.finalize())
    }
}

/// What the request does: method, path, query and body.
fn fingerprint(req: &Request) -> String {
    let mut hasher = Sha256::new();
    hasher.update(req.method().as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(req.path().as_bytes());
    let mut query: Vec<_> = req.query().iter().collect();
    query.sort();
    for (name, value) in query {
        hasher.update(b"\0");
        hasher.update(name.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
    }
    hasher.update(b"\0\0");
    hasher.update(req.body());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// A usable `Idempotency-Key`: 1 to 255 visible ASCII characters.
fn valid_key(key: &str) -> bool {
    (1..=255).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_graphic())
}

impl Middleware for Idempotency {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        if !self.methods.contains(&req.method()) {
            return next.handle(req);
        }
        let Some(client_key) = req.get_header("idempotency-key").map(str::trim) else {
            if self.required {
                let error = ServerError::BadRequest("an Idempotency-Key header is required".into());
                return Box::pin(async move { Err(error) });
            }
            return next.handle(req);
        };
        if !valid_key(client_key) {
            let error = ServerError::BadRequest("Idempotency-Key must be 1 to 255 visible ASCII characters".into());
            return Box::pin(async move { Err(error) });
        }
        // A streamed body can't be fingerprinted before the handler reads it.
        if req.extensions().get::<crate::message::convert::PendingBody>().is_some() {
            tracing::debug!("Idempotency-Key ignored on a streamed-body route");
            return next.handle(req);
        }
        let key = self.key(&req, client_key);
        let fingerprint = fingerprint(&req);
        let store = Arc::clone(&self.store);
        let (ttl, lock_ttl, fail_open) = (self.ttl, self.lock_ttl, self.fail_open);
        Box::pin(async move {
            let begin = match store.begin(&key, &fingerprint, lock_ttl).await {
                Ok(begin) => begin,
                Err(error) if fail_open => {
                    tracing::warn!(%error, "idempotency store failed; running the request unprotected");
                    return next.handle(req).await;
                }
                Err(error) => {
                    tracing::error!(%error, "idempotency store failed");
                    return Err(ServerError::Status(503, "idempotency store unavailable".into()));
                }
            };
            match begin {
                Begin::Completed(stored) => return Ok(stored.replay()),
                Begin::Mismatch => {
                    return Err(ServerError::Status(
                        422,
                        "this Idempotency-Key was used with a different request".into(),
                    ))
                }
                Begin::InProgress => {
                    let mut response =
                        Response::error(ServerError::Conflict("a request with this Idempotency-Key is still running".into()));
                    response.header("Retry-After", "1");
                    return Ok(response);
                }
                Begin::Started => {}
            }
            let result = next.handle(req).await;
            let stored = match &result {
                Ok(response) if response.status < 500 && !response.is_streaming() => Some(StoredResponse::capture(response)),
                _ => None,
            };
            let recorded = match &stored {
                Some(stored) => store.complete(&key, &fingerprint, stored, ttl).await,
                None => store.release(&key, &fingerprint).await,
            };
            if let Err(error) = recorded {
                // The claim expires after `lock_ttl`; until then retries get 409.
                tracing::error!(%error, "couldn't record the idempotent response");
            }
            result
        })
    }
}

/// Checks an [`IdempotencyStore`]'s behaviour, for store crates' tests.
/// Keys start with `prefix`, so runs don't collide.
#[doc(hidden)]
pub mod conformance {
    use super::*;

    /// Panics on the first wrong answer.
    pub async fn check<S: IdempotencyStore>(store: &S, prefix: &str) {
        let key = |name: &str| format!("{}{}", prefix, name);
        let lock = Duration::from_secs(30);
        let ttl = Duration::from_secs(60);
        let response = StoredResponse {
            status: 201,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Bytes::from_static(b"{\"id\":7}"),
        };

        // Claim, repeat while running, complete, replay.
        let a = key("a");
        assert_eq!(store.begin(&a, "fp1", lock).await.unwrap(), Begin::Started);
        assert_eq!(store.begin(&a, "fp1", lock).await.unwrap(), Begin::InProgress);
        assert_eq!(store.begin(&a, "fp2", lock).await.unwrap(), Begin::Mismatch);
        store.complete(&a, "fp1", &response, ttl).await.unwrap();
        assert_eq!(store.begin(&a, "fp1", lock).await.unwrap(), Begin::Completed(response.clone()));
        assert_eq!(store.begin(&a, "fp2", lock).await.unwrap(), Begin::Mismatch);
        // Completing or releasing again, or with another fingerprint, changes nothing.
        store.release(&a, "fp1").await.unwrap();
        store.complete(&a, "fp2", &StoredResponse { status: 500, ..response.clone() }, ttl).await.unwrap();
        assert_eq!(store.begin(&a, "fp1", lock).await.unwrap(), Begin::Completed(response.clone()));

        // Releasing frees the key for a retry, even with another request.
        let b = key("b");
        assert_eq!(store.begin(&b, "fp1", lock).await.unwrap(), Begin::Started);
        store.release(&b, "fp1").await.unwrap();
        assert_eq!(store.begin(&b, "fp2", lock).await.unwrap(), Begin::Started);

        // Claims and records expire.
        let c = key("c");
        let short = Duration::from_millis(300);
        assert_eq!(store.begin(&c, "fp1", short).await.unwrap(), Begin::Started);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(store.begin(&c, "fp2", short).await.unwrap(), Begin::Started);
        store.complete(&c, "fp2", &response, short).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(store.begin(&c, "fp3", lock).await.unwrap(), Begin::Started);

        // Only one of many concurrent claims wins.
        let d = key("d");
        let claims = futures::future::join_all((0..8).map(|_| store.begin(&d, "fp1", lock))).await;
        let started = claims.iter().filter(|c| matches!(c, Ok(Begin::Started))).count();
        assert_eq!(started, 1, "{:?}", claims);
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_conforms() {
        conformance::check(&MemoryIdempotencyStore::new(), "").await;
    }

    #[test]
    fn stored_responses_round_trip() {
        let stored = StoredResponse {
            status: 201,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Bytes::from_static(b"{\"id\":1}\xff"),
        };
        assert_eq!(StoredResponse::decode(&stored.encode()), Some(stored));
        assert_eq!(StoredResponse::decode(b"nope"), None);
    }

    #[test]
    fn keys_are_checked() {
        assert!(valid_key("3f2c-uuid"));
        assert!(!valid_key(""));
        assert!(!valid_key("has space"));
        assert!(!valid_key(&"x".repeat(256)));
    }
}
