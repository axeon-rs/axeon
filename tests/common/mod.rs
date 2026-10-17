//! Shared helpers for the integration tests: a server on an ephemeral port
//! and a raw HTTP/1.1 client, so the wire format itself is checked.
#![allow(dead_code)]

use axeon::Server;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

pub async fn start(app: Server) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = app.serve(listener).await;
    });
    addr
}

#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub struct Client {
    pub reader: BufReader<TcpStream>,
}

impl Client {
    pub async fn connect(addr: SocketAddr) -> Self {
        Client { reader: BufReader::new(TcpStream::connect(addr).await.unwrap()) }
    }

    pub async fn send(&mut self, raw: &[u8]) {
        self.reader.get_mut().write_all(raw).await.unwrap();
    }

    pub async fn get(&mut self, path: &str) -> Resp {
        self.send(format!("GET {} HTTP/1.1\r\nHost: test\r\n\r\n", path).as_bytes()).await;
        self.read().await.expect("connection closed")
    }

    /// Reads one response, or `None` if the server closed the connection.
    pub async fn read(&mut self) -> Option<Resp> {
        self.read_for(false).await
    }

    pub async fn read_for(&mut self, head_request: bool) -> Option<Resp> {
        timeout(Duration::from_secs(5), self.read_inner(head_request))
            .await
            .expect("timed out waiting for response")
    }

    pub async fn read_inner(&mut self, head_request: bool) -> Option<Resp> {
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
        if has_body && headers.get("transfer-encoding").map(String::as_str) == Some("chunked") {
            assert!(!headers.contains_key("content-length"), "chunked response with content-length");
            loop {
                line.clear();
                self.reader.read_line(&mut line).await.unwrap();
                let size = usize::from_str_radix(line.trim_end().split(';').next().unwrap(), 16).unwrap();
                let start = body.len();
                body.resize(start + size, 0);
                self.reader.read_exact(&mut body[start..]).await.unwrap();
                line.clear();
                self.reader.read_line(&mut line).await.unwrap(); // CRLF after the chunk (or trailers end)
                if size == 0 {
                    break;
                }
            }
        } else if has_body {
            let len: usize = headers.get("content-length").expect("content-length").parse().unwrap();
            body.resize(len, 0);
            self.reader.read_exact(&mut body).await.unwrap();
        }
        Some(Resp { status, headers, body })
    }

    /// True if the server closes the connection within the timeout.
    pub async fn closed_within(&mut self, dur: Duration) -> bool {
        let mut buf = [0u8; 1];
        matches!(timeout(dur, self.reader.read(&mut buf)).await, Ok(Ok(0)) | Ok(Err(_)))
    }
}

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
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
