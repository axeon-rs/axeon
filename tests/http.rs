//! End-to-end tests over a real TCP socket, using raw HTTP/1.1 so the wire
//! format itself is checked.

use axeon::middleware::{CompressionConfig, CompressionMiddleware, Cors, CorsConfig, RateLimitConfig, RateLimiter};
use axeon::{Response, Server, ServerError};
use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

async fn start(app: Server) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = app.serve(listener).await;
    });
    addr
}

#[derive(Debug)]
struct Resp {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

struct Client {
    reader: BufReader<TcpStream>,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        Client { reader: BufReader::new(TcpStream::connect(addr).await.unwrap()) }
    }

    async fn send(&mut self, raw: &[u8]) {
        self.reader.get_mut().write_all(raw).await.unwrap();
    }

    async fn get(&mut self, path: &str) -> Resp {
        self.send(format!("GET {} HTTP/1.1\r\nHost: test\r\n\r\n", path).as_bytes()).await;
        self.read().await.expect("connection closed")
    }

    /// Reads one response, or `None` if the server closed the connection.
    async fn read(&mut self) -> Option<Resp> {
        self.read_for(false).await
    }

    async fn read_for(&mut self, head_request: bool) -> Option<Resp> {
        timeout(Duration::from_secs(5), self.read_inner(head_request))
            .await
            .expect("timed out waiting for response")
    }

    async fn read_inner(&mut self, head_request: bool) -> Option<Resp> {
        let mut line = String::new();
        if self.reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let status: u16 = line.split(' ').nth(1).expect("status line").parse().unwrap();
        assert!(line.starts_with("HTTP/1."), "bad status line {:?}", line);
        let mut headers = HashMap::new();
        loop {
            line.clear();
            self.reader.read_line(&mut line).await.unwrap();
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            let (k, v) = l.split_once(':').expect("header line");
            let prev = headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
            assert!(prev.is_none(), "duplicate header {}", k);
        }
        let mut body = Vec::new();
        let has_body = !head_request && status != 204 && status != 304 && !(100..200).contains(&status);
        if has_body {
            let len: usize = headers.get("content-length").expect("content-length").parse().unwrap();
            body.resize(len, 0);
            self.reader.read_exact(&mut body).await.unwrap();
        }
        Some(Resp { status, headers, body })
    }

    /// True if the server closes the connection within the timeout.
    async fn closed_within(&mut self, dur: Duration) -> bool {
        let mut buf = [0u8; 1];
        matches!(timeout(dur, self.reader.read(&mut buf)).await, Ok(Ok(0)) | Ok(Err(_)))
    }
}

fn hello_app() -> Server {
    let mut app = Server::new();
    app.get("/", |_req| async { Response::text("Hello, World!") });
    app
}

#[tokio::test]
async fn keep_alive_serves_many_requests_on_one_connection() {
    let addr = start(hello_app()).await;
    let mut c = Client::connect(addr).await;
    for _ in 0..3 {
        let r = c.get("/").await;
        assert_eq!(r.status, 200);
        assert_eq!(r.text(), "Hello, World!");
        assert!(r.header("date").is_some());
        assert!(r.header("connection").is_none());
    }
}

