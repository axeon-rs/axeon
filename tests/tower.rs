//! Tower interop: tower-http layers as middleware, and the app as a tower
//! Service.

mod common;

use axeon::middleware::from_fn;
use axeon::{Json, Method, Request, Server, ServerError};
use common::{start, Client};
use futures::StreamExt;
use http::header::{HeaderName, HeaderValue};
use http_body_util::BodyExt;
use std::io::{Read, Write};
use std::time::Duration;
use tower::ServiceExt;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::decompression::RequestDecompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;

fn header_layer(name: &'static str, value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::overriding(HeaderName::from_static(name), HeaderValue::from_static(value))
}

#[tokio::test]
async fn tower_layers_run_as_middleware_at_every_level() {
    let mut server = Server::new();
    server.layer(header_layer("x-global", "1"));
    server.get("/plain", |_req| async { "plain" });
    let mut api = axeon::Router::new();
    api.layer(header_layer("x-router", "1"));
    api.route("/item").layer(header_layer("x-route", "1")).get(|req| async move {
        // Path params and the body survive the trip through tower.
        format!("item {}", req.path())
    });
    server.mount("/api", api);
    let app = server.into_app().await.unwrap();

    let res = app.call(Request::new(Method::GET, "/api/item")).await;
    assert_eq!(res.body_text(), "item /api/item");
    for header in ["x-global", "x-router", "x-route"] {
        assert_eq!(res.get_header(header), Some("1"), "{}", header);
    }
    let res = app.call(Request::new(Method::GET, "/plain")).await;
    assert_eq!((res.get_header("x-global"), res.get_header("x-router")), (Some("1"), None));
    // Global layers also cover 404s.
    assert_eq!(app.call(Request::new(Method::GET, "/missing")).await.get_header("x-global"), Some("1"));
}

#[tokio::test]
async fn timeout_layer_cuts_off_slow_handlers() {
    let mut server = Server::new();
    server.layer(TimeoutLayer::with_status_code(http::StatusCode::REQUEST_TIMEOUT, Duration::from_millis(100)));
    server.get("/slow", |_req| async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        "too late"
    });
    let app = server.into_app().await.unwrap();
    let started = std::time::Instant::now();
    assert_eq!(app.call(Request::new(Method::GET, "/slow")).await.status, 408);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn cors_layer_answers_preflights() {
    let mut server = Server::new();
    server.layer(CorsLayer::new().allow_origin(Any).allow_methods([http::Method::GET, http::Method::POST]));
    server.post("/items", |_req| async { "ok" });
    let app = server.into_app().await.unwrap();

    let preflight = Request::new(Method::OPTIONS, "/items")
        .with_header("origin", "https://example.com")
        .with_header("access-control-request-method", "POST");
    let res = app.call(preflight).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.get_header("access-control-allow-origin"), Some("*"));
    let res = app.call(Request::new(Method::POST, "/items").with_header("origin", "https://example.com")).await;
    assert_eq!(res.get_header("access-control-allow-origin"), Some("*"));
}

#[tokio::test]
async fn compression_layer_changes_the_body_type() {
    let mut server = Server::new();
    server.layer(CompressionLayer::new());
    server.get("/big", |_req| async { "axeon ".repeat(2000) });
    let addr = start(server).await;

    let mut c = Client::connect(addr).await;
    c.send(b"GET /big HTTP/1.1\r\nHost: t\r\nAccept-Encoding: gzip\r\n\r\n").await;
    let r = c.read().await.unwrap();
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    let mut decoded = String::new();
    flate2::read::GzDecoder::new(&r.body[..]).read_to_string(&mut decoded).unwrap();
    assert_eq!(decoded, "axeon ".repeat(2000));
}

#[tokio::test]
async fn request_decompression_layer_feeds_buffered_handlers() {
    #[derive(serde::Deserialize, serde::Serialize)]
    struct Item {
        name: String,
    }
    let mut server = Server::new();
    server.layer(RequestDecompressionLayer::new());
    server.post("/items", |req| async move {
        let item: Item = req.json()?;
        Ok::<_, ServerError>(Json(item))
    });
    let addr = start(server).await;

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(br#"{"name":"Ada"}"#).unwrap();
    let gz = encoder.finish().unwrap();
    let mut raw = format!(
        "POST /items HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
        gz.len()
    )
    .into_bytes();
    raw.extend_from_slice(&gz);
    let mut c = Client::connect(addr).await;
    c.send(&raw).await;
    assert_eq!(c.read().await.unwrap().text(), r#"{"name":"Ada"}"#);
}

#[tokio::test]
async fn streaming_routes_keep_streaming_through_layers() {
    let first_chunk_seen = std::sync::Arc::new(tokio::sync::Notify::new());
    let notify = first_chunk_seen.clone();
    let mut server = Server::new();
    server.layer(header_layer("x-layer", "1"));
    server.middleware(from_fn(|req, next| async move { next.handle(req).await }));
    server.route("/upload").layer(header_layer("x-route-layer", "1")).stream_body().post(move |req| {
        let notify = notify.clone();
        async move {
            let mut body = req.body_stream()?;
            let mut total = 0;
            while let Some(chunk) = body.next().await {
                total += chunk?.len();
                notify.notify_one();
            }
            Ok::<_, ServerError>(format!("{} bytes", total))
        }
    });
    let addr = start(server).await;
    let mut c = Client::connect(addr).await;
    c.send(b"POST /upload HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n").await;
    // Still a live stream after two tower layers: the first chunk arrives
    // while the client holds back the rest.
    tokio::time::timeout(Duration::from_secs(2), first_chunk_seen.notified())
        .await
        .expect("body was buffered by the layers");
    c.send(b"1\r\nb\r\n0\r\n\r\n").await;
    let r = c.read().await.unwrap();
    assert_eq!(r.text(), "2 bytes");
    assert_eq!((r.header("x-layer"), r.header("x-route-layer")), (Some("1"), Some("1")));
}

#[tokio::test]
async fn app_runs_as_a_tower_service() {
    let mut server = Server::new();
    server.max_body_size(16);
    server.get("/users/:id", |req| async move { format!("user {}", req.params()["id"]) });
    server.post("/echo", |req| async move { req.body().to_vec() });
    let service = server.into_app().await.unwrap().into_service();

    let res = service.clone().oneshot(http::Request::get("/users/7").body(String::new()).unwrap()).await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.into_body().collect().await.unwrap().to_bytes(), "user 7");

    let res = service.clone().oneshot(http::Request::post("/echo").body("hello".to_string()).unwrap()).await.unwrap();
    assert_eq!(res.into_body().collect().await.unwrap().to_bytes(), "hello");

    let big = http::Request::post("/echo").body("x".repeat(100)).unwrap();
    assert_eq!(service.clone().oneshot(big).await.unwrap().status(), 413);
    let missing = http::Request::get("/nope").body(String::new()).unwrap();
    assert_eq!(service.oneshot(missing).await.unwrap().status(), 404);
}
