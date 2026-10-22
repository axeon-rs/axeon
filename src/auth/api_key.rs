//! API keys for public APIs (feature `api-keys`).

use super::{challenge, constant_time_eq};
use crate::error::ServerError;
use crate::message::Request;
use crate::middleware::{Middleware, MiddlewareResult, Next};
use base64::Engine;
use futures::future::BoxFuture;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The SHA-256 hash of an API key: what you store instead of the key.
///
/// Keys from [`generate_key`] carry 256 random bits, so a fast hash is
/// enough; a slow password hash (argon2, bcrypt) only matters for secrets
/// people choose.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyHash([u8; 32]);

impl KeyHash {
    /// The hash of a whole key, as sent by clients.
    pub fn of(key: &str) -> Self {
        KeyHash(Sha256::digest(key.as_bytes()).into())
    }

    /// Lowercase hex, 64 characters, e.g. for a `TEXT` column.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Reads [`to_hex`](KeyHash::to_hex) output.
    pub fn from_hex(hex: &str) -> Option<Self> {
        let bytes = hex.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, pair) in bytes.chunks(2).enumerate() {
            out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(KeyHash(out))
    }

    /// The raw 32 bytes, e.g. for a `BYTEA` column.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// From raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        KeyHash(bytes)
    }

    fn matches(&self, key: &str) -> bool {
        constant_time_eq(&self.0, &KeyHash::of(key).0)
    }
}

impl fmt::Debug for KeyHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyHash({})", self.to_hex())
    }
}

/// A key made by [`generate_key`]. Show [`key`](NewApiKey::key) to its
/// owner once; store only [`id`](NewApiKey::id) and
/// [`hash`](NewApiKey::hash).
#[derive(Clone)]
pub struct NewApiKey {
    /// The whole key, `<prefix>_<id>_<secret>`, for the client.
    pub key: String,
    /// The public part, to look the key up by (16 lowercase letters and
    /// digits). Safe to show in dashboards and logs.
    pub id: String,
    /// What to store to check the key.
    pub hash: KeyHash,
}

impl fmt::Debug for NewApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewApiKey").field("id", &self.id).field("hash", &self.hash).finish_non_exhaustive()
    }
}

/// Makes a new API key `<prefix>_<id>_<secret>` with a 256-bit secret,
/// e.g. `vs_live_k3x9...`. The prefix (lowercase letters, digits and
/// underscores, such as `vs_live` or `vs_test`) tells keys apart and lets
/// secret scanners recognize leaked ones.
///
/// # Panics
///
/// When the prefix is empty or has other characters, or the system's
/// random number generator fails.
pub fn generate_key(prefix: &str) -> NewApiKey {
    assert!(valid_prefix(prefix), "an API key prefix needs lowercase letters, digits and underscores: {:?}", prefix);
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut random = [0u8; 48];
    getrandom::fill(&mut random).expect("the system's random number generator failed");
    // Modulo bias doesn't matter for the ID; the secret carries the entropy.
    let id: String = random[..16].iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&random[16..]);
    let key = format!("{}_{}_{}", prefix, id, secret);
    NewApiKey { hash: KeyHash::of(&key), key, id }
}

fn valid_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && !prefix.starts_with('_')
        && !prefix.ends_with('_')
        && prefix.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// What [`ApiKeyAuth`]'s lookup returns for a key ID.
#[derive(Clone, Debug)]
pub struct ApiKeyRecord<T> {
    /// The stored hash of the key.
    pub hash: KeyHash,
    /// What the key may do, e.g. `catalog:read`.
    pub scopes: Vec<String>,
    /// Whoever the key belongs to, attached to the request as an extension.
    pub identity: T,
}

/// The key that authenticated a request, as a request extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKey {
    /// The key's ID (never the secret).
    pub id: String,
    /// Its scopes.
    pub scopes: Vec<String>,
}

