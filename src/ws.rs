//! WebSockets over HTTP/1.1 (enabled by the default `ws` feature).
//!
//! ```rust
//! use axeon::Server;
//! use axeon::ws::Message;
//!
//! let mut app = Server::new();
//! app.get("/echo", |mut req| async move {
//!     req.upgrade_websocket(|mut socket| async move {
//!         while let Some(Ok(message)) = socket.recv().await {
//!             if let Message::Text(text) = message {
//!                 if socket.send(format!("echo: {}", text)).await.is_err() {
//!                     break;
//!                 }
//!             }
//!         }
//!     })
//! });
//! ```
//!
//! [`WebSocket`] is also a `Stream` of messages and a `Sink` for them, so
//! [`StreamExt`] and [`SinkExt`] work too, e.g. `socket.split()` to read
//! and write from separate tasks.
//!
//! To close sockets cleanly when the server shuts down, watch
//! [`Request::shutdown`] and send a `1001 Going Away` close frame, which
//! tells clients to reconnect (to another instance):
//!
//! ```rust
//! use axeon::Server;
//!
//! let mut app = Server::new();
//! app.get("/ws", |mut req| async move {
//!     let shutdown = req.shutdown();
//!     req.upgrade_websocket(move |mut socket| async move {
//!         loop {
//!             tokio::select! {
//!                 message = socket.recv() => match message {
//!                     Some(Ok(message)) => { let _ = socket.send(message).await; }
//!                     _ => break,
//!                 },
//!                 _ = shutdown.wait() => {
//!                     let _ = socket.close(1001, "server restarting").await;
//!                     break;
//!                 }
//!             }
//!         }
//!     })
//! });
//! ```
use crate::error::{ServerError, ServerResult};
use crate::message::{Method, Request, Response};
use http::header::{CONNECTION, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use bytes::Bytes;
use futures::{Sink, Stream};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::WebSocketStream;

pub use futures::{SinkExt, StreamExt};

/// A WebSocket message.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
    /// A UTF-8 text message.
    Text(String),
    /// A binary message.
    Binary(Bytes),
    /// A ping. Pongs are sent back automatically.
    Ping(Bytes),
    /// A pong.
    Pong(Bytes),
    /// The peer is closing the connection, optionally saying why.
    Close(Option<CloseFrame>),
}

/// Why a WebSocket is closing: a status code (1000 normal, 1001 going
/// away, ...) and a short reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseFrame {
    /// The close code.
    pub code: u16,
    /// The reason, at most 123 bytes.
    pub reason: String,
}

impl Message {
    /// A text message.
    pub fn text(text: impl Into<String>) -> Self {
        Message::Text(text.into())
    }

    /// A binary message.
    pub fn binary(data: impl Into<Bytes>) -> Self {
        Message::Binary(data.into())
    }

    /// A close message with a code and reason.
    pub fn close(code: u16, reason: impl Into<String>) -> Self {
        Message::Close(Some(CloseFrame { code, reason: reason.into() }))
    }

    /// Whether this is a text message.
    pub fn is_text(&self) -> bool {
        matches!(self, Message::Text(_))
    }

    /// Whether this is a binary message.
    pub fn is_binary(&self) -> bool {
        matches!(self, Message::Binary(_))
    }

    /// Whether this is a close message.
    pub fn is_close(&self) -> bool {
        matches!(self, Message::Close(_))
    }

    /// The text of a text message.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Message::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The payload as bytes: the text's UTF-8, or the binary, ping or pong
    /// data; empty for close.
    pub fn into_data(self) -> Bytes {
        match self {
            Message::Text(text) => Bytes::from(text),
            Message::Binary(data) | Message::Ping(data) | Message::Pong(data) => data,
            Message::Close(_) => Bytes::new(),
        }
    }

    fn into_tungstenite(self) -> tungstenite::Message {
        match self {
            Message::Text(text) => tungstenite::Message::text(text),
            Message::Binary(data) => tungstenite::Message::Binary(data),
            Message::Ping(data) => tungstenite::Message::Ping(data),
            Message::Pong(data) => tungstenite::Message::Pong(data),
            Message::Close(frame) => tungstenite::Message::Close(frame.map(|frame| {
                tungstenite::protocol::CloseFrame { code: CloseCode::from(frame.code), reason: frame.reason.into() }
            })),
        }
    }

