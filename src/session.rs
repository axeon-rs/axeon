//! Server-side sessions, from the default `cookies` feature.
//!
//! The client gets a random 128-bit session ID in an `HttpOnly` cookie; the
//! data stays on the server in a [`SessionStore`]. [`MemoryStore`] keeps it
//! in this process; implement [`SessionStore`] for Redis or a database when
//! you run several instances.
//!
//! ```rust
//! use axeon::{Server, ServerError};
//! use axeon::session::{MemoryStore, Sessions};
//! use std::time::Duration;
//!
//! let mut app = Server::new();
//! app.middleware(
//!     Sessions::new(MemoryStore::new())
//!         .ttl(Duration::from_secs(24 * 3600))
//!         .secure(true), // HTTPS only
//! );
//!
//! app.post("/login", |req| async move {
//!     let session = req.session()?;
//!     session.regenerate(); // new ID on login prevents session fixation
//!     session.insert("user_id", 42)?;
//!     Ok::<_, ServerError>("logged in")
//! });
//!
//! app.get("/me", |req| async move {
//!     let user_id: Option<u64> = req.session()?.get("user_id");
//!     Ok::<_, ServerError>(format!("{:?}", user_id))
//! });
//!
//! app.post("/logout", |req| async move {
//!     req.session()?.destroy();
//!     Ok::<_, ServerError>("bye")
//! });
//! ```
//!
//! Sessions are only stored, and the cookie only set, once something is
//! inserted, so anonymous visitors cost nothing.

use crate::body::BoxError;
use crate::cookies::{Cookie, SameSite};
use crate::error::{ServerError, ServerResult};
use crate::message::Request;
use crate::middleware::{Middleware, MiddlewareResult, Next};
use futures::future::BoxFuture;
use http::header::COOKIE;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The data of one session.
pub type Record = HashMap<String, Value>;

/// Where session data lives. Implement this for Redis, a database, etc.
pub trait SessionStore: Send + Sync + 'static {
    /// The session's data, or `None` if it doesn't exist or has expired.
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Record>, BoxError>>;
    /// Saves the data, expiring it after `ttl`.
    fn save<'a>(&'a self, id: &'a str, record: &'a Record, ttl: Duration) -> BoxFuture<'a, Result<(), BoxError>>;
    /// Deletes the session.
    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BoxError>>;
}

/// Sessions in this process's memory: fast, but lost on restart and not
/// shared between instances. Expired sessions are cleaned up periodically.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<MemoryInner>,
}

#[derive(Default)]
struct MemoryInner {
    sessions: HashMap<String, (Record, Instant)>,
    last_sweep: Option<Instant>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of stored (possibly expired) sessions.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).sessions.len()
    }

    /// True if no sessions are stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SessionStore for MemoryStore {
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Record>, BoxError>> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if inner.last_sweep.is_none_or(|last| now.duration_since(last) > Duration::from_secs(60)) {
            inner.sessions.retain(|_, (_, expires)| *expires > now);
            inner.last_sweep = Some(now);
        }
        let record = match inner.sessions.get(id) {
            Some((record, expires)) if *expires > now => Some(record.clone()),
            _ => None,
        };
        Box::pin(async move { Ok(record) })
    }

    fn save<'a>(&'a self, id: &'a str, record: &'a Record, ttl: Duration) -> BoxFuture<'a, Result<(), BoxError>> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.sessions.insert(id.to_string(), (record.clone(), Instant::now() + ttl));
        Box::pin(async { Ok(()) })
    }

    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BoxError>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).sessions.remove(id);
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct SessionState {
    id: Option<String>,
    data: Record,
    changed: bool,
    regenerate: bool,
    destroyed: bool,
}

/// The current session, from [`Request::session`]. Values are stored as
/// JSON.
#[derive(Clone)]
pub struct Session {
    state: Arc<Mutex<SessionState>>,
}

impl Session {
    fn state(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The session ID, if the session has been saved before.
    pub fn id(&self) -> Option<String> {
        self.state().id.clone()
    }

    /// A value, if present and of type `T`.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let value = self.state().data.get(key)?.clone();
        serde_json::from_value(value).ok()
    }

    /// Stores a value. Fails only if it can't be serialized to JSON.
    pub fn insert<T: Serialize>(&self, key: &str, value: T) -> ServerResult<()> {
        let value = serde_json::to_value(value)
            .map_err(|e| ServerError::InternalError(format!("session value not serializable: {}", e)))?;
        let mut state = self.state();
        state.data.insert(key.to_string(), value);
        state.changed = true;
        Ok(())
    }

    /// Removes a value.
    pub fn remove(&self, key: &str) -> Option<Value> {
        let mut state = self.state();
        let removed = state.data.remove(key);
        state.changed |= removed.is_some();
        removed
    }

    /// Removes all values, keeping the session.
    pub fn clear(&self) {
        let mut state = self.state();
        state.changed |= !state.data.is_empty();
        state.data.clear();
    }

    /// Gives the session a new ID, keeping its data. Call this when the
    /// user logs in or gains privileges, to prevent session fixation.
    pub fn regenerate(&self) {
        let mut state = self.state();
        state.regenerate = true;
        state.changed = true;
    }

    /// Deletes the session and its cookie (logout).
    pub fn destroy(&self) {
        let mut state = self.state();
        state.destroyed = true;
        state.data.clear();
    }

