//! The server survives malformed and hostile input on the wire.

mod common;

use axeon::{Json, Server, ServerError};
use common::{start, Client};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A small deterministic random generator, so failures are reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

/// Mostly-HTTP garbage: real tokens mixed with random bytes and broken framing.
fn garbage(rng: &mut Rng) -> Vec<u8> {
    const PIECES: &[&str] = &[
        "GET ", "POST ", "PUT ", "BREW ", "/", "/items/", "%00", "%", "?a=", "&", "[", "]", " HTTP/1.1", " HTTP/1.0",
        " HTTP/2.0", " HTTP/9", "\r\n", "\n", "\r", "Host: x", "Content-Length: ", "-1", "99999999999999999999",
        "Transfer-Encoding: chunked", "Transfer-Encoding: gzip, chunked", "Content-Type: application/json",
        "Content-Type: multipart/form-data; boundary=", ":", " ", "\t", "{", "}", "\"", "0\r\n\r\n", "ffffffff\r\n",
        "Expect: 100-continue", "Connection: upgrade", "Upgrade: websocket", "Cookie: sid=", ";",
    ];
    let mut out = Vec::new();
    for _ in 0..(rng.next() % 40) {
        if rng.next().is_multiple_of(4) {
            for _ in 0..(rng.next() % 16) {
                out.push(rng.next() as u8);
            }
        } else {
            out.extend_from_slice(rng.pick(PIECES).as_bytes());
        }
    }
    out
}

#[tokio::test]
async fn garbage_input_never_takes_the_server_down() {
    let mut app = Server::new();
    app.request_timeout(Duration::from_millis(300));
    app.get("/", |_req| async { "alive" });
    app.post("/items/:id", |req| async move {
        let id: u32 = req.param("id")?;
        let value: serde_json::Value = req.json()?;
        Ok::<_, ServerError>(Json(serde_json::json!({ "id": id, "value": value })))
    });
    let addr = start(app).await;
    let mut rng = Rng(0x5eed_1234_abcd_9876);

    for _ in 0..300 {
        let payload = garbage(&mut rng);
        let Ok(mut stream) = TcpStream::connect(addr).await else { panic!("server stopped accepting") };
        let _ = stream.write_all(&payload).await;
        let _ = stream.shutdown().await;
        let mut sink = Vec::new();
        let _ = tokio::time::timeout(Duration::from_millis(500), stream.read_to_end(&mut sink)).await;
        // Whatever came back must be HTTP (or nothing), never a crash.
        if !sink.is_empty() {
            assert!(sink.starts_with(b"HTTP/1."), "non-HTTP reply to {:?}", String::from_utf8_lossy(&payload));
        }
    }

    // Still healthy afterwards.
    assert_eq!(Client::connect(addr).await.get("/").await.text(), "alive");
}

#[tokio::test]
async fn many_idle_connections_do_not_block_others() {
    let mut app = Server::new();
    app.max_connections(64).request_timeout(Duration::from_millis(200));
    app.get("/", |_req| async { "alive" });
    let addr = start(app).await;

    // Slowloris: connections that send part of a request and stall.
    let mut idle = Vec::new();
    for _ in 0..60 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").await.unwrap();
        idle.push(stream);
    }
    // The header timeout frees their slots, so normal clients get through.
    tokio::time::sleep(Duration::from_millis(400)).await;
    for _ in 0..10 {
        assert_eq!(Client::connect(addr).await.get("/").await.text(), "alive");
    }
}
