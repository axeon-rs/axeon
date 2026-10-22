//! Against Redis (when `AXEON_REDIS_URL` is set).

use axeon::middleware::idempotency::{conformance, Idempotency};
use axeon::{Method, Request, Server};
use axeon_redis::RedisIdempotencyStore;

fn prefix(test: &str) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("axeon-test:{}:{}:", test, nanos)
}

#[tokio::test]
async fn redis_store_conforms() {
    let Ok(url) = std::env::var("AXEON_REDIS_URL") else {
        eprintln!("skipping: AXEON_REDIS_URL is not set");
        return;
    };
    let store = RedisIdempotencyStore::new(axeon_redis::connect(&url).await.unwrap()).prefix(prefix("conform"));
    conformance::check(&store, "").await;
}

#[tokio::test]
async fn retries_on_another_instance_replay() {
    let Ok(url) = std::env::var("AXEON_REDIS_URL") else { return };
    let prefix = prefix("instances");
    let mut apps = Vec::new();
    for i in 0..2 {
        let store = RedisIdempotencyStore::new(axeon_redis::connect(&url).await.unwrap()).prefix(&prefix);
        let mut app = Server::new();
        app.middleware(Idempotency::new().store(store));
        app.post("/", move |_req| async move { format!("instance {}", i) });
        apps.push(app.into_app().await.unwrap());
    }
    let call = |i: usize| apps[i].call(Request::new(Method::POST, "/").with_header("idempotency-key", "retry-1"));
    assert_eq!(call(0).await.body_text(), "instance 0");
    let replayed = call(1).await;
    assert_eq!(replayed.body_text(), "instance 0");
    assert_eq!(replayed.get_header("idempotent-replayed"), Some("true"));

    // Neither the client's key nor its credentials appear in Redis.
    let mut redis = axeon_redis::connect(&url).await.unwrap();
    let keys: Vec<String> = redis::cmd("KEYS").arg(format!("{}*", prefix)).query_async(&mut redis).await.unwrap();
    assert_eq!(keys.len(), 1);
    assert!(!keys[0].contains("retry-1"));
    let _: () = redis::cmd("DEL").arg(keys).query_async(&mut redis).await.unwrap();
}