#[tokio::test]
async fn pipelined_requests_are_answered_in_order() {
    let mut app = hello_app();
    app.get("/two", |_req| async { Response::text("two") });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;
    c.send(b"GET / HTTP/1.1\r\nHost: t\r\n\r\nGET /two HTTP/1.1\r\nHost: t\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().text(), "Hello, World!");
    assert_eq!(c.read().await.unwrap().text(), "two");
}

#[tokio::test]
async fn connection_close_is_honoured() {
    let addr = start(hello_app()).await;
    let mut c = Client::connect(addr).await;
    c.send(b"GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    let r = c.read().await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.header("connection"), Some("close"));
    assert!(c.closed_within(Duration::from_secs(2)).await);
}

#[tokio::test]
async fn http10_closes_unless_keep_alive_requested() {
    let addr = start(hello_app()).await;
    let mut c = Client::connect(addr).await;
    c.send(b"GET / HTTP/1.0\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().status, 200);
    assert!(c.closed_within(Duration::from_secs(2)).await);

    let mut c = Client::connect(addr).await;
    c.send(b"GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n").await;
    let r = c.read().await.unwrap();
    assert_eq!(r.header("connection"), Some("keep-alive"));
    c.send(b"GET / HTTP/1.0\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().status, 200);
}

#[tokio::test]
async fn head_reports_get_length_without_body() {
    let addr = start(hello_app()).await;
    let mut c = Client::connect(addr).await;
    c.send(b"HEAD / HTTP/1.1\r\nHost: t\r\n\r\n").await;
    let r = c.read_for(true).await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-length"), Some("13"));
    // No stray body bytes: the next response parses cleanly.
    assert_eq!(c.get("/").await.text(), "Hello, World!");
}

#[tokio::test]
async fn unknown_method_is_not_implemented() {
    let addr = start(hello_app()).await;
    let mut c = Client::connect(addr).await;
    c.send(b"FOO / HTTP/1.1\r\nHost: t\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().status, 501);
}

#[tokio::test]
async fn wrong_method_is_405_with_allow() {
    let addr = start(hello_app()).await;
    let mut c = Client::connect(addr).await;
    c.send(b"DELETE / HTTP/1.1\r\nHost: t\r\n\r\n").await;
    let r = c.read().await.unwrap();
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("GET, HEAD, OPTIONS"));
    // Unknown paths are still 404.
    assert_eq!(c.get("/missing").await.status, 404);
}

#[tokio::test]
async fn options_is_answered_automatically() {
    let addr = start(hello_app()).await;
    let mut c = Client::connect(addr).await;
    c.send(b"OPTIONS / HTTP/1.1\r\nHost: t\r\n\r\n").await;
    let r = c.read().await.unwrap();
    assert_eq!(r.status, 204);
    assert_eq!(r.header("allow"), Some("GET, HEAD, OPTIONS"));
}

fn echo_app() -> Server {
    let mut app = Server::new();
    app.max_body_size(1024);
    app.post("/echo", |req| async move {
        Response::binary("application/octet-stream", req.body.as_bytes().to_vec())
    });
    app
}

#[tokio::test]
async fn body_with_content_length_is_read() {
    let addr = start(echo_app()).await;
    let mut c = Client::connect(addr).await;
    c.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\n\r\nhello").await;
    assert_eq!(c.read().await.unwrap().text(), "hello");
}

#[tokio::test]
async fn oversized_body_is_rejected_before_reading() {
    let addr = start(echo_app()).await;
    let mut c = Client::connect(addr).await;
    c.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 1000000000\r\n\r\n").await;
    let r = c.read().await.unwrap();
    assert_eq!(r.status, 413);
    assert_eq!(r.header("connection"), Some("close"));
}

#[tokio::test]
async fn expect_continue_gets_100_then_response() {
    let addr = start(echo_app()).await;
    let mut c = Client::connect(addr).await;
    c.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\nExpect: 100-continue\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().status, 100);
    c.send(b"ok").await;
    assert_eq!(c.read().await.unwrap().text(), "ok");
}

#[tokio::test]
async fn chunked_body_is_decoded() {
    let addr = start(echo_app()).await;
    let mut c = Client::connect(addr).await;
    c.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nX-Trailer: y\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().text(), "hello world");
    // The connection is still in sync.
    c.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 1\r\n\r\n!").await;
    assert_eq!(c.read().await.unwrap().text(), "!");
}

#[tokio::test]
async fn oversized_chunked_body_is_rejected() {
    let addr = start(echo_app()).await;
    let mut c = Client::connect(addr).await;
    c.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n800\r\n").await;
    c.send(&[b'a'; 0x800]).await;
    assert_eq!(c.read().await.unwrap().status, 413);
}

#[tokio::test]
async fn ambiguous_framing_is_rejected() {
    let addr = start(echo_app()).await;
    for raw in [
        &b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\n"[..],
        b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: +3\r\n\r\n",
        b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length : 3\r\n\r\n",
    ] {
        let mut c = Client::connect(addr).await;
        c.send(raw).await;
        assert_eq!(c.read().await.unwrap().status, 400, "{}", String::from_utf8_lossy(raw));
    }
    // A request body must end with chunked (RFC 9112 §6.3).
    let mut c = Client::connect(addr).await;
    c.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: gzip\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().status, 400);

    // With both headers, Transfer-Encoding wins and the connection is closed
    // afterwards, so leftover bytes can never be read as another request.
    let mut c = Client::connect(addr).await;
    c.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n0\r\n\r\nGET / HTTP/1.1\r\n\r\n").await;
    let r = c.read().await.unwrap();
    assert_eq!(r.text(), "hi");
    assert_eq!(r.header("connection"), Some("close"));
    assert!(c.closed_within(Duration::from_secs(2)).await);
}

#[tokio::test]
async fn header_limits_are_enforced() {
    let mut app = hello_app();
    // 8 KiB is the smallest limit hyper supports; smaller values are raised.
    app.max_header_size(1024).max_headers(10);
    let addr = start(app).await;

    let mut c = Client::connect(addr).await;
    c.send(format!("GET / HTTP/1.1\r\nHost: t\r\nX-Big: {}\r\n\r\n", "a".repeat(6000)).as_bytes()).await;
    assert_eq!(c.read().await.unwrap().status, 200);

    let mut c = Client::connect(addr).await;
    c.send(format!("GET / HTTP/1.1\r\nHost: t\r\nX-Big: {}\r\n\r\n", "a".repeat(10_000)).as_bytes()).await;
    assert_eq!(c.read().await.unwrap().status, 431);

    let mut c = Client::connect(addr).await;
    let many: String = (0..20).map(|i| format!("X-{}: v\r\n", i)).collect();
    c.send(format!("GET / HTTP/1.1\r\n{}\r\n", many).as_bytes()).await;
    assert_eq!(c.read().await.unwrap().status, 431);

    let mut c = Client::connect(addr).await;
    c.send(format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(10_000)).as_bytes()).await;
    assert_eq!(c.read().await.unwrap().status, 431);
}