    fn from_tungstenite(message: tungstenite::Message) -> Option<Self> {
        Some(match message {
            tungstenite::Message::Text(text) => Message::Text(text.as_str().to_string()),
            tungstenite::Message::Binary(data) => Message::Binary(data),
            tungstenite::Message::Ping(data) => Message::Ping(data),
            tungstenite::Message::Pong(data) => Message::Pong(data),
            tungstenite::Message::Close(frame) => Message::Close(
                frame.map(|frame| CloseFrame { code: u16::from(frame.code), reason: frame.reason.as_str().to_string() }),
            ),
            // Raw frames are never yielded when reading.
            tungstenite::Message::Frame(_) => return None,
        })
    }
}

impl From<String> for Message {
    fn from(text: String) -> Self {
        Message::Text(text)
    }
}

impl From<&str> for Message {
    fn from(text: &str) -> Self {
        Message::Text(text.to_string())
    }
}

impl From<Bytes> for Message {
    fn from(data: Bytes) -> Self {
        Message::Binary(data)
    }
}

impl From<Vec<u8>> for Message {
    fn from(data: Vec<u8>) -> Self {
        Message::Binary(data.into())
    }
}

/// A WebSocket error: the connection failed, was closed, or the peer broke
/// the protocol.
#[derive(Debug)]
pub struct Error(tungstenite::Error);

impl Error {
    /// Whether the connection is already closed, so nothing more can be sent.
    pub fn is_closed(&self) -> bool {
        matches!(self.0, tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// An accepted WebSocket connection, from [`Request::upgrade_websocket`].
///
/// Read with [`recv`](WebSocket::recv) and write with
/// [`send`](WebSocket::send), or use it as a `Stream` and `Sink` of
/// [`Message`]s.
pub struct WebSocket {
    inner: WebSocketStream<TokioIo<Upgraded>>,
}

impl fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebSocket")
    }
}

impl WebSocket {
    /// The next message, or `None` once the connection has closed.
    pub async fn recv(&mut self) -> Option<Result<Message, Error>> {
        StreamExt::next(self).await
    }

    /// Sends a message: a `Message`, or text (`String`, `&str`) or binary
    /// data (`Vec<u8>`, `Bytes`).
    pub async fn send(&mut self, message: impl Into<Message>) -> Result<(), Error> {
        SinkExt::send(self, message.into()).await
    }

    /// Sends a close frame with a code (e.g. 1000 normal, 1001 going away)
    /// and a reason, then finishes the closing handshake.
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<(), Error> {
        self.send(Message::close(code, reason)).await?;
        SinkExt::close(self).await
    }
}

impl Stream for WebSocket {
    type Item = Result<Message, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            return match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(message))) => match Message::from_tungstenite(message) {
                    Some(message) => Poll::Ready(Some(Ok(message))),
                    None => continue,
                },
                Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(Error(error)))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

impl Sink<Message> for WebSocket {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.inner).poll_ready(cx).map_err(Error)
    }

    fn start_send(mut self: Pin<&mut Self>, message: Message) -> Result<(), Error> {
        Pin::new(&mut self.inner).start_send(message.into_tungstenite()).map_err(Error)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.inner).poll_flush(cx).map_err(Error)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.inner).poll_close(cx).map_err(Error)
    }
}

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
    /// The socket keeps counting toward `max_connections`. It has no idle
    /// timeout; watch [`Request::shutdown`] to close it when the server stops,
    /// or it is closed after `shutdown_timeout`.
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

        let session = async move {
            match on_upgrade.await {
                Ok(upgraded) => {
                    let inner = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
                    on_connect(WebSocket { inner }).await;
                }
                Err(e) => tracing::warn!(error = %e, "WebSocket upgrade failed"),
            }
        };
        // Keep the connection's slot and stay tracked for shutdown.
        match self.conn.as_ref().and_then(|conn| conn.upgrade.as_ref()) {
            Some(context) => context.spawn(session),
            None => {
                tokio::spawn(session);
            }
        }

        let mut response = Response::new(101);
        response
            .header("Upgrade", "websocket")
            .header("Connection", "Upgrade")
            .header("Sec-WebSocket-Accept", accept);
        Ok(response)
    }
}
