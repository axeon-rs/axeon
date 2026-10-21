//! Handler timeouts: the server default and per-route overrides.

mod common;

use axeon::metrics::Metrics;
use axeon::middleware::from_fn;
use axeon::sse::{self, Event};
use axeon::{Method, Request, Router, Server, ServerError};
use common::{start, Client};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

const SHORT: Duration = Duration::from_millis(100);
const SLOW: Duration = Duration::from_secs(2);

/// A handler that takes `delay`.
fn sleeps(delay: Duration) -> impl Fn(Request) -> std::pin::Pin<Box<dyn std::future::Future<Output = &'static str> + Send>> + Send + Sync {
    move |_req| {
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            "finished"
        })
    }
}

#[tokio::test]
async fn no_limit_by_default() {
    let mut server = Server::new();
    server.get("/slow", sleeps(Duration::from_millis(300)));
    let app = server.into_app().await.unwrap();
    let res = app.call(Request::new(Method::GET, "/slow")).await;
    assert_eq!((res.status, res.body_text().as_ref()), (200, "finished"));
}

#[tokio::test]
async fn the_server_default_and_route_overrides() {
    let mut server = Server::new();
    server.handler_timeout(SHORT);
    server.get("/fast", |_req| async { "fast" });
    server.get("/slow", sleeps(SLOW));
    server.route("/longer").timeout(Duration::from_secs(5)).get(sleeps(Duration::from_millis(300)));
    server.route("/shorter").timeout(Duration::from_millis(20)).get(sleeps(Duration::from_millis(60)));
    server.route("/unlimited").no_timeout().get(sleeps(Duration::from_millis(300)));
    // Set after the handler, it still applies; and to every method of the builder.
    server.route("/late").get(sleeps(SLOW)).post(sleeps(SLOW)).timeout(Duration::from_millis(30));
    let app = server.into_app().await.unwrap();

    let call = |method, path: &'static str| app.call(Request::new(method, path));
    assert_eq!(call(Method::GET, "/fast").await.status, 200);

    let started = Instant::now();
    let res = call(Method::GET, "/slow").await;
    assert_eq!(res.status, 503);
    assert!(started.elapsed() < Duration::from_secs(1), "gave up at the limit, not when the handler finished");
    let body: serde_json::Value = serde_json::from_str(&res.body_text()).unwrap();
    assert_eq!(body["error"]["status"], 503);
    assert_eq!(body["error"]["message"], "The server took too long to respond");

    assert_eq!(call(Method::GET, "/longer").await.status, 200);
    assert_eq!(call(Method::GET, "/shorter").await.status, 503);
    assert_eq!(call(Method::GET, "/unlimited").await.status, 200);
    assert_eq!(call(Method::GET, "/late").await.status, 503);
    assert_eq!(call(Method::POST, "/late").await.status, 503);
    // HEAD served by the GET handler keeps its limit.
    assert_eq!(call(Method::HEAD, "/slow").await.status, 503);
    // Not-found and 405 responses aren't affected.
    assert_eq!(call(Method::GET, "/nope").await.status, 404);
    assert_eq!(call(Method::DELETE, "/fast").await.status, 405);
}

