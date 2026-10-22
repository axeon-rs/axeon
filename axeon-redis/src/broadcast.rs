//! A broadcast backend on Redis Streams.

use axeon::broadcast::{Backend, Batch, BoxError, Message, Wakeup};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use redis::aio::ConnectionManager;
use std::time::Duration;

/// A [broadcast](axeon::broadcast) [`Backend`] on Redis: each topic's log
/// is a Redis Stream, and a pub/sub channel wakes every instance when a
/// topic has new messages. Needs Redis 6.2 or later.
///
/// ```no_run
/// use axeon::broadcast::Hub;
/// use axeon_redis::RedisBroadcast;
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let hub = Hub::new(RedisBroadcast::connect("redis://127.0.0.1:6379").await?);
/// # Ok(()) }
/// ```
///
/// Each topic keeps about its last 1000 messages ([`max_len`](RedisBroadcast::max_len)),
/// and a topic with no new message for a day is deleted
/// ([`idle_ttl`](RedisBroadcast::idle_ttl)).
#[derive(Clone)]
pub struct RedisBroadcast {
    connection: ConnectionManager,
    client: redis::Client,
    prefix: String,
    max_len: usize,
    idle_ttl: Duration,
}

impl RedisBroadcast {
    /// Connects to Redis at `url`, like [`connect`](crate::connect). The
    /// wake-up subscription gets its own connection.
    pub async fn connect(url: &str) -> redis::RedisResult<Self> {
        crate::install_crypto_provider();
        let client = redis::Client::open(url)?;
        let connection = ConnectionManager::new(client.clone()).await?;
        Ok(Self { connection, client, prefix: "axeon:broadcast:".to_string(), max_len: 1000, idle_ttl: Duration::from_secs(24 * 3600) })
    }

    /// Prefixes keys and the wake-up channel. Default: `axeon:broadcast:`.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// About how many messages each topic keeps for replay. Redis trims
    /// in whole blocks, so it may keep a few more. Default: 1000.
    pub fn max_len(mut self, max_len: usize) -> Self {
        self.max_len = max_len.max(1);
        self
    }

    /// How long a topic is kept after its last message. Default: a day.
    pub fn idle_ttl(mut self, ttl: Duration) -> Self {
        self.idle_ttl = ttl;
        self
    }

    fn key(&self, topic: &str) -> String {
        format!("{}log:{}", self.prefix, topic)
    }

    fn channel(&self) -> String {
        format!("{}wake", self.prefix)
    }
}

/// A stream entry as Redis returns it: the ID and `field, value, ...`.
type Entry = (String, Vec<String>);

fn message(entry: Entry) -> Message {
    let (id, fields) = entry;
    let mut data = String::new();
    let mut event = None;
    for pair in fields.chunks(2) {
        match pair {
            [name, value] if name == "data" => data = value.clone(),
            [name, value] if name == "event" => event = Some(value.clone()),
            _ => {}
        }
    }
    let message = Message::new(data).with_id(id);
    match event {
        Some(event) => message.event(event),
        None => message,
    }
}

/// `1700000000000-3` → `(1700000000000, 3)`.
fn parse(id: &str) -> (u64, u64) {
    let (ms, seq) = id.split_once('-').unwrap_or((id, "0"));
    (ms.parse().unwrap_or(0), seq.parse().unwrap_or(0))
}

impl Backend for RedisBroadcast {
    fn publish<'a>(&'a self, topic: &'a str, message: &'a Message) -> BoxFuture<'a, Result<String, BoxError>> {
        Box::pin(async move {
            let key = self.key(topic);
            let mut add = redis::cmd("XADD");
            add.arg(&key).arg("MAXLEN").arg("~").arg(self.max_len).arg("*").arg("data").arg(message.data());
            if let Some(event) = message.event_name() {
                add.arg("event").arg(event);
            }
            let ttl = self.idle_ttl.as_millis().clamp(1, i64::MAX as u128) as i64;
            // Append, keep the topic alive, then wake every instance.
            let (id,): (String,) = redis::pipe()
                .add_command(add)
                .cmd("PEXPIRE")
                .arg(&key)
                .arg(ttl)
                .ignore()
                .cmd("PUBLISH")
                .arg(self.channel())
                .arg(topic)
                .ignore()
                .query_async(&mut self.connection.clone())
                .await?;
            Ok(id)
        })
    }

    fn read<'a>(&'a self, topic: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Batch, BoxError>> {
        Box::pin(async move {
            let key = self.key(topic);
            let mut connection = self.connection.clone();
            let start = after.map(|id| format!("({}", id)).unwrap_or_else(|| "-".to_string());
            let entries: Vec<Entry> =
                redis::cmd("XRANGE").arg(&key).arg(start).arg("+").arg("COUNT").arg(limit).query_async(&mut connection).await?;
            // The client's last message was trimmed if it's older than the
            // oldest one kept (or nothing is kept any more).
            let missed = match after {
                Some(after) => {
                    let oldest: Vec<Entry> = redis::cmd("XRANGE").arg(&key).arg("-").arg("+").arg("COUNT").arg(1).query_async(&mut connection).await?;
                    match oldest.first() {
                        Some((oldest, _)) => parse(after) < parse(oldest),
                        None => true,
                    }
                }
                None => false,
            };
            Ok(Batch { messages: entries.into_iter().map(message).collect(), missed })
        })
    }

    fn latest<'a>(&'a self, topic: &'a str) -> BoxFuture<'a, Result<Option<String>, BoxError>> {
        Box::pin(async move {
            let newest: Vec<Entry> =
                redis::cmd("XREVRANGE").arg(self.key(topic)).arg("+").arg("-").arg("COUNT").arg(1).query_async(&mut self.connection.clone()).await?;
            Ok(newest.into_iter().next().map(|(id, _)| id))
        })
    }

    fn wakeups(&self) -> BoxFuture<'_, Result<BoxStream<'static, Wakeup>, BoxError>> {
        Box::pin(async move {
            let mut pubsub = self.client.get_async_pubsub().await?;
            pubsub.subscribe(self.channel()).await?;
            // Ends when the connection drops; the hub then reconnects.
            let stream = pubsub.into_on_message().filter_map(|message| async move { message.get_payload::<String>().ok().map(Wakeup::Topic) });
            Ok(stream.boxed())
        })
    }

    fn is_after(&self, id: &str, other: &str) -> bool {
        parse(id) > parse(other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_compare_numerically() {
        assert!(parse("1700000000000-10") > parse("1700000000000-9"));
        assert!(parse("1700000000001-0") > parse("1700000000000-99"));
        assert_eq!(parse("5"), (5, 0));
    }

    #[test]
    fn entries_become_messages() {
        let entry = ("1-2".to_string(), vec!["data".into(), "{}".into(), "event".into(), "step".into()]);
        let message = message(entry);
        assert_eq!((message.id(), message.data(), message.event_name()), ("1-2", "{}", Some("step")));
    }
}
