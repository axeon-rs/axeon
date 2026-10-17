use crate::error::ServerError;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use std::fmt;
use std::pin::Pin;

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, BoxError>> + Send>>;

/// An HTTP response: status, headers and body (bytes or a stream).
/// 
/// Build one with a constructor (`Response::text`, `json`, `html`, ...)
/// and the `with_*` builders, or return any [`IntoResponse`](crate::IntoResponse)
/// value from a handler.
pub struct Response {
    /// The status code, e.g. 200.
    pub status: u16,
    /// Raw body bytes. Binary content (images, compressed data) is sent unchanged.
    /// Cloning `Bytes` is cheap (reference counted). Ignored for streamed responses.
    pub body: Bytes,
    /// Response headers. Names are case-insensitive.
    pub headers: HeaderMap,
    stream: Option<BodyStream>,
    /// End the stream when the server shuts down (server-sent events).
    pub(crate) end_on_shutdown: bool,
    /// Length of the streamed body, sent as `Content-Length` when known.
    stream_length: Option<u64>,
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &self.body)
            .field("streaming", &self.stream.is_some())
            .finish()
    }
}

/// Statuses that never carry a body (RFC 9110 §6.4.1).
pub(crate) fn status_has_body(status: u16) -> bool {
    !(100..200).contains(&status) && status != 204 && status != 304
}

fn parse_header(name: &str, value: &str) -> Option<(HeaderName, HeaderValue)> {
    match (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
        (Ok(name), Ok(value)) => Some((name, value)),
        _ => {
            // Invalid names, or values containing CR/LF, would corrupt the response.
            tracing::warn!(header = %name, "ignoring invalid response header");
            None
        }
    }
}

impl Response {
    /// An empty response with this status.
    pub fn new(status: u16) -> Response {
        Response {
            status,
            headers: HeaderMap::new(),
            body: Bytes::new(),
            stream: None,
            end_on_shutdown: false,
            stream_length: None,
        }
    }

    /// A 200 response whose body is sent as the stream produces chunks,
    /// without buffering it all first (chunked over HTTP/1.1).
    ///
    /// If the stream yields an error the connection is aborted, since the
    /// status line has already been sent.
    ///
    /// ```rust
    /// use axeon::{Response, Server};
    /// use futures::stream;
    ///
    /// let mut app = Server::new();
    /// app.get("/numbers", |_req| async {
    ///     let chunks = stream::iter((1..=3).map(|n| Ok::<_, std::io::Error>(format!("{}\n", n))));
    ///     Response::stream("text/plain", chunks)
    /// });
    /// ```
    pub fn stream<S, B, E>(content_type: &str, stream: S) -> Response
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: Into<Bytes>,
        E: Into<BoxError>,
    {
        let mut response = Response::new(200);
        response.header("Content-Type", content_type);
        response.stream = Some(Box::pin(stream.map(|chunk| chunk.map(Into::into).map_err(Into::into))));
        response
    }

    /// Like [`Response::stream`] for a body whose length is known up front
    /// (a file, a proxied response). It is sent as `Content-Length`, so
    /// clients can show progress; the stream must yield exactly `length`
    /// bytes or the connection is aborted.
    pub fn stream_with_length<S, B, E>(content_type: &str, length: u64, stream: S) -> Response
    where
        S: Stream<Item = Result<B, E>> + Send + 'static,
        B: Into<Bytes>,
        E: Into<BoxError>,
    {
        let mut response = Response::stream(content_type, stream);
        response.stream_length = Some(length);
        response
    }

