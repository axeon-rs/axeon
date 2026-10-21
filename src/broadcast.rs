//! Messages published on any instance of the app, delivered to subscribers
//! on every instance, with replay for clients that reconnect.
//!
//! A [`Hub`] publishes messages to *topics* and streams them to
//! subscribers, typically browsers over [server-sent events](crate::sse).
//! Messages go through a shared [`Backend`]: a log that keeps recent
//! messages (so a client that reconnects with `Last-Event-ID` gets what it
//! missed, from whichever instance it reaches) plus a signal that tells
//! every instance a topic has new messages. Redis and PostgreSQL backends
//! live in the axeon-redis and axeon-orm crates; [`MemoryBackend`] works
//! within one process.
//!
//! ```rust
//! use axeon::broadcast::{Hub, MemoryBackend, Message};
//! use axeon::{Server, ServerError};
//!
//! let hub = Hub::new(MemoryBackend::new());
//! let mut app = Server::new();
//! app.state(hub.clone());
//!
//! // Browsers: `new EventSource("/runs/42/events")`, which reconnects with
//! // `Last-Event-ID` by itself.
//! app.get("/runs/:run/events", |req| async move {
//!     let run: u64 = req.param("run")?;
//!     let hub = req.state::<Hub>().ok_or_else(|| ServerError::InternalError("no hub".into()))?;
//!     hub.sse(&format!("run:{}", run), req.last_event_id()).await
//! });
//!
//! // Anywhere, e.g. in a background job on another instance:
//! # async fn publish(hub: Hub) -> Result<(), axeon::broadcast::BoxError> {
//! hub.publish("run:42", Message::new(r#"{"step":3,"status":"done"}"#).event("step")).await?;
//! # Ok(()) }
//! ```
//!
//! Each instance keeps one wake-up connection to the backend, however many
//! clients are subscribed, and reads each topic's new messages once for all
//! its subscribers. A subscriber that falls behind reads from the log
//! instead of losing messages. If a client's last message is no longer in
//! the log, the stream starts with a `reset` event, so the client knows to
//! reload its state instead of trusting the replay.

use crate::error::{ServerError, ServerResult};
use crate::message::{Request, Response};
use crate::sse::Event;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::broadcast;

/// An error from a [`Backend`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How many messages a backend read returns at most.
const PAGE: usize = 100;
/// Messages a topic's in-process feed buffers per subscriber.
const FEED_CAPACITY: usize = 256;

/// A message on a topic. Published messages get an ID from the backend,
/// sent to SSE clients as the event `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    id: String,
    event: Option<String>,
    data: String,
}

impl Message {
    /// A message carrying `data`, e.g. JSON.
    pub fn new(data: impl Into<String>) -> Self {
        Self { id: String::new(), event: None, data: data.into() }
    }

    /// A message carrying `value` as JSON.
    pub fn json<T: serde::Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        Ok(Self::new(serde_json::to_string(value)?))
    }

    /// The event name, which browsers dispatch to
    /// `addEventListener(name, ...)`; without one it's a `message` event.
    pub fn event(mut self, name: impl Into<String>) -> Self {
        self.event = Some(name.into());
        self
    }

    /// Sets the ID; for backends, when reading messages back.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// The backend's ID, empty before publishing.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The event name.
    pub fn event_name(&self) -> Option<&str> {
        self.event.as_deref()
    }

    /// The data.
    pub fn data(&self) -> &str {
        &self.data
    }

    fn to_sse(&self) -> Event {
        let mut event = Event::new().id(&self.id).data(&self.data);
        if let Some(name) = &self.event {
            event = event.event(name);
        }
        event
    }
}

/// Messages from [`Backend::read`].
#[derive(Clone, Debug, Default)]
pub struct Batch {
    /// Messages after the given ID, oldest first.
    pub messages: Vec<Message>,
    /// The given ID is no longer in the log (it was trimmed), so messages
    /// may be missing before these.
    pub missed: bool,
}

