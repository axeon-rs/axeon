//! Cookies: plain, signed (tamper-proof) and private (encrypted), from the
//! default `cookies` feature.
//!
//! Add [`CookieManager`] to read and write cookies through
//! [`Request::cookies`]; changes are sent as `Set-Cookie` headers. Cookies
//! you add get safe defaults unless you set them yourself: `Path=/`,
//! `HttpOnly` and `SameSite=Lax`.
//!
//! ```rust
//! use axeon::Server;
//! use axeon::cookies::{Cookie, CookieManager, Key};
//!
//! let mut app = Server::new();
//! // Load the key from configuration in production so cookies survive restarts.
//! app.middleware(CookieManager::new().key(Key::generate()));
//!
//! app.get("/visit", |req| async move {
//!     let cookies = req.cookies();
//!     let visits: u32 = cookies.signed().get("visits").and_then(|c| c.value().parse().ok()).unwrap_or(0);
//!     cookies.signed().add(Cookie::new("visits", (visits + 1).to_string()));
//!     format!("visit number {}", visits + 1)
//! });
//! ```

use crate::message::{Request, Response};
use crate::middleware::{Middleware, MiddlewareResult, Next};
use cookie::CookieJar;
use http::header::{HeaderValue, COOKIE, SET_COOKIE};
use http::HeaderMap;
use std::sync::{Arc, Mutex};

pub use cookie::{Cookie, Expiration, Key, SameSite};

/// Fills in `Path=/`, `HttpOnly` and `SameSite=Lax` where not set.
fn with_defaults(mut cookie: Cookie<'static>) -> Cookie<'static> {
    if cookie.path().is_none() {
        cookie.set_path("/");
    }
    if cookie.http_only().is_none() {
        cookie.set_http_only(true);
    }
    if cookie.same_site().is_none() {
        cookie.set_same_site(SameSite::Lax);
    }
    cookie
}

/// The request's cookies, from [`Request::cookies`]. Changes are sent with
/// the response when [`CookieManager`] is installed.
#[derive(Clone)]
pub struct Cookies {
    jar: Arc<Mutex<CookieJar>>,
    key: Option<Arc<Key>>,
    tracked: bool,
}

impl Cookies {
    fn from_headers(headers: &HeaderMap, key: Option<Arc<Key>>, tracked: bool) -> Self {
        let mut jar = CookieJar::new();
        for header in headers.get_all(COOKIE) {
            let Ok(header) = header.to_str() else { continue };
            for pair in header.split(';') {
                if let Ok(cookie) = Cookie::parse_encoded(pair.trim().to_string()) {
                    jar.add_original(cookie);
                }
            }
        }
        Self { jar: Arc::new(Mutex::new(jar)), key, tracked }
    }

    fn jar(&self) -> std::sync::MutexGuard<'_, CookieJar> {
        self.jar.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn check_tracked(&self) {
        if !self.tracked {
            tracing::warn!("cookie change ignored: add CookieManager middleware to send cookies");
        }
    }

    fn key(&self) -> &Key {
        self.key
            .as_deref()
            .expect("signed and private cookies need a key: CookieManager::new().key(..)")
    }

    /// A cookie sent by the client (or added during this request).
    pub fn get(&self, name: &str) -> Option<Cookie<'static>> {
        self.jar().get(name).cloned()
    }

    /// Adds or replaces a cookie, with safe defaults for unset attributes.
    pub fn add(&self, cookie: impl Into<Cookie<'static>>) {
        self.check_tracked();
        self.jar().add(with_defaults(cookie.into()));
    }

    /// Tells the client to delete a cookie (set with the default `Path=/`).
    pub fn remove(&self, name: &str) {
        self.check_tracked();
        self.jar().remove(Cookie::build(name.to_string()).path("/"));
    }

    /// Cookies signed with the key: clients can read but not change them.
    /// Panics if no key was configured.
    pub fn signed(&self) -> SignedCookies<'_> {
        SignedCookies { cookies: self }
    }

    /// Cookies encrypted with the key: clients can neither read nor change
    /// them. Panics if no key was configured.
    pub fn private(&self) -> PrivateCookies<'_> {
        PrivateCookies { cookies: self }
    }

    fn delta(&self) -> Vec<HeaderValue> {
        self.jar()
            .delta()
            .filter_map(|cookie| HeaderValue::from_str(&cookie.encoded().to_string()).ok())
            .collect()
    }
}

