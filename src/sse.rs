//! Server-sent events (`text/event-stream`).
//!
//! Push events from any task with [`channel`]:
//!
//! ```rust
//! use axeon::Server;
//! use axeon::sse::{self, Event};
//! use std::time::Duration;
//!
//! let mut app = Server::new();
//! app.get("/events", |_req| async {
//!     let (tx, response) = sse::channel(16);
//!     tokio::spawn(async move {
//!         for i in 0.. {
//!             // Fails once the client disconnects.
//!             if tx.send(Event::new().event("tick").data(i.to_string())).await.is_err() {
//!                 break;
//!             }
//!             tokio::time::sleep(Duration::from_secs(1)).await;
//!         }
//!     });
//!     response
//! });
//! ```
//!
//! Or turn any `Stream` of events into a response with [`Response::sse`].

use crate::http::Response;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{sleep, Instant, Sleep};

/// One server-sent event.
#[derive(Debug, Clone, Default)]
pub struct Event {
    event: Option<String>,
    id: Option<String>,
    retry: Option<Duration>,
    data: Option<String>,
    comment: Option<String>,
}

/// Field values can't contain line breaks; they would end the field early.
fn single_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

impl Event {
    pub fn new() -> Self {
        Self::default()
    }

    /// The event payload. Multi-line text is sent as several `data:` lines
    /// and arrives in the browser joined with `\n`.
    pub fn data(mut self, data: impl Into<String>) -> Self {
        self.data = Some(data.into());
        self
    }

    /// The payload as JSON.
    pub fn json_data<T: serde::Serialize>(self, data: &T) -> Result<Self, serde_json::Error> {
        Ok(self.data(serde_json::to_string(data)?))
    }

    /// The event type, dispatched to `addEventListener(name, ...)` in browsers.
    pub fn event(mut self, name: impl AsRef<str>) -> Self {
        self.event = Some(single_line(name.as_ref()));
        self
    }

    /// The event id; browsers send it back as `Last-Event-ID` when reconnecting.
    pub fn id(mut self, id: impl AsRef<str>) -> Self {
        self.id = Some(single_line(id.as_ref()).replace('\0', ""));
        self
    }

    /// How long the browser waits before reconnecting.
    pub fn retry(mut self, after: Duration) -> Self {
        self.retry = Some(after);
        self
    }

    /// A comment line, ignored by browsers.
    pub fn comment(mut self, text: impl AsRef<str>) -> Self {
        self.comment = Some(single_line(text.as_ref()));
        self
    }

    fn to_bytes(&self) -> Bytes {
        let mut out = String::new();
        if let Some(comment) = &self.comment {
            out.push_str(&format!(": {}\n", comment));
        }
        if let Some(event) = &self.event {
            out.push_str(&format!("event: {}\n", event));
        }
        if let Some(id) = &self.id {
            out.push_str(&format!("id: {}\n", id));
        }
        if let Some(retry) = self.retry {
            out.push_str(&format!("retry: {}\n", retry.as_millis()));
        }
        if let Some(data) = &self.data {
            for line in data.split('\n') {
                out.push_str("data: ");
                out.push_str(line.strip_suffix('\r').unwrap_or(line));
                out.push('\n');
            }
        }
        out.push('\n');
        Bytes::from(out)
    }
}

impl Response {
    /// A `text/event-stream` response that sends each event as it is produced.
    pub fn sse<S>(events: S) -> Response
    where
        S: Stream<Item = Event> + Send + 'static,
    {
        let mut response = Response::stream(
            "text/event-stream",
            events.map(|event| Ok::<_, Infallible>(event.to_bytes())),
        );
        response
            .header("Cache-Control", "no-cache")
            // Stop nginx from buffering the stream.
            .header("X-Accel-Buffering", "no");
        // Browsers' EventSource reconnects, so end cleanly on shutdown.
        response.end_on_shutdown = true;
        response
    }

    /// Like [`Response::sse`], also sending a comment whenever no event has
    /// been sent for `interval`, so proxies don't drop an idle connection.
    pub fn sse_with_keep_alive<S>(events: S, interval: Duration) -> Response
    where
        S: Stream<Item = Event> + Send + 'static,
    {
        Response::sse(KeepAlive {
            events: Box::pin(events),
            sleep: Box::pin(sleep(interval)),
            interval,
        })
    }
}

struct KeepAlive {
    events: Pin<Box<dyn Stream<Item = Event> + Send>>,
    sleep: Pin<Box<Sleep>>,
    interval: Duration,
}

impl Stream for KeepAlive {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        let interval = self.interval;
        match self.events.as_mut().poll_next(cx) {
            Poll::Ready(Some(event)) => {
                self.sleep.as_mut().reset(Instant::now() + interval);
                Poll::Ready(Some(event))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => match self.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    self.sleep.as_mut().reset(Instant::now() + interval);
                    Poll::Ready(Some(Event::new().comment("keep-alive")))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

/// Sends events to the client of an [`channel`] response.
#[derive(Clone, Debug)]
pub struct Sender {
    tx: mpsc::Sender<Event>,
}

/// The client disconnected (or the response was dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "event stream closed")
    }
}

impl std::error::Error for Closed {}

impl Sender {
    /// Queues an event, waiting if `buffer` events are already queued.
    pub async fn send(&self, event: Event) -> Result<(), Closed> {
        self.tx.send(event).await.map_err(|_| Closed)
    }

    /// True once the client has gone away.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// An event-stream response fed by the returned [`Sender`]. The stream ends
/// when every sender is dropped; `send` fails once the client disconnects.
pub fn channel(buffer: usize) -> (Sender, Response) {
    let (tx, rx) = mpsc::channel(buffer.max(1));
    let events = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|event| (event, rx)) });
    (Sender { tx }, Response::sse(events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_events() {
        let event = Event::new().event("update").id("7").retry(Duration::from_secs(3)).data("a\nb");
        assert_eq!(&event.to_bytes()[..], b"event: update\nid: 7\nretry: 3000\ndata: a\ndata: b\n\n");
        assert_eq!(&Event::new().comment("hi").to_bytes()[..], b": hi\n\n");
        // Line breaks can't escape their field.
        assert_eq!(&Event::new().event("a\nb").to_bytes()[..], b"event: a b\n\n");
    }
}