    /// True if the body is a stream rather than `body`.
    pub fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }


    // ---- Constructors ----

    /// A 200 `text/plain` response.
    pub fn text(content: impl Into<String>) -> Response {
        Response::new(200)
            .with_header("Content-Type", "text/plain; charset=utf-8")
            .with_body(content.into())
    }

    /// A 200 `text/html` response.
    pub fn html(content: impl Into<String>) -> Response {
        Response::new(200)
            .with_header("Content-Type", "text/html; charset=utf-8")
            .with_body(content.into())
    }

    /// A 200 `application/json` response. If `value` can't be serialized
    /// (rare: e.g. a map with non-string keys) the error is logged and a 500
    /// is returned. Handlers can also return [`Json`](crate::Json).
    pub fn json<T: Serialize + ?Sized>(value: &T) -> Response {
        match serde_json::to_vec(value) {
            Ok(json) => Response::new(200).with_header("Content-Type", "application/json").with_body(json),
            Err(error) => {
                tracing::error!(error = %error, "failed to serialize JSON response");
                Response::error(ServerError::InternalError("failed to serialize response".to_string()))
            }
        }
    }

    /// A 200 response with raw bytes, e.g. an image or a generated file.
    pub fn binary(content_type: &str, data: impl Into<Bytes>) -> Response {
        Response::new(200).with_header("Content-Type", content_type).with_body(data)
    }

    /// An empty 204 No Content response.
    pub fn no_content() -> Response {
        Response::new(204)
    }

    /// A 302 Found redirect.
    pub fn redirect(location: &str) -> Response {
        Response::redirect_with_status(302, location)
    }

    /// A 301 Moved Permanently redirect.
    pub fn permanent_redirect(location: &str) -> Response {
        Response::redirect_with_status(301, location)
    }

    /// A redirect with a specific status: 303 See Other after a form POST,
    /// or 307/308 to keep the request method.
    pub fn redirect_with_status(status: u16, location: &str) -> Response {
        Response::new(status).with_header("Location", location)
    }

    /// The response for an error: its status and a JSON body
    /// `{"error": {"message": ..., "status": ...}}`. Internal details of IO
    /// errors and panics are not included. Used when no
    /// [`on_error`](crate::Server::on_error) handler is set.
    pub fn error(error: ServerError) -> Response {
        let status = error.status_code();
        let body = match &error {
            ServerError::Invalid(fields) => serde_json::json!({
                "error": { "message": "Validation failed", "status": status, "fields": fields }
            }),
            _ => serde_json::json!({
                "error": { "message": error.public_message(), "status": status }
            }),
        };
        let mut response = Response::new(status)
            .with_header("Content-Type", "application/json")
            .with_body(body.to_string());
        match &error {
            ServerError::MethodNotAllowed(allow) => {
                response.header("Allow", allow);
            }
            ServerError::TooManyRequests { retry_after: Some(wait) } => {
                // Whole seconds, rounded up so clients don't retry too early.
                let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
                response.header("Retry-After", secs.max(1).to_string());
            }
            _ => {}
        }
        response
    }

    // ---- Builders (by value, for one-expression responses) ----

    /// Sets the status: `Response::text("made").with_status(201)`.
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    /// Sets a header, replacing existing values.
    pub fn with_header(mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.header(name, value);
        self
    }

    /// Sets the body.
    pub fn with_body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    // ---- Setters (by reference, for building step by step) ----

    /// Sets the status.
    pub fn status(&mut self, status: u16) -> &mut Self {
        self.status = status;
        self
    }

    /// Sets the body: accepts `String`, `&str`, `Vec<u8>`, `&[u8]`, ...
    pub fn body<T: Into<Vec<u8>>>(&mut self, body: T) -> &mut Self {
        self.body = Bytes::from(body.into());
        self
    }

    /// Sets the body from `Bytes` without copying, e.g. a cached or shared buffer.
    pub fn body_bytes(&mut self, body: Bytes) -> &mut Self {
        self.body = body;
        self
    }

    /// The body as text, replacing invalid UTF-8.
    pub fn body_text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    /// Sets a header, replacing any existing values (names are case-insensitive).
    /// Invalid names or values (e.g. containing CR/LF) are ignored and logged.
    pub fn header<K: AsRef<str>, V: AsRef<str>>(&mut self, name: K, value: V) -> &mut Self {
        if let Some((name, value)) = parse_header(name.as_ref(), value.as_ref()) {
            self.headers.insert(name, value);
        }
        self
    }

    /// Adds a header value, keeping existing ones (e.g. several `Set-Cookie`).
    pub fn append_header<K: AsRef<str>, V: AsRef<str>>(&mut self, name: K, value: V) -> &mut Self {
        if let Some((name, value)) = parse_header(name.as_ref(), value.as_ref()) {
            self.headers.append(name, value);
        }
        self
    }

    /// Sets several headers, e.g. from a `HashMap<String, String>`.
    pub fn headers<I, K, V>(&mut self, headers: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        for (name, value) in headers {
            self.header(name, value);
        }
        self
    }

    /// The first value of a header, if it is valid text.
    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// Removes a header, returning its first value.
    pub fn remove_header(&mut self, name: &str) -> Option<String> {
        let value = self.headers.remove(name)?;
        Some(String::from_utf8_lossy(value.as_bytes()).into_owned())
    }

    /// Adds `name` to the `Vary` header, keeping existing entries.
    pub fn append_vary(&mut self, name: &str) -> &mut Self {
        match self.get_header("vary") {
            Some(existing) if existing.split(',').any(|v| v.trim().eq_ignore_ascii_case(name) || v.trim() == "*") => self,
            Some(existing) => {
                let value = format!("{}, {}", existing, name);
                self.header("Vary", value)
            }
            None => self.header("Vary", name),
        }
    }

    /// Sets `Cache-Control`, e.g. `public, max-age=3600`.
    pub fn cache_control(&mut self, directive: &str) -> &mut Self {
        self.header("Cache-Control", directive)
    }

    /// Tells browsers and proxies not to store the response.
    pub fn no_cache(&mut self) -> &mut Self {
        self.cache_control("no-store")
    }

    /// Makes browsers download the body as a file named `filename`
    /// (`Content-Disposition: attachment`). Non-ASCII names are supported.
    pub fn attachment(&mut self, filename: &str) -> &mut Self {
        // An ASCII fallback, plus the exact name per RFC 6266 / RFC 5987.
        let fallback: String = filename
            .chars()
            .map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' })
            .collect();
        let encoded = urlencoding::encode(filename);
        self.header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"; filename*=UTF-8''{}", fallback, encoded),
        )
    }
}

