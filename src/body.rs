//! Request and response bodies as [`http_body::Body`] types: for reading
//! uploads as a stream, and for interoperating with tower and hyper.

use crate::error::{ServerError, ServerResult};
use bytes::Bytes;
use futures::Stream;
use http_body::{Body, Frame, SizeHint};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, Instant, Sleep};

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Maps a body error to the response it should produce.
pub(crate) fn body_error(error: BoxError) -> ServerError {
    if error.downcast_ref::<LengthLimitError>().is_some() {
        return ServerError::PayloadTooLarge;
    }
    match error.downcast::<ServerError>() {
        Ok(error) => *error,
        Err(_) => ServerError::BadRequest("invalid request body".to_string()),
    }
}

/// A request body that can be read as it arrives.
///
/// Get it with [`Request::body_stream`](crate::Request::body_stream). It is a
/// [`Stream`] of chunks and an [`http_body::Body`], and has helpers to collect
/// it ([`bytes`](RequestBody::bytes)) or write it to a file
/// ([`save_to`](RequestBody::save_to)). Reading fails with
/// `ServerError::PayloadTooLarge` past the route's body limit and with
/// `ServerError::RequestTimeout` if the client stops sending.
pub struct RequestBody {
    kind: Kind,
}

enum Kind {
    Full(Option<Bytes>),
    Stream {
        body: UnsyncBoxBody<Bytes, ServerError>,
        idle: Option<(Duration, Pin<Box<Sleep>>)>,
    },
}

impl RequestBody {
    /// An empty body.
    pub fn empty() -> Self {
        Self { kind: Kind::Full(None) }
    }

    /// Wraps any [`http_body::Body`].
    pub fn new<B>(body: B) -> Self
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        Self::from_box(body.map_err(|e| body_error(e.into())).boxed_unsync(), None)
    }

    /// A streamed body limited to `limit` bytes that fails if no data
    /// arrives for `idle_timeout`.
    pub(crate) fn limited<B>(body: B, limit: usize, idle_timeout: Duration) -> Self
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        let body = Limited::new(body, limit).map_err(body_error).boxed_unsync();
        Self::from_box(body, Some(idle_timeout))
    }

    fn from_box(body: UnsyncBoxBody<Bytes, ServerError>, idle_timeout: Option<Duration>) -> Self {
        let idle = idle_timeout.map(|timeout| (timeout, Box::pin(sleep(timeout))));
        Self { kind: Kind::Stream { body, idle } }
    }

    /// The buffered bytes, if this body isn't a stream.
    pub(crate) fn into_full(self) -> Result<Bytes, Self> {
        match self.kind {
            Kind::Full(bytes) => Ok(bytes.unwrap_or_default()),
            kind => Err(Self { kind }),
        }
    }

    /// Reads the whole body into memory.
    pub async fn bytes(self) -> ServerResult<Bytes> {
        match self.into_full() {
            Ok(bytes) => Ok(bytes),
            Err(body) => Ok(BodyExt::collect(body).await?.to_bytes()),
        }
    }

    /// Writes the body to a new file at `path` as it arrives, returning the
    /// number of bytes written. The file is removed if reading fails.
    ///
    /// Don't build `path` from client input such as an uploaded file name.
    pub async fn save_to(self, path: impl AsRef<Path>) -> ServerResult<u64> {
        save_stream(self, path.as_ref()).await
    }
}

/// Writes a stream of chunks to a new file, removing it on failure.
pub(crate) async fn save_stream<S>(mut stream: S, path: &Path) -> ServerResult<u64>
where
    S: Stream<Item = ServerResult<Bytes>> + Unpin,
{
    use futures::StreamExt;
    let mut file = tokio::fs::File::create(path).await?;
    let mut written = 0u64;
    let result: ServerResult<()> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        drop(file);
        let _ = tokio::fs::remove_file(path).await;
        return Err(error);
    }
    Ok(written)
}

impl Default for RequestBody {
    fn default() -> Self {
        Self::empty()
    }
}

impl From<Bytes> for RequestBody {
    fn from(bytes: Bytes) -> Self {
        Self { kind: Kind::Full((!bytes.is_empty()).then_some(bytes)) }
    }
}

impl From<Vec<u8>> for RequestBody {
    fn from(bytes: Vec<u8>) -> Self {
        Bytes::from(bytes).into()
    }
}

impl From<String> for RequestBody {
    fn from(text: String) -> Self {
        Bytes::from(text).into()
    }
}

