//! Graceful shutdown, request IDs, request logging and in-process calls.

mod common;

use axeon::middleware::{from_fn, RequestId, RequestLogger, SetRequestId};
use axeon::{App, Json, Method, Request, Server, ServerError};
use common::Client;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// Starts `app` with a shutdown trigger.
async fn start_with_shutdown(app: Server) -> (SocketAddr, oneshot::Sender<()>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        app.serve_with_shutdown(listener, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });
    (addr, tx, server)
}

fn slow_app(delay: Duration) -> Server {
    let mut app = Server::new();
    app.get("/", |_req| async { "fast" });
    app.get("/slow", move |_req| async move {
        tokio::time::sleep(delay).await;
        "slow done"
    });
    app
}

#[tokio::test]
async fn shutdown_finishes_in_flight_requests_then_stops_accepting() {
    let (addr, trigger, server) = start_with_shutdown(slow_app(Duration::from_millis(300))).await;
    let mut c = Client::connect(addr).await;
    c.send(b"GET /slow HTTP/1.1\r\nHost: t\r\n\r\n").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    trigger.send(()).unwrap();

    // The in-flight request still completes...
    let r = c.read().await.unwrap();
    assert_eq!((r.status, r.text().as_str()), (200, "slow done"));
    // ...then the connection closes and the server returns.
    assert!(c.closed_within(Duration::from_secs(2)).await);
    timeout(Duration::from_secs(2), server).await.expect("server did not stop").unwrap();
    assert!(TcpStream::connect(addr).await.is_err(), "still accepting connections");
}

