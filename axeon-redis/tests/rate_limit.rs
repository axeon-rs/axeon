//! Against Redis (when `AXEON_REDIS_URL` is set). Each test uses its own
//! key prefix.

use axeon::middleware::rate_limit::{Quota, RateLimitStore, RateLimiter};
use axeon::{Method, Request, Server};
use axeon_redis::RedisRateLimitStore;
use std::time::Duration;

fn prefix(test: &str) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("axeon-test:{}:{}:", test, nanos)
}

#[tokio::test]
async fn instances_share_one_limit() {
    let Ok(url) = std::env::var("AXEON_REDIS_URL") else {
        eprintln!("skipping: AXEON_REDIS_URL is not set");
        return;
    };
    let prefix = prefix("shared");
    // Two app instances with their own connections to the same Redis.
    let mut apps = Vec::new();
    for _ in 0..2 {
        let store = RedisRateLimitStore::new(axeon_redis::connect(&url).await.unwrap()).prefix(&prefix);
        let mut app = Server::new();
        app.middleware(
            RateLimiter::new(Quota::per_minute(6).burst(3))
                .key_by(|req| req.get_header("x-api-key").map(String::from))
                .store(store)
                .headers(true),
        );
        app.get("/", |_req| async { "ok" });
        apps.push(app.into_app().await.unwrap());
    }
    let call = |i: usize, key: &'static str| apps[i].call(Request::new(Method::GET, "/").with_header("x-api-key", key));

    let mut statuses = Vec::new();
    for i in [0, 1, 0, 1, 0] {
        statuses.push(call(i, "a").await.status);
    }
    assert_eq!(statuses, [200, 200, 200, 429, 429], "the burst of 3 is shared, not 3 per instance");
    let limited = call(1, "a").await;
    let retry: u64 = limited.get_header("retry-after").unwrap().parse().unwrap();
    assert!((9..=10).contains(&retry), "{}", retry);
    assert_eq!(limited.get_header("ratelimit-remaining"), Some("0"));
    // Other clients are unaffected.
    assert_eq!(call(1, "b").await.status, 200);
}

#[tokio::test]
async fn rates_refill_and_windows_count() {
    let Ok(url) = std::env::var("AXEON_REDIS_URL") else { return };
    let redis = axeon_redis::connect(&url).await.unwrap();
    let prefix = prefix("refill");
    let store = RedisRateLimitStore::new(redis.clone()).prefix(&prefix);

    // 10 a second: one request's capacity every 100 ms.
    let quota = Quota::per_second(10).burst(2);
    let a = store.check("k", &quota).await.unwrap();
    assert!(a.allowed && a.remaining == 1 && a.limit == 2);
    assert!(store.check("k", &quota).await.unwrap().allowed);
    let refused = store.check("k", &quota).await.unwrap();
    assert!(!refused.allowed);
    assert!(refused.retry_after.unwrap() <= Duration::from_millis(100));
    tokio::time::sleep(refused.retry_after.unwrap() + Duration::from_millis(5)).await;
    assert!(store.check("k", &quota).await.unwrap().allowed);

    // A window allows exactly its limit; refused requests aren't counted.
    let daily = Quota::daily(3);
    let decisions = futures::future::join_all((0..5).map(|_| store.check("w", &daily))).await;
    let mut allowed: Vec<_> = decisions.iter().map(|d| d.as_ref().unwrap().allowed).collect();
    allowed.sort();
    assert_eq!(allowed, [false, false, true, true, true]);
    let refused = store.check("w", &daily).await.unwrap();
    assert!(!refused.allowed && refused.reset <= Duration::from_secs(86_400));

    // Keys expire once the state no longer matters, and hide the client key.
    let mut redis = redis;
    let keys: Vec<String> = redis::cmd("KEYS").arg(format!("{}*", prefix)).query_async(&mut redis).await.unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().all(|k| k.len() == prefix.len() + 64), "{:?}", keys);
    for key in &keys {
        let ttl: i64 = redis::cmd("PTTL").arg(key).query_async(&mut redis).await.unwrap();
        assert!(ttl > 0, "{} has no expiry", key);
    }
    let _: () = redis::cmd("DEL").arg(keys).query_async(&mut redis).await.unwrap();
}
