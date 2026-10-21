//! Checked uploads: per-field limits, type sniffing and SHA-256.

#![cfg(feature = "multipart")]

mod common;

use axeon::multipart::sniff;
use axeon::{Json, Method, Request, Server, ServerError};
use common::{start, Client, TempDir};
use std::sync::{Arc, Mutex};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR rest of a png";
const PDF: &[u8] = b"%PDF-1.7\n1 0 obj << >> endobj\n%%EOF";

/// A multipart body with one file field.
fn form(content_type: &str, data: &[u8]) -> (String, Vec<u8>) {
    let mut body = Vec::new();
    body.extend_from_slice(b"--XYZ\r\nContent-Disposition: form-data; name=\"doc\"; filename=\"report.pdf\"\r\n");
    body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", content_type).as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(b"\r\n--XYZ--\r\n");
    ("multipart/form-data; boundary=XYZ".to_string(), body)
}

fn request(content_type: &str, data: &[u8]) -> Request {
    let (header, body) = form(content_type, data);
    Request::new(Method::POST, "/upload").with_body(&header, body)
}

/// An app storing uploads in memory, with the given limit and types.
fn app(limit: u64, accept: &'static [&'static str], stored: Arc<Mutex<Vec<u8>>>) -> Server {
    let mut app = Server::new();
    app.route("/upload").stream_body().post(move |req| {
        let stored = Arc::clone(&stored);
        async move {
            let mut form = req.multipart()?;
            let mut field = form.next_field().await?.unwrap().limit(limit);
            if !accept.is_empty() {
                field = field.accept(accept);
            }
            let mut buffer: Vec<u8> = Vec::new();
            let upload = field.write_to(&mut buffer).await?;
            *stored.lock().unwrap() = buffer;
            Ok::<_, ServerError>(Json(axeon::json!({
                "name": upload.name(),
                "file_name": upload.file_name(),
                "declared": upload.declared_content_type(),
                "type": upload.content_type(),
                "size": upload.size(),
                "sha256": upload.sha256_hex(),
            })))
        }
    });
    app
}

