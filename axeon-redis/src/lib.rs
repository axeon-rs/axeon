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
//! [`RedisRateLimitStore`] shares [rate limits](axeon::middleware::rate_limit)
//! between instances, and [`RedisBroadcast`] fans [broadcast](axeon::broadcast)
//! messages out to all of them.
//!
//! [`connect`] gives a [`ConnectionManager`]: one multiplexed connection,
//! cheap to clone, that reconnects by itself. Use `rediss://` URLs with
//! the `tls` feature for managed Redis services. The [`redis`] crate is
//! re-exported for anything else, such as caching.

#![warn(missing_docs)]

mod broadcast;
mod rate_limit;
mod session;

pub use redis;
pub use redis::aio::ConnectionManager;
pub use broadcast::RedisBroadcast;
pub use rate_limit::RedisRateLimitStore;
pub use session::RedisSessionStore;

/// Connects to Redis at `url` (`redis://[:password@]host[:port][/db]`, or
/// `rediss://` with the `tls` feature). Retries for several seconds, so an
/// app starting alongside Redis waits for it, then fails if Redis still
/// can't be reached. Once connected, it reconnects by itself.
pub async fn connect(url: &str) -> redis::RedisResult<ConnectionManager> {
    install_crypto_provider();
    ConnectionManager::new(redis::Client::open(url)?).await
}

/// The `redis` crate builds its TLS config with rustls' process-wide crypto
/// provider, which panics if several providers are compiled in and none is
/// installed. Installs aws-lc-rs unless the app already chose one.
pub(crate) fn install_crypto_provider() {
    #[cfg(feature = "tls")]
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}
