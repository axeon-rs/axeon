//! # Axeon
//! 
//! A modern, flexible, and feature-rich web framework for Rust.
//! 
//! ## Features
//! 
//! - Express-style routing with path parameters
//! - Middleware support (logging, security, rate limiting, etc.)
//! - Static file serving
//! - Built-in security features
//! - JSON request/response handling
//! - Async/await support
//! 
//! ## Quick Start
//! 
//! ```rust
//! use axeon::app::Application;
//! use axeon::ok_json;
//! 
//! fn main() {
//!     let mut app = Application::new();
//!     
//!     // Add routes
//!     app.get("/", |_req| async {
//!         ok_json!({
//!             "message": "Hello, World!"
//!         })
//!     });
//!     
//!     // Start server
//!     app.listen("127.0.0.1:3000").unwrap();
//! }
//! ```
//! 
//! ## Middleware Usage
//! 
//! ```rust
//! use axeon::middleware::{SecurityConfig, SecurityHeaders};
//! 
//! let security_config = SecurityConfig::default();
//! app.middleware(SecurityHeaders::new(security_config));
//! ```

pub mod app;
pub mod handler;
pub mod http;
pub mod middleware;
pub mod router;
pub mod error;
pub mod buffer;
pub mod plugins;
pub mod database;
pub mod cache;
pub extern crate serde_json;

// Reexport serde_json
pub use serde_json::{json, Value};
