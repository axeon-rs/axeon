//! Realtime example for Axeon
//!
//! This example demonstrates:
//! - Handlers returning plain values (`&str`, `Json`, tuples)
//! - Closure middleware with `from_fn`
//! - Server-sent events at /events
//! - A WebSocket echo server at /ws
//!
//! Try it:
//!   curl -N http://127.0.0.1:3000/events
//!   websocat ws://127.0.0.1:3000/ws

use axeon::middleware::from_fn;
use axeon::sse::{self, Event};
use axeon::{json, Json, Server};
use std::time::{Duration, Instant};

fn main() {
    let mut app = Server::new();

    // Log every request with its duration.
    app.middleware(from_fn(|req, next| async move {
        let start = Instant::now();
        let (method, path) = (req.method(), req.path().to_string());
        let res = next.handle(req).await;
        let status = res.as_ref().map(|r| r.status).unwrap_or_else(|e| e.status_code());
        println!("[{}] {} {} - {:?}", status, method, path, start.elapsed());
        res
    }));

    app.get("/", |_req| async { "Realtime demo: GET /events or open a WebSocket at /ws" });
    app.get("/health", |_req| async { Json(json!({ "status": "ok" })) });

    // A clock ticking once per second until the client disconnects.
    app.get("/events", |_req| async {
        let (tx, response) = sse::channel(8);
        tokio::spawn(async move {
            let started = Instant::now();
            loop {
                let event = Event::new().event("tick").data(format!("{}s", started.elapsed().as_secs()));
                if tx.send(event).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        response
    });

    // Echo every text or binary message back.
    app.get("/ws", |mut req| async move {
        req.upgrade_websocket(|mut socket| async move {
            while let Some(Ok(message)) = socket.recv().await {
                if (message.is_text() || message.is_binary()) && socket.send(message).await.is_err() {
                    break;
                }
            }
        })
    });

    app.listen("127.0.0.1:3000")
        .expect("Server failed to start");
}
