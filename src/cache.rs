//! HTTP caching for API responses: `Cache-Control`, `ETag` and
//! `Last-Modified`, and `304 Not Modified` for conditional requests.
//!
//! The simplest setup is the [`ConditionalGet`] middleware: it gives
//! successful `GET` responses an ETag and answers `If-None-Match` with 304,
//! saving bandwidth (the handler still runs). To also skip the work, check
//! freshness in the handler with a cheap validator, such as a version or
//! `updated_at` column:
//!
//! ```rust
//! use axeon::cache::{CacheControl, ETag};
//! use axeon::{Response, Server};
//! use std::time::Duration;
//!
//! let mut app = Server::new();
//! app.get("/v1/products/:id", |req| async move {
//!     let version = 42; // e.g. SELECT version FROM products WHERE id = $1
//!     let etag = ETag::strong(format!("p{}-v{}", req.param::<i64>("id")?, version));
//!     if req.is_fresh(Some(&etag), None) {
//!         return Ok(Response::not_modified(&etag));
//!     }
//!     let body = serde_json::json!({ "id": 1, "version": version }); // the expensive part
//!     Ok::<_, axeon::ServerError>(
//!         Response::json(&body)
//!             .with_etag(&etag)
//!             .with_cache_control(CacheControl::new().private().max_age(Duration::from_secs(60))),
//!     )
//! });
//! ```

use crate::message::{Method, Request, Response};
use crate::middleware::{Middleware, MiddlewareResult, Next};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A `Cache-Control` header value.
///
/// ```rust
/// use axeon::cache::CacheControl;
/// use std::time::Duration;
///
/// let cc = CacheControl::new().public().max_age(Duration::from_secs(300)).stale_while_revalidate(Duration::from_secs(60));
/// assert_eq!(cc.to_string(), "public, max-age=300, stale-while-revalidate=60");
/// assert_eq!(CacheControl::revalidate().to_string(), "no-cache");
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheControl {
    directives: Vec<String>,
}

impl CacheControl {
    /// No directives yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// `no-cache`: caches may store the response but must revalidate it
    /// (with its ETag) before every use. The usual choice for API data that
    /// must never be stale.
    pub fn revalidate() -> Self {
        Self::new().no_cache()
    }

    /// `no-store`: never store it (personal or sensitive data).
    pub fn never() -> Self {
        Self::new().no_store()
    }

    fn with(mut self, directive: impl Into<String>) -> Self {
        self.directives.push(directive.into());
        self
    }

    /// `public`: shared caches (CDNs, proxies) may store it, even for
    /// authenticated requests.
    pub fn public(self) -> Self {
        self.with("public")
    }

    /// `private`: only the client's own cache may store it.
    pub fn private(self) -> Self {
        self.with("private")
    }

    /// `no-cache`: revalidate before every use.
    pub fn no_cache(self) -> Self {
        self.with("no-cache")
    }

    /// `no-store`: never store.
    pub fn no_store(self) -> Self {
        self.with("no-store")
    }

    /// `max-age`: fresh for this long.
    pub fn max_age(self, age: Duration) -> Self {
        self.with(format!("max-age={}", age.as_secs()))
    }

    /// `s-maxage`: fresh for this long in shared caches.
    pub fn s_maxage(self, age: Duration) -> Self {
        self.with(format!("s-maxage={}", age.as_secs()))
    }

    /// `must-revalidate`: once stale, never use without revalidating.
    pub fn must_revalidate(self) -> Self {
        self.with("must-revalidate")
    }

    /// `immutable`: it never changes while fresh (versioned URLs).
    pub fn immutable(self) -> Self {
        self.with("immutable")
    }

    /// `stale-while-revalidate`: may be used this long after going stale
    /// while a fresh copy is fetched in the background.
    pub fn stale_while_revalidate(self, window: Duration) -> Self {
        self.with(format!("stale-while-revalidate={}", window.as_secs()))
    }
}

impl fmt::Display for CacheControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.directives.join(", "))
    }
}

/// An entity tag: identifies one version of a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ETag {
    value: String,
    weak: bool,
}

impl ETag {
    /// A strong tag: the bytes are identical whenever the tag is. Quotes
    /// and other characters ETags can't hold are dropped from `value`.
    pub fn strong(value: impl AsRef<str>) -> Self {
        ETag { value: clean(value.as_ref()), weak: false }
    }

    /// A weak tag: the content is equivalent whenever the tag is (e.g. the
    /// same data, maybe compressed or formatted differently).
    pub fn weak(value: impl AsRef<str>) -> Self {
        ETag { value: clean(value.as_ref()), weak: true }
    }

