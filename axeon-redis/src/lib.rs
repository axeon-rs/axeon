//! Redis integrations for [Axeon](https://docs.rs/axeon).
//!
//! [`RedisSessionStore`] keeps [sessions](axeon::session) in Redis, so they
//! survive restarts and work across every instance of the app:
//!
//! ```no_run
//! use axeon::session::Sessions;
//! use axeon::Server;
//! use axeon_redis::RedisSessionStore;
//! use std::time::Duration;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let redis = axeon_redis::connect("redis://127.0.0.1:6379").await?;
//!
//! let mut app = Server::new();
//! app.middleware(
//!     Sessions::new(RedisSessionStore::new(redis.clone()).prefix("studio:session:"))
//!         .ttl(Duration::from_secs(24 * 3600))
//!         .secure(true),
//! );
//! # Ok(()) }
//! ```
//!
//! [`connect`] gives a [`ConnectionManager`]: one multiplexed connection,
//! cheap to clone, that reconnects by itself. Use `rediss://` URLs with
//! the `tls` feature for managed Redis services. The [`redis`] crate is
//! re-exported for anything else, such as caching.

#![warn(missing_docs)]

mod session;

pub use redis;
pub use redis::aio::ConnectionManager;
pub use session::RedisSessionStore;

/// Connects to Redis at `url` (`redis://[:password@]host[:port][/db]`, or
/// `rediss://` with the `tls` feature). Retries for several seconds, so an
/// app starting alongside Redis waits for it, then fails if Redis still
/// can't be reached. Once connected, it reconnects by itself.
pub async fn connect(url: &str) -> redis::RedisResult<ConnectionManager> {
    ConnectionManager::new(redis::Client::open(url)?).await
}
