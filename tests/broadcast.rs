//! Broadcast hubs: the memory backend, and server-sent events end to end.

mod common;

use axeon::broadcast::{Backend, Batch, BoxError, Hub, MemoryBackend, Message, Wakeup};
use axeon::{Method, Request, Server, ServerError};
use common::{start, Client};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn memory_backend_conforms() {
    let backend = Arc::new(MemoryBackend::new());
    axeon::broadcast::conformance::check(Hub::new(Arc::clone(&backend)), Hub::new(backend), "").await;
}

#[tokio::test]
async fn trimmed_logs_report_missed_messages() {
    let hub = Hub::new(MemoryBackend::with_capacity(3));
    let first = hub.publish("t", Message::new("1")).await.unwrap();
    let second = hub.publish("t", Message::new("2")).await.unwrap();
    for i in 3..=5 {
        hub.publish("t", Message::new(i.to_string())).await.unwrap();
    }
    // "1" and "2" were trimmed: resuming after them misses messages.
    let mut resumed = hub.subscribe("t", Some(&first)).await.unwrap();
    assert!(resumed.missed());
    let got: Vec<String> = resumed.by_ref().take(3).map(|m| m.unwrap().data().to_string()).collect().await;
    assert_eq!(got, ["3", "4", "5"]);
    // Resuming after the newest trimmed message right before the log isn't
    // a gap only if it's still there.
    assert!(hub.subscribe("t", Some(&second)).await.unwrap().missed());
    let kept = hub.publish("t", Message::new("6")).await.unwrap();
    assert!(!hub.subscribe("t", Some(&kept)).await.unwrap().missed());
}

/// Reads an SSE response until `until` appears.
async fn read_until(client: &mut Client, until: &str) -> String {
    let mut seen = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = [0u8; 4096];
        while !seen.contains(until) {
            let n = client.reader.read(&mut buf).await.unwrap();
            assert!(n > 0, "stream ended: {}", seen);
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    })
    .await
    .unwrap_or_else(|_| panic!("didn't see {:?} in {:?}", until, seen));
    seen
}

fn events_app(hub: &Hub) -> Server {
    let mut app = Server::new();
    app.state(hub.clone());
    app.get("/runs/:run/events", |req| async move {
        let run: u64 = req.param("run")?;
        let hub = req.state::<Hub>().unwrap();
        hub.sse(&format!("run:{}", run), req.last_event_id()).await
    });
    app
}