impl From<&'static str> for RequestBody {
    fn from(text: &'static str) -> Self {
        Bytes::from_static(text.as_bytes()).into()
    }
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = ServerError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, ServerError>>> {
        match &mut self.kind {
            Kind::Full(bytes) => Poll::Ready(bytes.take().map(|b| Ok(Frame::data(b)))),
            Kind::Stream { body, idle } => match Pin::new(body).poll_frame(cx) {
                Poll::Ready(frame) => {
                    if let Some((timeout, timer)) = idle {
                        let deadline = Instant::now() + *timeout;
                        timer.as_mut().reset(deadline);
                    }
                    Poll::Ready(frame)
                }
                Poll::Pending => {
                    let timed_out = match idle {
                        Some((_, timer)) => timer.as_mut().poll(cx).is_ready(),
                        None => false,
                    };
                    if timed_out {
                        Poll::Ready(Some(Err(ServerError::RequestTimeout)))
                    } else {
                        Poll::Pending
                    }
                }
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.kind {
            Kind::Full(bytes) => bytes.is_none(),
            Kind::Stream { body, .. } => body.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.kind {
            Kind::Full(bytes) => SizeHint::with_exact(bytes.as_ref().map_or(0, |b| b.len() as u64)),
            Kind::Stream { body, .. } => body.size_hint(),
        }
    }
}

impl Stream for RequestBody {
    type Item = ServerResult<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            return match self.as_mut().poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => Poll::Ready(Some(Ok(data))),
                    Err(_trailers) => continue,
                },
                Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

/// A response body: buffered bytes or a stream. This is the body type of
/// responses produced by [`AppService`](crate::AppService) and seen by tower
/// layers.
pub struct ResponseBody {
    kind: ResponseKind,
}

enum ResponseKind {
    Full(Option<Bytes>),
    Stream(UnsyncBoxBody<Bytes, BoxError>),
}

impl ResponseBody {
    pub(crate) fn full(bytes: Bytes) -> Self {
        Self { kind: ResponseKind::Full((!bytes.is_empty()).then_some(bytes)) }
    }

    /// Wraps any [`http_body::Body`].
    pub fn new<B>(body: B) -> Self
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        Self { kind: ResponseKind::Stream(body.map_err(Into::into).boxed_unsync()) }
    }

    /// Splits into buffered bytes or the stream.
    pub(crate) fn into_parts(self) -> Result<Bytes, UnsyncBoxBody<Bytes, BoxError>> {
        match self.kind {
            ResponseKind::Full(bytes) => Ok(bytes.unwrap_or_default()),
            ResponseKind::Stream(body) => Err(body),
        }
    }

    /// Ends a streamed body early (cleanly) when `stop` completes.
    pub(crate) fn take_until<F>(self, stop: F) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        use futures::StreamExt;
        match self.kind {
            ResponseKind::Stream(body) => {
                let frames = http_body_util::BodyStream::new(body).take_until(stop);
                Self::new(http_body_util::StreamBody::new(frames))
            }
            full => Self { kind: full },
        }
    }
}

impl Default for ResponseBody {
    fn default() -> Self {
        Self::full(Bytes::new())
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        match &mut self.kind {
            ResponseKind::Full(bytes) => Poll::Ready(bytes.take().map(|b| Ok(Frame::data(b)))),
            ResponseKind::Stream(body) => Pin::new(body).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.kind {
            ResponseKind::Full(bytes) => bytes.is_none(),
            ResponseKind::Stream(body) => body.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.kind {
            ResponseKind::Full(bytes) => SizeHint::with_exact(bytes.as_ref().map_or(0, |b| b.len() as u64)),
            ResponseKind::Stream(body) => body.size_hint(),
        }
    }
}

/// Moves `value` into `U` if it is one, without copying (for recognising
/// our own body types after a round trip through tower).
pub(crate) fn downcast<T: 'static, U: 'static>(value: T) -> Result<U, T> {
    let boxed: Box<dyn std::any::Any> = Box::new(value);
    match boxed.downcast::<U>() {
        Ok(value) => Ok(*value),
        Err(boxed) => Err(*boxed.downcast::<T>().expect("same type")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use http_body_util::{Full, StreamBody};

    #[tokio::test]
    async fn streams_and_collects() {
        let chunks = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(Frame::data(Bytes::from("ab"))),
            Ok(Frame::data(Bytes::from("cd"))),
        ]);
        let mut body = RequestBody::new(StreamBody::new(chunks));
        assert_eq!(body.next().await.unwrap().unwrap(), "ab");
        assert_eq!(body.bytes().await.unwrap(), "cd");
        assert_eq!(RequestBody::from("full").bytes().await.unwrap(), "full");
    }

    #[tokio::test]
    async fn enforces_limit_and_idle_timeout() {
        let body = RequestBody::limited(Full::new(Bytes::from(vec![0u8; 10])), 5, Duration::from_secs(5));
        assert!(matches!(body.bytes().await, Err(ServerError::PayloadTooLarge)));

        let stalled = StreamBody::new(futures::stream::pending::<Result<Frame<Bytes>, std::io::Error>>());
        let body = RequestBody::limited(stalled, 5, Duration::from_millis(50));
        assert!(matches!(body.bytes().await, Err(ServerError::RequestTimeout)));
    }

    #[test]
    fn downcasts_without_copying() {
        assert_eq!(downcast::<_, Bytes>(Bytes::from("x")).unwrap(), "x");
        assert!(downcast::<_, Bytes>(5u8).is_err());
    }
}