    /// True if the session holds no values.
    pub fn is_empty(&self) -> bool {
        self.state().data.is_empty()
    }
}

/// A new random session ID: 128 bits from the OS, as 32 hex characters.
fn new_id() -> ServerResult<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| ServerError::InternalError(format!("random number generator failed: {}", e)))?;
    Ok(bytes.iter().map(|b| format!("{:02x}", b)).collect())
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Session middleware; see the [module docs](self).
pub struct Sessions {
    config: Arc<SessionConfig>,
}

#[derive(Clone)]
struct SessionConfig {
    store: Arc<dyn SessionStore>,
    cookie_name: String,
    ttl: Duration,
    secure: bool,
    same_site: SameSite,
    domain: Option<String>,
}

impl Sessions {
    /// Sessions in `store`, with a `sid` cookie and a 24-hour lifetime.
    pub fn new(store: impl SessionStore) -> Self {
        Self {
            config: Arc::new(SessionConfig {
                store: Arc::new(store),
                cookie_name: "sid".to_string(),
                ttl: Duration::from_secs(24 * 3600),
                secure: false,
                same_site: SameSite::Lax,
                domain: None,
            }),
        }
    }

    fn config(&mut self) -> &mut SessionConfig {
        Arc::make_mut(&mut self.config)
    }

    /// The session cookie's name. Default: `sid`.
    pub fn cookie_name(mut self, name: &str) -> Self {
        self.config().cookie_name = name.to_string();
        self
    }

    /// How long a session lasts after it was last changed. Default: 24 hours.
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.config().ttl = ttl;
        self
    }

    /// Only send the cookie over HTTPS. Enable in production. Default: false.
    pub fn secure(mut self, secure: bool) -> Self {
        self.config().secure = secure;
        self
    }

    /// The cookie's `SameSite` attribute. Default: `Lax`.
    pub fn same_site(mut self, same_site: SameSite) -> Self {
        self.config().same_site = same_site;
        self
    }

    /// The cookie's `Domain`, to share sessions across subdomains.
    pub fn domain(mut self, domain: &str) -> Self {
        self.config().domain = Some(domain.to_string());
        self
    }
}

impl SessionConfig {
    fn cookie(&self, value: String, max_age: Duration) -> Cookie<'static> {
        let mut cookie = Cookie::build((self.cookie_name.clone(), value))
            .path("/")
            .http_only(true)
            .secure(self.secure)
            .same_site(self.same_site)
            .max_age(cookie::time::Duration::seconds(max_age.as_secs() as i64))
            .build();
        if let Some(domain) = &self.domain {
            cookie.set_domain(domain.clone());
        }
        cookie
    }
}

impl Middleware for Sessions {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        let config = Arc::clone(&self.config);
        let incoming = req.headers().get_all(COOKIE).iter().find_map(|header| {
            header.to_str().ok()?.split(';').find_map(|pair| {
                let (name, value) = pair.trim().split_once('=')?;
                (name == config.cookie_name && valid_id(value)).then(|| value.to_string())
            })
        });
        Box::pin(async move {
            let store = &config.store;
            let settings = &config;
            let mut state = SessionState::default();
            if let Some(id) = incoming {
                match store.load(&id).await {
                    Ok(Some(data)) => {
                        state.id = Some(id);
                        state.data = data;
                    }
                    Ok(None) => {} // expired or unknown: start fresh
                    Err(error) => {
                        tracing::error!(error = %error, "session store failed to load");
                        return Err(ServerError::InternalError("session store unavailable".to_string()));
                    }
                }
            }
            let session = Session { state: Arc::new(Mutex::new(state)) };
            req.insert_extension(session.clone());

            let mut response = next.handle(req).await?;

            let (old_id, new_id, data) = {
                let mut state = session.state();
                if state.destroyed {
                    (state.id.take(), None, None)
                } else if state.changed && !state.data.is_empty() {
                    let old = if state.regenerate { state.id.take() } else { None };
                    let id = match state.id.clone() {
                        Some(id) => id,
                        None => new_id()?,
                    };
                    state.id = Some(id.clone());
                    (old, Some(id), Some(state.data.clone()))
                } else if state.changed {
                    // Emptied: same as destroying it.
                    (state.id.take(), None, None)
                } else {
                    (None, None, None)
                }
            };
            let destroyed = new_id.is_none() && old_id.is_some();
            if let Some(old) = &old_id {
                if let Err(error) = store.delete(old).await {
                    tracing::error!(error = %error, "session store failed to delete");
                }
            }
            if let (Some(id), Some(data)) = (new_id, data) {
                if let Err(error) = store.save(&id, &data, settings.ttl).await {
                    tracing::error!(error = %error, "session store failed to save");
                    return Err(ServerError::InternalError("session store unavailable".to_string()));
                }
                response.append_header("Set-Cookie", settings.cookie(id, settings.ttl).encoded().to_string());
            } else if destroyed {
                response.append_header("Set-Cookie", settings.cookie(String::new(), Duration::ZERO).encoded().to_string());
            }
            Ok(response)
        })
    }
}

impl Request {
    /// The current session. Fails with 500 if the [`Sessions`] middleware
    /// isn't installed.
    pub fn session(&self) -> ServerResult<Session> {
        self.extension::<Session>()
            .cloned()
            .ok_or_else(|| ServerError::InternalError("sessions are not enabled: add the Sessions middleware".to_string()))
    }
}
