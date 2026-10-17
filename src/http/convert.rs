//! Conversions between Axeon requests and `http::Request`, so requests can
//! pass through tower layers and arrive from tower services.

use crate::body::{body_error, downcast, BoxError, RequestBody};
use crate::error::{ServerError, ServerResult};
use crate::http::request::parse_query;
use crate::http::{Body, Method, Request};
use crate::plugins::Plugins;
use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// A streamed request body waiting to be taken by the handler.
#[derive(Clone)]
pub(crate) struct PendingBody(Arc<Mutex<Option<RequestBody>>>);

impl PendingBody {
    pub(crate) fn new(body: RequestBody) -> Self {
        Self(Arc::new(Mutex::new(Some(body))))
    }

    pub(crate) fn take(&self) -> Option<RequestBody> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// The request URI as received, for rebuilding an `http::Request`.
#[derive(Clone)]
pub(crate) struct OriginalUri(pub(crate) http::Uri);

/// The body size limit that applied to this request.
#[derive(Clone, Copy)]
pub(crate) struct BodyLimit(pub(crate) usize);

/// Request fields with no place in `http::Request`, carried through tower.
#[derive(Clone)]
struct Carry {
    params: HashMap<String, String>,
    data: HashMap<String, Value>,
    plugins: Plugins,
    remote_addr: Option<SocketAddr>,
    streaming: bool,
}

/// Normalizes a request path: no trailing slash, `/` for the root.
pub(crate) fn normalize_path(path: &str) -> String {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

impl Request {
    pub(crate) fn into_http(mut self) -> http::Request<RequestBody> {
        let pending = self.extensions.remove::<PendingBody>().and_then(|p| p.take());
        let uri = match self.extensions.remove::<OriginalUri>() {
            Some(OriginalUri(uri)) if normalize_path(uri.path()) == self.path => uri,
            _ => {
                let query = serde_urlencoded::to_string(&self.query).unwrap_or_default();
                let target = if query.is_empty() { self.path.clone() } else { format!("{}?{}", self.path, query) };
                target.parse().unwrap_or_else(|_| http::Uri::from_static("/"))
            }
        };
        let streaming = pending.is_some();
        let body = pending.unwrap_or_else(|| RequestBody::from(self.body.data.clone()));
        self.extensions.insert(Carry {
            params: self.params,
            data: self.data,
            plugins: self.plugins,
            remote_addr: self.remote_addr,
            streaming,
        });
        let mut request = http::Request::new(body);
        *request.method_mut() = http::Method::from_bytes(self.method.as_str().as_bytes()).expect("valid method");
        *request.uri_mut() = uri;
        *request.headers_mut() = self.headers;
        *request.extensions_mut() = self.extensions;
        request
    }

    pub(crate) async fn from_http<B>(request: http::Request<B>) -> ServerResult<Request>
    where
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        let (mut parts, body) = request.into_parts();
        let method = Method::parse(parts.method.as_str())
            .ok_or_else(|| ServerError::NotImplemented(format!("method {}", parts.method)))?;
        let carry = parts.extensions.remove::<Carry>();
        let content_type = parts
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("none")
            .to_string();

        let streaming = carry.as_ref().is_some_and(|c| c.streaming);
        let data = if streaming {
            // Our own stream if no layer replaced it; otherwise wrap the new body.
            let body = match downcast::<B, RequestBody>(body) {
                Ok(ours) => ours,
                Err(replaced) => RequestBody::new(replaced),
            };
            parts.extensions.insert(PendingBody::new(body));
            Bytes::new()
        } else {
            match downcast::<B, RequestBody>(body) {
                Ok(ours) => ours.bytes().await?,
                Err(other) => {
                    // A layer replaced the body (e.g. decompression): buffer it again.
                    let limit = parts.extensions.get::<BodyLimit>().map_or(usize::MAX, |l| l.0);
                    Limited::new(other, limit).collect().await.map_err(body_error)?.to_bytes()
                }
            }
        };

        let (params, request_data, plugins, remote_addr) = match carry {
            Some(c) => (c.params, c.data, c.plugins, c.remote_addr),
            None => (HashMap::new(), HashMap::new(), Plugins::default(), None),
        };
        Ok(Request {
            method,
            path: normalize_path(parts.uri.path()),
            query: parts.uri.query().map(parse_query).unwrap_or_default(),
            params,
            headers: parts.headers,
            data: request_data,
            body: Body { content_type, data },
            plugins,
            remote_addr,
            extensions: parts.extensions,
        })
    }
}