/// What a [`Backend`]'s wake-up stream reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wakeup {
    /// This topic has new messages.
    Topic(String),
    /// The stream reconnected and may have missed wake-ups: check every
    /// topic.
    Reconnected,
}

/// Where a [`Hub`]'s messages go: a log of recent messages per topic, and
/// a way to tell every instance that a topic has new ones.
///
/// IDs are the backend's own, and must increase within a topic.
pub trait Backend: Send + Sync + 'static {
    /// Appends a message to `topic`'s log and wakes every instance; returns
    /// the message's ID.
    fn publish<'a>(&'a self, topic: &'a str, message: &'a Message) -> BoxFuture<'a, Result<String, BoxError>>;

    /// Up to `limit` messages of `topic` after `after` (from the oldest
    /// kept, with `None`), oldest first, with their IDs.
    fn read<'a>(&'a self, topic: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Batch, BoxError>>;

    /// The ID of `topic`'s newest message, if any.
    fn latest<'a>(&'a self, topic: &'a str) -> BoxFuture<'a, Result<Option<String>, BoxError>>;

    /// A stream of wake-ups for all topics. When it ends, the hub calls
    /// this again after a pause.
    fn wakeups(&self) -> BoxFuture<'_, Result<BoxStream<'static, Wakeup>, BoxError>>;

    /// Whether ID `id` comes after `other` in a topic.
    fn is_after(&self, id: &str, other: &str) -> bool;
}

/// Publishes messages and streams them to subscribers; see the
/// [module docs](self). Clones share everything; keep one per process.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

struct Inner {
    backend: Box<dyn Backend>,
    feeds: Mutex<HashMap<String, Arc<Feed>>>,
    listening: std::sync::Once,
    keep_alive: Duration,
}

/// One topic's messages within this process, read from the backend once
/// for all its subscribers.
struct Feed {
    tx: broadcast::Sender<Message>,
    /// The last message read from the backend; held while reading, so
    /// reads of one topic don't overlap.
    last: tokio::sync::Mutex<Option<String>>,
}

impl Hub {
    /// A hub on `backend`.
    pub fn new(backend: impl Backend) -> Self {
        Self {
            inner: Arc::new(Inner {
                backend: Box::new(backend),
                feeds: Mutex::new(HashMap::new()),
                listening: std::sync::Once::new(),
                keep_alive: Duration::from_secs(15),
            }),
        }
    }

    /// Publishes `message` on `topic` for subscribers on every instance;
    /// returns its ID.
    pub async fn publish(&self, topic: &str, message: Message) -> Result<String, BoxError> {
        self.inner.backend.publish(topic, &message).await
    }

    /// Subscribes to `topic`: messages after `after` (a message ID, e.g.
    /// from `Last-Event-ID`) first, then new ones as they're published.
    /// Without `after`, only new ones.
    pub async fn subscribe(&self, topic: &str, after: Option<&str>) -> Result<Subscription, BoxError> {
        self.listen();
        // Subscribed before reading the log, so nothing published meanwhile
        // is lost; duplicates are skipped by ID.
        let live = self.feed(topic).await?;
        let backend = &self.inner.backend;
        let (replay, missed, delivered) = match after {
            Some(after) => {
                let (messages, missed) = read_all(&**backend, topic, Some(after)).await?;
                let delivered = messages.last().map(|m| m.id.clone()).unwrap_or_else(|| after.to_string());
                (messages, missed, Some(delivered))
            }
            None => (Vec::new(), false, backend.latest(topic).await?),
        };
        Ok(Subscription {
            stream: Box::pin(subscription(Arc::clone(&self.inner), topic.to_string(), replay.into(), live, delivered)),
            missed,
        })
    }

