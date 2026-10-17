//! Route builder (per-route middleware and body settings), streaming
//! request bodies and multipart uploads.

mod common;

use axeon::middleware::from_fn;
use axeon::{Method, Request, Response, Router, Server, ServerError};
use common::{start, Client, TempDir};
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

fn tag(name: &'static str) -> impl axeon::middleware::Middleware {
    from_fn(move |req, next| async move {
        let mut res = next.handle(req).await?;
        let seen = match res.get_header("x-seen") {
            Some(existing) => format!("{},{}", name, existing),
            None => name.to_string(),
        };
        res.header("X-Seen", seen);
        Ok(res)
    })
}

fn require_key() -> impl axeon::middleware::Middleware {
    from_fn(|req, next| async move {
        if req.get_header("x-key") != Some("secret") {
            return Err(ServerError::Unauthorized("key required".into()));
        }
        next.handle(req).await
    })
}

#[tokio::test]
async fn route_middleware_is_scoped_to_its_builder() {
    let mut server = Server::new();
    server.route("/posts").get(|_req| async { "list" });
    server.route("/posts").middleware(require_key()).post(|_req| async { (201, "created") });
    let app = server.into_app().await.unwrap();

    assert_eq!(app.call(Request::new(Method::GET, "/posts")).await.status, 200);
    assert_eq!(app.call(Request::new(Method::POST, "/posts")).await.status, 401);
    let ok = app.call(Request::new(Method::POST, "/posts").with_header("x-key", "secret")).await;
    assert_eq!(ok.status, 201);
}

#[tokio::test]
async fn route_middleware_runs_innermost_and_in_any_order() {
    let mut server = Server::new();
    server.middleware(tag("global"));
    let mut api = Router::new();
    api.middleware(tag("router"));
    // Middleware added after the handler still applies to it.
    api.route("/item").get(|_req| async { "item" }).middleware(tag("route"));
    server.mount("/api", api);
    let app = server.into_app().await.unwrap();

    let res = app.call(Request::new(Method::GET, "/api/item")).await;
    assert_eq!(res.get_header("x-seen"), Some("global,router,route"));
    // Other methods on the path aren't registered, so they get 405 (with global middleware only).
    let res = app.call(Request::new(Method::DELETE, "/api/item")).await;
    assert_eq!((res.status, res.get_header("x-seen")), (405, Some("global")));
}

#[tokio::test]
async fn body_limits_are_per_route() {
    let mut server = Server::new();
    server.max_body_size(1024);
    server.post("/small", |req| async move { format!("{}", req.body.as_bytes().len()) });
    server.route("/big").body_limit(512 * 1024).post(|req| async move { format!("{}", req.body.as_bytes().len()) });
    let addr = start(server).await;

    let body = vec![b'x'; 100 * 1024];
    let send = |path: &str| {
        let mut raw = format!("POST {} HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n", path, body.len()).into_bytes();
        raw.extend_from_slice(&body);
        raw
    };
    let mut c = Client::connect(addr).await;
    c.send(&send("/big")).await;
    assert_eq!(c.read().await.unwrap().text(), "102400");
    let mut c = Client::connect(addr).await;
    c.send(&send("/small")).await;
    assert_eq!(c.read().await.unwrap().status, 413);
}

#[tokio::test]
async fn streamed_bodies_reach_the_handler_before_they_finish() {
    let first_chunk_seen = Arc::new(Notify::new());
    let notify = first_chunk_seen.clone();
    let mut server = Server::new();
    server.route("/upload").stream_body().post(move |req| {
        let notify = notify.clone();
        async move {
            let mut body = req.body_stream()?;
            let mut total = 0;
            let mut chunks = 0;
            while let Some(chunk) = body.next().await {
                total += chunk?.len();
                chunks += 1;
                if chunks == 1 {
                    notify.notify_one();
                }
            }
            Ok::<_, ServerError>(format!("{} bytes", total))
        }
    });
    let addr = start(server).await;

    let mut c = Client::connect(addr).await;
    c.send(b"POST /upload HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n").await;
    // The handler sees the first chunk while the client still holds the rest.
    timeout(Duration::from_secs(2), first_chunk_seen.notified()).await.expect("body was buffered, not streamed");
    c.send(b"6\r\n world\r\n0\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().text(), "11 bytes");
}

#[tokio::test]
async fn streamed_uploads_can_be_saved_to_disk() {
    let dir = TempDir::new("stream-save");
    let target = dir.0.join("upload.bin");
    let target_for_handler = target.clone();
    let mut server = Server::new();
    server.route("/upload").body_limit(16 * 1024 * 1024).stream_body().post(move |req| {
        let target = target_for_handler.clone();
        async move {
            let written = req.body_stream()?.save_to(&target).await?;
            Ok::<_, ServerError>(written.to_string())
        }
    });
    let addr = start(server).await;

    let data: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i % 253) as u8).collect();
    let mut raw = format!("POST /upload HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n", data.len()).into_bytes();
    raw.extend_from_slice(&data);
    let mut c = Client::connect(addr).await;
    c.send(&raw).await;
    assert_eq!(c.read().await.unwrap().text(), data.len().to_string());
    assert!(std::fs::read(&target).unwrap() == data, "saved file differs");
}

