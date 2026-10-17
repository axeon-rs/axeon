//! Static files: streaming, range requests, conditional requests and
//! hidden files.

mod common;

use axeon::Server;
use common::{start, Client, TempDir};
use std::net::SocketAddr;

const LARGE: usize = 3 * 1024 * 1024 + 17;

fn large_contents() -> Vec<u8> {
    (0..LARGE).map(|i| (i % 251) as u8).collect()
}

async fn serve_dir(configure: impl FnOnce(&mut Server)) -> (SocketAddr, TempDir) {
    let dir = TempDir::new("static-files");
    std::fs::write(dir.0.join("small.txt"), "0123456789abcdefghij").unwrap();
    std::fs::write(dir.0.join("large.bin"), large_contents()).unwrap();
    std::fs::write(dir.0.join("page.html"), "<p>".repeat(2000)).unwrap();
    std::fs::write(dir.0.join(".env"), "SECRET=1").unwrap();
    std::fs::create_dir_all(dir.0.join(".well-known")).unwrap();
    std::fs::write(dir.0.join(".well-known/security.txt"), "Contact: x").unwrap();
    std::fs::create_dir_all(dir.0.join("sub/.git")).unwrap();
    std::fs::write(dir.0.join("sub/.git/config"), "[core]").unwrap();

    let mut app = Server::new();
    app.static_dir(dir.0.to_str().unwrap());
    configure(&mut app);
    (start(app).await, dir)
}

async fn get(addr: SocketAddr, path: &str, headers: &str) -> common::Resp {
    let mut c = Client::connect(addr).await;
    c.send(format!("GET {} HTTP/1.1\r\nHost: t\r\n{}\r\n", path, headers).as_bytes()).await;
    c.read().await.unwrap()
}

#[tokio::test]
async fn large_files_are_streamed_with_a_content_length() {
    let (addr, _dir) = serve_dir(|_| {}).await;
    let r = get(addr, "/large.bin", "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-length"), Some(LARGE.to_string().as_str()));
    assert_eq!(r.header("transfer-encoding"), None);
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    assert_eq!(r.header("content-type"), Some("application/octet-stream"));
    assert!(r.body == large_contents(), "streamed body differs");

    // HEAD reports the length without a body; the connection stays usable.
    let mut c = Client::connect(addr).await;
    c.send(b"HEAD /large.bin HTTP/1.1\r\nHost: t\r\n\r\n").await;
    let head = c.read_for(true).await.unwrap();
    assert_eq!(head.header("content-length"), Some(LARGE.to_string().as_str()));
    assert_eq!(c.get("/small.txt").await.text(), "0123456789abcdefghij");
}

#[tokio::test]
async fn range_requests() {
    let (addr, _dir) = serve_dir(|_| {}).await;

    let r = get(addr, "/small.txt", "Range: bytes=0-4\r\n").await;
    assert_eq!((r.status, r.text().as_str()), (206, "01234"));
    assert_eq!(r.header("content-range"), Some("bytes 0-4/20"));
    assert_eq!(r.header("content-length"), Some("5"));

    let r = get(addr, "/small.txt", "Range: bytes=-3\r\n").await;
    assert_eq!((r.status, r.text().as_str()), (206, "hij"));
    let r = get(addr, "/small.txt", "Range: bytes=15-\r\n").await;
    assert_eq!((r.status, r.text().as_str()), (206, "fghij"));

    // A large range is streamed.
    let start = 1_000_000;
    let r = get(addr, "/large.bin", &format!("Range: bytes={}-\r\n", start)).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.header("content-range"), Some(format!("bytes {}-{}/{}", start, LARGE - 1, LARGE).as_str()));
    assert!(r.body == large_contents()[start..], "range body differs");

    let r = get(addr, "/small.txt", "Range: bytes=20-\r\n").await;
    assert_eq!(r.status, 416);
    assert_eq!(r.header("content-range"), Some("bytes */20"));

    // Multiple ranges and invalid syntax fall back to the whole file.
    assert_eq!(get(addr, "/small.txt", "Range: bytes=0-1,4-5\r\n").await.status, 200);
    assert_eq!(get(addr, "/small.txt", "Range: bytes=x-y\r\n").await.status, 200);
}