impl ApiKey {
    /// Whether the key has `scope`.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

type Lookup<T> = Arc<dyn Fn(String) -> BoxFuture<'static, Result<Option<ApiKeyRecord<T>>, ServerError>> + Send + Sync>;
type Flush = Arc<dyn Fn(Vec<String>) -> BoxFuture<'static, ()> + Send + Sync>;
/// Key ID → record and when it was looked up.
type Cache<T> = Arc<Mutex<HashMap<String, (ApiKeyRecord<T>, Instant)>>>;

/// Most cached keys; the cache is emptied when full.
const CACHE_LIMIT: usize = 10_000;

/// Authenticates requests with API keys from [`generate_key`], sent as
/// `Authorization: Bearer <key>` or `X-API-Key: <key>`.
///
/// The key's ID is looked up with your async function (usually a database
/// query by ID), and the key is checked against the stored [`KeyHash`] in
/// constant time. On success the request gets an [`ApiKey`] extension
/// (ID and scopes) and an extension with the record's identity; otherwise
/// it gets 401.
///
/// ```rust
/// use axeon::auth::api_key::{ApiKey, ApiKeyAuth, ApiKeyRecord, KeyHash};
/// use axeon::{Server, ServerError};
/// use std::time::Duration;
///
/// #[derive(Clone)]
/// struct Org { id: i64 }
///
/// let mut app = Server::new();
/// app.middleware(
///     ApiKeyAuth::new("vs_live", |id: String| async move {
///         // SELECT hash, scopes, org_id FROM api_keys WHERE id = $1 AND revoked_at IS NULL
///         Ok::<_, ServerError>(Some(ApiKeyRecord {
///             hash: KeyHash::from_hex("...stored hash...").unwrap_or(KeyHash::from_bytes([0; 32])),
///             scopes: vec!["catalog:read".into()],
///             identity: Org { id: 7 },
///         }))
///     })
///     .on_used(Duration::from_secs(60), |ids| async move {
///         // UPDATE api_keys SET last_used_at = now() WHERE id = ANY($1)
///         let _ = ids;
///     }),
/// );
/// app.get("/v1/products", |req| async move {
///     let key = req.extension::<ApiKey>().unwrap();
///     if !key.has_scope("catalog:read") {
///         return Err(ServerError::Forbidden("needs catalog:read".into()));
///     }
///     Ok(format!("org {}", req.extension::<Org>().unwrap().id))
/// });
/// ```
///
/// Lookups are cached for 60 seconds by default
/// ([`cache_ttl`](ApiKeyAuth::cache_ttl)), so a revoked key keeps working
/// that long. Unknown IDs aren't cached; rate-limit by IP in front of this
/// middleware to keep guessing from reaching the database.
pub struct ApiKeyAuth<T> {
    prefix: String,
    lookup: Lookup<T>,
    header: Option<String>,
    cache_ttl: Duration,
    cache: Cache<T>,
    used: Option<Arc<Used>>,
}

struct Used {
    interval: Duration,
    flush: Flush,
    state: Mutex<(HashSet<String>, Instant)>,
}

impl<T: Clone + Send + Sync + 'static> ApiKeyAuth<T> {
    /// Accepts keys starting with `<prefix>_` (as made by
    /// [`generate_key`]), looking each key's ID up with `lookup`.
    ///
    /// # Panics
    ///
    /// When the prefix isn't valid for [`generate_key`].
    pub fn new<F, Fut>(prefix: &str, lookup: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<ApiKeyRecord<T>>, ServerError>> + Send + 'static,
    {
        assert!(valid_prefix(prefix), "an API key prefix needs lowercase letters, digits and underscores: {:?}", prefix);
        Self {
            prefix: format!("{}_", prefix),
            lookup: Arc::new(move |id| Box::pin(lookup(id))),
            header: Some("x-api-key".to_string()),
            cache_ttl: Duration::from_secs(60),
            cache: Arc::new(Mutex::new(HashMap::new())),
            used: None,
        }
    }

    /// Also accept the key in this header (default `X-API-Key`); `None`
    /// for `Authorization: Bearer` only.
    pub fn header(mut self, name: Option<&str>) -> Self {
        self.header = name.map(str::to_ascii_lowercase);
        self
    }

    /// How long a successful lookup is reused. `Duration::ZERO` looks every
    /// request up. Default: 60 seconds.
    pub fn cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// Reports used key IDs to `flush` at most once per `interval`, for a
    /// `last_used_at` column, instead of one write per request. Reported
    /// after the interval, on the next authenticated request.
    pub fn on_used<F, Fut>(mut self, interval: Duration, flush: F) -> Self
    where
        F: Fn(Vec<String>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.used = Some(Arc::new(Used {
            interval,
            flush: Arc::new(move |ids| Box::pin(flush(ids))),
            state: Mutex::new((HashSet::new(), Instant::now())),
        }));
        self
    }

    /// The key from `Authorization: Bearer` or the configured header.
    fn key<'r>(&self, req: &'r Request) -> Option<&'r str> {
        req.bearer_token().or_else(|| self.header.as_deref().and_then(|h| req.get_header(h)).map(str::trim))
    }

    /// The ID part of a key with this prefix.
    fn id<'k>(&self, key: &'k str) -> Option<&'k str> {
        let (id, secret) = key.strip_prefix(&self.prefix)?.split_once('_')?;
        let well_formed = !id.is_empty()
            && id.len() <= 64
            && id.bytes().all(|b| b.is_ascii_alphanumeric())
            && !secret.is_empty();
        well_formed.then_some(id)
    }

    fn cached(&self, id: &str) -> Option<ApiKeyRecord<T>> {
        if self.cache_ttl.is_zero() {
            return None;
        }
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.get(id).filter(|(_, at)| at.elapsed() < self.cache_ttl).map(|(record, _)| record.clone())
    }
}

