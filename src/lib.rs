//! # Axeon
//!
//! A modern, flexible, and feature-rich web framework for Rust.
//!
//! ## Features
//!
//! - Express-style routing with support for path parameters and query parameters
//! - Modular router system with mounting capabilities
//! - Powerful middleware system for request/response processing
//! - Built-in security features and authentication support
//! - JSON request/response handling with type safety
//! - Async/await support throughout the framework
//!
//! ## Quick Start
//!
//! ```rust
//! use axeon::{Response, Server};
//!
//! fn main() {
//!     let mut app = Server::new();
//!     app.get("/", |_req| async {
//!         Response::text("Hello, World!")
//!     });
//!     app.listen("127.0.0.1:3000")
//!         .expect("Server failed to start");
//! }
//! ```
//!
//! ## Routing Examples
//!
//! ### Basic Routes
//!
//! ```rust
//! use axeon::{Response, Server};
//!
//! let mut app = Server::new();
//!
//! // Basic route
//! app.get("/", |_req| async {
//!     Response::text("Welcome!")
//! });
//!
//! // Route with path parameter
//! app.get("/users/:id", |req| async move {
//!     let user_id = req.params.get("id").unwrap();
//!     Response::text(format!("User ID: {}", user_id))
//! });
//! ```
//!
//! ### JSON Handling
//!
//! ```rust
//! use axeon::{Response, Server, ServerError};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Serialize, Deserialize)]
//! struct User {
//!     name: String,
//!     role: String,
//! }
//!
//! app.post("/users", |req| async move {
//!     match req.body.json::<User>() {
//!         Some(user) => Response::ok(&user),
//!         None => Err(ServerError::BadRequest("Invalid JSON body".to_string())),
//!     }
//! });
//! ```
//!
//! ## Middleware System
//!
//! ```rust
//! use axeon::{Request, Response, ServerError};
//! use axeon::middleware::{Middleware, MiddlewareResult, Next};
//! use std::time::Instant;
//!
//! // Example logging middleware
//! struct Logger;
//! impl Middleware for Logger {
//!     fn call(&self, req: Request, next: Next) -> MiddlewareResult {
//!         Box::pin(async move {
//!             let start = Instant::now();
//!             let method = req.method.clone();
//!             let url = req.path.clone();
//!             
//!             let res = next.handle(req).await;
//!             
//!             let status = match &res {
//!                 Ok(res) => res.status,
//!                 Err(err) => err.status_code(),
//!             };
//!             println!("[{}] {:?} {} - {}ms", status, method, url, start.elapsed().as_millis());
//!             res
//!         })
//!     }
//!     
//!     fn clone_box(&self) -> Box<dyn Middleware> {
//!         Box::new(Self)
//!     }
//! }
//! ```
//!
//! ## Router Groups
//!
//! ```rust
//! use axeon::{Router, Server, Response};
//!
//! let mut app = Server::new();
//! let mut api = Router::new();
//!
//! // API routes
//! api.get("/status", |_req| async {
//!     Response::ok(&serde_json::json!({
//!         "status": "operational",
//!         "version": "1.0.0"
//!     }))
//! });
//!
//! // Mount API routes with prefix
//! app.mount("/api", api);
//! ```
//!
pub extern crate serde_json;
pub(crate) mod app;
pub mod buffer;
pub mod cache;
pub mod database;
pub(crate) mod error;
pub(crate) mod handler;
pub(crate) mod http;
pub mod middleware;
pub(crate) mod plugins;
pub(crate) mod router;

pub use app::Server;
pub use router::Router;

pub use crate::error::ServerError;
pub use crate::http::request::{Body, Method, ParseError, Request};
pub use crate::http::response::Response;

// Reexport serde_json
pub use serde_json::{json, Value};