#[tokio::test]
async fn hashes_sizes_and_types_while_streaming() {
    let stored = Arc::new(Mutex::new(Vec::new()));
    let app = app(1024, &[], Arc::clone(&stored)).into_app().await.unwrap();
    let res = app.call(request("text/plain", b"hello")).await;
    assert_eq!(res.status, 200, "{}", res.body_text());
    let body: serde_json::Value = serde_json::from_str(&res.body_text()).unwrap();
    // `printf hello | shasum -a 256`
    assert_eq!(body["sha256"], "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    assert_eq!((body["size"].as_u64(), body["type"].is_null()), (Some(5), true));
    assert_eq!((body["name"].as_str(), body["file_name"].as_str(), body["declared"].as_str()), (Some("doc"), Some("report.pdf"), Some("text/plain")));
    assert_eq!(*stored.lock().unwrap(), b"hello");

    let res = app.call(request("application/octet-stream", PDF)).await;
    let body: serde_json::Value = serde_json::from_str(&res.body_text()).unwrap();
    assert_eq!(body["type"], "application/pdf", "detected, not declared");
    assert_eq!(*stored.lock().unwrap(), PDF);

    // Empty files are fine.
    let res = app.call(request("text/plain", b"")).await;
    let body: serde_json::Value = serde_json::from_str(&res.body_text()).unwrap();
    assert_eq!(body["sha256"], "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    assert_eq!(body["size"], 0);
}

#[tokio::test]
async fn enforces_the_field_limit() {
    let stored = Arc::new(Mutex::new(Vec::new()));
    let app = app(10, &[], Arc::clone(&stored)).into_app().await.unwrap();
    assert_eq!(app.call(request("text/plain", b"0123456789")).await.status, 200, "exactly at the limit");
    let res = app.call(request("text/plain", b"0123456789A")).await;
    assert_eq!(res.status, 413);
}

#[tokio::test]
async fn accepts_only_the_listed_types() {
    let stored = Arc::new(Mutex::new(b"untouched".to_vec()));
    let app = app(1 << 20, &["application/pdf", "image/*"], Arc::clone(&stored)).into_app().await.unwrap();
    assert_eq!(app.call(request("application/pdf", PDF)).await.status, 200);
    assert_eq!(app.call(request("image/png", PNG)).await.status, 200, "image/* matches");

    *stored.lock().unwrap() = b"untouched".to_vec();
    // A script renamed to .pdf, declared as a PDF: rejected, nothing written.
    let res = app.call(request("application/pdf", b"#!/bin/sh\nrm -rf /\n")).await;
    assert_eq!(res.status, 415);
    let body: serde_json::Value = serde_json::from_str(&res.body_text()).unwrap();
    assert_eq!(body["error"]["message"], "Unsupported media type: file type unknown is not accepted");
    // A ZIP isn't in the list.
    let res = app.call(request("application/pdf", b"PK\x03\x04 a zip file entry")).await;
    assert_eq!(res.status, 415);
    assert!(res.body_text().contains("application/zip"), "{}", res.body_text());
    // Too short to recognise.
    assert_eq!(app.call(request("application/pdf", b"%P")).await.status, 415);
    assert_eq!(*stored.lock().unwrap(), b"untouched", "rejected files write nothing");
}

#[tokio::test]
async fn type_check_waits_for_enough_bytes_across_chunks() {
    let stored = Arc::new(Mutex::new(Vec::new()));
    let addr = start(app(1 << 20, &["application/pdf"], Arc::clone(&stored))).await;
    let (header, body) = form("application/pdf", PDF);
    // Send the body in small pieces, splitting the file's first bytes.
    let mut client = Client::connect(addr).await;
    client
        .send(format!("POST /upload HTTP/1.1\r\nHost: t\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n", header, body.len()).as_bytes())
        .await;
    let split = body.windows(5).position(|w| w == b"%PDF-").unwrap() + 2;
    for piece in [&body[..split], &body[split..split + 3], &body[split + 3..]] {
        client.send(piece).await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    let res = client.read().await.unwrap();
    assert_eq!(res.status, 200, "{}", res.text());
    assert!(res.text().contains("\"type\":\"application/pdf\""), "{}", res.text());
    assert_eq!(*stored.lock().unwrap(), PDF);
}

#[tokio::test]
async fn save_removes_the_file_on_failure() {
    let dir = TempDir::new("checked-uploads");
    let target = dir.0.join("upload.bin");
    let mut app = Server::new();
    let path = target.clone();
    app.route("/upload").stream_body().post(move |req| {
        let path = path.clone();
        async move {
            let mut form = req.multipart()?;
            let field = form.next_field().await?.unwrap();
            let upload = field.limit(8).accept(&["application/pdf"]).save(&path).await?;
            Ok::<_, ServerError>(upload.sha256_hex())
        }
    });
    let app = app.into_app().await.unwrap();
    assert_eq!(app.call(request("application/pdf", PDF)).await.status, 413);
    assert!(!target.exists(), "too large: removed");
    assert_eq!(app.call(request("application/pdf", b"nope")).await.status, 415);
    assert!(!target.exists(), "wrong type: removed");
    let res = app.call(request("application/pdf", b"%PDF-1.")).await;
    assert_eq!(res.status, 200);
    assert_eq!(std::fs::read(&target).unwrap(), b"%PDF-1.");
}

#[tokio::test]
async fn limits_apply_to_text_fields_too() {
    let mut app = Server::new();
    app.post("/upload", |req| async move {
        let mut form = req.multipart()?;
        let text = form.next_field().await?.unwrap().limit(4).text().await?;
        Ok::<_, ServerError>(text)
    });
    let app = app.into_app().await.unwrap();
    assert_eq!(app.call(request("text/plain", b"abcd")).await.body_text(), "abcd");
    assert_eq!(app.call(request("text/plain", b"abcde")).await.status, 413);
}

#[test]
fn sniffs_common_formats() {
    let cases: &[(&[u8], Option<&str>)] = &[
        (PDF, Some("application/pdf")),
        (PNG, Some("image/png")),
        (b"\xFF\xD8\xFF\xE0\0\x10JFIF", Some("image/jpeg")),
        (b"GIF89a\x01\0\x01\0", Some("image/gif")),
        (b"RIFF\x24\0\0\0WEBPVP8 ", Some("image/webp")),
        (b"II*\0\x08\0\0\0", Some("image/tiff")),
        (b"\0\0\0\x18ftypheic\0\0\0\0", Some("image/heic")),
        (b"\0\0\0\x1cftypavif\0\0\0\0", Some("image/avif")),
        (b"\0\0\0\x20ftypisom\0\0\x02\0", Some("video/mp4")),
        (b"PK\x03\x04\x14\0\x06\0", Some("application/zip")),
        (b"\x1f\x8b\x08\0", Some("application/gzip")),
        (b"7z\xBC\xAF\x27\x1C\0\x04", Some("application/x-7z-compressed")),
        (b"BM", None), // too short to be a bitmap
        (b"<svg xmlns=\"http://www.w3.org/2000/svg\">", None),
        (b"", None),
    ];
    for (data, expected) in cases {
        assert_eq!(sniff(data), *expected, "{:?}", String::from_utf8_lossy(data));
    }
}

/// A writer that records every write, even if the upload then fails.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<u8>>>);

impl tokio::io::AsyncWrite for Recorder {
    fn poll_write(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn rejected_types_never_reach_the_writer() {
    let written = Recorder::default();
    let mut app = Server::new();
    let writer = written.clone();
    app.route("/upload").stream_body().post(move |req| {
        let mut writer = writer.clone();
        async move {
            let mut form = req.multipart()?;
            let field = form.next_field().await?.unwrap();
            field.accept(&["image/png"]).write_to(&mut writer).await?;
            Ok::<_, ServerError>("stored")
        }
    });
    let addr = start(app).await;
    // A large non-image, arriving in pieces: none of it may be written.
    let data = vec![b'x'; 200_000];
    let (header, body) = form("image/png", &data);
    let mut client = Client::connect(addr).await;
    client
        .send(format!("POST /upload HTTP/1.1\r\nHost: t\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n", header, body.len()).as_bytes())
        .await;
    for piece in body.chunks(7_000) {
        client.send(piece).await;
    }
    let res = client.read().await.unwrap();
    assert_eq!(res.status, 415);
    assert!(written.0.lock().unwrap().is_empty(), "{} bytes reached the writer", written.0.lock().unwrap().len());
}
