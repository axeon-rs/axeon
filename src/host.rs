//! Routing by host name: separate routes for other domains and
//! subdomains ([`Server::host`](crate::Server::host)), and custom domains
//! looked up at runtime ([`ResolveHost`]).
//!
//! ```rust
//! use axeon::{Router, Server};
//!
//! let mut storefronts = Router::new();
//! storefronts.get("/", |req| async move {
//!     format!("store {}", req.subdomain().unwrap_or("?")) // "acme" for acme.stores.example.com
//! });
//!
//! let mut app = Server::new();
//! app.host("*.stores.example.com", storefronts);
//! app.get("/", |_req| async { "the main site" });              // every other host
//! ```

use crate::error::ServerError;
use crate::message::Request;
use crate::middleware::{Middleware, MiddlewareResult, Next};
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The host name of a request from its `Host` header (or the HTTP/2
/// `:authority`): lowercase, without port or trailing dot.
pub(crate) fn normalize(raw: &str) -> Option<String> {
    let raw = raw.trim();
    // An IPv6 literal keeps its brackets; otherwise drop the port.
    let host = if raw.starts_with('[') {
        &raw[..=raw.find(']')?]
    } else {
        raw.rsplit_once(':').map_or(raw, |(host, port)| if port.bytes().all(|b| b.is_ascii_digit()) { host } else { raw })
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    // Only what a host name or an IP literal can hold. The value comes from
    // the client, and ends up in `Request::subdomain` and in lookups.
    let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_');
    let ipv6 = |b: u8| b.is_ascii_hexdigit() || matches!(b, b'[' | b']' | b':' | b'.');
    let valid = if host.starts_with('[') { host.bytes().all(ipv6) } else { host.bytes().all(plain) };
    (!host.is_empty() && valid).then_some(host)
}

/// The request's host from its headers or URI.
pub(crate) fn host_of(headers: &http::HeaderMap, uri: Option<&http::Uri>) -> Option<String> {
    uri.and_then(|u| u.authority())
        .map(|a| a.as_str().to_string())
        .or_else(|| headers.get(http::header::HOST).and_then(|v| v.to_str().ok()).map(str::to_string))
        .and_then(|raw| normalize(&raw))
}

/// An exact host (`admin.example.com`) or one wildcard label in front
/// (`*.stores.example.com`, matching `acme.stores.example.com` but neither
/// `stores.example.com` nor `a.b.stores.example.com`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostPattern {
    Exact(String),
    Wildcard(String),
}

impl HostPattern {
    pub(crate) fn parse(pattern: &str) -> Result<HostPattern, String> {
        let pattern = pattern.trim().to_ascii_lowercase();
        let valid = |name: &str| {
            !name.is_empty()
                && name.split('.').all(|label| !label.is_empty() && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        };
        match pattern.strip_prefix("*.") {
            Some(suffix) if valid(suffix) => Ok(HostPattern::Wildcard(format!(".{}", suffix))),
            None if valid(&pattern) => Ok(HostPattern::Exact(pattern)),
            _ => Err(format!("invalid host pattern {:?}: expected `example.com` or `*.example.com`", pattern)),
        }
    }

    /// Whether `host` matches, and the wildcard label if any.
    pub(crate) fn matches<'h>(&self, host: &'h str) -> Option<Option<&'h str>> {
        match self {
            HostPattern::Exact(exact) => (host == exact).then_some(None),
            HostPattern::Wildcard(suffix) => {
                let label = host.strip_suffix(suffix.as_str())?;
                let valid = !label.is_empty() && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
                valid.then_some(Some(label))
            }
        }
    }
}

/// The label a wildcard host route matched, as a request extension.
#[derive(Clone, Debug)]
pub(crate) struct Subdomain(pub(crate) Arc<str>);

impl Request {
    /// The host name the client asked for (`Host` header or HTTP/2
    /// `:authority`): lowercase, without port. Behind a proxy, configure it
    /// to pass the original `Host` through.
    pub fn host(&self) -> Option<String> {
        host_of(&self.headers, self.uri.as_ref())
    }

    /// The label matched by a wildcard [`host`](crate::Server::host) route:
    /// `acme` for `acme.stores.example.com` on `*.stores.example.com`.
    pub fn subdomain(&self) -> Option<&str> {
        self.extension::<Subdomain>().map(|s| &*s.0)
    }
}

type Lookup<T> = Arc<dyn Fn(String) -> BoxFuture<'static, Result<Option<T>, ServerError>> + Send + Sync>;
/// Host → what the lookup returned and when.
type Cache<T> = Arc<Mutex<HashMap<String, (Option<T>, Instant)>>>;

/// Most cached hosts; when full, expired entries are dropped and new hosts
/// go uncached until some expire.
const CACHE_LIMIT: usize = 10_000;