#[tokio::test]
async fn slow_request_times_out() {
    let mut app = hello_app();
    app.request_timeout(Duration::from_millis(200));
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;
    c.send(b"GET / HTTP/1.1\r\nHost: t\r\n").await; // never finishes the head
    assert!(c.closed_within(Duration::from_secs(2)).await);
}

#[tokio::test]
async fn idle_keep_alive_connection_is_closed() {
    let mut app = hello_app();
    app.keep_alive(Duration::from_millis(200));
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;
    assert_eq!(c.get("/").await.status, 200);
    assert!(c.closed_within(Duration::from_secs(2)).await);
}

#[tokio::test]
async fn max_connections_applies_backpressure() {
    let mut app = hello_app();
    app.max_connections(1);
    let addr = start(app).await;

    let mut first = Client::connect(addr).await;
    assert_eq!(first.get("/").await.status, 200);

    // The second connection waits in the backlog while the first is open...
    let mut second = Client::connect(addr).await;
    second.send(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n").await;
    let waited = timeout(Duration::from_millis(300), second.read_inner(false)).await;
    assert!(waited.is_err(), "second connection was served while at the limit");

    // ...and is served once the first one closes.
    drop(first);
    assert_eq!(second.read().await.unwrap().status, 200);
}

#[tokio::test]
async fn binary_bodies_are_sent_unchanged() {
    let bytes: Vec<u8> = (0..=255).collect();
    let expected = bytes.clone();
    let mut app = Server::new();
    app.get("/bin", move |_req| {
        let bytes = bytes.clone();
        async move { Response::binary("application/octet-stream", bytes) }
    });
    let addr = start(app).await;
    let r = Client::connect(addr).await.get("/bin").await;
    assert_eq!(r.body, expected);
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("axeon-{}-{}-{}", name, std::process::id(), nanos));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn static_files_are_binary_safe_and_contained() {
    let dir = TempDir::new("static");
    let public = dir.0.join("public");
    std::fs::create_dir_all(&public).unwrap();
    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00, 0xfe];
    std::fs::write(public.join("logo.png"), &png).unwrap();
    std::fs::write(public.join("my file.txt"), "spaced").unwrap();
    std::fs::write(dir.0.join("secret.txt"), "secret").unwrap();

    let mut app = Server::new();
    app.static_dir(public.to_str().unwrap());
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;

    let r = c.get("/logo.png").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, png);
    assert_eq!(r.header("content-type"), Some("image/png"));
    assert_eq!(c.get("/my%20file.txt").await.text(), "spaced");
    assert_eq!(c.get("/../secret.txt").await.status, 404);
    assert_eq!(c.get("/%2e%2e/secret.txt").await.status, 404);

    c.send(b"POST /logo.png HTTP/1.1\r\nHost: t\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().status, 404);
}