impl Used {
    fn record(&self, id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0.insert(id.to_string());
        if state.1.elapsed() >= self.interval {
            let ids: Vec<String> = state.0.drain().collect();
            state.1 = Instant::now();
            tokio::spawn((self.flush)(ids));
        }
    }
}

impl<T: Clone + Send + Sync + 'static> Middleware for ApiKeyAuth<T> {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        let Some(key) = self.key(&req).map(str::to_string) else {
            return Box::pin(async { Ok(challenge("Bearer", "missing API key")) });
        };
        let invalid = || challenge("Bearer error=\"invalid_token\"", "invalid API key");
        let Some(id) = self.id(&key).map(str::to_string) else {
            return Box::pin(async move { Ok(invalid()) });
        };
        let cached = self.cached(&id);
        let lookup = Arc::clone(&self.lookup);
        let cache = Arc::clone(&self.cache);
        let cache_ttl = self.cache_ttl;
        let used = self.used.clone();
        Box::pin(async move {
            let record = match cached {
                Some(record) => record,
                None => match lookup(id.clone()).await? {
                    Some(record) => {
                        if !cache_ttl.is_zero() && record.hash.matches(&key) {
                            let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
                            if cache.len() >= CACHE_LIMIT {
                                cache.clear();
                            }
                            cache.insert(id.clone(), (record.clone(), Instant::now()));
                        }
                        record
                    }
                    None => return Ok(invalid()),
                },
            };
            if !record.hash.matches(&key) {
                return Ok(invalid());
            }
            if let Some(used) = used {
                used.record(&id);
            }
            req.insert_extension(ApiKey { id, scopes: record.scopes });
            req.insert_extension(record.identity);
            next.handle(req).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_parse_and_verify() {
        let new = generate_key("vs_live");
        assert!(new.key.starts_with("vs_live_"));
        assert_eq!(new.id.len(), 16);
        assert!(new.hash.matches(&new.key));
        assert!(!new.hash.matches(&format!("{}x", new.key)));
        assert_eq!(KeyHash::from_hex(&new.hash.to_hex()), Some(new.hash));
        assert_ne!(generate_key("vs_live").key, new.key);
        assert!(!format!("{:?}", new).contains(&new.key), "Debug must not print the secret");

        let auth = ApiKeyAuth::new("vs_live", |_id: String| async { Ok::<Option<ApiKeyRecord<()>>, ServerError>(None) });
        assert_eq!(auth.id(&new.key), Some(new.id.as_str()));
        for bad in ["vs_test_abc_def", "vs_live_", "vs_live_abc", "vs_live_abc_", "vs_live__def", "vs_live_a-b_c"] {
            assert_eq!(auth.id(bad), None, "{}", bad);
        }
    }

    #[test]
    fn prefixes_are_checked() {
        for good in ["vs", "vs_live", "acme2"] {
            assert!(valid_prefix(good));
        }
        for bad in ["", "_vs", "vs_", "VS", "vs-live", "vs live"] {
            assert!(!valid_prefix(bad), "{:?}", bad);
        }
    }
}
