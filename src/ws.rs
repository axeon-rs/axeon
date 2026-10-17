//! WebSockets over HTTP/1.1 (enabled by the default `ws` feature).
//!
//! ```rust
//! use axeon::Server;
//! use axeon::ws::{Message, SinkExt, StreamExt};
//!
//! let mut app = Server::new();
//! app.get("/echo", |mut req| async move {
//!     req.upgrade_websocket(|mut socket| async move {
//!         while let Some(Ok(message)) = socket.next().await {
//!             if message.is_text() || message.is_binary() {
//!                 if socket.send(message).await.is_err() {
//!                     break;
//!                 }
//!             }
//!         }
//!     })
//! });
//! ```

use crate::error::{ServerError, ServerResult};
use crate::http::{Method, Request, Response};
use http::header::{CONNECTION, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use std::future::Future;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::WebSocketStream;

pub use futures::{SinkExt, StreamExt};
pub use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
pub use tokio_tungstenite::tungstenite::{Error, Message};

/// An accepted WebSocket connection. Read messages with `StreamExt::next`
/// and send them with `SinkExt::send`.
pub type WebSocket = WebSocketStream<TokioIo<Upgraded>>;

fn bad_request(msg: &str) -> ServerError {
    ServerError::BadRequest(msg.to_string())
}

impl Request {
    /// Accepts a WebSocket handshake. Return the result from the handler;
    /// once the `101 Switching Protocols` response is sent, `on_connect`
    /// runs on its own task with the socket.
    ///
    /// Fails with 400 Bad Request if this isn't a valid WebSocket upgrade
    /// request, or if the connection can't be upgraded (HTTP/2).
    ///
    /// Upgraded connections no longer count toward `max_connections` and
    /// have no idle timeout; close them from `on_connect` when appropriate.
    pub fn upgrade_websocket<F, Fut>(&mut self, on_connect: F) -> ServerResult<Response>
    where
        F: FnOnce(WebSocket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if self.method != Method::GET {
            return Err(bad_request("WebSocket upgrade requires GET"));
        }
        let has_token = |name, token: &str| {
            self.headers.get_all(name).iter().any(|value| {
                value
                    .to_str()
                    .map(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
                    .unwrap_or(false)
            })
        };
        if !has_token(CONNECTION, "upgrade") || !has_token(UPGRADE, "websocket") {
            return Err(bad_request("not a WebSocket upgrade request"));
        }
        if self.get_header(SEC_WEBSOCKET_VERSION.as_str()) != Some("13") {
            return Err(bad_request("unsupported WebSocket version"));
        }
        let key = self
            .headers
            .get(SEC_WEBSOCKET_KEY)
            .ok_or_else(|| bad_request("missing Sec-WebSocket-Key"))?;
        let accept = derive_accept_key(key.as_bytes());
        let on_upgrade = self
            .extensions
            .remove::<OnUpgrade>()
            .ok_or_else(|| bad_request("this connection can't be upgraded; WebSockets need HTTP/1.1"))?;

        tokio::spawn(async move {
            match on_upgrade.await {
                Ok(upgraded) => {
                    let socket = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
                    on_connect(socket).await;
                }
                Err(e) => eprintln!("WebSocket upgrade failed: {}", e),
            }
        });

        let mut response = Response::new(101);
        response
            .header("Upgrade", "websocket")
            .header("Connection", "Upgrade")
            .header("Sec-WebSocket-Accept", accept);
        Ok(response)
    }
}
