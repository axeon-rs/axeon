//! Cross-site request forgery protection (feature `cookies`).
//!
//! [`Csrf`] gives each browser a random token in a cookie. Requests that
//! change state (POST, PUT, PATCH, DELETE) must send the same token back in
//! the `X-CSRF-Token` header or a `_csrf` form field; another site can make
//! the browser send the cookie, but can't read it to copy the token.
//!
//! ```rust
//! use axeon::{Html, Server};
//! use axeon::csrf::Csrf;
//!
//! let mut app = Server::new();
//! app.middleware(Csrf::new());
//! app.get("/form", |req| async move {
//!     let token = req.csrf_token().unwrap_or_default().to_string();
//!     Html(format!(r#"<form method="post" action="/save">
//!         <input type="hidden" name="_csrf" value="{}">
//!         <button>Save</button></form>"#, token))
//! });
//! app.post("/save", |_req| async { "saved" });
//! ```
//!
//! Single-page apps can read the cookie (`csrf_token` by default) from
//! JavaScript and send it as the `X-CSRF-Token` header. APIs authenticated
//! with bearer tokens instead of cookies don't need this middleware.

use crate::auth::constant_time_eq;
use crate::cookies::{Cookie, SameSite};
use crate::error::ServerError;
use crate::message::{Method, Request};
use crate::middleware::{Middleware, MiddlewareResult, Next};
use http::header::COOKIE;
use std::collections::HashMap;
use std::sync::Arc;

/// The CSRF token of the current request; see [`Request::csrf_token`].
#[derive(Clone, Debug)]
pub struct CsrfToken(pub String);

/// CSRF middleware; see the [module docs](self).
#[derive(Clone)]
pub struct Csrf {
    config: Arc<Config>,
}

#[derive(Clone)]
struct Config {
    cookie_name: String,
    header_name: String,
    field_name: String,
    secure: bool,
}

impl Default for Csrf {
    fn default() -> Self {
        Self::new()
    }
}

impl Csrf {
    /// A `csrf_token` cookie, checked against `X-CSRF-Token` or `_csrf`.
    pub fn new() -> Self {
        Self {
            config: Arc::new(Config {
                cookie_name: "csrf_token".to_string(),
                header_name: "x-csrf-token".to_string(),
                field_name: "_csrf".to_string(),
                secure: false,
            }),
        }
    }

    fn config(&mut self) -> &mut Config {
        Arc::make_mut(&mut self.config)
    }

    /// The token cookie's name. Default: `csrf_token`.
    pub fn cookie_name(mut self, name: &str) -> Self {
        self.config().cookie_name = name.to_string();
        self
    }

    /// The request header carrying the token. Default: `X-CSRF-Token`.
    pub fn header_name(mut self, name: &str) -> Self {
        self.config().header_name = name.to_ascii_lowercase();
        self
    }

    /// The form field carrying the token. Default: `_csrf`.
    pub fn field_name(mut self, name: &str) -> Self {
        self.config().field_name = name.to_string();
        self
    }

    /// Only send the cookie over HTTPS. Enable in production. Default: false.
    pub fn secure(mut self, secure: bool) -> Self {
        self.config().secure = secure;
        self
    }
}

fn new_token() -> Result<String, ServerError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| ServerError::InternalError(format!("random number generator failed: {}", e)))?;
    Ok(bytes.iter().map(|b| format!("{:02x}", b)).collect())
}

fn valid_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Middleware for Csrf {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        let config = Arc::clone(&self.config);
        Box::pin(async move {
            let existing = req.headers().get_all(COOKIE).iter().find_map(|header| {
                header.to_str().ok()?.split(';').find_map(|pair| {
                    let (name, value) = pair.trim().split_once('=')?;
                    (name == config.cookie_name && valid_token(value)).then(|| value.to_string())
                })
            });

            let safe = matches!(req.method(), Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE);
            if !safe {
                let Some(expected) = &existing else {
                    return Err(ServerError::Forbidden("missing CSRF token".to_string()));
                };
                let submitted = match req.get_header(&config.header_name) {
                    Some(token) => Some(token.to_string()),
                    None if req.mime_is("application/x-www-form-urlencoded") => {
                        serde_urlencoded::from_bytes::<HashMap<String, String>>(req.body())
                            .ok()
                            .and_then(|mut form| form.remove(&config.field_name))
                    }
                    None => None,
                };
                let valid = submitted.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()));
                if !valid {
                    return Err(ServerError::Forbidden("invalid CSRF token".to_string()));
                }
            }

            let token = match &existing {
                Some(token) => token.clone(),
                None => new_token()?,
            };
            req.insert_extension(CsrfToken(token.clone()));
            let mut response = next.handle(req).await?;
            if existing.is_none() {
                // Readable by JavaScript on purpose (SPAs send it back as a
                // header); SameSite=Strict keeps it off cross-site requests.
                let cookie = Cookie::build((config.cookie_name.clone(), token))
                    .path("/")
                    .http_only(false)
                    .same_site(SameSite::Strict)
                    .secure(config.secure)
                    .build();
                response.append_header("Set-Cookie", cookie.encoded().to_string());
            }
            Ok(response)
        })
    }
}

impl Request {
    /// The CSRF token to embed in forms (as `_csrf`) or send as
    /// `X-CSRF-Token`, when [`Csrf`] is installed.
    pub fn csrf_token(&self) -> Option<&str> {
        self.extension::<CsrfToken>().map(|token| token.0.as_str())
    }
}
