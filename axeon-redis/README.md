# axeon-redis

Redis integrations for [Axeon](https://docs.rs/axeon): a shared session
store, so sessions survive restarts and work across every instance of an
app, a shared rate-limit store, and a broadcast backend for server-sent
events across instances.

```rust
use axeon::broadcast::Hub;
use axeon_redis::RedisBroadcast;

let hub = Hub::new(RedisBroadcast::connect("redis://127.0.0.1:6379").await?.max_len(1000));
```

Each topic is a Redis Stream (`XADD` with approximate `MAXLEN` trimming,
expiring a day after its last message) and one pub/sub channel wakes every
instance. Needs Redis 6.2+.

## Sessions

```rust
use axeon::session::Sessions;
use axeon_redis::RedisSessionStore;
use std::time::Duration;

let redis = axeon_redis::connect("redis://127.0.0.1:6379").await?;
app.middleware(
    Sessions::new(RedisSessionStore::new(redis.clone()).prefix("studio:session:"))
        .ttl(Duration::from_secs(24 * 3600))
        .secure(true),
);
```

- Each session is one key holding JSON, expiring with the session's TTL.
- Keys are the prefix plus a SHA-256 hash of the session ID, so anyone who
  can read Redis (a backup, a `KEYS` in a debugging session) can't use them
  as cookies.
- `connect` returns a `ConnectionManager`: one multiplexed connection,
  cheap to clone, that reconnects by itself. It retries for several
  seconds at startup, so an app can start alongside Redis.
- While Redis is down, requests that use a session get 500 at once (they
  don't wait); others are unaffected. When Redis is back, requests work
  again with no restart.
- `RedisSessionStore` works over any Redis connection type, including
  cluster connections.
- The `tls` feature enables `rediss://` URLs for managed Redis services.
  It uses rustls' aws-lc-rs provider and installs it as the process
  default if none is set, like the other Axeon crates.

The `redis` crate is re-exported as `axeon_redis::redis` for caching and
other commands.

## Rate limits

```rust
use axeon::middleware::rate_limit::{Quota, RateLimiter};
use axeon_redis::RedisRateLimitStore;

app.middleware(
    RateLimiter::new(Quota::per_minute(120).burst(20))
        .store(RedisRateLimitStore::new(redis.clone()))
        .key_by(|req| req.extension::<ApiKey>().map(|k| k.id.to_string()))
        .quota_by(|req| req.extension::<ApiKey>().map(|k| Quota::daily(k.plan.daily_quota())))
        .headers(true),
);
```

- One limit for all instances: each check is a single Lua script (GCRA, or
  a counter for daily and other fixed windows) timed by Redis' clock, so
  instances with skewed clocks agree.
- Keys are the prefix plus a SHA-256 hash of the client key and quota, and
  expire as soon as their state no longer matters.
- If Redis is down, requests go through and a warning is logged;
  `fail_open(false)` refuses them with 503 instead.

## Testing

```bash
export AXEON_REDIS_URL=redis://127.0.0.1:6379
export AXEON_REDIS_SERVER=$(which redis-server)   # optional: the restart test
cargo test -p axeon-redis
```

## License

MIT