fn compressed_app() -> Server {
    let mut app = Server::new();
    app.middleware(CompressionMiddleware::new(CompressionConfig::default()));
    app.get("/big", |_req| async { Response::text("axeon ".repeat(1000)) });
    app.get("/small", |_req| async { Response::text("tiny") });
    app
}

#[tokio::test]
async fn compression_uses_request_accept_encoding() {
    let addr = start(compressed_app()).await;
    let mut c = Client::connect(addr).await;
    let original = "axeon ".repeat(1000);

    c.send(b"GET /big HTTP/1.1\r\nHost: t\r\nAccept-Encoding: gzip, deflate\r\n\r\n").await;
    let r = c.read().await.unwrap();
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert_eq!(r.header("vary"), Some("Accept-Encoding"));
    assert!(r.body.len() < original.len());
    let mut decoded = String::new();
    flate2::read::GzDecoder::new(&r.body[..]).read_to_string(&mut decoded).unwrap();
    assert_eq!(decoded, original);

    c.send(b"GET /big HTTP/1.1\r\nHost: t\r\nAccept-Encoding: deflate\r\n\r\n").await;
    let r = c.read().await.unwrap();
    assert_eq!(r.header("content-encoding"), Some("deflate"));
    let mut decoded = String::new();
    flate2::read::ZlibDecoder::new(&r.body[..]).read_to_string(&mut decoded).unwrap();
    assert_eq!(decoded, original);

    let r = c.get("/big").await;
    assert_eq!(r.header("content-encoding"), None);
    assert_eq!(r.text(), original);

    c.send(b"GET /small HTTP/1.1\r\nHost: t\r\nAccept-Encoding: gzip\r\n\r\n").await;
    assert_eq!(c.read().await.unwrap().text(), "tiny");
}

#[tokio::test]
async fn rate_limiter_uses_peer_address_by_default() {
    let mut app = Server::new();
    app.middleware(RateLimiter::new(RateLimitConfig { requests_per_minute: 60, burst_size: 3, trust_proxy: false }));
    app.get("/", |_req| async { Response::text("ok") });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;

    let mut statuses = Vec::new();
    for i in 0..5 {
        // A spoofed X-Forwarded-For must not create a fresh bucket.
        c.send(format!("GET / HTTP/1.1\r\nHost: t\r\nX-Forwarded-For: 10.0.0.{}\r\n\r\n", i).as_bytes()).await;
        statuses.push(c.read().await.unwrap().status);
    }
    assert_eq!(statuses, vec![200, 200, 200, 429, 429]);
}

