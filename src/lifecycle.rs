//! Connection lifecycle: the shutdown notice handed to handlers, the context
//! that lets upgraded connections stay tracked, and the write timeout.

use crate::message::Request;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{watch, OwnedSemaphorePermit};
use tokio::time::{sleep, Instant, Sleep};
#[cfg(feature = "ws")]
use tokio_util::sync::CancellationToken;
#[cfg(feature = "ws")]
use tokio_util::task::TaskTracker;

/// Tells long-running handlers that the server is shutting down. Get one
/// with [`Request::shutdown`]. See the [`ws`](crate::ws) module docs for
/// closing WebSockets on shutdown.
///
/// ```rust
/// use axeon::Server;
/// use std::time::Duration;
///
/// let mut app = Server::new();
/// app.get("/report", |req| async move {
///     let shutdown = req.shutdown();
///     tokio::select! {
///         report = build_report() => report,
///         _ = shutdown.wait() => "server is restarting, please retry".to_string(),
///     }
/// });
///
/// async fn build_report() -> String {
///     tokio::time::sleep(Duration::from_secs(60)).await;
///     "report".to_string()
/// }
/// ```
#[derive(Clone, Debug)]
pub struct Shutdown {
    rx: Option<watch::Receiver<bool>>,
}

impl Shutdown {
    pub(crate) fn new(rx: watch::Receiver<bool>) -> Self {
        Self { rx: Some(rx) }
    }

    /// True once shutdown has started.
    pub fn is_shutting_down(&self) -> bool {
        self.rx.as_ref().is_some_and(|rx| *rx.borrow())
    }

    /// Completes when shutdown starts (immediately if it already has).
    /// Never completes for requests built by hand outside a server.
    pub async fn wait(&self) {
        match &self.rx {
            Some(rx) => {
                let mut rx = rx.clone();
                // An error means the server is gone, which counts too.
                let _ = rx.wait_for(|stopping| *stopping).await;
            }
            None => std::future::pending().await,
        }
    }
}

impl Request {
    /// A handle that reports when the server starts shutting down, for
    /// handlers that run long (WebSockets, streams, background work).
    pub fn shutdown(&self) -> Shutdown {
        match &self.conn {
            Some(conn) => Shutdown::new(conn.draining.clone()),
            None => Shutdown { rx: None },
        }
    }
}

/// Per-connection context, shared (one `Arc`) by the connection's requests.
pub(crate) struct ConnContext {
    pub(crate) draining: watch::Receiver<bool>,
    #[cfg(feature = "ws")]
    pub(crate) upgrade: Option<UpgradeContext>,
}

impl std::fmt::Debug for ConnContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnContext").finish_non_exhaustive()
    }
}

/// Per-connection state that lets an upgraded connection (a WebSocket) keep
/// its `max_connections` slot and stay tracked for shutdown.
#[cfg(feature = "ws")]
#[derive(Clone)]
pub(crate) struct UpgradeContext {
    pub(crate) tasks: TaskTracker,
    pub(crate) hard_stop: CancellationToken,
    pub(crate) slot: Arc<Mutex<Option<OwnedSemaphorePermit>>>,
}

#[cfg(feature = "ws")]
impl UpgradeContext {
    /// Runs `task` as a tracked connection, holding this connection's slot.
    /// It is dropped (closing the socket) if shutdown times out.
    pub(crate) fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let permit = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        let hard_stop = self.hard_stop.clone();
        self.tasks.spawn(async move {
            let _permit = permit;
            tokio::select! {
                _ = task => {}
                _ = hard_stop.cancelled() => {}
            }
        });
    }
}

/// The connection slot, shared by a connection's requests.
pub(crate) type Slot = Arc<Mutex<Option<OwnedSemaphorePermit>>>;

/// How long, and how much, a closing connection reads and discards; it stops
/// sooner once the client has gone quiet.
const LINGER_TIME: Duration = Duration::from_secs(2);
const LINGER_QUIET: Duration = Duration::from_millis(500);
const LINGER_BYTES: usize = 1 << 20;

/// Closes gracefully: after shutting down the write side, reads and discards
/// what the client is still sending (an unread request body, say) for a
/// while. Closing a socket with unread data resets the connection, and a
/// reset can make the client drop the response it was sent before reading it.
pub(crate) struct LingeringClose<S> {
    inner: S,
    linger: Option<Linger>,
}

struct Linger {
    deadline: Pin<Box<Sleep>>,
    quiet: Pin<Box<Sleep>>,
    left: usize,
}

impl<S> LingeringClose<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self { inner, linger: None }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for LingeringClose<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for LingeringClose<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if this.linger.is_none() {
            ready!(Pin::new(&mut this.inner).poll_shutdown(cx))?;
            this.linger = Some(Linger {
                deadline: Box::pin(sleep(LINGER_TIME)),
                quiet: Box::pin(sleep(LINGER_QUIET)),
                left: LINGER_BYTES,
            });
        }
        let linger = this.linger.as_mut().expect("set above");
        let mut scratch = [0u8; 8192];
        loop {
            if linger.left == 0 || linger.deadline.as_mut().poll(cx).is_ready() || linger.quiet.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Ok(()));
            }
            let mut buf = ReadBuf::new(&mut scratch);
            match Pin::new(&mut this.inner).poll_read(cx, &mut buf) {
                Poll::Ready(Ok(())) if buf.filled().is_empty() => return Poll::Ready(Ok(())),
                Poll::Ready(Ok(())) => {
                    linger.left = linger.left.saturating_sub(buf.filled().len());
                    linger.quiet.as_mut().reset(Instant::now() + LINGER_QUIET);
                }
                // The write side is already shut down: nothing left to protect.
                Poll::Ready(Err(_)) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Fails writes that make no progress for `timeout`, so a client that stops
/// reading can't hold a connection (and its buffers) forever.
pub(crate) struct WriteTimeout<S> {
    inner: S,
    timeout: Duration,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<S> WriteTimeout<S> {
    pub(crate) fn new(inner: S, timeout: Duration) -> Self {
        Self { inner, timeout, stalled: None }
    }

    /// Tracks a pending write: starts the timer on the first `Pending`,
    /// clears it on progress, and fails once it fires.
    fn check<T>(&mut self, cx: &mut Context<'_>, result: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        match result {
            Poll::Ready(result) => {
                self.stalled = None;
                Poll::Ready(result)
            }
            Poll::Pending => {
                let timeout = self.timeout;
                let timer = self.stalled.get_or_insert_with(|| Box::pin(sleep(timeout)));
                match timer.as_mut().poll(cx) {
                    Poll::Ready(()) => Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "write timed out"))),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for WriteTimeout<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for WriteTimeout<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.check(cx, result)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.check(cx, result)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        self.check(cx, result)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.check(cx, result)
    }
}
