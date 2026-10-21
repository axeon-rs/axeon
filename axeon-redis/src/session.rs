//! Sessions in Redis.

use axeon::session::{Record, SessionStore};
use futures::future::BoxFuture;
use redis::aio::{ConnectionLike, ConnectionManager};
use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::time::Duration;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A [`SessionStore`] in Redis: each session is one key holding its data as
/// JSON, expiring with the session's TTL.
///
/// Keys are the [prefix](RedisSessionStore::prefix) plus a SHA-256 hash of
/// the session ID, so someone who can read Redis (a backup, a debugging
/// session) can't use the keys as session cookies.
///
/// Works over any Redis connection: a [`ConnectionManager`] from
/// [`connect`](crate::connect), a multiplexed connection, or a cluster
/// connection.
#[derive(Clone)]
pub struct RedisSessionStore<C = ConnectionManager> {
    connection: C,
    prefix: String,
}

impl<C> RedisSessionStore<C> {
    /// A store on `connection`, with keys prefixed `axeon:session:`.
    pub fn new(connection: C) -> Self {
        Self { connection, prefix: "axeon:session:".to_string() }
    }

    /// Prefixes keys with `prefix`, e.g. to keep two apps sharing a Redis
    /// apart.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// The Redis key for a session ID.
    fn key(&self, id: &str) -> String {
        let mut key = String::with_capacity(self.prefix.len() + 64);
        key.push_str(&self.prefix);
        for byte in Sha256::digest(id.as_bytes()) {
            let _ = write!(key, "{:02x}", byte);
        }
        key
    }
}

impl<C> SessionStore for RedisSessionStore<C>
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
{
    fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Record>, BoxError>> {
        let key = self.key(id);
        let mut connection = self.connection.clone();
        Box::pin(async move {
            let value: Option<String> = redis::cmd("GET").arg(&key).query_async(&mut connection).await?;
            let Some(value) = value else { return Ok(None) };
            match serde_json::from_str(&value) {
                Ok(record) => Ok(Some(record)),
                Err(error) => {
                    // Treat it like an expired session rather than failing
                    // every request from this client.
                    tracing::warn!(%error, %key, "unreadable session in Redis; starting a new one");
                    Ok(None)
                }
            }
        })
    }

    fn save<'a>(&'a self, id: &'a str, record: &'a Record, ttl: Duration) -> BoxFuture<'a, Result<(), BoxError>> {
        let key = self.key(id);
        let mut connection = self.connection.clone();
        Box::pin(async move {
            let value = serde_json::to_string(record)?;
            // Redis needs a positive expiry; round up below a millisecond.
            let millis = ttl.as_millis().clamp(1, u64::MAX as u128) as u64;
            redis::cmd("SET").arg(&key).arg(value).arg("PX").arg(millis).query_async::<()>(&mut connection).await?;
            Ok(())
        })
    }

    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), BoxError>> {
        let key = self.key(id);
        let mut connection = self.connection.clone();
        Box::pin(async move {
            redis::cmd("DEL").arg(&key).query_async::<()>(&mut connection).await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_hash_the_id() {
        let store = RedisSessionStore::new(()).prefix("app:");
        let key = store.key("abc");
        // SHA-256 of "abc".
        assert_eq!(key, "app:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(!key.contains("abc"));
        assert_ne!(store.key("abd"), key);
    }
}
