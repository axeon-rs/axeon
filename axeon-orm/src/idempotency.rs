//! An [`Idempotency-Key`](axeon::middleware::idempotency) store on
//! PostgreSQL, for apps that already run on it and don't want Redis.
//!
//! ```no_run
//! use axeon::middleware::idempotency::Idempotency;
//! use axeon::Server;
//! use axeon_orm::idempotency::{self, PgIdempotencyStore};
//!
//! # async fn run(db: axeon_orm::Db) -> axeon_orm::Result<()> {
//! idempotency::install(&db).await?; // or add idempotency::migration() to your migrations
//! let mut app = Server::new();
//! app.middleware(Idempotency::new().store(PgIdempotencyStore::new(db.clone())));
//! # Ok(()) }
//! ```
//!
//! Expired records are replaced when their key comes back, but otherwise
//! stay until [`prune`](PgIdempotencyStore::prune)d: run it regularly, e.g.
//! hourly from a scheduled job.

use crate::{Db, Error, Result};
use axeon::middleware::idempotency::{Begin, IdempotencyStore, StoredResponse};
use futures::future::BoxFuture;
use std::time::Duration;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The `axeon_idempotency` table. Safe to run again.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS axeon_idempotency (
    key TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    response BYTEA,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS axeon_idempotency_expires ON axeon_idempotency (expires_at);
"#;

/// The schema as a [migration](crate::migrate::Migration).
pub fn migration() -> crate::migrate::Migration {
    crate::migrate::Migration::new(20261022000002, "axeon_idempotency", SCHEMA)
}

/// Creates the table if it doesn't exist.
pub async fn install(db: &Db) -> Result<()> {
    pool(db)?;
    db.execute(SCHEMA).await
}

fn pool(db: &Db) -> Result<&sqlx::PgPool> {
    db.postgres().ok_or_else(|| Error::Config("the idempotency store needs PostgreSQL".into()))
}

/// An [`IdempotencyStore`] on PostgreSQL: one row per key, with the
/// response once it's done (`NULL` while running). Clones share the pool.
#[derive(Clone, Debug)]
pub struct PgIdempotencyStore {
    db: Db,
}

impl PgIdempotencyStore {
    /// A store on `db`, which must be PostgreSQL with the [schema](SCHEMA)
    /// installed.
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Deletes expired records; returns how many.
    pub async fn prune(&self) -> Result<u64> {
        let done = sqlx::query("DELETE FROM axeon_idempotency WHERE expires_at < now()").execute(pool(&self.db)?).await?;
        Ok(done.rows_affected())
    }
}

impl IdempotencyStore for PgIdempotencyStore {
    fn begin<'a>(&'a self, key: &'a str, fingerprint: &'a str, lock_ttl: Duration) -> BoxFuture<'a, Result<Begin, BoxError>> {
        Box::pin(async move {
            let pool = pool(&self.db)?;
            // Claim the key, or take over an expired record.
            let claimed = sqlx::query_scalar::<_, bool>(
                "INSERT INTO axeon_idempotency (key, fingerprint, expires_at) \
                 VALUES ($1, $2, now() + make_interval(secs => $3)) \
                 ON CONFLICT (key) DO UPDATE SET fingerprint = EXCLUDED.fingerprint, response = NULL, \
                     expires_at = EXCLUDED.expires_at \
                 WHERE axeon_idempotency.expires_at < now() \
                 RETURNING true",
            )
            .bind(key)
            .bind(fingerprint)
            .bind(lock_ttl.as_secs_f64())
            .fetch_optional(pool)
            .await?;
            if claimed.is_some() {
                return Ok(Begin::Started);
            }
            let row: Option<(String, Option<Vec<u8>>)> =
                sqlx::query_as("SELECT fingerprint, response FROM axeon_idempotency WHERE key = $1 AND expires_at >= now()")
                    .bind(key)
                    .fetch_optional(pool)
                    .await?;
            Ok(match row {
                // Expired just now; the next retry claims it.
                None => Begin::InProgress,
                Some((stored, _)) if stored != fingerprint => Begin::Mismatch,
                Some((_, None)) => Begin::InProgress,
                Some((_, Some(response))) => {
                    Begin::Completed(StoredResponse::decode(&response).ok_or("unreadable idempotency record")?)
                }
            })
        })
    }

    fn complete<'a>(
        &'a self,
        key: &'a str,
        fingerprint: &'a str,
        response: &'a StoredResponse,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<(), BoxError>> {
        Box::pin(async move {
            sqlx::query(
                "UPDATE axeon_idempotency SET response = $3, expires_at = now() + make_interval(secs => $4) \
                 WHERE key = $1 AND fingerprint = $2 AND response IS NULL",
            )
            .bind(key)
            .bind(fingerprint)
            .bind(response.encode())
            .bind(ttl.as_secs_f64())
            .execute(pool(&self.db)?)
            .await?;
            Ok(())
        })
    }

    fn release<'a>(&'a self, key: &'a str, fingerprint: &'a str) -> BoxFuture<'a, Result<(), BoxError>> {
        Box::pin(async move {
            sqlx::query("DELETE FROM axeon_idempotency WHERE key = $1 AND fingerprint = $2 AND response IS NULL")
                .bind(key)
                .bind(fingerprint)
                .execute(pool(&self.db)?)
                .await?;
            Ok(())
        })
    }
}
