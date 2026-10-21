//! Against Redis (when `AXEON_REDIS_URL` is set, e.g. `redis://127.0.0.1:6379`).
//! Each test uses its own key prefix.

use axeon::session::{Record, SessionStore, Sessions};
use axeon::{App, Method, Request, Server, ServerError};
use axeon_redis::{ConnectionManager, RedisSessionStore};
use std::time::Duration;

async fn redis() -> Option<ConnectionManager> {
    let Ok(url) = std::env::var("AXEON_REDIS_URL") else {
        eprintln!("skipping: AXEON_REDIS_URL is not set");
        return None;
    };
    Some(axeon_redis::connect(&url).await.unwrap())
}

fn prefix(test: &str) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("axeon-test:{}:{}:", test, nanos)
}

async fn keys(redis: &mut ConnectionManager, prefix: &str) -> Vec<String> {
    redis::cmd("KEYS").arg(format!("{}*", prefix)).query_async(redis).await.unwrap()
}

#[tokio::test]
async fn stores_loads_expires_and_deletes() {
    let Some(mut redis) = redis().await else { return };
    let prefix = prefix("store");
    let store = RedisSessionStore::new(redis.clone()).prefix(&prefix);
    let mut record = Record::new();
    record.insert("user_id".into(), serde_json::json!(42));
    record.insert("roles".into(), serde_json::json!(["ops", "reviewer"]));

    assert_eq!(store.load("missing").await.unwrap(), None);
    store.save("id-1", &record, Duration::from_secs(60)).await.unwrap();
    assert_eq!(store.load("id-1").await.unwrap(), Some(record.clone()));

    // One key, prefixed, holding JSON, with the TTL, not named after the ID.
    let stored = keys(&mut redis, &prefix).await;
    assert_eq!(stored.len(), 1);
    assert!(!stored[0].contains("id-1"));
    let ttl: i64 = redis::cmd("PTTL").arg(&stored[0]).query_async(&mut redis).await.unwrap();
    assert!((59_000..=60_000).contains(&ttl), "{}", ttl);
    let raw: String = redis::cmd("GET").arg(&stored[0]).query_async(&mut redis).await.unwrap();
    assert_eq!(serde_json::from_str::<Record>(&raw).unwrap(), record);

    // Saving again replaces the data and the TTL.
    record.insert("user_id".into(), serde_json::json!(7));
    store.save("id-1", &record, Duration::from_millis(150)).await.unwrap();
    assert_eq!(store.load("id-1").await.unwrap().unwrap()["user_id"], 7);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(store.load("id-1").await.unwrap(), None, "expired");

    store.save("id-2", &record, Duration::ZERO).await.unwrap(); // rounds up to 1ms
    store.save("id-3", &record, Duration::from_secs(60)).await.unwrap();
    store.delete("id-3").await.unwrap();
    store.delete("never-existed").await.unwrap();
    assert_eq!(store.load("id-3").await.unwrap(), None);

    // Another prefix doesn't see these sessions.
    store.save("shared-id", &record, Duration::from_secs(60)).await.unwrap();
    let other = RedisSessionStore::new(redis.clone()).prefix(format!("{}other:", prefix));
    assert_eq!(other.load("shared-id").await.unwrap(), None);
    store.delete("shared-id").await.unwrap();

    // Garbage in the key is treated as no session, not an error.
    store.save("id-4", &record, Duration::from_secs(60)).await.unwrap();
    let key = keys(&mut redis, &prefix).await.into_iter().next().unwrap();
    redis::cmd("SET").arg(&key).arg("not json").query_async::<()>(&mut redis).await.unwrap();
    assert_eq!(store.load("id-4").await.unwrap(), None);
    redis::cmd("DEL").arg(&key).query_async::<()>(&mut redis).await.unwrap();
}

/// An app instance with Redis sessions, like one pod.
async fn app(redis: &ConnectionManager, prefix: &str) -> App {
    let mut server = Server::new();
    server.middleware(Sessions::new(RedisSessionStore::new(redis.clone()).prefix(prefix)));
    server.post("/login", |req| async move {
        let session = req.session()?;
        session.regenerate();
        session.insert("user_id", 42)?;
        Ok::<_, ServerError>("in")
    });
    server.get("/me", |req| async move {
        let user: Option<u64> = req.session()?.get("user_id");
        Ok::<_, ServerError>(format!("{:?}", user))
    });
    server.post("/logout", |req| async move {
        req.session()?.destroy();
        Ok::<_, ServerError>("out")
    });
    server.into_app().await.unwrap()
}

fn with_cookie(method: Method, path: &str, cookie: &str) -> Request {
    let mut req = Request::new(method, path);
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    req
}

