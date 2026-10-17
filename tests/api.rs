//! Tests for the handler and middleware API: return types, closure
//! middleware, typed request helpers, streaming, SSE and WebSockets.

mod common;

use axeon::middleware::from_fn;
#[cfg(feature = "compression")]
use axeon::middleware::{CompressionConfig, CompressionMiddleware};
use axeon::sse::{self, Event};
use axeon::{json, Html, Json, Response, Server, ServerError};
use common::{start, Client};
use futures::stream;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;

#[tokio::test]
async fn handlers_can_return_many_types() {
    let mut app = Server::new();
    app.get("/str", |_req| async { "plain" });
    app.get("/string", |_req| async { format!("{}-{}", "a", "b") });
    app.get("/json", |_req| async { Json(json!({ "ok": true })) });
    app.get("/html", |_req| async { Html("<h1>hi</h1>") });
    app.post("/created", |_req| async { (201, Json(json!({ "id": 1 }))) });
    app.get("/none", |_req| async { None::<&'static str> });
    app.get("/some", |_req| async { Some("found") });
    app.delete("/unit", |_req| async {});
    app.get("/err", |_req| async { Err::<&'static str, _>(ServerError::Forbidden("no".into())) });
    app.get("/bytes", |_req| async { vec![0u8, 255] });
    app.get("/legacy", |_req| async { Response::text("still works") });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;

    let r = c.get("/str").await;
    assert_eq!((r.status, r.text().as_str()), (200, "plain"));
    assert!(r.header("content-type").unwrap().starts_with("text/plain"));
    assert_eq!(c.get("/string").await.text(), "a-b");
    let r = c.get("/json").await;
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.text(), r#"{"ok":true}"#);
    assert_eq!(c.get("/html").await.header("content-type"), Some("text/html"));
    c.send(b"POST /created HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().status, 201);
    assert_eq!(c.get("/none").await.status, 404);
    assert_eq!(c.get("/some").await.text(), "found");
    c.send(b"DELETE /unit HTTP/1.1\r\nHost: t\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().status, 204);
    assert_eq!(c.get("/err").await.status, 403);
    assert_eq!(c.get("/bytes").await.body, vec![0, 255]);
    assert_eq!(c.get("/legacy").await.text(), "still works");
}

#[derive(Clone, Debug)]
struct User {
    name: String,
}

#[tokio::test]
async fn closure_middleware_can_reject_annotate_and_pass_data() {
    let mut app = Server::new();
    app.middleware(from_fn(|mut req, next| async move {
        let name = match req.get_header("x-user") {
            Some(name) => name.to_string(),
            None => return Err(ServerError::Unauthorized("who are you?".into())),
        };
        req.insert_extension(User { name });
        let mut res = next.handle(req).await?;
        res.header("X-Middleware", "ran");
        Ok(res)
    }));
    app.get("/me", |req| async move {
        let user = req.extension::<User>().cloned();
        user.map(|u| format!("hello {}", u.name))
    });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;

    assert_eq!(c.get("/me").await.status, 401);
    c.send(b"GET /me HTTP/1.1\r\nHost: t\r\nX-User: ada\r\n\r\n").await;
    let r = c.read().await.unwrap();
    assert_eq!(r.text(), "hello ada");
    assert_eq!(r.header("x-middleware"), Some("ran"));
}

#[derive(Deserialize, Serialize, Debug, PartialEq)]
struct Search {
    q: String,
    page: Option<u32>,
    exact: bool,
}

#[derive(Deserialize, Serialize)]
struct NewUser {
    name: String,
    age: u8,
}

struct Config {
    greeting: &'static str,
}

#[tokio::test]
async fn typed_request_helpers() {
    let mut app = Server::new();
    app.plugins(Config { greeting: "hey" });
    app.get("/users/:id", |req| async move {
        let id: u32 = req.param("id")?;
        Ok::<_, ServerError>(format!("user {}", id + 1))
    });
    app.get("/search", |req| async move { req.query_as::<Search>().map(Json) });
    app.post("/users", |req| async move {
        let user: NewUser = req.json()?;
        Ok::<_, ServerError>((201, Json(user)))
    });
    app.post("/form", |req| async move { req.form::<NewUser>().map(|u| format!("{} {}", u.name, u.age)) });
    app.get("/greet", |req| async move { req.state::<Config>().map(|c| c.greeting) });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;

    assert_eq!(c.get("/users/41").await.text(), "user 42");
    assert_eq!(c.get("/users/abc").await.status, 400);

    let r = c.get("/search?q=rust+web&page=2&exact=true").await;
    let parsed: Search = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(parsed, Search { q: "rust web".into(), page: Some(2), exact: true });
    let r = c.get("/search?q=x&exact=false").await;
    assert_eq!(serde_json::from_slice::<Search>(&r.body).unwrap().page, None);
    assert_eq!(c.get("/search?q=x&exact=maybe").await.status, 400);

    let post = |ct: &str, body: &str| {
        format!("POST /users HTTP/1.1\r\nHost: t\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n{}", ct, body.len(), body)
    };
    c.send(post("application/json", r#"{"name":"ada","age":36}"#).as_bytes()).await;
    let r = c.read().await.unwrap();
    assert_eq!((r.status, r.text().as_str()), (201, r#"{"name":"ada","age":36}"#));
    c.send(post("text/plain", "hi").as_bytes()).await;
    assert_eq!(c.read().await.unwrap().status, 415);
    c.send(post("application/json", "{not json").as_bytes()).await;
    assert_eq!(c.read().await.unwrap().status, 400);
    c.send(post("application/json", r#"{"name":"ada","age":"old"}"#).as_bytes()).await;
    assert_eq!(c.read().await.unwrap().status, 422);

    let body = "name=Grace+Hopper&age=85";
    c.send(format!("POST /form HTTP/1.1\r\nHost: t\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).as_bytes()).await;
    assert_eq!(c.read().await.unwrap().text(), "Grace Hopper 85");

    assert_eq!(c.get("/greet").await.text(), "hey");
}

#[tokio::test]
async fn streamed_responses_are_sent_incrementally() {
    let mut app = Server::new();
    #[cfg(feature = "compression")]
    app.middleware(CompressionMiddleware::new(CompressionConfig { min_size: 0, ..CompressionConfig::default() }));
    app.get("/stream", |_req| async {
        let chunks = stream::unfold(0, |n| async move {
            match n {
                0 => Some((Ok::<_, std::io::Error>("first ".to_string()), 1)),
                1 => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Some((Ok("second".to_string()), 2))
                }
                _ => None,
            }
        });
        Response::stream("text/plain", chunks)
    });
    let addr = start(app).await;

    // The first chunk arrives before the stream has finished.
    let mut c = Client::connect(addr).await;
    c.send(b"GET /stream HTTP/1.1\r\nHost: t\r\nAccept-Encoding: gzip\r\n\r\n").await;
    let mut seen = Vec::new();
    let early = timeout(Duration::from_millis(250), async {
        let mut buf = [0u8; 1024];
        while !String::from_utf8_lossy(&seen).contains("first") {
            let n = c.reader.read(&mut buf).await.unwrap();
            seen.extend_from_slice(&buf[..n]);
        }
    })
    .await;
    assert!(early.is_ok(), "first chunk was not sent early: {:?}", String::from_utf8_lossy(&seen));
    let head = String::from_utf8_lossy(&seen).to_ascii_lowercase();
    assert!(head.contains("transfer-encoding: chunked"));
    assert!(!head.contains("content-encoding"), "streams must not be compressed");

    // And the full body reassembles correctly.
    let r = Client::connect(addr).await.get("/stream").await;
    assert_eq!(r.text(), "first second");
}

#[tokio::test]
async fn sse_channel_sends_events_and_notices_disconnects() {
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    let mut app = Server::new();
    app.get("/events", move |_req| {
        let flag = flag.clone();
        async move {
            let (tx, response) = sse::channel(4);
            tokio::spawn(async move {
                for i in 0.. {
                    if tx.send(Event::new().event("tick").id(i.to_string()).data(format!("n={}", i))).await.is_err() {
                        flag.store(true, Ordering::SeqCst);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
            response
        }
    });
    let addr = start(app).await;

    let mut c = Client::connect(addr).await;
    c.send(b"GET /events HTTP/1.1\r\nHost: t\r\n\r\n").await;
    let mut seen = String::new();
    timeout(Duration::from_secs(2), async {
        let mut buf = [0u8; 1024];
        while !seen.contains("data: n=1\n") {
            let n = c.reader.read(&mut buf).await.unwrap();
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    })
    .await
    .expect("events not received");
    assert!(seen.to_ascii_lowercase().contains("content-type: text/event-stream"));
    assert!(seen.contains("event: tick\nid: 0\ndata: n=0\n\n"));

    drop(c);
    timeout(Duration::from_secs(2), async {
        while !stopped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("producer did not notice the disconnect");
}

#[tokio::test]
async fn sse_keep_alive_comments_are_sent_while_idle() {
    let mut app = Server::new();
    app.get("/idle", |_req| async {
        Response::sse_with_keep_alive(stream::pending::<Event>(), Duration::from_millis(50))
    });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;
    c.send(b"GET /idle HTTP/1.1\r\nHost: t\r\n\r\n").await;
    let mut seen = String::new();
    timeout(Duration::from_secs(2), async {
        let mut buf = [0u8; 1024];
        while !seen.contains(": keep-alive\n\n") {
            let n = c.reader.read(&mut buf).await.unwrap();
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    })
    .await
    .expect("no keep-alive comment");
}

#[cfg(feature = "ws")]
#[tokio::test]
async fn websocket_echo() {
    use axeon::ws::{Message, SinkExt, StreamExt};

    let mut app = Server::new();
    app.get("/ws", |mut req| async move {
        req.upgrade_websocket(|mut socket| async move {
            while let Some(Ok(message)) = socket.next().await {
                if (message.is_text() || message.is_binary()) && socket.send(message).await.is_err() {
                    break;
                }
            }
        })
    });
    let addr = start(app).await;

    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut ws, response) = tokio_tungstenite::client_async(format!("ws://{}/ws", addr), tcp).await.unwrap();
    assert_eq!(response.status(), 101);
    ws.send(Message::text("hello")).await.unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap(), Message::text("hello"));
    ws.send(Message::binary(vec![1u8, 2, 3])).await.unwrap();
    assert_eq!(ws.next().await.unwrap().unwrap(), Message::binary(vec![1u8, 2, 3]));
    ws.close(None).await.unwrap();

    // A plain GET to the WebSocket route is rejected.
    assert_eq!(Client::connect(addr).await.get("/ws").await.status, 400);
}