#[tokio::test]
async fn if_range_only_applies_the_range_when_unchanged() {
    let (addr, _dir) = serve_dir(|_| {}).await;
    let etag = get(addr, "/small.txt", "").await.header("etag").unwrap().to_string();

    let r = get(addr, "/small.txt", &format!("Range: bytes=0-1\r\nIf-Range: {}\r\n", etag)).await;
    assert_eq!((r.status, r.text().as_str()), (206, "01"));
    let r = get(addr, "/small.txt", "Range: bytes=0-1\r\nIf-Range: \"stale\"\r\n").await;
    assert_eq!((r.status, r.text().len()), (200, 20));
}

#[tokio::test]
async fn conditional_requests_get_304() {
    let (addr, _dir) = serve_dir(|_| {}).await;
    let first = get(addr, "/small.txt", "").await;
    let etag = first.header("etag").unwrap().to_string();
    let last_modified = first.header("last-modified").unwrap().to_string();

    let r = get(addr, "/small.txt", &format!("If-None-Match: {}\r\n", etag)).await;
    assert_eq!(r.status, 304);
    assert!(r.body.is_empty());
    assert_eq!(r.header("etag"), Some(etag.as_str()));
    assert_eq!(get(addr, "/small.txt", &format!("If-None-Match: W/{}\r\n", etag)).await.status, 304);
    assert_eq!(get(addr, "/small.txt", "If-None-Match: *\r\n").await.status, 304);
    assert_eq!(get(addr, "/small.txt", "If-None-Match: \"other\"\r\n").await.status, 200);

    assert_eq!(get(addr, "/small.txt", &format!("If-Modified-Since: {}\r\n", last_modified)).await.status, 304);
    assert_eq!(get(addr, "/small.txt", "If-Modified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\n").await.status, 200);
    // If-None-Match takes precedence over If-Modified-Since.
    let both = format!("If-None-Match: \"other\"\r\nIf-Modified-Since: {}\r\n", last_modified);
    assert_eq!(get(addr, "/small.txt", &both).await.status, 200);
}

#[tokio::test]
async fn hidden_files_are_not_served_except_well_known() {
    let (addr, _dir) = serve_dir(|_| {}).await;
    assert_eq!(get(addr, "/.env", "").await.status, 404);
    assert_eq!(get(addr, "/%2Eenv", "").await.status, 404);
    assert_eq!(get(addr, "/sub/.git/config", "").await.status, 404);
    assert_eq!(get(addr, "/.well-known/security.txt", "").await.text(), "Contact: x");
}

#[tokio::test]
async fn cache_control_defaults_to_revalidation_and_is_configurable() {
    let (addr, _dir) = serve_dir(|_| {}).await;
    assert_eq!(get(addr, "/small.txt", "").await.header("cache-control"), Some("no-cache"));

    let (addr, _dir) = serve_dir(|app| {
        app.static_cache_control("public, max-age=31536000, immutable");
    })
    .await;
    assert_eq!(get(addr, "/small.txt", "").await.header("cache-control"), Some("public, max-age=31536000, immutable"));
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn compressed_static_files_get_weak_etags() {
    use axeon::middleware::{CompressionConfig, CompressionMiddleware};
    let (addr, _dir) = serve_dir(|app| {
        app.middleware(CompressionMiddleware::new(CompressionConfig::default()));
    })
    .await;
    let r = get(addr, "/page.html", "Accept-Encoding: gzip\r\n").await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert!(r.header("etag").unwrap().starts_with("W/\""), "{:?}", r.header("etag"));
    assert_eq!(r.header("accept-ranges"), None);
    // Revalidating with the weak tag still gives 304.
    let etag = r.header("etag").unwrap().to_string();
    let again = get(addr, "/page.html", &format!("Accept-Encoding: gzip\r\nIf-None-Match: {}\r\n", etag)).await;
    assert_eq!(again.status, 304);
}