#[tokio::test]
async fn streamed_bodies_enforce_limits_and_timeouts() {
    let mut server = Server::new();
    server.request_timeout(Duration::from_millis(200));
    server.route("/upload").body_limit(10).stream_body().post(|req| async move {
        let bytes = req.body_stream()?.bytes().await?;
        Ok::<_, ServerError>(bytes.len().to_string())
    });
    let addr = start(server).await;

    // Declared too large: refused before reading.
    let mut c = Client::connect(addr).await;
    c.send(b"POST /upload HTTP/1.1\r\nHost: t\r\nContent-Length: 11\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().status, 413);

    // Too large while streaming (chunked, no length).
    let mut c = Client::connect(addr).await;
    c.send(b"POST /upload HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n8\r\n12345678\r\n8\r\n12345678\r\n0\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().status, 413);

    // The client stops sending.
    let mut c = Client::connect(addr).await;
    c.send(b"POST /upload HTTP/1.1\r\nHost: t\r\nContent-Length: 8\r\n\r\n1234").await;
    assert_eq!(c.read().await.unwrap().status, 408);
}

#[tokio::test]
async fn body_stream_on_buffered_routes_and_taking_twice() {
    let mut server = Server::new();
    server.post("/buffered", |req| async move {
        let bytes = req.body_stream()?.bytes().await?;
        Ok::<_, ServerError>(String::from_utf8_lossy(&bytes).into_owned())
    });
    server.route("/twice").stream_body().post(|req| async move {
        let _first = req.body_stream()?;
        req.body_stream()?;
        Ok::<_, ServerError>("unreachable")
    });
    let addr = start(server).await;
    let mut c = Client::connect(addr).await;
    c.send(b"POST /buffered HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\n\r\nabc").await;
    assert_eq!(c.read().await.unwrap().text(), "abc");
    c.send(b"POST /twice HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\n\r\nabc").await;
    assert_eq!(c.read().await.unwrap().status, 500);
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn multipart_uploads_stream_fields_and_files() {
    let dir = TempDir::new("multipart");
    let saved = dir.0.join("file.bin");
    let saved_for_handler = saved.clone();
    let mut server = Server::new();
    server.route("/upload").body_limit(4 * 1024 * 1024).stream_body().post(move |req| {
        let saved = saved_for_handler.clone();
        async move {
            let mut form = req.multipart()?;
            let mut summary = Vec::new();
            while let Some(field) = form.next_field().await? {
                let name = field.name().unwrap_or("").to_string();
                if let Some(file_name) = field.file_name().map(str::to_string) {
                    let content_type = field.content_type().unwrap_or("").to_string();
                    let size = field.save_to(&saved).await?;
                    summary.push(format!("{}={}({},{})", name, file_name, content_type, size));
                } else {
                    summary.push(format!("{}={}", name, field.text().await?));
                }
            }
            Ok::<_, ServerError>(summary.join(";"))
        }
    });
    let addr = start(server).await;

    let file: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
    let mut body = b"--XYZ\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nHoliday\r\n--XYZ\r\n\
Content-Disposition: form-data; name=\"photo\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n"
        .to_vec();
    body.extend_from_slice(&file);
    body.extend_from_slice(b"\r\n--XYZ--\r\n");
    let mut raw = format!(
        "POST /upload HTTP/1.1\r\nHost: t\r\nContent-Type: multipart/form-data; boundary=XYZ\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(&body);
    let mut c = Client::connect(addr).await;
    c.send(&raw).await;
    let r = c.read().await.unwrap();
    assert_eq!(r.text(), "title=Holiday;photo=a.png(image/png,300000)");
    assert!(std::fs::read(&saved).unwrap() == file, "saved file differs");

    // Not multipart: 415.
    c.send(b"POST /upload HTTP/1.1\r\nHost: t\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nhi").await;
    assert_eq!(c.read().await.unwrap().status, 415);
}

#[tokio::test]
async fn route_builder_supports_all_methods_and_responses() {
    let mut server = Server::new();
    server
        .route("/thing")
        .get(|_req| async { "get" })
        .put(|_req| async { "put" })
        .patch(|_req| async { "patch" })
        .delete(|_req| async {});
    let app = server.into_app().await.unwrap();
    let call = |m| app.call(Request::new(m, "/thing"));
    assert_eq!(call(Method::GET).await.body_text(), "get");
    assert_eq!(call(Method::PUT).await.body_text(), "put");
    assert_eq!(call(Method::PATCH).await.body_text(), "patch");
    assert_eq!(call(Method::DELETE).await.status, 204);
    let res: Response = call(Method::POST).await;
    assert_eq!(res.status, 405);
}
