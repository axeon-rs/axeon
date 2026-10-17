use crate::plugins::State;
use crate::message::form;
use serde_json::Value;
use crate::error::{ServerError, ServerResult};
use bytes::Bytes;
use http::{Extensions, HeaderMap};
use std::str::FromStr;
use std::collections::HashMap;
use std::net::SocketAddr;

/// An HTTP request method.
#[derive(Eq, Hash, PartialEq, Copy, Clone, Debug)]
#[non_exhaustive]
pub enum Method {
    /// Retrieve a resource.
    GET,
    /// Submit data, e.g. create a resource.
    POST,
    /// Replace a resource.
    PUT,
    /// Delete a resource.
    DELETE,
    /// Like GET, without the body.
    HEAD,
    /// Open a tunnel (proxies).
    CONNECT,
    /// Ask which methods are allowed (CORS preflights).
    OPTIONS,
    /// Echo the request (diagnostics).
    TRACE,
    /// Partially update a resource.
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

    /// The method name, e.g. `"GET"`.
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

/// The path parameters of the matched route, e.g. `id` for `/users/:id`.
///
/// Values are percent-decoded. Index with a name (`params["id"]`, which
/// panics if absent) or use [`get`](Params::get).
#[derive(Debug, Clone, Default)]
pub struct Params {
    entries: Vec<(std::sync::Arc<str>, String)>,
}

impl Params {
    pub(crate) fn from_entries(entries: Vec<(std::sync::Arc<str>, String)>) -> Self {
        Self { entries }
    }

    /// The parameter's value.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries.iter().find(|(key, _)| &**key == name).map(|(_, value)| value.as_str())
    }

    /// All parameters as `(name, value)`, in path order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(key, value)| (&**key, value.as_str()))
    }

    /// The number of parameters.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if the route has no parameters.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl std::ops::Index<&str> for Params {
    type Output = String;

    fn index(&self, name: &str) -> &String {
        self.entries
            .iter()
            .find(|(key, _)| &**key == name)
            .map(|(_, value)| value)
            .unwrap_or_else(|| panic!("no path parameter `{}`", name))
    }
}

/// An incoming HTTP request.
///
/// Read it with accessors ([`method`](Request::method),
/// [`path`](Request::path), [`get_header`](Request::get_header),
/// [`body`](Request::body)) and typed helpers that fail with the right 4xx
/// error: [`param`](Request::param), [`query_as`](Request::query_as),
/// [`json`](Request::json), [`form`](Request::form).
#[derive(Debug)]
pub struct Request {
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) query: HashMap<String, String>,
    pub(crate) params: Params,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
    pub(crate) state: State,
    pub(crate) remote_addr: Option<SocketAddr>,
    pub(crate) extensions: Extensions,
    /// The connection this request arrived on (shutdown, upgrades).
    pub(crate) conn: Option<std::sync::Arc<crate::lifecycle::ConnContext>>,
    /// The URI as received, for rebuilding an `http::Request`.
    pub(crate) uri: Option<http::Uri>,
    /// The body size limit that applied.
    pub(crate) body_limit: usize,
    /// The matched route pattern, set by routing.
    pub(crate) matched: Option<std::sync::Arc<str>>,
}