    /// A weak tag from a body's length and a 64-bit FNV-1a hash, as
    /// [`ConditionalGet`] makes them.
    pub fn of(body: &[u8]) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in body {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        ETag::weak(format!("{:x}-{:016x}", body.len(), hash))
    }

    /// Whether this is a weak tag.
    pub fn is_weak(&self) -> bool {
        self.weak
    }

    /// The header value: `"value"` or `W/"value"`.
    pub fn header_value(&self) -> String {
        if self.weak {
            format!("W/\"{}\"", self.value)
        } else {
            format!("\"{}\"", self.value)
        }
    }
}

impl fmt::Display for ETag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.header_value())
    }
}

/// Keeps the characters an ETag may hold (visible ASCII, no quotes).
fn clean(value: &str) -> String {
    value.chars().filter(|c| c.is_ascii_graphic() && *c != '"').collect()
}

/// Weak comparison of an `If-None-Match` list against an ETag header value.
pub(crate) fn etag_matches(header: &str, etag: &str) -> bool {
    let bare = |tag: &str| tag.trim().trim_start_matches("W/").to_string();
    header.trim() == "*" || tags(header).any(|tag| bare(tag) == bare(etag))
}

/// The entity tags in a comma-separated list. A quoted tag may itself
/// contain commas, so only commas outside quotes separate tags.
fn tags(list: &str) -> impl Iterator<Item = &str> {
    let mut rest = list;
    std::iter::from_fn(move || {
        if rest.trim().is_empty() {
            return None;
        }
        let mut quoted = false;
        let mut end = rest.len();
        for (i, c) in rest.char_indices() {
            match c {
                '"' => quoted = !quoted,
                ',' if !quoted => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        let tag = &rest[..end];
        rest = rest.get(end + 1..).unwrap_or("");
        Some(tag)
    })
}

/// Whether a resource last modified at `modified` is unchanged since an
/// `If-Modified-Since` date (to the second, as HTTP dates are).
pub(crate) fn unmodified_since(header: &str, modified: SystemTime) -> bool {
    let Ok(since) = httpdate::parse_http_date(header.trim()) else { return false };
    let seconds = |t: SystemTime| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).ok();
    matches!((seconds(modified), seconds(since)), (Some(m), Some(s)) if m <= s)
}

/// The conditional-request rule: `If-None-Match` wins; `If-Modified-Since`
/// counts only without it.
fn fresh(req: &Request, etag: Option<&str>, modified: Option<SystemTime>) -> bool {
    match req.get_header("if-none-match") {
        Some(tags) => etag.is_some_and(|etag| etag_matches(tags, etag)),
        None => match (req.get_header("if-modified-since"), modified) {
            (Some(since), Some(modified)) => unmodified_since(since, modified),
            _ => false,
        },
    }
}

impl Request {
    /// Whether the client already has this version, per its
    /// `If-None-Match` (compared with `etag`) or, without that header,
    /// `If-Modified-Since` (compared with `last_modified`). Only meaningful
    /// for `GET` and `HEAD`. Answer `true` with
    /// [`Response::not_modified`].
    pub fn is_fresh(&self, etag: Option<&ETag>, last_modified: Option<SystemTime>) -> bool {
        let etag = etag.map(ETag::header_value);
        fresh(self, etag.as_deref(), last_modified)
    }
}

impl Response {
    /// `304 Not Modified` with the ETag, for [`Request::is_fresh`]. Add
    /// the same `Cache-Control` as the full response, if any.
    pub fn not_modified(etag: &ETag) -> Response {
        let mut response = Response::new(304);
        response.header("ETag", etag.header_value());
        response
    }

    /// Sets the `ETag` header.
    pub fn with_etag(self, etag: &ETag) -> Self {
        self.with_header("ETag", etag.header_value())
    }

    /// Sets `Last-Modified`.
    pub fn with_last_modified(self, modified: SystemTime) -> Self {
        self.with_header("Last-Modified", httpdate::fmt_http_date(modified))
    }

    /// Sets `Cache-Control`.
    pub fn with_cache_control(self, cache_control: CacheControl) -> Self {
        self.with_header("Cache-Control", cache_control.to_string())
    }
}

/// Headers describing the body, which a 304 (having none) leaves out.
const BODY_HEADERS: [&str; 6] =
    ["content-type", "content-length", "content-encoding", "content-language", "content-range", "transfer-encoding"];

/// Conditional `GET` for API responses: gives successful `GET` and `HEAD`
/// responses without one a weak ETag from the body ([`ETag::of`]), and
/// answers `If-None-Match` / `If-Modified-Since` with `304 Not Modified`
/// and no body.
///
/// ```rust
/// use axeon::cache::{CacheControl, ConditionalGet};
/// use axeon::Server;
///
/// let mut app = Server::new();
/// // Add it after Compression, so ETags describe the uncompressed data.
/// app.middleware(ConditionalGet::new().cache_control(CacheControl::revalidate()));
/// app.get("/v1/categories", |_req| async { axeon::Json(vec!["lighting", "furniture"]) });
/// ```
///
/// The handler still runs; for a 304 only the transfer is saved. Streamed
/// responses are left alone. Handlers can set their own `ETag` or
/// `Last-Modified`, which are used instead.
#[derive(Clone, Debug, Default)]
pub struct ConditionalGet {
    cache_control: Option<String>,
    auto_etag: bool,
}

impl ConditionalGet {
    /// ETags from bodies, no `Cache-Control`.
    pub fn new() -> Self {
        Self { cache_control: None, auto_etag: true }
    }

    /// `Cache-Control` for successful responses that don't set their own.
    pub fn cache_control(mut self, cache_control: CacheControl) -> Self {
        self.cache_control = Some(cache_control.to_string());
        self
    }

    /// Whether to make ETags from bodies (default `true`). With `false`,
    /// only responses that set `ETag` or `Last-Modified` get 304s.
    pub fn auto_etag(mut self, auto: bool) -> Self {
        self.auto_etag = auto;
        self
    }
}

impl Middleware for ConditionalGet {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        if !matches!(req.method(), Method::GET | Method::HEAD) {
            return next.handle(req);
        }
        let this = self.clone();
        Box::pin(async move {
            let if_none_match = req.get_header("if-none-match").map(str::to_string);
            let if_modified_since = req.get_header("if-modified-since").map(str::to_string);
            let mut response = next.handle(req).await?;
            if response.status != 200 || response.is_streaming() {
                return Ok(response);
            }
            if let Some(cache_control) = &this.cache_control {
                if response.get_header("cache-control").is_none() {
                    response.header("Cache-Control", cache_control);
                }
            }
            if this.auto_etag && response.get_header("etag").is_none() {
                response.header("ETag", ETag::of(&response.body).header_value());
            }
            let etag = response.get_header("etag");
            let modified = response.get_header("last-modified").and_then(|d| httpdate::parse_http_date(d).ok());
            let not_modified = match (&if_none_match, &if_modified_since) {
                (Some(tags), _) => etag.is_some_and(|etag| etag_matches(tags, etag)),
                (None, Some(since)) => modified.is_some_and(|modified| unmodified_since(since, modified)),
                (None, None) => false,
            };
            if not_modified {
                response.status = 304;
                response.body = bytes::Bytes::new();
                for name in BODY_HEADERS {
                    response.headers.remove(name);
                }
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etags() {
        assert_eq!(ETag::strong("v1").header_value(), "\"v1\"");
        assert_eq!(ETag::weak("v1").header_value(), "W/\"v1\"");
        assert_eq!(ETag::strong("a\"b c\n").header_value(), "\"abc\"");
        assert_eq!(ETag::of(b"hello"), ETag::of(b"hello"));
        assert_ne!(ETag::of(b"hello"), ETag::of(b"hellp"));
        assert!(ETag::of(b"").is_weak());
    }

    #[test]
    fn compares_etags_weakly() {
        assert!(etag_matches("\"a-1\"", "\"a-1\""));
        assert!(etag_matches("W/\"a-1\"", "\"a-1\""));
        assert!(etag_matches("\"a-1\"", "W/\"a-1\""));
        assert!(etag_matches("\"x\", \"a-1\"", "\"a-1\""));
        assert!(etag_matches("*", "\"a-1\""));
        assert!(!etag_matches("\"a-2\"", "\"a-1\""));
    }

    #[test]
    fn tags_may_contain_commas() {
        let tag = ETag::strong("a,b").header_value();
        assert!(etag_matches(&tag, &tag));
        assert!(etag_matches(&format!("\"x\", {}, \"y\"", tag), &tag));
        assert!(!etag_matches("\"a\", \"b\"", &tag), "two tags are not one with a comma");
        assert_eq!(tags("\"a,b\" , W/\"c\",").collect::<Vec<_>>(), ["\"a,b\" ", " W/\"c\""]);
    }

    #[test]
    fn modified_since_compares_seconds() {
        let t = UNIX_EPOCH + Duration::from_millis(1_700_000_000_500);
        let header = httpdate::fmt_http_date(t);
        assert!(unmodified_since(&header, t));
        assert!(!unmodified_since(&header, t + Duration::from_secs(1)));
        assert!(!unmodified_since("not a date", t));
    }
}
