use crate::plugins::Plugins;
use crate::http::form;
use serde_json::Value;
use crate::error::{ServerError, ServerResult};
use bytes::Bytes;
use http::{Extensions, HeaderMap};
use std::str::FromStr;
use std::collections::HashMap;
use std::net::SocketAddr;

#[derive(Eq, Hash, PartialEq, Copy, Clone, Debug)]
pub enum Method {
    GET,
    POST,
    PUT,
    DELETE,
    HEAD,
    CONNECT,
    OPTIONS,
    TRACE,
    PATCH,
}

impl Method {
    /// All supported methods, in the order used for `Allow` headers.
    pub const ALL: [Method; 9] = [
        Method::GET,
        Method::HEAD,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::CONNECT,
        Method::OPTIONS,
        Method::TRACE,
    ];

    /// Parses a method token (case-sensitive, per RFC 9110). Returns `None` for unknown methods.
    pub fn parse(s: &str) -> Option<Method> {
        match s {
            "GET" => Some(Method::GET),
            "POST" => Some(Method::POST),
            "PUT" => Some(Method::PUT),
            "DELETE" => Some(Method::DELETE),
            "HEAD" => Some(Method::HEAD),
            "CONNECT" => Some(Method::CONNECT),
            "OPTIONS" => Some(Method::OPTIONS),
            "TRACE" => Some(Method::TRACE),
            "PATCH" => Some(Method::PATCH),
            _ => None,
        }
    }

    #[deprecated(since = "0.3.0", note = "unknown methods silently became GET; use `Method::parse`")]
    pub fn from_string(s: &str) -> Method {
        Method::parse(s).unwrap_or(Method::GET)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Method::GET => "GET",
            Method::POST => "POST",
            Method::PUT => "PUT",
            Method::DELETE => "DELETE",
            Method::HEAD => "HEAD",
            Method::CONNECT => "CONNECT",
            Method::OPTIONS => "OPTIONS",
            Method::TRACE => "TRACE",
            Method::PATCH => "PATCH",
        }
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug)]
pub struct Body {
    pub(crate) content_type: String,
    pub(crate) data: Bytes,
}

impl Default for Body {
    fn default() -> Self {
        Self::new()
    }
}

impl Body {
    pub fn new() -> Body {
        Body {
            content_type: String::new(),
            data: Bytes::new(),
        }
    }

    pub fn from_string(s: &str) -> Body {
        Body {
            content_type: "text/plain".to_string(),
            data: Bytes::copy_from_slice(s.as_bytes()),
        }
    }

    pub fn from_bytes(b: Vec<u8>) -> Body {
        Body {
            content_type: "application/octet-stream".to_string(),
            data: Bytes::from(b),
        }
    }

    pub fn as_string(&self) -> String {
        String::from_utf8_lossy(&self.data).to_string()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// The body as `Bytes`; cloning it does not copy the data.
    pub fn bytes(&self) -> &Bytes {
        &self.data
    }

    /// The media type without parameters, e.g. `application/json` for
    /// `application/json; charset=utf-8`.
    pub(crate) fn mime_is(&self, expected: &str) -> bool {
        self.content_type
            .split(';')
            .next()
            .map(|essence| essence.trim().eq_ignore_ascii_case(expected))
            .unwrap_or(false)
    }

    pub fn json<T>(&self) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        if self.mime_is("application/json") {
            // Use from_slice instead of converting to string first
            serde_json::from_slice(&self.data).ok()
        } else {
            None
        }
    }

    pub fn x_www_form_urlencoded<T>(&self) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        if self.mime_is("application/x-www-form-urlencoded") {
            serde_json::from_value(form::parse_urlencoded(&self.data).ok()?).ok()
        } else {
            None
        }
    }

    pub fn form_data<T>(&self) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        if self.mime_is("multipart/form-data") {
            serde_json::from_value(form::parse_multipart(&self.content_type, &self.data).ok()?).ok()
        } else {
            None
        }
    }
}

impl From<Vec<u8>> for Body {
    fn from(b: Vec<u8>) -> Body {
        Body::from_bytes(b)
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: HashMap<String, String>,
    pub params: HashMap<String, String>,
    /// Request headers. Names are case-insensitive.
    pub headers: HeaderMap,
    pub data: HashMap<String, Value>,
    pub body: Body,
    pub plugins: Plugins,
    /// Address of the TCP peer. This is the proxy's address when behind a reverse proxy.
    pub remote_addr: Option<SocketAddr>,
    /// Typed per-request values, e.g. the current user set by an auth
    /// middleware. See [`Request::extension`] and [`Request::insert_extension`].
    pub extensions: Extensions,
}

impl Request {
    /// A request for in-process use with [`App::call`](crate::App::call),
    /// e.g. `Request::new(Method::GET, "/search?q=rust")`.
    pub fn new(method: Method, uri: &str) -> Request {
        let (path, query) = match uri.split_once('?') {
            Some((path, query)) => (path, parse_query(query)),
            None => (uri, HashMap::new()),
        };
        let path = path.trim_end_matches('/');
        Request {
            method,
            path: if path.is_empty() { "/".to_string() } else { path.to_string() },
            query,
            params: HashMap::new(),
            headers: HeaderMap::new(),
            data: HashMap::new(),
            body: Body { content_type: "none".to_string(), data: Bytes::new() },
            plugins: Plugins::default(),
            remote_addr: None,
            extensions: Extensions::new(),
        }
    }