/// Middleware for hosts known only at runtime, such as customers' own
/// domains for their storefronts: looks each request's host up with your
/// async function, attaches what it returns as a request extension, and
/// answers `404` for unknown hosts.
///
/// ```rust
/// use axeon::host::ResolveHost;
/// use axeon::{Server, ServerError};
///
/// #[derive(Clone)]
/// struct Storefront { id: i64, name: String }
///
/// let mut app = Server::new();
/// app.middleware(ResolveHost::new(|host: String| async move {
///     // SELECT id, name FROM storefronts WHERE domain = $1 AND verified
///     Ok::<_, ServerError>((host == "shop.acme.com").then(|| Storefront { id: 7, name: "Acme".into() }))
/// }));
/// app.get("/", |req| async move { req.extension::<Storefront>().map(|s| s.name.clone()) });
/// ```
///
/// Lookups, including unknown hosts, are cached for a minute
/// ([`cache_ttl`](ResolveHost::cache_ttl)), so a domain removed from the
/// database keeps working that long. Lookup errors aren't cached.
pub struct ResolveHost<T> {
    lookup: Lookup<T>,
    cache: Cache<T>,
    ttl: Duration,
    skip: Vec<String>,
}

impl<T: Clone + Send + Sync + 'static> ResolveHost<T> {
    /// Resolves hosts with `lookup`; `Ok(None)` means unknown (404).
    pub fn new<F, Fut>(lookup: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<T>, ServerError>> + Send + 'static,
    {
        Self {
            lookup: Arc::new(move |host| Box::pin(lookup(host))),
            cache: Arc::new(Mutex::new(HashMap::new())),
            ttl: Duration::from_secs(60),
            skip: Vec::new(),
        }
    }

    /// How long lookups are reused; `Duration::ZERO` looks up every
    /// request. Default: 60 seconds.
    pub fn cache_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Lets requests for these hosts (your own domains) through without a
    /// lookup or extension.
    pub fn skip<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.skip.extend(hosts.into_iter().filter_map(|h| normalize(h.as_ref())));
        self
    }

    fn cached(&self, host: &str) -> Option<Option<T>> {
        if self.ttl.is_zero() {
            return None;
        }
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.get(host).filter(|(_, at)| at.elapsed() < self.ttl).map(|(value, _)| value.clone())
    }
}

impl<T: Clone + Send + Sync + 'static> Middleware for ResolveHost<T> {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        let Some(host) = req.host() else {
            return Box::pin(async { Err(ServerError::BadRequest("missing Host header".into())) });
        };
        if self.skip.contains(&host) {
            return next.handle(req);
        }
        let cached = self.cached(&host);
        let lookup = Arc::clone(&self.lookup);
        let cache = Arc::clone(&self.cache);
        let ttl = self.ttl;
        Box::pin(async move {
            let found = match cached {
                Some(found) => found,
                None => {
                    let found = lookup(host.clone()).await?;
                    if !ttl.is_zero() {
                        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
                        if cache.len() >= CACHE_LIMIT {
                            // Make room by dropping what has expired. Never
                            // clear the lot: a flood of made-up Host values
                            // could then keep real domains out of the cache.
                            cache.retain(|_, (_, at)| at.elapsed() < ttl);
                        }
                        if cache.len() < CACHE_LIMIT {
                            cache.insert(host, (found.clone(), Instant::now()));
                        }
                    }
                    found
                }
            };
            match found {
                Some(value) => {
                    req.insert_extension(value);
                    next.handle(req).await
                }
                None => Err(ServerError::NotFound),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hosts() {
        assert_eq!(normalize("Shop.Example.COM:8443").as_deref(), Some("shop.example.com"));
        assert_eq!(normalize("example.com.").as_deref(), Some("example.com"));
        assert_eq!(normalize("[::1]:8080").as_deref(), Some("[::1]"));
        assert_eq!(normalize("127.0.0.1:3000").as_deref(), Some("127.0.0.1"));
        assert_eq!(normalize("  ").as_deref(), None);
        for bad in ["x/y.example.com", "a'b.example.com", "u@v.example.com", "a b.example.com", "ex%41mple.com", "a,b.example.com"] {
            assert_eq!(normalize(bad), None, "{:?}", bad);
        }
    }

    #[test]
    fn wildcard_labels_are_plain() {
        let wild = HostPattern::parse("*.stores.example.com").unwrap();
        assert_eq!(wild.matches("my-shop_2.stores.example.com"), Some(Some("my-shop_2")));
        for bad in ["x/y.stores.example.com", "a'b.stores.example.com", "u@v.stores.example.com", "<s>.stores.example.com"] {
            assert_eq!(wild.matches(bad), None, "{:?}", bad);
        }
    }

    #[test]
    fn patterns() {
        let exact = HostPattern::parse("Admin.Example.com").unwrap();
        assert_eq!(exact.matches("admin.example.com"), Some(None));
        assert_eq!(exact.matches("example.com"), None);
        let wild = HostPattern::parse("*.stores.example.com").unwrap();
        assert_eq!(wild.matches("acme.stores.example.com"), Some(Some("acme")));
        assert_eq!(wild.matches("stores.example.com"), None);
        assert_eq!(wild.matches("a.b.stores.example.com"), None);
        assert_eq!(wild.matches("evilstores.example.com"), None);
        for bad in ["", "*.", "*", "a..b", "exa mple.com", "*.*.example.com", "ex/ample.com"] {
            assert!(HostPattern::parse(bad).is_err(), "{:?}", bad);
        }
    }
}
