//! PostgreSQL `LISTEN` / `NOTIFY`: lightweight messages between
//! connections, for waking workers or fanning events out to every server.
//!
//! [`Db::notify`] sends a message on a channel, and [`Db::listen`] gives a
//! [`Listener`] that receives them:
//!
//! ```no_run
//! # use axeon_orm::Db;
//! # async fn run(db: Db) -> axeon_orm::Result<()> {
//! let mut listener = db.listen(["jobs"]).await?;
//! db.notify("jobs", "42").await?;
//! let message = listener.recv().await?;
//! assert_eq!((message.channel.as_str(), message.payload.as_str()), ("jobs", "42"));
//! # Ok(()) }
//! ```
//!
//! Sent from a [`Transaction`], a message is delivered only when the
//! transaction commits, and dropped if it rolls back, so listeners never
//! hear about changes that didn't happen.
//!
//! Messages are not stored: a listener only receives those sent while it
//! is connected. Treat a message as a hint to look at the database, not as
//! the data itself, and use [`Listener::try_recv`] to find out when the
//! connection dropped so you can catch up.

use crate::db::{Conn, Db, Transaction};
use crate::error::{Error, Result};
use crate::sql::Sql;
use crate::value::Value;
use crate::Backend;
use futures::{Stream, StreamExt};

/// A message received by a [`Listener`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Notification {
    /// The channel it was sent on.
    pub channel: String,
    /// The payload; empty when none was given.
    pub payload: String,
    /// The process ID of the database connection that sent it.
    pub process_id: u32,
}

impl From<sqlx::postgres::PgNotification> for Notification {
    fn from(n: sqlx::postgres::PgNotification) -> Self {
        Notification { channel: n.channel().to_string(), payload: n.payload().to_string(), process_id: n.process_id() }
    }
}

/// Receives messages sent with [`Db::notify`] on the channels it listens
/// to, from [`Db::listen`]. Holds one connection from the pool for as long
/// as it lives, so share one per process rather than one per request.
#[derive(Debug)]
pub struct Listener {
    inner: sqlx::postgres::PgListener,
}

impl Listener {
    /// Also listens on `channel`. Channel names are case-sensitive.
    pub async fn listen(&mut self, channel: &str) -> Result<()> {
        Ok(self.inner.listen(channel).await?)
    }

    /// Stops listening on `channel`.
    pub async fn unlisten(&mut self, channel: &str) -> Result<()> {
        Ok(self.inner.unlisten(channel).await?)
    }

    /// Waits for the next message on any channel. If the connection drops,
    /// it reconnects and listens again on the same channels, but messages
    /// sent in between are lost; see [`try_recv`](Listener::try_recv).
    pub async fn recv(&mut self) -> Result<Notification> {
        Ok(self.inner.recv().await?.into())
    }

    /// Waits for the next message, like [`recv`](Listener::recv), but
    /// returns `None` when the connection dropped and messages may have
    /// been missed. By then it has reconnected and listens again, so
    /// re-read the state you were tracking, then keep receiving.
    ///
    /// ```no_run
    /// # use axeon_orm::Db;
    /// # async fn catch_up(db: &Db) {}
    /// # async fn handle(n: axeon_orm::notify::Notification) {}
    /// # async fn run(db: Db) -> axeon_orm::Result<()> {
    /// let mut listener = db.listen(["orders"]).await?;
    /// loop {
    ///     catch_up(&db).await;
    ///     while let Some(message) = listener.try_recv().await? {
    ///         handle(message).await;
    ///     }
    /// }
    /// # }
    /// ```
    pub async fn try_recv(&mut self) -> Result<Option<Notification>> {
        Ok(self.inner.try_recv().await?.map(Into::into))
    }

    /// The messages as a stream, reconnecting like [`recv`](Listener::recv).
    pub fn into_stream(self) -> impl Stream<Item = Result<Notification>> + Unpin + Send {
        self.inner.into_stream().map(|n| n.map(Notification::from).map_err(Error::from))
    }
}

impl Db {
    /// Listens on `channels` (case-sensitive names), taking a connection
    /// from the pool for the [`Listener`]. PostgreSQL only.
    pub async fn listen<I>(&self, channels: I) -> Result<Listener>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let Some(pool) = self.postgres() else {
            return Err(unsupported(self.backend()));
        };
        let mut inner = sqlx::postgres::PgListener::connect_with(pool).await?;
        let channels: Vec<I::Item> = channels.into_iter().collect();
        inner.listen_all(channels.iter().map(AsRef::as_ref)).await?;
        Ok(Listener { inner })
    }

    /// Sends `payload` to everyone listening on `channel`. PostgreSQL only;
    /// payloads are limited to 8000 bytes.
    pub async fn notify(&self, channel: &str, payload: &str) -> Result<()> {
        notify(Conn::Db(self), channel, payload).await
    }
}

impl Transaction {
    /// Sends `payload` to everyone listening on `channel` when the
    /// transaction commits; nothing is sent if it rolls back. PostgreSQL
    /// only; see [`Db::notify`].
    pub async fn notify(&mut self, channel: &str, payload: &str) -> Result<()> {
        notify(Conn::Tx(self), channel, payload).await
    }
}

async fn notify(mut conn: Conn<'_>, channel: &str, payload: &str) -> Result<()> {
    if conn.backend() != Backend::Postgres {
        return Err(unsupported(conn.backend()));
    }
    // `pg_notify` takes the channel as a value, so any name is safe.
    let mut sql = Sql::new(Backend::Postgres);
    sql.push("SELECT pg_notify(").arg(Value::Text(channel.into())).push(", ").arg(Value::Text(payload.into())).push(")");
    let (sql, args) = sql.finish();
    conn.fetch_optional(sql, args).await?;
    Ok(())
}

fn unsupported(backend: Backend) -> Error {
    Error::Config(format!("LISTEN/NOTIFY needs PostgreSQL, not {:?}", backend))
}