#[tokio::test]
async fn rate_limiter_can_trust_proxy_headers() {
    let mut app = Server::new();
    app.middleware(RateLimiter::new(RateLimitConfig { requests_per_minute: 60, burst_size: 1, trust_proxy: true }));
    app.get("/", |_req| async { Response::text("ok") });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;

    let request = |ip: &'static str| {
        format!("GET / HTTP/1.1\r\nHost: t\r\nX-Forwarded-For: 203.0.113.9, {}\r\n\r\n", ip)
    };
    for (ip, expected) in [("10.0.0.1", 200), ("10.0.0.1", 429), ("10.0.0.2", 200)] {
        c.send(request(ip).as_bytes()).await;
        assert_eq!(c.read().await.unwrap().status, expected, "{}", ip);
    }
}

#[tokio::test]
async fn rate_limiters_do_not_share_state() {
    let config = RateLimitConfig { requests_per_minute: 60, burst_size: 1, trust_proxy: false };
    let mut a = Server::new();
    a.middleware(RateLimiter::new(config.clone()));
    a.get("/", |_req| async { Response::text("a") });
    let mut b = Server::new();
    b.middleware(RateLimiter::new(config));
    b.get("/", |_req| async { Response::text("b") });
    let (a, b) = (start(a).await, start(b).await);
    assert_eq!(Client::connect(a).await.get("/").await.status, 200);
    assert_eq!(Client::connect(b).await.get("/").await.status, 200);
}

async fn request_with_origin(addr: SocketAddr, method: &str, origin: &str, extra: &str) -> Resp {
    let mut c = Client::connect(addr).await;
    c.send(format!("{} / HTTP/1.1\r\nHost: t\r\nOrigin: {}\r\n{}\r\n", method, origin, extra).as_bytes()).await;
    c.read().await.unwrap()
}

#[tokio::test]
async fn cors_wildcard_does_not_reflect_origin() {
    let mut app = Server::new();
    app.middleware(Cors::new(CorsConfig::default()));
    app.get("/", |_req| async { Response::text("ok") });
    let addr = start(app).await;
    let r = request_with_origin(addr, "GET", "https://evil.example", "").await;
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));
    assert_eq!(r.header("access-control-allow-credentials"), None);
}

#[tokio::test]
async fn cors_origin_list_with_credentials() {
    let mut app = Server::new();
    app.middleware(Cors::new(CorsConfig {
        allow_origins: vec!["https://app.example".to_string()],
        allow_credentials: true,
        ..CorsConfig::default()
    }));
    app.post("/", |_req| async { Response::text("ok") });
    let addr = start(app).await;

    let r = request_with_origin(addr, "POST", "https://app.example", "Content-Length: 0\r\n").await;
    assert_eq!(r.header("access-control-allow-origin"), Some("https://app.example"));
    assert_eq!(r.header("access-control-allow-credentials"), Some("true"));
    assert_eq!(r.header("vary"), Some("Origin"));

    let r = request_with_origin(addr, "POST", "https://evil.example", "Content-Length: 0\r\n").await;
    assert_eq!(r.header("access-control-allow-origin"), None);
    assert_eq!(r.header("access-control-allow-credentials"), None);

    // Preflight for a POST-only route reaches the CORS middleware.
    let r = request_with_origin(addr, "OPTIONS", "https://app.example", "Access-Control-Request-Method: POST\r\n").await;
    assert_eq!(r.status, 204);
    assert_eq!(r.header("access-control-allow-origin"), Some("https://app.example"));
    assert!(r.header("access-control-allow-methods").unwrap().contains("POST"));
}

#[test]
#[should_panic(expected = "allow_credentials")]
fn cors_wildcard_with_credentials_panics() {
    Cors::new(CorsConfig { allow_credentials: true, ..CorsConfig::default() });
}

#[tokio::test]
async fn params_query_and_headers_are_decoded() {
    let mut app = Server::new();
    app.get("/users/:name", |req| async move {
        Response::ok(&serde_json::json!({
            "name": req.params.get("name"),
            "q": req.query.get("q"),
            "auth": req.get_header("Authorization"),
        }))
    });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;
    c.send(b"GET /users/John%20Doe?q=a+b%26c=d HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer x\r\n\r\n").await;
    let r = c.read().await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v, serde_json::json!({ "name": "John Doe", "q": "a b&c=d", "auth": "Bearer x" }));
}

