//! A fast, Express-style web framework for Rust, built on
//! [hyper](https://hyper.rs).
//!
//! ```rust,no_run
//! use axeon::{Json, Server, ServerError, json};
//!
//! fn main() {
//!     let mut app = Server::new();
//!     app.get("/", |_req| async { "Hello, World!" });
//!     app.get("/users/:id", |req| async move {
//!         let id: u64 = req.param("id")?;
//!         Ok::<_, ServerError>(Json(json!({ "id": id })))
//!     });
//!     // Serves until Ctrl+C or SIGTERM, then shuts down gracefully.
//!     app.listen("127.0.0.1:3000").expect("server failed to start");
//! }
//! ```
//!
//! # Overview
//!
//! - [`Server`] registers routes (`get`, `post`, ...), middleware and
//!   settings, then serves with [`Server::listen`], [`Server::serve`] or
//!   [`Server::serve_with_shutdown`].
//! - [`Router`] groups routes to [`mount`](Server::mount) under a prefix.
//!   Paths use `:name` parameters and `*name` catch-alls.
//!   [`Server::route`] configures one path: per-route middleware, body limit,
//!   and streaming uploads read with [`Request::body_stream`] or
//!   `Request::multipart`.
//! - Handlers are async closures taking a [`Request`] and returning anything
//!   that implements [`IntoResponse`]: text, [`Json`], [`Html`], bytes,
//!   `(status, body)`, `Option`, `Result` or a [`Response`].
//! - [`Request`] has typed helpers: [`param`](Request::param),
//!   [`json`](Request::json), [`query_as`](Request::query_as),
//!   [`form`](Request::form), [`state`](Request::state) and
//!   [`extension`](Request::extension).
//! - Errors are [`ServerError`]s, turned into responses by the status they
//!   carry or by [`Server::on_error`].
//! - [`middleware`] has [`from_fn`](middleware::from_fn) for closure
//!   middleware, the [`Middleware`](middleware::Middleware) trait, and
//!   built-ins for request IDs, logging, security headers, CORS, rate limiting
//!   and compression.
//! - [`sse`] streams server-sent events, [`Response::stream`] streams any
//!   body, and [`ws`] (feature `ws`) accepts WebSockets.
//! - [`tower`] interop: [`Server::layer`] adds any tower layer (such as
//!   `tower-http`) as middleware, and [`App::into_service`] runs the app as a
//!   tower `Service`.
//! - [`Server::into_app`] builds an [`App`] that handles requests in-process,
//!   for tests.
//!
//! # Backend toolkit
//!
//! | Module | For |
//! |---|---|
//! | [`validate`] | Validated JSON, query and form input with per-field 422 errors |
//! | [`auth`] | Bearer, Basic, API key (feature `api-keys`) and JWT (feature `jwt`) authentication |
//! | [`cache`] | `Cache-Control`, ETags and `304 Not Modified` for API responses |
//! | `cookies`, `session`, `csrf` | Signed/encrypted cookies, sessions, CSRF protection (feature `cookies`) |
//! | [`config`] | Typed configuration from environment variables and `.env` |
//! | `templates` | HTML templates (feature `templates`) |
//! | [`health`] | Liveness and readiness endpoints |
//! | [`metrics`] | Prometheus metrics |
//! | [`body`], `multipart` | Streaming request bodies and multipart uploads |
//!
//! Static files ([`Server::static_dir`]) are streamed with range and
//! conditional request support, with an optional SPA fallback
//! ([`Server::spa_fallback`]). Graceful shutdown, request limits, timeouts
//! and backpressure are built in; see [`Server`].
//!
//! # Example: an API with middleware
//!
//! ```rust
//! use axeon::{Json, Router, Server, ServerError};
//! use axeon::middleware::{from_fn, RequestLogger, SetRequestId};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Deserialize, Serialize)]
//! struct Todo {
//!     title: String,
//!     done: bool,
//! }
//!
//! let mut app = Server::new();
//! app.middleware(SetRequestId::new());
//! app.middleware(RequestLogger::new());
//!
//! let mut api = Router::new();
//! api.middleware(from_fn(|req, next| async move {
//!     if req.get_header("authorization").is_none() {
//!         return Err(ServerError::Unauthorized("login required".into()));
//!     }
//!     next.handle(req).await
//! }));
//! api.post("/todos", |req| async move {
//!     let todo: Todo = req.json()?;
//!     Ok::<_, ServerError>((201, Json(todo)))
//! });
//! app.mount("/api", api);
//! ```
//!
//! # Cargo features
//!
//! All on by default: `tls` (HTTPS via rustls), `compression` (gzip/deflate
//! middleware), `ws` (WebSockets), `multipart` (streaming uploads),
//! `cookies` (cookies, sessions, CSRF), `jwt` (JSON Web Tokens),
//! `api-keys` (API key authentication), `idempotency` (`Idempotency-Key`
//! middleware) and `templates` (HTML templates).
//!
//! See the [README](https://github.com/axeon-rs/axeon#readme) for a full
//! guide and the [changelog](https://github.com/axeon-rs/axeon/blob/master/CHANGELOG.md)
//! for upgrading from 0.x.

#![warn(missing_docs)]

pub extern crate serde_json;
pub(crate) mod app;
pub mod auth;
pub mod body;
pub mod cache;
pub mod config;
#[cfg(feature = "cookies")]
pub mod cookies;
#[cfg(feature = "cookies")]
pub mod csrf;
pub(crate) mod error;
pub(crate) mod handler;
mod lifecycle;
mod static_files;
pub(crate) mod message;
pub mod broadcast;
pub mod health;
pub mod metrics;
pub mod middleware;
#[cfg(feature = "cookies")]
pub mod session;
#[cfg(feature = "multipart")]
pub mod multipart;
pub mod sse;
#[cfg(feature = "templates")]
pub mod templates;
pub mod tower;
pub mod validate;
#[cfg(feature = "ws")]
pub mod ws;
pub(crate) mod plugins;
pub(crate) mod router;
#[cfg(feature = "tls")]
mod tls;

pub use app::{shutdown_signal, App, Server};
pub use crate::tower::AppService;
pub use lifecycle::Shutdown;
pub use router::{RouteBuilder, RouteInfo, Router};

pub use crate::error::{ServerError, ServerResult};
pub use crate::handler::{Html, IntoResponse, Json};
pub use crate::message::request::{Method, Params, Request};

/// The [`http`](https://docs.rs/http) crate, re-exported so tower layers and
/// `http` types use the same version as Axeon.
pub use ::http;
pub use crate::message::response::Response;

// Reexport serde_json
pub use serde_json::{json, Value};

// Compile the README's examples as doctests so they stay correct. The README
// documents the default build, so this needs every default feature.
#[cfg(all(
    doctest,
    feature = "tls",
    feature = "compression",
    feature = "ws",
    feature = "multipart",
    feature = "cookies",
    feature = "jwt",
    feature = "templates",
    feature = "api-keys",
    feature = "idempotency"
))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