/// Signed cookies; see [`Cookies::signed`].
pub struct SignedCookies<'a> {
    cookies: &'a Cookies,
}

impl SignedCookies<'_> {
    /// The verified cookie, or `None` if missing or tampered with.
    pub fn get(&self, name: &str) -> Option<Cookie<'static>> {
        let key = self.cookies.key();
        self.cookies.jar().signed(key).get(name)
    }

    /// Adds or replaces a signed cookie.
    pub fn add(&self, cookie: impl Into<Cookie<'static>>) {
        self.cookies.check_tracked();
        let key = self.cookies.key().clone();
        self.cookies.jar().signed_mut(&key).add(with_defaults(cookie.into()));
    }

    /// Tells the client to delete the cookie.
    pub fn remove(&self, name: &str) {
        self.cookies.remove(name);
    }
}

/// Encrypted cookies; see [`Cookies::private`].
pub struct PrivateCookies<'a> {
    cookies: &'a Cookies,
}

impl PrivateCookies<'_> {
    /// The decrypted cookie, or `None` if missing or tampered with.
    pub fn get(&self, name: &str) -> Option<Cookie<'static>> {
        let key = self.cookies.key();
        self.cookies.jar().private(key).get(name)
    }

    /// Adds or replaces an encrypted cookie.
    pub fn add(&self, cookie: impl Into<Cookie<'static>>) {
        self.cookies.check_tracked();
        let key = self.cookies.key().clone();
        self.cookies.jar().private_mut(&key).add(with_defaults(cookie.into()));
    }

    /// Tells the client to delete the cookie.
    pub fn remove(&self, name: &str) {
        self.cookies.remove(name);
    }
}

/// Makes [`Request::cookies`] writable: changes become `Set-Cookie`
/// headers. Give it a [`Key`] to use signed and private cookies.
#[derive(Clone, Default)]
pub struct CookieManager {
    key: Option<Arc<Key>>,
}

impl CookieManager {
    /// Plain cookies only; add a key for signed and private ones.
    pub fn new() -> Self {
        Self::default()
    }

    /// The key for signed and private cookies. Use `Key::from(&bytes)` with
    /// at least 64 bytes from your configuration (or `Key::generate()` for
    /// development; cookies then stop verifying after a restart).
    pub fn key(mut self, key: Key) -> Self {
        self.key = Some(Arc::new(key));
        self
    }
}

impl Middleware for CookieManager {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        let cookies = Cookies::from_headers(req.headers(), self.key.clone(), true);
        req.insert_extension(cookies.clone());
        let response = next.handle(req);
        Box::pin(async move {
            let mut response = response.await?;
            for value in cookies.delta() {
                response.headers.append(SET_COOKIE, value);
            }
            Ok(response)
        })
    }
}

impl Request {
    /// The request's cookies. Reading always works; to set or remove
    /// cookies (and use signed or private ones), add [`CookieManager`].
    pub fn cookies(&self) -> Cookies {
        match self.extensions.get::<Cookies>() {
            Some(cookies) => cookies.clone(),
            None => Cookies::from_headers(&self.headers, None, false),
        }
    }
}

impl Response {
    /// Adds a `Set-Cookie` header, with safe defaults for unset attributes
    /// (`Path=/`, `HttpOnly`, `SameSite=Lax`). Works without
    /// [`CookieManager`].
    pub fn set_cookie(&mut self, cookie: impl Into<Cookie<'static>>) -> &mut Self {
        let cookie = with_defaults(cookie.into());
        self.append_header("Set-Cookie", cookie.encoded().to_string())
    }

    /// Tells the client to delete a cookie (set with `Path=/`).
    pub fn remove_cookie(&mut self, name: &str) -> &mut Self {
        let mut cookie = Cookie::build(name.to_string()).path("/").build();
        cookie.make_removal();
        self.append_header("Set-Cookie", cookie.encoded().to_string())
    }
}
