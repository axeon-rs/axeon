//! Against Redis (when `AXEON_REDIS_URL` is set). Each test uses its own
//! key prefix.

use axeon::broadcast::{Hub, Message};
use axeon_redis::RedisBroadcast;
use futures::StreamExt;
use std::time::Duration;

fn prefix(test: &str) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("axeon-test:{}:{}:", test, nanos)
}

async fn backend(url: &str, prefix: &str) -> RedisBroadcast {
    RedisBroadcast::connect(url).await.unwrap().prefix(prefix)
}

#[tokio::test]
async fn redis_backend_conforms() {
    let Ok(url) = std::env::var("AXEON_REDIS_URL") else {
        eprintln!("skipping: AXEON_REDIS_URL is not set");
        return;
    };
    let prefix = prefix("conform");
    // Two instances: separate connections, same Redis.
    let (a, b) = (Hub::new(backend(&url, &prefix).await), Hub::new(backend(&url, &prefix).await));
    axeon::broadcast::conformance::check(a, b, "").await;

    // Logs are Redis Streams that expire when idle.
    let mut redis = axeon_redis::connect(&url).await.unwrap();
    let ttl: i64 = redis::cmd("PTTL").arg(format!("{}log:run:1", prefix)).query_async(&mut redis).await.unwrap();
    assert!(ttl > 23 * 3600 * 1000, "{}", ttl);
    let keys: Vec<String> = redis::cmd("KEYS").arg(format!("{}*", prefix)).query_async(&mut redis).await.unwrap();
    let _: () = redis::cmd("DEL").arg(keys).query_async(&mut redis).await.unwrap();
}

#[tokio::test]
async fn trimmed_streams_report_missed_messages() {
    let Ok(url) = std::env::var("AXEON_REDIS_URL") else { return };
    let prefix = prefix("trim");
    let hub = Hub::new(backend(&url, &prefix).await);
    let first = hub.publish("t", Message::new("1")).await.unwrap();
    for i in 2..=5 {
        hub.publish("t", Message::new(i.to_string())).await.unwrap();
    }
    assert!(!hub.subscribe("t", Some(&first)).await.unwrap().missed());
    let mut redis = axeon_redis::connect(&url).await.unwrap();
    let key = format!("{}log:t", prefix);
    let _: i64 = redis::cmd("XTRIM").arg(&key).arg("MAXLEN").arg(2).query_async(&mut redis).await.unwrap();
    let mut resumed = hub.subscribe("t", Some(&first)).await.unwrap();
    assert!(resumed.missed());
    let got: Vec<String> = resumed.by_ref().take(2).map(|m| m.unwrap().data().to_string()).collect().await;
    assert_eq!(got, ["4", "5"]);
    // An expired topic: everything was missed.
    let _: () = redis::cmd("DEL").arg(&key).query_async(&mut redis).await.unwrap();
    assert!(hub.subscribe("t", Some(&first)).await.unwrap().missed());
}

#[tokio::test]
async fn subscribers_survive_a_redis_restart() {
    let Ok(binary) = std::env::var("AXEON_REDIS_SERVER") else {
        eprintln!("skipping: AXEON_REDIS_SERVER is not set");
        return;
    };
    let port = 56391;
    let start = || {
        std::process::Command::new(&binary)
            .args(["--port", &port.to_string(), "--bind", "127.0.0.1", "--save", "", "--appendonly", "no"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    let url = format!("redis://127.0.0.1:{}", port);
    let mut server = start();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (a, b) = (Hub::new(backend(&url, "r:").await), Hub::new(backend(&url, "r:").await));
    let mut on_a = a.subscribe("t", None).await.unwrap();
    b.publish("t", Message::new("before")).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), on_a.next()).await.expect("no message").unwrap().unwrap();
    assert_eq!(first.data(), "before");

    server.kill().unwrap();
    server.wait().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut server = start();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The publisher's connection reconnects; the wake-up subscription is
    // re-established within a second or so, and the subscriber keeps going.
    let mut delivered = false;
    for attempt in 0..50 {
        if b.publish("t", Message::new(format!("after {}", attempt))).await.is_ok() {
            if let Ok(Some(Ok(message))) = tokio::time::timeout(Duration::from_millis(300), on_a.next()).await {
                assert!(message.data().starts_with("after"));
                delivered = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(delivered, "never delivered after the restart");
    server.kill().unwrap();
    server.wait().unwrap();
}