#[tokio::test]
async fn route_middleware_counts_and_global_middleware_sees_the_503() {
    let metrics = Metrics::new();
    let mut server = Server::new();
    server.middleware(metrics.clone());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    server.middleware(from_fn(move |req, next| {
        let log = log.clone();
        async move {
            let result = next.handle(req).await;
            log.lock().unwrap().push(result.as_ref().map(|r| r.status).unwrap_or_else(|e| e.status_code()));
            result
        }
    }));
    // Slow route middleware uses up the route's time.
    let slow_auth = from_fn(|req, next| async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        next.handle(req).await
    });
    server.route("/guarded").timeout(SHORT).middleware(slow_auth).get(|_req| async { "ok" });
    server.mount("/", metrics.router());
    let app = server.into_app().await.unwrap();

    assert_eq!(app.call(Request::new(Method::GET, "/guarded")).await.status, 503);
    assert_eq!(*seen.lock().unwrap(), [503]);
    let text = app.call(Request::new(Method::GET, "/metrics")).await.body_text().to_string();
    assert!(text.contains(r#"http_requests_total{method="GET",route="/guarded",status="503"} 1"#), "{}", text);
}

#[tokio::test]
async fn custom_error_handlers_see_the_timeout() {
    let mut server = Server::new();
    server.handler_timeout(SHORT);
    server.on_error(|err| {
        let timed_out = matches!(err, ServerError::HandlerTimeout);
        let mut res = axeon::Response::new(err.status_code());
        res.body(format!("timed out: {}", timed_out));
        res
    });
    server.get("/slow", sleeps(SLOW));
    let app = server.into_app().await.unwrap();
    let res = app.call(Request::new(Method::GET, "/slow")).await;
    assert_eq!((res.status, res.body_text().as_ref()), (503, "timed out: true"));
}

#[tokio::test]
async fn timed_out_handlers_are_cancelled() {
    struct SetOnDrop(Arc<AtomicBool>);
    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let (d, f) = (dropped.clone(), finished.clone());
    let mut server = Server::new();
    server.route("/work").timeout(SHORT).post(move |_req| {
        let (guard, finished) = (SetOnDrop(d.clone()), f.clone());
        async move {
            let _guard = guard; // e.g. an open transaction
            tokio::time::sleep(SLOW).await;
            finished.store(true, Ordering::SeqCst);
            "done"
        }
    });
    let app = server.into_app().await.unwrap();
    assert_eq!(app.call(Request::new(Method::POST, "/work")).await.status, 503);
    assert!(dropped.load(Ordering::SeqCst), "what the handler held was dropped");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!finished.load(Ordering::SeqCst), "the handler didn't keep running");
}

#[tokio::test]
async fn mounted_routers_keep_their_timeouts() {
    let mut api = Router::new();
    api.get("/slow", sleeps(Duration::from_millis(300)));
    api.route("/report").timeout(Duration::from_secs(5)).get(sleeps(Duration::from_millis(300)));
    let mut v1 = Router::new();
    v1.mount("/v1", api);

    let mut server = Server::new();
    server.handler_timeout(SHORT);
    server.mount("/api", v1);
    let app = server.into_app().await.unwrap();
    assert_eq!(app.call(Request::new(Method::GET, "/api/v1/slow")).await.status, 503, "server default applies");
    assert_eq!(app.call(Request::new(Method::GET, "/api/v1/report")).await.status, 200, "route override survives mounting");
}

#[tokio::test]
async fn streamed_responses_only_have_to_start_in_time() {
    let mut server = Server::new();
    server.handler_timeout(SHORT);
    server.get("/events", |_req| async {
        let (tx, response) = sse::channel(4);
        tokio::spawn(async move {
            for i in 0..5 {
                tokio::time::sleep(Duration::from_millis(60)).await;
                if tx.send(Event::new().data(format!("n={}", i))).await.is_err() {
                    break;
                }
            }
        });
        response
    });
    let addr = start(server).await;

    let mut client = Client::connect(addr).await;
    client.send(b"GET /events HTTP/1.1\r\nHost: t\r\n\r\n").await;
    let mut seen = String::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut buf = [0u8; 1024];
        while !seen.contains("data: n=4\n") {
            let n = client.reader.read(&mut buf).await.unwrap();
            assert!(n > 0, "stream ended early: {}", seen);
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    })
    .await
    .expect("events stopped");
    assert!(seen.starts_with("HTTP/1.1 200"), "{}", seen);
}

#[tokio::test]
async fn streamed_request_bodies_are_read_within_the_limit() {
    let mut server = Server::new();
    server.route("/upload").stream_body().timeout(Duration::from_millis(300)).post(|req| async move {
        let size = req.body_stream()?.bytes().await?.len();
        Ok::<_, ServerError>(format!("{} bytes", size))
    });
    let addr = start(server).await;

    // In time.
    let mut client = Client::connect(addr).await;
    client.send(b"POST /upload HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\n\r\nhello").await;
    let res = client.read().await.unwrap();
    assert_eq!((res.status, res.text()), (200, "5 bytes".to_string()));

    // A client that stops sending part-way runs out the route's time.
    let mut client = Client::connect(addr).await;
    client.send(b"POST /upload HTTP/1.1\r\nHost: t\r\nContent-Length: 10\r\n\r\nhel").await;
    let res = tokio::time::timeout(Duration::from_secs(2), client.read()).await.expect("no response").unwrap();
    assert_eq!(res.status, 503);
}
