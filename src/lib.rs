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
//! - [`Server::into_app`] builds an [`App`] that handles requests in-process,
//!   for tests.
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
//! middleware), `ws` (WebSockets) and `cache` (moka-backed cache helpers).
//!
//! See the [README](https://github.com/axeon-rs/axeon#readme) for a full
//! guide and the [changelog](https://github.com/axeon-rs/axeon/blob/master/CHANGELOG.md)
//! for upgrading from 0.2.

pub extern crate serde_json;
pub(crate) mod app;
pub mod buffer;
#[cfg(feature = "cache")]
pub mod cache;
pub mod database;
pub(crate) mod error;
pub(crate) mod handler;
mod lifecycle;
mod static_files;
pub(crate) mod http;
pub mod middleware;
pub mod sse;
#[cfg(feature = "ws")]
pub mod ws;
pub(crate) mod plugins;
pub(crate) mod router;
#[cfg(feature = "tls")]
mod tls;

pub use app::{shutdown_signal, App, Server};
pub use lifecycle::Shutdown;
pub use router::Router;

pub use crate::error::{ServerError, ServerResult};
pub use crate::handler::{Html, IntoResponse, Json};
pub use crate::http::request::{Body, Method, ParseError, Request};
pub use crate::http::response::Response;

// Reexport serde_json
pub use serde_json::{json, Value};

// Compile the README's examples as doctests so they stay correct. The README
// documents the default build, so this needs every default feature.
#[cfg(all(doctest, feature = "tls", feature = "compression", feature = "ws", feature = "cache"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
