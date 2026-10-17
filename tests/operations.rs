//! Health checks, metrics and the matched route.

mod common;

use axeon::health::Health;
use axeon::metrics::Metrics;
use axeon::{Method, Request, Server, ServerError};
use std::time::Duration;

#[tokio::test]
async fn liveness_and_readiness() {
    let mut server = Server::new();
    server.mount(
        "/",
        Health::new()
            .timeout(Duration::from_millis(100))
            .check("database", || async { Ok::<_, std::io::Error>(()) })
            .check("cache", || async { Err::<(), _>("connection refused") })
            .check("slow", || async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok::<_, std::io::Error>(())
            })
            .router(),
    );
    let app = server.into_app().await.unwrap();

    let live = app.call(Request::new(Method::GET, "/health")).await;
    assert_eq!((live.status, live.body_text().as_ref()), (200, r#"{"status":"ok"}"#));

    let started = std::time::Instant::now();
    let ready = app.call(Request::new(Method::GET, "/ready")).await;
    assert!(started.elapsed() < Duration::from_secs(1), "checks didn't run concurrently with a timeout");
    assert_eq!(ready.status, 503);
    let body: serde_json::Value = serde_json::from_slice(&ready.body).unwrap();
    assert_eq!(body["status"], "unavailable");
    assert_eq!(body["checks"]["database"]["status"], "up");
    assert_eq!(body["checks"]["cache"]["error"], "connection refused");
    assert!(body["checks"]["slow"]["error"].as_str().unwrap().contains("timed out"));
}

#[tokio::test]
async fn readiness_fails_during_the_shutdown_delay() {
    let mut server = Server::new();
    server.shutdown_delay(Duration::from_millis(300));
    server.mount("/", Health::new().router());
    server.get("/", |_req| async { "still serving" });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move { server.serve_with_shutdown(listener, async { let _ = rx.await; }).await });

    assert_eq!(common::Client::connect(addr).await.get("/ready").await.status, 200);
    tx.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // During the delay new connections are still accepted and served, but
    // readiness reports shutting down so load balancers move traffic away.
    let mut c = common::Client::connect(addr).await;
    let ready = c.get("/ready").await;
    assert_eq!(ready.status, 503);
    assert!(ready.text().contains("shutting_down"));
    assert_eq!(c.get("/").await.text(), "still serving");

    // After the delay the server stops.
    tokio::time::timeout(Duration::from_secs(2), server).await.expect("server did not stop").unwrap().unwrap();
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn metrics_count_requests_by_route_pattern() {
    let metrics = Metrics::new();
    let mut server = Server::new();
    server.middleware(metrics.clone());
    server.get("/users/:id", |req| async move { req.matched_path().unwrap_or("").to_string() });
    server.get("/fail", |_req| async { Err::<&'static str, _>(ServerError::InternalError("x".into())) });
    server.mount("/", metrics.router());
    let app = server.into_app().await.unwrap();

    let res = app.call(Request::new(Method::GET, "/users/1")).await;
    assert_eq!(res.body_text(), "/users/:id");
    app.call(Request::new(Method::GET, "/users/2")).await;
    app.call(Request::new(Method::GET, "/fail")).await;
    app.call(Request::new(Method::GET, "/nope")).await;

    let res = app.call(Request::new(Method::GET, "/metrics")).await;
    assert!(res.get_header("content-type").unwrap().starts_with("text/plain; version=0.0.4"));
    let text = res.body_text().to_string();
    assert!(text.contains(r#"http_requests_total{method="GET",route="/users/:id",status="200"} 2"#), "{}", text);
    assert!(text.contains(r#"http_requests_total{method="GET",route="/fail",status="500"} 1"#), "{}", text);
    assert!(text.contains(r#"http_requests_total{method="GET",route="unmatched",status="404"} 1"#), "{}", text);
    assert!(text.contains(r#"http_request_duration_seconds_bucket{method="GET",route="/users/:id",status="200",le="+Inf"} 2"#), "{}", text);
    assert!(text.contains(r#"http_request_duration_seconds_count{method="GET",route="/users/:id",status="200"} 2"#), "{}", text);
    // The /metrics request itself is still in flight while rendering.
    assert!(text.contains("http_requests_in_flight 1"), "{}", text);
    // Buckets are cumulative and never decrease.
    let buckets: Vec<u64> = text
        .lines()
        .filter(|l| l.starts_with(r#"http_request_duration_seconds_bucket{method="GET",route="/users/:id""#))
        .map(|l| l.rsplit(' ').next().unwrap().parse().unwrap())
        .collect();
    assert!(buckets.windows(2).all(|w| w[0] <= w[1]), "{:?}", buckets);
}
