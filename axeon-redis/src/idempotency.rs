//! Idempotency records shared by every instance of the app.

use axeon::middleware::idempotency::{Begin, IdempotencyStore, StoredResponse};
use futures::future::BoxFuture;
use redis::aio::{ConnectionLike, ConnectionManager};
use redis::Script;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// Values are "R\n<fingerprint>" while running and
// "D\n<fingerprint>\n<response>" once done.

// Stores the response if the key is still claimed by this fingerprint.
static COMPLETE: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r#"
if redis.call('GET', KEYS[1]) == 'R\n' .. ARGV[1] then
  redis.call('SET', KEYS[1], 'D\n' .. ARGV[1] .. '\n' .. ARGV[2], 'PX', ARGV[3])
end
return 0
"#,
    )
});

// Drops the claim if it is still this fingerprint's and unfinished.
static RELEASE: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r#"
if redis.call('GET', KEYS[1]) == 'R\n' .. ARGV[1] then
  redis.call('DEL', KEYS[1])
end
return 0
"#,
    )
});

/// An [`IdempotencyStore`] in Redis, so a retry that lands on another
/// instance still gets the first response.
///
/// A claim is one `SET NX` with the lock's expiry; completing and
/// releasing are scripts that only act on this request's own claim. Keys
/// are the [prefix](RedisIdempotencyStore::prefix) plus the hash the
/// middleware computes (the client's key never reaches Redis), and expire
/// with the record.
///
/// ```no_run
/// use axeon::middleware::idempotency::Idempotency;
/// use axeon::Server;
/// use axeon_redis::RedisIdempotencyStore;
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let redis = axeon_redis::connect("redis://127.0.0.1:6379").await?;
/// let mut app = Server::new();
/// app.middleware(Idempotency::new().store(RedisIdempotencyStore::new(redis.clone())));
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct RedisIdempotencyStore<C = ConnectionManager> {
    connection: C,
    prefix: Arc<str>,
}

impl<C> RedisIdempotencyStore<C> {
    /// A store on `connection`, with keys prefixed `axeon:idempotency:`.
    pub fn new(connection: C) -> Self {
        Self { connection, prefix: "axeon:idempotency:".into() }
    }

    /// Prefixes keys with `prefix`.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into().into();
        self
    }

    fn key(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }
}

/// Reads a stored value for `fingerprint`.
fn parse(value: &str, fingerprint: &str) -> Result<Begin, BoxError> {
    let mut parts = value.splitn(3, '\n');
    let (state, stored) = (parts.next(), parts.next());
    if stored != Some(fingerprint) {
        return Ok(Begin::Mismatch);
    }
    match (state, parts.next()) {
        (Some("R"), None) => Ok(Begin::InProgress),
        (Some("D"), Some(response)) => StoredResponse::decode(response.as_bytes())
            .map(Begin::Completed)
            .ok_or_else(|| "unreadable idempotency record".into()),
        _ => Err("unreadable idempotency record".into()),
    }
}

fn millis(duration: Duration) -> u64 {
    (duration.as_millis() as u64).max(1)
}

impl<C> IdempotencyStore for RedisIdempotencyStore<C>
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
{
    fn begin<'a>(&'a self, key: &'a str, fingerprint: &'a str, lock_ttl: Duration) -> BoxFuture<'a, Result<Begin, BoxError>> {
        let key = self.key(key);
        let mut connection = self.connection.clone();
        Box::pin(async move {
            // If the record expires between the two commands, claim again.
            for _ in 0..3 {
                let claimed: Option<String> = redis::cmd("SET")
                    .arg(&key)
                    .arg(format!("R\n{}", fingerprint))
                    .arg("NX")
                    .arg("PX")
                    .arg(millis(lock_ttl))
                    .query_async(&mut connection)
                    .await?;
                if claimed.is_some() {
                    return Ok(Begin::Started);
                }
                let value: Option<String> = redis::cmd("GET").arg(&key).query_async(&mut connection).await?;
                if let Some(value) = value {
                    return parse(&value, fingerprint);
                }
            }
            Ok(Begin::InProgress)
        })
    }

    fn complete<'a>(
        &'a self,
        key: &'a str,
        fingerprint: &'a str,
        response: &'a StoredResponse,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<(), BoxError>> {
        let key = self.key(key);
        let encoded = String::from_utf8(response.encode()).expect("the encoding is JSON");
        let mut connection = self.connection.clone();
        Box::pin(async move {
            let _: i64 = COMPLETE
                .key(&key)
                .arg(fingerprint)
                .arg(encoded)
                .arg(millis(ttl))
                .invoke_async(&mut connection)
                .await?;
            Ok(())
        })
    }

    fn release<'a>(&'a self, key: &'a str, fingerprint: &'a str) -> BoxFuture<'a, Result<(), BoxError>> {
        let key = self.key(key);
        let mut connection = self.connection.clone();
        Box::pin(async move {
            let _: i64 = RELEASE.key(&key).arg(fingerprint).invoke_async(&mut connection).await?;
            Ok(())
        })
    }
}
