//! A [broadcast](axeon::broadcast) backend on PostgreSQL: messages in a
//! table, `LISTEN`/`NOTIFY` to wake every instance.
//!
//! ```no_run
//! use axeon::broadcast::{Hub, Message};
//! use axeon_orm::broadcast::{self, PgBroadcast};
//!
//! # async fn run(db: axeon_orm::Db) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//! broadcast::install(&db).await?; // or add broadcast::migration() to your migrations
//! let hub = Hub::new(PgBroadcast::new(db.clone()));
//!
//! // Inside a transaction: sent only if it commits.
//! let mut tx = db.begin().await?;
//! // ... update the onboarding step ...
//! broadcast::publish_in(&mut tx, "run:42", &Message::new(r#"{"step":3}"#).event("step")).await?;
//! tx.commit().await?;
//! # Ok(()) }
//! ```
//!
//! Messages stay in the `axeon_broadcast` table until
//! [`prune`](PgBroadcast::prune)d: run it regularly (e.g. hourly from a
//! scheduled job) with the replay window you want.

use crate::{Db, Error, Result, Transaction};
use axeon::broadcast::{Backend, Batch, BoxError, Message, Wakeup};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::time::Duration;

const CHANNEL: &str = "axeon_broadcast";

/// The `axeon_broadcast` table. Safe to run again.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS axeon_broadcast (
    id BIGSERIAL PRIMARY KEY,
    topic TEXT NOT NULL,
    event TEXT,
    data TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS axeon_broadcast_topic ON axeon_broadcast (topic, id);
CREATE INDEX IF NOT EXISTS axeon_broadcast_created ON axeon_broadcast (created_at);
"#;

/// The schema as a [migration](crate::migrate::Migration).
pub fn migration() -> crate::migrate::Migration {
    crate::migrate::Migration::new(20261021000001, "axeon_broadcast", SCHEMA)
}

/// Creates the table if it doesn't exist.
pub async fn install(db: &Db) -> Result<()> {
    pool(db)?;
    db.execute(SCHEMA).await
}

fn pool(db: &Db) -> Result<&sqlx::PgPool> {
    db.postgres().ok_or_else(|| Error::Config("the broadcast backend needs PostgreSQL".into()))
}

/// A [`Backend`] on PostgreSQL. Clones share the pool.
#[derive(Clone, Debug)]
pub struct PgBroadcast {
    db: Db,
}

impl PgBroadcast {
    /// A backend on `db`, which must be PostgreSQL with the [schema](SCHEMA)
    /// installed. Wake-ups take one connection from its pool.
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Deletes messages older than `age`; returns how many. A client that
    /// reconnects after its last message was pruned gets a `reset` event.
    pub async fn prune(&self, age: Duration) -> Result<u64> {
        let deleted = sqlx::query("DELETE FROM axeon_broadcast WHERE created_at < now() - make_interval(secs => $1)")
            .bind(age.as_secs_f64())
            .execute(pool(&self.db)?)
            .await?;
        Ok(deleted.rows_affected())
    }
}

/// Publishes `message` on `topic` inside `tx`: delivered when `tx` commits,
/// never if it rolls back. Returns the message's ID.
///
/// It holds a lock on the topic until `tx` ends, so that messages of one
/// topic get IDs in commit order (readers rely on that). Publish late in a
/// transaction, and keep the transaction short.
pub async fn publish_in(tx: &mut Transaction, topic: &str, message: &Message) -> Result<String> {
    let conn = tx.postgres().ok_or_else(|| Error::Config("the broadcast backend needs PostgreSQL".into()))?;
    insert(conn, topic, message).await
}

/// Locks the topic, inserts, and notifies, on a connection in a transaction.
async fn insert(conn: &mut sqlx::PgConnection, topic: &str, message: &Message) -> Result<String> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))").bind(topic).execute(&mut *conn).await?;
    let id: i64 = sqlx::query_scalar("INSERT INTO axeon_broadcast (topic, event, data) VALUES ($1, $2, $3) RETURNING id")
        .bind(topic)
        .bind(message.event_name())
        .bind(message.data())
        .fetch_one(&mut *conn)
        .await?;
    sqlx::query("SELECT pg_notify($1, $2)").bind(CHANNEL).bind(topic).execute(&mut *conn).await?;
    Ok(id.to_string())
}

fn parse(id: &str) -> i64 {
    id.parse().unwrap_or(0)
}

impl Backend for PgBroadcast {
    fn publish<'a>(&'a self, topic: &'a str, message: &'a Message) -> BoxFuture<'a, std::result::Result<String, BoxError>> {
        Box::pin(async move {
            let mut tx = pool(&self.db)?.begin().await.map_err(Error::from)?;
            let id = insert(&mut tx, topic, message).await?;
            tx.commit().await.map_err(Error::from)?;
            Ok(id)
        })
    }

    fn read<'a>(&'a self, topic: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, std::result::Result<Batch, BoxError>> {
        Box::pin(async move {
            let pool = pool(&self.db)?;
            let rows: Vec<(i64, Option<String>, String)> =
                sqlx::query_as("SELECT id, event, data FROM axeon_broadcast WHERE topic = $1 AND id > $2 ORDER BY id LIMIT $3")
                    .bind(topic)
                    .bind(after.map(parse).unwrap_or(0))
                    .bind(limit as i64)
                    .fetch_all(pool)
                    .await
                    .map_err(Error::from)?;
            // The client's last message was pruned if it's older than the
            // oldest one kept (or nothing is kept any more).
            let missed = match after {
                Some(after) => {
                    let oldest: Option<i64> = sqlx::query_scalar("SELECT min(id) FROM axeon_broadcast WHERE topic = $1")
                        .bind(topic)
                        .fetch_one(pool)
                        .await
                        .map_err(Error::from)?;
                    oldest.is_none_or(|oldest| parse(after) < oldest)
                }
                None => false,
            };
            let messages = rows
                .into_iter()
                .map(|(id, event, data)| {
                    let message = Message::new(data).with_id(id.to_string());
                    match event {
                        Some(event) => message.event(event),
                        None => message,
                    }
                })
                .collect();
            Ok(Batch { messages, missed })
        })
    }

    fn latest<'a>(&'a self, topic: &'a str) -> BoxFuture<'a, std::result::Result<Option<String>, BoxError>> {
        Box::pin(async move {
            let newest: Option<i64> = sqlx::query_scalar("SELECT max(id) FROM axeon_broadcast WHERE topic = $1")
                .bind(topic)
                .fetch_one(pool(&self.db)?)
                .await
                .map_err(Error::from)?;
            Ok(newest.map(|id| id.to_string()))
        })
    }

    fn wakeups(&self) -> BoxFuture<'_, std::result::Result<BoxStream<'static, Wakeup>, BoxError>> {
        Box::pin(async move {
            let listener = self.db.listen([CHANNEL]).await?;
            let stream = futures::stream::unfold(listener, |mut listener| async move {
                match listener.try_recv().await {
                    Ok(Some(notification)) => Some((Wakeup::Topic(notification.payload), listener)),
                    // Reconnected; notifications may have been missed.
                    Ok(None) => Some((Wakeup::Reconnected, listener)),
                    Err(_) => None,
                }
            });
            Ok(stream.boxed())
        })
    }

    fn is_after(&self, id: &str, other: &str) -> bool {
        parse(id) > parse(other)
    }
}