#[tokio::test]
async fn sessions_are_shared_between_instances() {
    let Some(mut redis) = redis().await else { return };
    let prefix = prefix("shared");
    let (pod_a, pod_b) = (app(&redis, &prefix).await, app(&redis, &prefix).await);

    let res = pod_a.call(Request::new(Method::POST, "/login")).await;
    let set_cookie = res.get_header("set-cookie").unwrap().to_string();
    let cookie = set_cookie.split(';').next().unwrap().to_string();
    assert!(cookie.starts_with("sid="), "{}", set_cookie);

    // Logged in on A, recognised on B.
    assert_eq!(pod_b.call(with_cookie(Method::GET, "/me", &cookie)).await.body_text(), "Some(42)");
    assert_eq!(keys(&mut redis, &prefix).await.len(), 1);

    // Logging in again regenerates the ID and removes the old session.
    let res = pod_b.call(with_cookie(Method::POST, "/login", &cookie)).await;
    let new_cookie = res.get_header("set-cookie").unwrap().split(';').next().unwrap().to_string();
    assert_ne!(new_cookie, cookie);
    assert_eq!(pod_a.call(with_cookie(Method::GET, "/me", &cookie)).await.body_text(), "None", "old ID is gone");
    assert_eq!(pod_a.call(with_cookie(Method::GET, "/me", &new_cookie)).await.body_text(), "Some(42)");
    assert_eq!(keys(&mut redis, &prefix).await.len(), 1);

    // Logged out on A, gone on B and from Redis.
    pod_a.call(with_cookie(Method::POST, "/logout", &new_cookie)).await;
    assert_eq!(pod_b.call(with_cookie(Method::GET, "/me", &new_cookie)).await.body_text(), "None");
    assert!(keys(&mut redis, &prefix).await.is_empty());

    // Visitors without a session cost nothing.
    pod_a.call(Request::new(Method::GET, "/me")).await;
    assert!(keys(&mut redis, &prefix).await.is_empty());
}

#[tokio::test]
async fn unreachable_redis_fails_to_connect() {
    // Nothing listens on port 1. Connecting retries for a few seconds first.
    let result = tokio::time::timeout(Duration::from_secs(30), axeon_redis::connect("redis://127.0.0.1:1")).await;
    assert!(matches!(result, Ok(Err(_))), "connect should fail, not hang");
}

/// Starts a throwaway `redis-server` (path in `AXEON_REDIS_SERVER`) on `port`.
fn start_redis(binary: &str, port: u16) -> std::process::Child {
    std::process::Command::new(binary)
        .args(["--port", &port.to_string(), "--bind", "127.0.0.1", "--save", "", "--appendonly", "no"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[tokio::test]
async fn requests_fail_while_redis_is_down_then_recover() {
    let Ok(binary) = std::env::var("AXEON_REDIS_SERVER") else {
        eprintln!("skipping: AXEON_REDIS_SERVER is not set");
        return;
    };
    let port = 56390;
    let mut server = start_redis(&binary, port);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let redis = axeon_redis::connect(&format!("redis://127.0.0.1:{}", port)).await.unwrap();
    let pod = app(&redis, "restart:").await;
    let res = pod.call(Request::new(Method::POST, "/login")).await;
    let cookie = res.get_header("set-cookie").unwrap().split(';').next().unwrap().to_string();
    assert_eq!(pod.call(with_cookie(Method::GET, "/me", &cookie)).await.body_text(), "Some(42)");

    // Redis goes away: requests with a session get 500, without hanging.
    server.kill().unwrap();
    server.wait().unwrap();
    let started = std::time::Instant::now();
    let res = tokio::time::timeout(Duration::from_secs(10), pod.call(with_cookie(Method::GET, "/me", &cookie))).await.expect("hung");
    assert_eq!(res.status, 500);
    assert!(started.elapsed() < Duration::from_secs(2), "fails fast, not after retries: {:?}", started.elapsed());
    // Requests that don't touch sessions still work.
    assert_eq!(pod.call(Request::new(Method::GET, "/me")).await.body_text(), "None");

    // Redis is back (empty: nothing was saved): the same connection
    // reconnects, and the old session is simply gone.
    let mut server = start_redis(&binary, port);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut recovered = false;
    for _ in 0..50 {
        let res = pod.call(with_cookie(Method::GET, "/me", &cookie)).await;
        if res.status == 200 {
            assert_eq!(res.body_text(), "None");
            recovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(recovered, "never reconnected");
    let res = pod.call(Request::new(Method::POST, "/login")).await;
    assert_eq!(res.status, 200);
    server.kill().unwrap();
    server.wait().unwrap();
}