/// Kept on an `http::Response` so a streamed response survives a round trip
/// through tower layers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StreamMeta {
    pub(crate) end_on_shutdown: bool,
    pub(crate) length: Option<u64>,
}

impl Response {
    /// Converts to an `http::Response`, deciding the body framing.
    pub(crate) fn into_http(self) -> http::Response<crate::body::ResponseBody> {
        use crate::body::ResponseBody;
        use http::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
        use http_body_util::StreamBody;

        let Response { status, body, mut headers, stream, end_on_shutdown, stream_length } = self;
        let status = http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
        let has_body = status_has_body(status.as_u16());
        // Framing is ours to decide.
        headers.remove(TRANSFER_ENCODING);
        headers.remove(CONTENT_LENGTH);
        let mut meta = None;
        let body = match stream {
            Some(stream) if has_body => {
                if let Some(length) = stream_length {
                    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
                }
                meta = Some(StreamMeta { end_on_shutdown, length: stream_length });
                let frames = stream.map(|chunk| chunk.map(http_body::Frame::data));
                ResponseBody::new(StreamBody::new(frames))
            }
            _ => {
                let body = if has_body { body } else { Bytes::new() };
                if has_body {
                    headers.insert(CONTENT_LENGTH, HeaderValue::from(body.len()));
                }
                ResponseBody::full(body)
            }
        };
        let mut out = http::Response::new(body);
        *out.status_mut() = status;
        *out.headers_mut() = headers;
        if let Some(meta) = meta {
            out.extensions_mut().insert(meta);
        }
        out
    }

    /// Converts from an `http::Response` with any body type.
    pub(crate) fn from_http<B>(response: http::Response<B>) -> Response
    where
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        use crate::body::{downcast, ResponseBody};
        use http::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
        use http_body_util::BodyExt;

        let (parts, body) = response.into_parts();
        let meta = parts.extensions.get::<StreamMeta>().copied();
        let mut out = Response::new(parts.status.as_u16());
        out.headers = parts.headers;
        out.headers.remove(CONTENT_LENGTH);
        out.headers.remove(TRANSFER_ENCODING);
        match downcast::<B, ResponseBody>(body) {
            Ok(ours) => match ours.into_parts() {
                Ok(bytes) => out.body = bytes,
                Err(stream) => {
                    out.stream = Some(Box::pin(stream.into_data_stream()));
                    out.stream_length = meta.and_then(|m| m.length);
                    out.end_on_shutdown = meta.is_some_and(|m| m.end_on_shutdown);
                }
            },
            Err(other) => {
                out.stream_length = other.size_hint().exact();
                out.stream = Some(Box::pin(other.into_data_stream().map(|chunk| chunk.map_err(Into::into))));
            }
        }
        out
    }
}
