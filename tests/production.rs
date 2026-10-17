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