impl Request {
    /// A request for in-process use with [`App::call`](crate::App::call),
    /// e.g. `Request::new(Method::GET, "/search?q=rust")`.
    pub fn new(method: Method, uri: &str) -> Request {
        let (path, query) = match uri.split_once('?') {
            Some((path, query)) => (path, parse_query(query)),
            None => (uri, HashMap::new()),
        };
        Request {
            method,
            path: crate::message::convert::normalize_path(path),
            query,
            params: Params::default(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            state: State::default(),
            remote_addr: None,
            extensions: Extensions::new(),
            conn: None,
            uri: None,
            body_limit: usize::MAX,
            matched: None,
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
        self.body = body.into();
        self.headers.remove(http::header::CONTENT_TYPE);
        self.with_header("content-type", content_type)
    }

    /// Sets a JSON body.
    pub fn with_json<T: serde::Serialize>(self, value: &T) -> Self {
        let json = serde_json::to_vec(value).expect("value serializes to JSON");
        self.with_body("application/json", json)
    }

    /// The request method.
    pub fn method(&self) -> Method {
        self.method
    }

    /// The path, percent-encoded as received, without a trailing slash
    /// (`/users/42`).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The pattern of the route handling this request, e.g. `/users/:id`.
    /// `None` before routing (in global middleware) and for unmatched
    /// requests.
    pub fn matched_path(&self) -> Option<&str> {
        self.matched.as_deref()
    }

    /// The route's path parameters, percent-decoded: for `/users/:id`,
    /// `req.params()["id"]`. See [`param`](Request::param) for typed access.
    pub fn params(&self) -> &Params {
        &self.params
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

    /// The query string parameters, percent-decoded. For repeated keys the
    /// last value wins.
    pub fn query(&self) -> &HashMap<String, String> {
        &self.query
    }

    /// One query string parameter: `req.query_param("page")` for `?page=2`.
    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
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

    /// The request headers. Names are case-insensitive.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The headers, for middleware that adds or rewrites them.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    /// The first value of a header (case-insensitive), if it is valid text.
    /// Use `headers().get_all(name)` for headers that may repeat.
    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// The `Content-Type` header.
    pub fn content_type(&self) -> Option<&str> {
        self.get_header("content-type")
    }

    /// True if the media type (without parameters such as `charset`) is
    /// `expected`, e.g. `application/json`.
    pub(crate) fn mime_is(&self, expected: &str) -> bool {
        self.content_type()
            .and_then(|ct| ct.split(';').next())
            .is_some_and(|essence| essence.trim().eq_ignore_ascii_case(expected))
    }

    /// The buffered request body. Empty on
    /// [`stream_body`](crate::RouteBuilder::stream_body) routes; use
    /// [`body_stream`](Request::body_stream) there.
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// The body as UTF-8 text. Fails with 400 if it isn't valid UTF-8.
    pub fn text(&self) -> ServerResult<&str> {
        std::str::from_utf8(&self.body).map_err(|_| ServerError::BadRequest("body is not valid UTF-8".to_string()))
    }

    /// The body deserialized from JSON. Fails with 415 if the content type
    /// isn't JSON, 400 for malformed JSON, and 422 when it doesn't match `T`.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> ServerResult<T> {
        if !self.mime_is("application/json") {
            return Err(ServerError::UnsupportedMediaType("expected application/json".to_string()));
        }
        serde_json::from_slice(&self.body).map_err(|e| {
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
        if !self.mime_is("application/x-www-form-urlencoded") {
            return Err(ServerError::UnsupportedMediaType(
                "expected application/x-www-form-urlencoded".to_string(),
            ));
        }
        serde_urlencoded::from_bytes(&self.body).map_err(|e| ServerError::ValidationError(e.to_string()))
    }

    /// An `application/x-www-form-urlencoded` body with bracket keys
    /// (`user[name]=Ada&tags[]=a&tags[]=b`) as nested JSON, values as
    /// strings. Nesting is limited to 32 levels, and array indexes must be
    /// contiguous. Fails with 415 for other content types and 400 for
    /// malformed or too deeply nested input.
    pub fn form_value(&self) -> ServerResult<Value> {
        if !self.mime_is("application/x-www-form-urlencoded") {
            return Err(ServerError::UnsupportedMediaType(
                "expected application/x-www-form-urlencoded".to_string(),
            ));
        }
        form::parse_urlencoded(&self.body).map_err(|e| ServerError::BadRequest(format!("invalid form body: {:?}", e)))
    }

    /// The request body as a stream of chunks, for reading large uploads
    /// without buffering them. On routes configured with
    /// [`stream_body`](crate::RouteBuilder::stream_body) the data
    /// arrives as the client sends it and can be taken only once; elsewhere
    /// this streams the already-buffered body.
    pub fn body_stream(&self) -> ServerResult<crate::body::RequestBody> {
        match self.extensions.get::<crate::message::convert::PendingBody>() {
            Some(pending) => pending
                .take()
                .ok_or_else(|| ServerError::InternalError("request body already taken".to_string())),
            None => Ok(crate::body::RequestBody::from(self.body.clone())),
        }
    }

    /// The address of the TCP peer. Behind a reverse proxy this is the
    /// proxy's address. `None` for in-process and tower-service requests.
    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr
    }

    /// Application state registered with [`Server::state`](crate::Server::state).
    pub fn state<T: 'static>(&self) -> Option<&T> {
        self.state.get::<T>()
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

    /// All typed per-request values.
    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// All typed per-request values, mutably.
    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        /// Arbitrary query strings and URIs never panic.
        #[test]
        fn query_and_uri_parsing_never_panic(input in ".{0,128}") {
            let _ = parse_query(&input);
            let request = Request::new(Method::GET, &format!("/{}", input));
            let _ = request.query_as::<HashMap<String, String>>();
        }
    }
}