    /// A server-sent events response for `topic`, resuming after
    /// `last_event_id` (pass [`Request::last_event_id`]). It starts with a
    /// `reset` event when messages were missed, sends a keep-alive comment
    /// every 15 seconds, and ends if the backend fails, so the browser
    /// reconnects and resumes.
    pub async fn sse(&self, topic: &str, last_event_id: Option<&str>) -> ServerResult<Response> {
        let subscription = self.subscribe(topic, last_event_id).await.map_err(|error| {
            tracing::error!(%error, topic, "couldn't subscribe to the broadcast backend");
            ServerError::Status(503, "event stream unavailable".into())
        })?;
        let reset = subscription.missed.then(|| Event::new().event("reset").data(""));
        let messages = subscription
            .take_while(|message| {
                if let Err(error) = message {
                    tracing::warn!(%error, "broadcast subscription failed; ending the stream");
                }
                futures::future::ready(message.is_ok())
            })
            .filter_map(|message| futures::future::ready(message.ok().map(|m| m.to_sse())));
        let events = futures::stream::iter(reset).chain(messages);
        Ok(Response::sse_with_keep_alive(events, self.inner.keep_alive))
    }

    /// A receiver on the feed for `topic`, creating the feed with the
    /// topic's newest ID as its starting point.
    async fn feed(&self, topic: &str) -> Result<broadcast::Receiver<Message>, BoxError> {
        // Receiving starts under the lock that `refresh` checks, so a feed
        // is never dropped as unused while someone is joining it.
        let (feed, live, created) = {
            let mut feeds = self.inner.feeds.lock().unwrap_or_else(|e| e.into_inner());
            match feeds.get(topic) {
                Some(feed) => (Arc::clone(feed), feed.tx.subscribe(), false),
                None => {
                    let (tx, live) = broadcast::channel(FEED_CAPACITY);
                    let feed = Arc::new(Feed { tx, last: tokio::sync::Mutex::new(None) });
                    feeds.insert(topic.to_string(), Arc::clone(&feed));
                    (feed, live, true)
                }
            }
        };
        if created {
            // A wake-up meanwhile waits for this, then reads after it.
            let mut last = feed.last.lock().await;
            *last = self.inner.backend.latest(topic).await?;
        }
        Ok(live)
    }

    /// Starts the wake-up loop, once per hub.
    fn listen(&self) {
        let weak = Arc::downgrade(&self.inner);
        self.inner.listening.call_once(|| {
            tokio::spawn(wake_loop(weak));
        });
    }
}