#[tokio::test]
async fn shutdown_closes_idle_keep_alive_connections_promptly() {
    let mut app = slow_app(Duration::ZERO);
    app.keep_alive(Duration::from_secs(60));
    let (addr, trigger, server) = start_with_shutdown(app).await;
    let mut c = Client::connect(addr).await;
    assert_eq!(c.get("/").await.status, 200);

    let started = Instant::now();
    trigger.send(()).unwrap();
    assert!(c.closed_within(Duration::from_secs(1)).await);
    timeout(Duration::from_secs(1), server).await.expect("server did not stop").unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn shutdown_timeout_closes_stuck_connections() {
    let mut app = slow_app(Duration::from_secs(30));
    app.shutdown_timeout(Duration::from_millis(200));
    let (addr, trigger, server) = start_with_shutdown(app).await;
    let mut c = Client::connect(addr).await;
    c.send(b"GET /slow HTTP/1.1\r\nHost: t\r\n\r\n").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = Instant::now();
    trigger.send(()).unwrap();
    timeout(Duration::from_secs(2), server).await.expect("server did not stop").unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(c.closed_within(Duration::from_secs(1)).await);
}

#[tokio::test]
async fn request_ids_are_generated_reused_and_validated() {
    let mut app = Server::new();
    app.middleware(SetRequestId::new());
    app.get("/", |req| async move { req.extension::<RequestId>().map(|id| id.to_string()) });
    let (addr, _trigger, _server) = start_with_shutdown(app).await;
    let mut c = Client::connect(addr).await;

    let r = c.get("/").await;
    let generated = r.header("x-request-id").unwrap().to_string();
    assert_eq!(r.text(), generated, "handler sees the same id");
    assert_ne!(c.get("/").await.header("x-request-id").unwrap(), generated, "ids are unique");

    c.send(b"GET / HTTP/1.1\r\nHost: t\r\nX-Request-Id: upstream-123\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().header("x-request-id"), Some("upstream-123"));

    c.send(b"GET / HTTP/1.1\r\nHost: t\r\nX-Request-Id: has spaces\r\n\r\n").await;
    assert_ne!(c.read().await.unwrap().header("x-request-id"), Some("has spaces"));
}

/// Collects formatted tracing output.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn request_logger_records_status_latency_and_request_id() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut server = Server::new();
    server.middleware(SetRequestId::new());
    server.middleware(RequestLogger::new());
    server.get("/hello", |_req| async {
        tracing::info!("inside handler");
        "hi"
    });
    server.get("/boom", |_req| async { Err::<&'static str, _>(ServerError::InternalError("db down".into())) });
    let app = server.into_app().await.unwrap();

    app.call(Request::new(Method::GET, "/hello").with_header("x-request-id", "req-42")).await;
    app.call(Request::new(Method::GET, "/boom")).await;

    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<&str> = logs.lines().collect();
    let handler_line = lines.iter().find(|l| l.contains("inside handler")).expect(&logs);
    assert!(handler_line.contains("request_id=req-42"), "{}", handler_line);
    let done = lines.iter().find(|l| l.contains("request completed")).expect(&logs);
    assert!(done.contains("INFO") && done.contains("status=200") && done.contains("latency_ms="), "{}", done);
    assert!(done.contains("method=GET") && done.contains("path=/hello"), "{}", done);
    let failed = lines.iter().find(|l| l.contains("request failed")).expect(&logs);
    assert!(failed.contains("ERROR") && failed.contains("status=500"), "{}", failed);
}

#[tokio::test]
async fn in_process_calls_run_routing_middleware_and_error_handlers() {
    let mut server = Server::new();
    server.on_error(|err| {
        let mut res = axeon::Response::new(err.status_code());
        res.body(format!("custom: {}", err));
        res
    });
    server.middleware(from_fn(|req, next| async move {
        let mut res = next.handle(req).await?;
        res.header("X-Seen", "yes");
        Ok(res)
    }));
    server.post("/users/:id", |req| async move {
        let id: u32 = req.param("id")?;
        let body: serde_json::Value = req.json()?;
        Ok::<_, ServerError>(Json(axeon::json!({ "id": id, "name": body["name"] })))
    });
    let app: App = server.into_app().await.unwrap();

    let res = app.call(Request::new(Method::POST, "/users/5").with_json(&axeon::json!({ "name": "Ada" }))).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.get_header("x-seen"), Some("yes"));
    assert_eq!(res.body_text(), r#"{"id":5,"name":"Ada"}"#);

    let res = app.call(Request::new(Method::GET, "/missing")).await;
    assert_eq!((res.status, res.body_text().as_ref()), (404, "custom: Not found"));
    let res = app.call(Request::new(Method::POST, "/users/x").with_json(&axeon::json!({}))).await;
    assert_eq!(res.status, 400);
}

#[tokio::test]
async fn sse_streams_end_cleanly_on_shutdown() {
    let mut app = Server::new();
    app.shutdown_timeout(Duration::from_secs(10));
    app.get("/events", |_req| async {
        let (tx, response) = axeon::sse::channel(4);
        tokio::spawn(async move {
            while tx.send(axeon::sse::Event::new().data("tick")).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        response
    });
    let (addr, trigger, server) = start_with_shutdown(app).await;
    let mut c = Client::connect(addr).await;
    c.send(b"GET /events HTTP/1.1\r\nHost: t\r\n\r\n").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let started = Instant::now();
    trigger.send(()).unwrap();
    // The chunked stream is terminated properly, then the connection closes.
    let mut raw = Vec::new();
    use tokio::io::AsyncReadExt;
    timeout(Duration::from_secs(2), c.reader.read_to_end(&mut raw)).await.expect("stream did not end").unwrap();
    assert!(raw.ends_with(b"0\r\n\r\n"), "{:?}", String::from_utf8_lossy(&raw[raw.len().saturating_sub(40)..]));
    timeout(Duration::from_secs(2), server).await.expect("server did not stop").unwrap();
    assert!(started.elapsed() < Duration::from_secs(2), "waited for the shutdown timeout");
}

#[tokio::test]
async fn write_timeout_closes_clients_that_stop_reading() {
    let mut app = Server::new();
    app.max_connections(1).write_timeout(Duration::from_millis(200));
    app.get("/big", |_req| async { vec![b'x'; 32 * 1024 * 1024] });
    app.get("/", |_req| async { "ok" });
    let (addr, _trigger, _server) = start_with_shutdown(app).await;

    // This client requests a large body and never reads it.
    let mut stalled = Client::connect(addr).await;
    stalled.send(b"GET /big HTTP/1.1\r\nHost: t\r\n\r\n").await;

    // With one connection slot, the next client is served only once the
    // stalled connection is closed by the write timeout.
    let started = Instant::now();
    let mut next = Client::connect(addr).await;
    next.send(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n").await;
    let r = next.read().await.unwrap();
    assert_eq!(r.text(), "ok");
    assert!(started.elapsed() >= Duration::from_millis(150), "slot was free too early");
    assert!(started.elapsed() < Duration::from_secs(3));

    // The stalled client never gets the whole body.
    let mut raw = Vec::new();
    use tokio::io::AsyncReadExt;
    let _ = timeout(Duration::from_secs(2), stalled.reader.read_to_end(&mut raw)).await;
    assert!(raw.len() < 32 * 1024 * 1024);
}

#[tokio::test]
async fn hostile_form_bodies_are_rejected_without_crashing() {
    let mut server = Server::new();
    server.post("/form", |req| async move {
        req.form_value().map(Json)
    });
    let app = server.into_app().await.unwrap();
    let post = |body: String| Request::new(Method::POST, "/form").with_body("application/x-www-form-urlencoded", body);

    // Would overflow the stack and abort the process with a recursive parser.
    let deep = format!("a{}=1", "[x]".repeat(200_000));
    assert_eq!(app.call(post(deep)).await.status, 400);
    // Would allocate a billion-element array.
    assert_eq!(app.call(post("a[999999999]=x".into())).await.status, 400);
    let ok = app.call(post("user[name]=Ada&tags[]=a&tags[]=b".into())).await;
    assert_eq!(ok.body_text(), r#"{"tags":["a","b"],"user":{"name":"Ada"}}"#);
}

#[cfg(feature = "ws")]
mod websocket {
    use super::*;
    use axeon::ws::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    async fn connect(addr: SocketAddr) -> tokio_tungstenite::WebSocketStream<TcpStream> {
        let tcp = TcpStream::connect(addr).await.unwrap();
        tokio_tungstenite::client_async(format!("ws://{}/ws", addr), tcp).await.unwrap().0
    }

    fn cooperative_app() -> Server {
        let mut app = Server::new();
        app.shutdown_timeout(Duration::from_secs(10));
        app.get("/ws", |mut req| async move {
            let shutdown = req.shutdown();
            req.upgrade_websocket(move |mut socket| async move {
                loop {
                    tokio::select! {
                        message = socket.recv() => match message {
                            Some(Ok(message)) if message.is_text() => { let _ = socket.send(message).await; }
                            Some(Ok(_)) => {}
                            _ => break,
                        },
                        _ = shutdown.wait() => {
                            let _ = socket.close(1001, "restarting").await;
                            break;
                        }
                    }
                }
            })
        });
        app
    }

    #[tokio::test]
    async fn handlers_are_told_about_shutdown() {
        let (addr, trigger, server) = start_with_shutdown(cooperative_app()).await;
        let mut ws = connect(addr).await;
        ws.send(Message::text("hi")).await.unwrap();
        assert_eq!(ws.next().await.unwrap().unwrap(), Message::text("hi"));

        let started = Instant::now();
        trigger.send(()).unwrap();
        match timeout(Duration::from_secs(2), ws.next()).await.expect("no close frame") {
            Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 1001),
            other => panic!("expected a close frame, got {:?}", other),
        }
        timeout(Duration::from_secs(2), server).await.expect("server did not stop").unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn sockets_that_ignore_shutdown_are_closed_after_the_timeout() {
        let mut app = Server::new();
        app.shutdown_timeout(Duration::from_millis(200));
        app.get("/ws", |mut req| async move {
            req.upgrade_websocket(|mut socket| async move { while socket.next().await.is_some() {} })
        });
        let (addr, trigger, server) = start_with_shutdown(app).await;
        let mut ws = connect(addr).await;

        let started = Instant::now();
        trigger.send(()).unwrap();
        timeout(Duration::from_secs(2), server).await.expect("server did not stop").unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        let next = timeout(Duration::from_secs(1), ws.next()).await.expect("socket still open");
        assert!(!matches!(next, Some(Ok(Message::Text(_)))));
    }

    #[tokio::test]
    async fn sockets_count_toward_max_connections() {
        let mut app = cooperative_app();
        app.max_connections(1);
        app.get("/", |_req| async { "ok" });
        let (addr, _trigger, _server) = start_with_shutdown(app).await;
        let mut ws = connect(addr).await;
        ws.send(Message::text("ping")).await.unwrap();
        ws.next().await.unwrap().unwrap();

        let mut c = Client::connect(addr).await;
        c.send(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n").await;
        let waited = timeout(Duration::from_millis(300), c.read_inner(false)).await;
        assert!(waited.is_err(), "served while the WebSocket held the only slot");

        ws.close(None).await.unwrap();
        assert_eq!(c.read().await.unwrap().text(), "ok");
    }
}