#[tokio::test]
async fn json_body_accepts_charset_parameter() {
    let mut app = Server::new();
    app.post("/", |req| async move {
        match req.body.json::<serde_json::Value>() {
            Some(v) => Response::ok(&v),
            None => Err(ServerError::BadRequest("no json".to_string())),
        }
    });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;
    let body = r#"{"a":1}"#;
    c.send(format!("POST / HTTP/1.1\r\nHost: t\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).as_bytes()).await;
    let r = c.read().await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), body);
}

#[tokio::test]
async fn panics_become_generic_500_and_connection_survives() {
    let mut app = hello_app();
    app.get("/boom", |_req| async { panic!("database password is hunter2") });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;
    let r = c.get("/boom").await;
    assert_eq!(r.status, 500);
    assert!(!r.text().contains("hunter2"));
    assert_eq!(c.get("/").await.status, 200);
}

#[tokio::test]
async fn header_injection_is_dropped() {
    let mut app = Server::new();
    app.get("/", |_req| async {
        let mut r = Response::new(200);
        r.header("X-Name", "a\r\nSet-Cookie: evil=1").header("X-Ok", "fine");
        Ok(r)
    });
    let addr = start(app).await;
    let r = Client::connect(addr).await.get("/").await;
    assert_eq!(r.header("set-cookie"), None);
    assert_eq!(r.header("x-name"), None);
    assert_eq!(r.header("x-ok"), Some("fine"));
}

#[tokio::test]
async fn slow_body_times_out_with_408() {
    let mut app = echo_app();
    app.request_timeout(Duration::from_millis(200));
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;
    c.send(b"POST /echo HTTP/1.1\r\nHost: t\r\nContent-Length: 10\r\n\r\nabc").await; // body never completes
    let r = c.read().await.unwrap();
    assert_eq!(r.status, 408);
    assert_eq!(r.header("connection"), Some("close"));
}

#[tokio::test]
async fn http2_prior_knowledge_is_served() {
    let mut app = Server::new();
    app.post("/echo", |req| async move { Response::binary("text/plain", req.body.as_bytes().to_vec()) });
    let addr = start(app).await;

    let tcp = TcpStream::connect(addr).await.unwrap();
    let (client, connection) = h2::client::handshake(tcp).await.unwrap();
    tokio::spawn(connection);
    let mut client = client.ready().await.unwrap();

    // Several concurrent streams over one connection.
    let mut pending = Vec::new();
    for i in 0..3 {
        let request = http::Request::post(format!("http://{}/echo", addr)).body(()).unwrap();
        let (response, mut stream) = client.send_request(request, false).unwrap();
        stream.send_data(bytes::Bytes::from(format!("stream {}", i)), true).unwrap();
        pending.push(response);
        client = client.ready().await.unwrap();
    }
    for (i, response) in pending.into_iter().enumerate() {
        let response = response.await.unwrap();
        assert_eq!(response.status(), 200);
        let mut body = response.into_body();
        let mut data = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.unwrap();
            body.flow_control().release_capacity(chunk.len()).unwrap();
            data.extend_from_slice(&chunk);
        }
        assert_eq!(data, format!("stream {}", i).into_bytes());
    }
}

#[tokio::test]
async fn catch_all_and_static_priority() {
    let mut app = Server::new();
    app.get("/files/*path", |req| async move {
        Response::text(format!("file:{}", req.params.get("path").unwrap()))
    });
    app.get("/files/index", |_req| async { Response::text("index") });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;
    assert_eq!(c.get("/files/a/b%20c.txt").await.text(), "file:a/b c.txt");
    assert_eq!(c.get("/files/index").await.text(), "index");
}