/// Reacts to wake-ups until the hub is dropped, reconnecting as needed.
async fn wake_loop(hub: Weak<Inner>) {
    loop {
        let Some(inner) = hub.upgrade() else { return };
        match inner.backend.wakeups().await {
            Ok(mut wakeups) => {
                // Anything published before the stream was up.
                refresh_all(&inner);
                drop(inner);
                while let Some(wakeup) = wakeups.next().await {
                    let Some(inner) = hub.upgrade() else { return };
                    match wakeup {
                        Wakeup::Topic(topic) => refresh(&inner, topic),
                        Wakeup::Reconnected => refresh_all(&inner),
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "broadcast wake-ups unavailable; retrying"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn refresh_all(inner: &Arc<Inner>) {
    let topics: Vec<String> = inner.feeds.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
    for topic in topics {
        refresh(inner, topic);
    }
}

/// Reads `topic`'s new messages into its feed, in the background. A feed
/// nobody listens to any more is dropped instead.
fn refresh(inner: &Arc<Inner>, topic: String) {
    let feed = {
        let mut feeds = inner.feeds.lock().unwrap_or_else(|e| e.into_inner());
        match feeds.get(&topic) {
            Some(feed) if feed.tx.receiver_count() == 0 => {
                feeds.remove(&topic);
                return;
            }
            Some(feed) => Arc::clone(feed),
            None => return,
        }
    };
    let inner = Arc::clone(inner);
    tokio::spawn(async move {
        let mut last = feed.last.lock().await;
        loop {
            match inner.backend.read(&topic, last.as_deref(), PAGE).await {
                Ok(batch) => {
                    let full = batch.messages.len() >= PAGE;
                    for message in batch.messages {
                        *last = Some(message.id.clone());
                        let _ = feed.tx.send(message);
                    }
                    if !full {
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, topic, "couldn't read broadcast messages");
                    break;
                }
            }
        }
    });
}

/// Every message after `after`, page by page.
async fn read_all(backend: &dyn Backend, topic: &str, after: Option<&str>) -> Result<(Vec<Message>, bool), BoxError> {
    let mut messages: Vec<Message> = Vec::new();
    let mut missed = false;
    loop {
        let from = messages.last().map(|m| m.id.clone()).or_else(|| after.map(str::to_string));
        let batch = backend.read(topic, from.as_deref(), PAGE).await?;
        missed |= batch.missed && messages.is_empty();
        let full = batch.messages.len() >= PAGE;
        messages.extend(batch.messages);
        if !full {
            return Ok((messages, missed));
        }
    }
}

/// A subscriber's messages: the replay, then the live feed, skipping what
/// was already delivered, and re-reading the log after falling behind.
fn subscription(
    hub: Arc<Inner>,
    topic: String,
    replay: VecDeque<Message>,
    live: broadcast::Receiver<Message>,
    delivered: Option<String>,
) -> impl Stream<Item = Result<Message, BoxError>> + Send {
    struct State {
        hub: Arc<Inner>,
        topic: String,
        pending: VecDeque<Message>,
        live: broadcast::Receiver<Message>,
        delivered: Option<String>,
    }
    let state = State { hub, topic, pending: replay, live, delivered };
    futures::stream::unfold(Some(state), |state| async move {
        let mut state = state?;
        loop {
            if let Some(message) = state.pending.pop_front() {
                state.delivered = Some(message.id.clone());
                return Some((Ok(message), Some(state)));
            }
            match state.live.recv().await {
                Ok(message) => {
                    let new = match &state.delivered {
                        Some(delivered) => state.hub.backend.is_after(&message.id, delivered),
                        None => true,
                    };
                    if new {
                        state.delivered = Some(message.id.clone());
                        return Some((Ok(message), Some(state)));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Too slow for the feed: catch up from the log.
                    match read_all(&*state.hub.backend, &state.topic, state.delivered.as_deref()).await {
                        Ok((messages, _)) => state.pending = messages.into(),
                        Err(error) => return Some((Err(error), None)),
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

/// Messages of one topic, from [`Hub::subscribe`].
pub struct Subscription {
    stream: std::pin::Pin<Box<dyn Stream<Item = Result<Message, BoxError>> + Send>>,
    missed: bool,
}

impl Subscription {
    /// Messages were missed before the first one: the client's last
    /// message is no longer in the log.
    pub fn missed(&self) -> bool {
        self.missed
    }
}

impl Stream for Subscription {
    type Item = Result<Message, BoxError>;

    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}

impl Request {
    /// The `Last-Event-ID` header: the ID of the last server-sent event a
    /// reconnecting client received.
    pub fn last_event_id(&self) -> Option<&str> {
        self.get_header("last-event-id").filter(|id| !id.is_empty())
    }
}

/// A [`Backend`] within this process, keeping the last `capacity` messages
/// of each topic: for tests and apps that run one instance.
pub struct MemoryBackend {
    inner: Mutex<MemoryInner>,
    wake: broadcast::Sender<String>,
    capacity: usize,
}

#[derive(Default)]
struct MemoryInner {
    next_id: u64,
    logs: HashMap<String, VecDeque<Message>>,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBackend {
    /// A backend keeping 1000 messages per topic.
    pub fn new() -> Self {
        Self::with_capacity(1000)
    }

    /// A backend keeping `capacity` messages per topic.
    pub fn with_capacity(capacity: usize) -> Self {
        Self { inner: Mutex::new(MemoryInner::default()), wake: broadcast::channel(1024).0, capacity: capacity.max(1) }
    }
}

fn parse_id(id: &str) -> u64 {
    id.parse().unwrap_or(0)
}

impl Backend for MemoryBackend {
    fn publish<'a>(&'a self, topic: &'a str, message: &'a Message) -> BoxFuture<'a, Result<String, BoxError>> {
        let id = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.next_id += 1;
            let id = inner.next_id.to_string();
            let log = inner.logs.entry(topic.to_string()).or_default();
            log.push_back(message.clone().with_id(&id));
            while log.len() > self.capacity {
                log.pop_front();
            }
            id
        };
        let _ = self.wake.send(topic.to_string());
        Box::pin(async move { Ok(id) })
    }

    fn read<'a>(&'a self, topic: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Batch, BoxError>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let log = inner.logs.get(topic);
        let after = after.map(parse_id);
        let messages: Vec<Message> = log
            .into_iter()
            .flatten()
            .filter(|m| after.is_none_or(|after| parse_id(&m.id) > after))
            .take(limit)
            .cloned()
            .collect();
        let missed = match (after, log.and_then(|log| log.front())) {
            (Some(after), Some(oldest)) => after < parse_id(&oldest.id) && !log.unwrap().iter().any(|m| parse_id(&m.id) == after),
            _ => false,
        };
        Box::pin(async move { Ok(Batch { messages, missed }) })
    }

    fn latest<'a>(&'a self, topic: &'a str) -> BoxFuture<'a, Result<Option<String>, BoxError>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let latest = inner.logs.get(topic).and_then(|log| log.back()).map(|m| m.id.clone());
        Box::pin(async move { Ok(latest) })
    }

    fn wakeups(&self) -> BoxFuture<'_, Result<BoxStream<'static, Wakeup>, BoxError>> {
        let rx = self.wake.subscribe();
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            match rx.recv().await {
                Ok(topic) => Some((Wakeup::Topic(topic), rx)),
                Err(broadcast::error::RecvError::Lagged(_)) => Some((Wakeup::Reconnected, rx)),
                Err(broadcast::error::RecvError::Closed) => None,
            }
        });
        Box::pin(async move { Ok(stream.boxed()) })
    }

    fn is_after(&self, id: &str, other: &str) -> bool {
        parse_id(id) > parse_id(other)
    }
}

impl<B: Backend> Backend for Arc<B> {
    fn publish<'a>(&'a self, topic: &'a str, message: &'a Message) -> BoxFuture<'a, Result<String, BoxError>> {
        (**self).publish(topic, message)
    }
    fn read<'a>(&'a self, topic: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Batch, BoxError>> {
        (**self).read(topic, after, limit)
    }
    fn latest<'a>(&'a self, topic: &'a str) -> BoxFuture<'a, Result<Option<String>, BoxError>> {
        (**self).latest(topic)
    }
    fn wakeups(&self) -> BoxFuture<'_, Result<BoxStream<'static, Wakeup>, BoxError>> {
        (**self).wakeups()
    }
    fn is_after(&self, id: &str, other: &str) -> bool {
        (**self).is_after(id, other)
    }
}

/// Checks a [`Backend`]'s behaviour, for backend crates' tests.
#[doc(hidden)]
pub mod conformance {
    use super::*;
    use std::time::Duration;

    async fn next(subscription: &mut Subscription) -> Message {
        tokio::time::timeout(Duration::from_secs(5), subscription.next())
            .await
            .expect("no message within 5s")
            .expect("subscription ended")
            .expect("subscription failed")
    }

    async fn quiet(subscription: &mut Subscription) -> bool {
        tokio::time::timeout(Duration::from_millis(200), subscription.next()).await.is_err()
    }

    fn data(messages: &[Message]) -> Vec<&str> {
        messages.iter().map(|m| m.data()).collect()
    }

    /// Runs the checks with two hubs on separate connections to the same
    /// backend storage, standing for two instances of the app. `prefix`
    /// keeps topics apart between test runs; the log must keep at least
    /// 400 messages per topic.
    pub async fn check(a: Hub, b: Hub, prefix: &str) {
        let topic = format!("{}run:1", prefix);

        // Live: published on B, delivered on both, in order.
        let mut on_a = a.subscribe(&topic, None).await.unwrap();
        let mut on_b = b.subscribe(&topic, None).await.unwrap();
        assert!(!on_a.missed());
        let mut ids = Vec::new();
        for i in 0..20 {
            ids.push(b.publish(&topic, Message::new(i.to_string()).event("step")).await.unwrap());
        }
        for window in ids.windows(2) {
            assert!(a.inner.backend.is_after(&window[1], &window[0]), "IDs increase: {:?}", ids);
        }
        for subscription in [&mut on_a, &mut on_b] {
            let mut got = Vec::new();
            for _ in 0..20 {
                got.push(next(subscription).await);
            }
            let expected: Vec<String> = (0..20).map(|i| i.to_string()).collect();
            assert_eq!(data(&got), expected.iter().map(String::as_str).collect::<Vec<_>>());
            assert_eq!(got.iter().map(|m| m.id().to_string()).collect::<Vec<_>>(), ids);
            assert!(got.iter().all(|m| m.event_name() == Some("step")));
            assert!(quiet(subscription).await, "no duplicates");
        }

        // Resume: a client that saw up to message 9 on A reconnects to B.
        drop(on_a);
        b.publish(&topic, Message::new("20")).await.unwrap();
        let mut resumed = b.subscribe(&topic, Some(&ids[9])).await.unwrap();
        assert!(!resumed.missed());
        let mut got = Vec::new();
        for _ in 0..11 {
            got.push(next(&mut resumed).await);
        }
        assert_eq!(data(&got), ["10", "11", "12", "13", "14", "15", "16", "17", "18", "19", "20"]);
        assert_eq!(got[0].event_name(), Some("step"));
        assert_eq!(got[10].event_name(), None);
        // …then live, without repeats.
        a.publish(&topic, Message::new("21")).await.unwrap();
        assert_eq!(next(&mut resumed).await.data(), "21");
        assert!(quiet(&mut resumed).await);
        assert_eq!(next(&mut on_b).await.data(), "20");
        assert_eq!(next(&mut on_b).await.data(), "21");

        // Resuming from the newest message gets nothing old.
        let latest = a.publish(&topic, Message::new("22")).await.unwrap();
        let mut up_to_date = a.subscribe(&topic, Some(&latest)).await.unwrap();
        assert!(quiet(&mut up_to_date).await);

        // A new subscriber gets only new messages; other topics stay apart.
        let other = format!("{}run:2", prefix);
        let mut fresh = a.subscribe(&topic, None).await.unwrap();
        let mut elsewhere = a.subscribe(&other, None).await.unwrap();
        b.publish(&other, Message::new("other")).await.unwrap();
        b.publish(&topic, Message::new("23")).await.unwrap();
        assert_eq!(next(&mut fresh).await.data(), "23");
        assert_eq!(next(&mut elsewhere).await.data(), "other");
        assert!(quiet(&mut elsewhere).await);

        // Many subscribers on one instance.
        let crowd_topic = format!("{}crowd", prefix);
        let mut crowd = Vec::new();
        for _ in 0..50 {
            crowd.push(a.subscribe(&crowd_topic, None).await.unwrap());
        }
        b.publish(&crowd_topic, Message::new("hello all")).await.unwrap();
        for subscription in &mut crowd {
            assert_eq!(next(subscription).await.data(), "hello all");
        }

        // A subscriber that falls behind the in-process feed catches up
        // from the log, in order, without gaps.
        let flood = format!("{}flood", prefix);
        let mut slow = a.subscribe(&flood, None).await.unwrap();
        let mut keeping_up = a.subscribe(&flood, None).await.unwrap();
        for i in 0..400 {
            b.publish(&flood, Message::new(i.to_string())).await.unwrap();
            if i % 50 == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        for i in 0..400 {
            assert_eq!(next(&mut keeping_up).await.data(), i.to_string());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        for i in 0..400 {
            assert_eq!(next(&mut slow).await.data(), i.to_string(), "slow subscriber at {}", i);
        }
        assert!(quiet(&mut slow).await);
    }
}