#[tokio::test]
async fn server_sent_events_resume_on_another_instance() {
    let backend = Arc::new(MemoryBackend::new());
    let (pod_a, pod_b) = (Hub::new(Arc::clone(&backend)), Hub::new(Arc::clone(&backend)));
    let addr_a = start(events_app(&pod_a)).await;
    let addr_b = start(events_app(&pod_b)).await;

    let mut browser = Client::connect(addr_a).await;
    browser.send(b"GET /runs/42/events HTTP/1.1\r\nHost: t\r\n\r\n").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Published through B, streamed by A.
    let id = pod_b.publish("run:42", Message::new(r#"{"step":1}"#).event("step")).await.unwrap();
    let seen = read_until(&mut browser, "\n\n").await;
    assert!(seen.starts_with("HTTP/1.1 200"), "{}", seen);
    assert!(seen.to_ascii_lowercase().contains("content-type: text/event-stream"));
    assert!(seen.contains(&format!("event: step\nid: {}\ndata: {{\"step\":1}}\n\n", id)), "{}", seen);
    drop(browser);

    // While disconnected, two more steps; the browser reconnects to B.
    pod_a.publish("run:42", Message::new(r#"{"step":2}"#).event("step")).await.unwrap();
    pod_a.publish("run:42", Message::new(r#"{"step":3}"#).event("step")).await.unwrap();
    let mut browser = Client::connect(addr_b).await;
    browser.send(format!("GET /runs/42/events HTTP/1.1\r\nHost: t\r\nLast-Event-ID: {}\r\n\r\n", id).as_bytes()).await;
    let seen = read_until(&mut browser, "data: {\"step\":3}").await;
    assert!(seen.contains("data: {\"step\":2}"), "{}", seen);
    assert!(!seen.contains("data: {\"step\":1}"), "already seen: {}", seen);
    assert!(!seen.contains("event: reset"));
}

#[tokio::test]
async fn reset_event_when_the_log_was_trimmed() {
    let hub = Hub::new(MemoryBackend::with_capacity(2));
    let old = hub.publish("run:7", Message::new("a")).await.unwrap();
    for data in ["b", "c", "d"] {
        hub.publish("run:7", Message::new(data)).await.unwrap();
    }
    let addr = start(events_app(&hub)).await;
    let mut browser = Client::connect(addr).await;
    browser.send(format!("GET /runs/7/events HTTP/1.1\r\nHost: t\r\nLast-Event-ID: {}\r\n\r\n", old).as_bytes()).await;
    let seen = read_until(&mut browser, "data: d").await;
    let body = &seen[seen.find("\r\n\r\n").unwrap()..];
    let reset = body.find("event: reset").expect("a reset event");
    assert!(reset < body.find("data: c").unwrap(), "reset comes first: {}", body);
}

#[test]
fn last_event_id_header() {
    let mut req = Request::new(Method::GET, "/");
    assert_eq!(req.last_event_id(), None);
    req.headers_mut().insert("last-event-id", "".parse().unwrap());
    assert_eq!(req.last_event_id(), None);
    req.headers_mut().insert("last-event-id", "17".parse().unwrap());
    assert_eq!(req.last_event_id(), Some("17"));
}

/// A backend that's down.
struct Down;

impl Backend for Down {
    fn publish<'a>(&'a self, _: &'a str, _: &'a Message) -> BoxFuture<'a, Result<String, BoxError>> {
        Box::pin(async { Err("down".into()) })
    }
    fn read<'a>(&'a self, _: &'a str, _: Option<&'a str>, _: usize) -> BoxFuture<'a, Result<Batch, BoxError>> {
        Box::pin(async { Err("down".into()) })
    }
    fn latest<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Option<String>, BoxError>> {
        Box::pin(async { Err("down".into()) })
    }
    fn wakeups(&self) -> BoxFuture<'_, Result<BoxStream<'static, Wakeup>, BoxError>> {
        Box::pin(async { Err("down".into()) })
    }
    fn is_after(&self, _: &str, _: &str) -> bool {
        false
    }
}

#[tokio::test]
async fn backend_failures_are_errors_not_hangs() {
    let hub = Hub::new(Down);
    assert!(hub.publish("t", Message::new("x")).await.is_err());
    assert!(hub.subscribe("t", None).await.is_err());
    let err = hub.sse("t", None).await.unwrap_err();
    assert!(matches!(err, ServerError::Status(503, _)), "{:?}", err);
}

/// The memory backend, with a slow `latest`: widens the moment between a
/// topic's feed being created and its first subscriber joining.
struct SlowLatest(Arc<MemoryBackend>);

impl Backend for SlowLatest {
    fn publish<'a>(&'a self, topic: &'a str, message: &'a Message) -> BoxFuture<'a, Result<String, BoxError>> {
        self.0.publish(topic, message)
    }
    fn read<'a>(&'a self, topic: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Batch, BoxError>> {
        self.0.read(topic, after, limit)
    }
    fn latest<'a>(&'a self, topic: &'a str) -> BoxFuture<'a, Result<Option<String>, BoxError>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            self.0.latest(topic).await
        })
    }
    fn wakeups(&self) -> BoxFuture<'_, Result<BoxStream<'static, Wakeup>, BoxError>> {
        self.0.wakeups()
    }
    fn is_after(&self, id: &str, other: &str) -> bool {
        self.0.is_after(id, other)
    }
}

#[tokio::test]
async fn a_new_topic_isnt_dropped_while_its_first_subscriber_joins() {
    let hub = Hub::new(SlowLatest(Arc::new(MemoryBackend::new())));
    let mut subscription = hub.subscribe("t", None).await.unwrap();
    hub.publish("t", Message::new("hello")).await.unwrap();
    let message = tokio::time::timeout(Duration::from_secs(5), subscription.next()).await.expect("no message");
    assert_eq!(message.expect("subscription ended").unwrap().data(), "hello");
}