    /// Adds a header. Panics if the name or value is invalid.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        let name = http::HeaderName::from_bytes(name.as_bytes()).expect("invalid header name");
        let value = http::HeaderValue::from_str(value).expect("invalid header value");
        self.headers.append(name, value);
        self
    }

    /// Sets the body and its `Content-Type`.
    pub fn with_body(mut self, content_type: &str, body: impl Into<Bytes>) -> Self {
        self.body = Body { content_type: content_type.to_string(), data: body.into() };
        self.with_header("content-type", content_type)
    }

    /// Sets a JSON body.
    pub fn with_json<T: serde::Serialize>(self, value: &T) -> Self {
        let json = serde_json::to_vec(value).expect("value serializes to JSON");
        self.with_body("application/json", json)
    }

    /// A path parameter parsed into `T`, e.g. `req.param::<u64>("id")` for `/users/:id`.
    /// Fails with 400 Bad Request if it is missing or doesn't parse.
    pub fn param<T: FromStr>(&self, name: &str) -> ServerResult<T> {
        let raw = self
            .params
            .get(name)
            .ok_or_else(|| ServerError::BadRequest(format!("missing path parameter `{}`", name)))?;
        raw.parse()
            .map_err(|_| ServerError::BadRequest(format!("invalid path parameter `{}`", name)))
    }

    /// The query string deserialized into `T`. Missing optional fields are
    /// `None`; values are parsed into numbers, booleans, etc.
    /// Fails with 400 Bad Request when it doesn't match `T`.
    pub fn query_as<T: serde::de::DeserializeOwned>(&self) -> ServerResult<T> {
        let encoded = serde_urlencoded::to_string(&self.query)
            .map_err(|e| ServerError::InternalError(e.to_string()))?;
        serde_urlencoded::from_str(&encoded)
            .map_err(|e| ServerError::BadRequest(format!("invalid query string: {}", e)))
    }

    /// The body deserialized from JSON. Fails with 415 if the content type
    /// isn't JSON, 400 for malformed JSON, and 422 when it doesn't match `T`.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> ServerResult<T> {
        if !self.body.mime_is("application/json") {
            return Err(ServerError::UnsupportedMediaType("expected application/json".to_string()));
        }
        serde_json::from_slice(self.body.as_bytes()).map_err(|e| {
            if e.is_data() {
                ServerError::ValidationError(e.to_string())
            } else {
                ServerError::BadRequest(format!("invalid JSON: {}", e))
            }
        })
    }

    /// The body deserialized from `application/x-www-form-urlencoded`.
    /// Fails with 415 for other content types and 422 when it doesn't match `T`.
    pub fn form<T: serde::de::DeserializeOwned>(&self) -> ServerResult<T> {
        if !self.body.mime_is("application/x-www-form-urlencoded") {
            return Err(ServerError::UnsupportedMediaType(
                "expected application/x-www-form-urlencoded".to_string(),
            ));
        }
        serde_urlencoded::from_bytes(self.body.as_bytes())
            .map_err(|e| ServerError::ValidationError(e.to_string()))
    }

    /// Application state registered with [`Server::plugins`](crate::Server::plugins).
    pub fn state<T: 'static>(&self) -> Option<&T> {
        self.plugins.get::<T>()
    }

    /// A typed value stored on this request by earlier middleware.
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.extensions.get::<T>()
    }

    /// Stores a typed value for later middleware and the handler, replacing
    /// any previous value of the same type.
    pub fn insert_extension<T: Clone + Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        self.extensions.insert(value)
    }

    /// The first value of a header (case-insensitive), if it is valid text.
    /// Use `headers.get_all(name)` for headers that may repeat.
    pub fn get_header(&self, key: &str) -> Option<&str> {
        self.headers.get(key)?.to_str().ok()
    }

    pub fn get_method(&self) -> &Method {
        &self.method
    }

    pub fn get_data(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }

    pub fn set_data<T>(&mut self, key: &str, value: T)
    where
        T: serde::Serialize,
    {
        if let Ok(value) = serde_json::to_value(value) {
            self.data.insert(key.to_string(), value);
        }
    }

    // New method to get typed data
    pub fn get_typed_data<T>(&self, key: &str) -> Option<T>
    where
        T: serde::de::DeserializeOwned,
    {
        self.data.get(key).and_then(|value| {
            // Use from_value instead of clone + from_value for better performance
            serde_json::from_value(value.to_owned()).ok()
        })
    }
}

/// Parses a query string, percent-decoding keys and values ('+' is a space).
pub(crate) fn parse_query(query: &str) -> HashMap<String, String> {
    fn decode(s: &str) -> String {
        let s = s.replace('+', " ");
        match urlencoding::decode(&s) {
            Ok(decoded) => decoded.into_owned(),
            Err(_) => s,
        }
    }
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (decode(key), decode(value)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

pub enum ParseError {
    InvalidRequest,
}

impl std::fmt::Debug for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "ParseError")
    }
}