#[tokio::test]
async fn conflicting_routes_fail_at_startup() {
    let mut app = Server::new();
    app.get("/users/:id", |_req| async { Response::text("a") });
    app.get("/users/:name", |_req| async { Response::text("b") });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err = app.serve(listener).await.unwrap_err();
    assert!(err.to_string().contains("/users/"), "{}", err);
}

struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl axeon::middleware::Middleware for Counter {
    fn call(&self, req: axeon::Request, next: axeon::middleware::Next) -> axeon::middleware::MiddlewareResult {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        next.handle(req)
    }
}

struct Tag(&'static str);

impl axeon::middleware::Middleware for Tag {
    fn call(&self, req: axeon::Request, next: axeon::middleware::Next) -> axeon::middleware::MiddlewareResult {
        let tag = self.0;
        Box::pin(async move {
            let mut res = next.handle(req).await?;
            let order = match res.get_header("x-order") {
                Some(existing) => format!("{},{}", tag, existing),
                None => tag.to_string(),
            };
            res.header("X-Order", order);
            Ok(res)
        })
    }
}

#[tokio::test]
async fn middleware_is_shared_and_runs_in_order() {
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut app = Server::new();
    app.middleware(Counter(count.clone()));
    app.middleware(Tag("app"));
    let mut api = axeon::Router::new();
    api.middleware(Tag("api"));
    api.get("/x", |_req| async { Response::text("x") });
    app.mount("/api", api);
    let addr = start(app).await;

    for _ in 0..3 {
        let r = Client::connect(addr).await.get("/api/x").await;
        assert_eq!(r.header("x-order"), Some("app,api"));
    }
    // One middleware instance across all connections.
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn repeated_response_headers_are_kept() {
    let mut app = Server::new();
    app.get("/", |_req| async {
        let mut r = Response::new(200);
        r.append_header("Set-Cookie", "a=1").append_header("Set-Cookie", "b=2");
        Ok(r)
    });
    let addr = start(app).await;
    let mut c = Client::connect(addr).await;
    c.send(b"GET / HTTP/1.1\r\nHost: t\r\n\r\n").await;
    let mut raw = vec![0u8; 512];
    let n = timeout(Duration::from_secs(5), c.reader.read(&mut raw)).await.unwrap().unwrap();
    let text = String::from_utf8_lossy(&raw[..n]).to_ascii_lowercase();
    assert!(text.contains("set-cookie: a=1") && text.contains("set-cookie: b=2"), "{}", text);
}

#[tokio::test]
async fn tls_negotiates_http2_and_http11() {
    let dir = TempDir::new("tls");
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_path = dir.0.join("cert.pem");
    let key_path = dir.0.join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    let mut app = Server::new();
    app.get("/", |_req| async { Response::text("secure") });
    app.with_tls(&cert_path, &key_path);
    let addr = start(app).await;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let connect = |alpn: &[u8]| {
        let mut config = rustls::ClientConfig::builder().with_root_certificates(roots.clone()).with_no_client_auth();
        config.alpn_protocols = vec![alpn.to_vec()];
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
        async move {
            let tcp = TcpStream::connect(addr).await.unwrap();
            let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            connector.connect(name, tcp).await.unwrap()
        }
    };

    // HTTP/2 via ALPN.
    let tls = connect(b"h2").await;
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (client, connection) = h2::client::handshake(tls).await.unwrap();
    tokio::spawn(connection);
    let mut client = client.ready().await.unwrap();
    let (response, _) = client.send_request(http::Request::get("https://localhost/").body(()).unwrap(), true).unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.status(), 200);
    let chunk = response.into_body().data().await.unwrap().unwrap();
    assert_eq!(&chunk[..], b"secure");

    // HTTP/1.1 via ALPN.
    let mut tls = connect(b"http/1.1").await;
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut raw = Vec::new();
    tls.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 200 OK"), "{}", text);
    assert!(text.ends_with("secure"), "{}", text);
}
