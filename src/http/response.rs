use crate::error::ServerError;
use bytes::Bytes;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    /// Raw body bytes. Binary content (images, compressed data) is sent unchanged.
    /// Cloning `Bytes` is cheap (reference counted).
    pub body: Bytes,
    /// Response headers. Names are case-insensitive.
    pub headers: HeaderMap,
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
            eprintln!("Ignoring invalid response header {:?}", name);
            None
        }
    }
}

impl Response {
    pub fn new(status: u16) -> Response {
        Response {
            status,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    // Chainable status setter
    pub fn status(&mut self, status: u16) -> &mut Self {
        self.status = status;
        self
    }

    // Generic body setter: accepts `String`, `&str`, `Vec<u8>`, `&[u8]`, ...
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
    /// Invalid names or values (e.g. containing CR/LF) are ignored.
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

    // Set multiple headers at once, e.g. from a `HashMap<String, String>`
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

    // Enhanced JSON response handling
    pub(crate) fn json<T: Serialize>(&mut self, value: &T) -> Result<&mut Self, ServerError> {
        let json_string = serde_json::to_string(value)
            .map_err(|e| ServerError::InternalError(format!("JSON serialization error: {}", e)))?;
        self.header("Content-Type", "application/json");
        self.body(json_string);
        Ok(self)
    }

    // Static constructors for common responses
    pub fn ok<T: Serialize>(data: &T) -> Result<Response, ServerError> {
        let mut response = Response::new(200);
        response.json(data)?;
        Ok(response)
    }

    pub fn created<T: Serialize>(data: &T) -> Result<Response, ServerError> {
        let mut response = Response::new(201);
        response.json(data)?;
        Ok(response)
    }

    pub fn accepted<T: Serialize>(data: &T) -> Result<Response, ServerError> {
        let mut response = Response::new(202);
        response.json(data)?;
        Ok(response)
    }

    pub fn no_content() -> Response {
        Response::new(204)
    }

    pub fn bad_request<T: Serialize>(data: &T) -> Result<Response, ServerError> {
        let mut response = Response::new(400);
        response.json(data)?;
        Ok(response)
    }

    pub fn unauthorized<T: Serialize>(data: &T) -> Result<Response, ServerError> {
        let mut response = Response::new(401);
        response.json(data)?;
        Ok(response)
    }

    pub fn forbidden<T: Serialize>(data: &T) -> Result<Response, ServerError> {
        let mut response = Response::new(403);
        response.json(data)?;
        Ok(response)
    }

    pub fn not_found<T: Serialize>(data: &T) -> Result<Response, ServerError> {
        let mut response = Response::new(404);
        response.json(data)?;
        Ok(response)
    }

    // Enhanced error response
    pub fn error(err: ServerError) -> Response {
        let status = err.status_code();
        let error_message = err.public_message();
        let mut response = Response::new(status);
        if let ServerError::MethodNotAllowed(allow) = &err {
            response.header("Allow", allow);
        }
        response.json(&serde_json::json!({
            "error": {
                "message": error_message,
                "status": status
            }
        })).expect("Error creating JSON response");
        response
    }

    /// Not supported yet: the server always sends a `Content-Length` body, and
    /// any `Transfer-Encoding` header is dropped. Only sets the content type.
    #[deprecated(since = "0.3.0", note = "streaming responses are not supported yet; this only sets Content-Type")]
    pub fn stream(&mut self, content_type: &str) -> &mut Self {
        self.header("Content-Type", content_type)
    }

    // Helper for CORS headers
    pub fn with_cors(&mut self, origin: &str) -> &mut Self {
        self.header("Access-Control-Allow-Origin", origin)
            .header("Access-Control-Allow-Methods", "GET, POST, PUT, DELETE, OPTIONS")
            .header("Access-Control-Allow-Headers", "Content-Type, Authorization")
    }

    pub fn send(&self) {
        println!("HTTP/1.1 {} OK", self.status);
        for (name, value) in &self.headers {
            println!("{}: {}", name, String::from_utf8_lossy(value.as_bytes()));
        }
        println!("\r\n{}", self.body_text());
    }

    // New convenience methods
    pub fn text<T: AsRef<str>>(content: T) -> Result<Response, ServerError> {
        let mut response = Response::new(200);
        response
            .header("Content-Type", "text/plain")
            .body(content.as_ref());
        Ok(response)
    }

    pub fn html<T: AsRef<str>>(content: T) -> Result<Response, ServerError> {
        let mut response = Response::new(200);
        response
            .header("Content-Type", "text/html")
            .body(content.as_ref());
        Ok(response)
    }

    pub fn xml<T: AsRef<str>>(content: T) -> Result<Response, ServerError> {
        let mut response = Response::new(200);
        response
            .header("Content-Type", "application/xml")
            .body(content.as_ref());
        Ok(response)
    }

    /// A 200 response with raw bytes, e.g. an image or a generated file.
    pub fn binary<T: Into<Vec<u8>>>(content_type: &str, data: T) -> Result<Response, ServerError> {
        let mut response = Response::new(200);
        response.header("Content-Type", content_type).body(data);
        Ok(response)
    }

    pub fn redirect(location: &str) -> Result<Response, ServerError> {
        let mut response = Response::new(302);
        response.header("Location", location);
        Ok(response)
    }

    pub fn permanent_redirect(location: &str) -> Result<Response, ServerError> {
        let mut response = Response::new(301);
        response.header("Location", location);
        Ok(response)
    }

    pub fn method_not_allowed(allowed_methods: &[&str]) -> Result<Response, ServerError> {
        let mut response = Response::new(405);
        response
            .header("Allow", allowed_methods.join(", "))
            .json(&serde_json::json!({
                "error": {
                    "message": "Method not allowed",
                    "allowed_methods": allowed_methods
                }
            })).expect("Error creating JSON response");
        Ok(response)
    }

    pub fn with_cache_control(&mut self, directive: &str) -> &mut Self {
        self.header("Cache-Control", directive)
    }

    pub fn no_cache(&mut self) -> &mut Self {
        self.with_cache_control("no-cache, no-store, must-revalidate")
            .header("Pragma", "no-cache")
            .header("Expires", "0")
    }

    // Security headers
    pub fn with_security_headers(&mut self) -> &mut Self {
        self.header("X-Content-Type-Options", "nosniff")
            .header("X-Frame-Options", "DENY")
            .header("X-XSS-Protection", "1; mode=block")
            .header("Strict-Transport-Security", "max-age=31536000; includeSubDomains")
            .header("Referrer-Policy", "strict-origin-when-cross-origin")
    }

    pub fn conflict<T: Serialize>(data: &T) -> Result<Response, ServerError> {
        let mut response = Response::new(409);
        response.json(data)?;
        Ok(response)
    }

    pub fn unprocessable_entity<T: Serialize>(data: &T) -> Result<Response, ServerError> {
        let mut response = Response::new(422);
        response.json(data)?;
        Ok(response)
    }

    pub fn too_many_requests<T: Serialize>(data: &T) -> Result<Response, ServerError> {
        let mut response = Response::new(429);
        response.json(data)?;
        Ok(response)
    }

    pub fn service_unavailable<T: Serialize>(data: &T) -> Result<Response, ServerError> {
        let mut response = Response::new(503);
        response.json(data)?;
        Ok(response)
    }

    pub fn file_download(&mut self, filename: &str, content_type: &str) -> &mut Self {
        self.header("Content-Type", content_type)
            .header("Content-Disposition", &format!("attachment; filename=\"{}\"", filename))
    }

    pub fn vary(&mut self, headers: &[&str]) -> &mut Self {
        self.header("Vary", headers.join(", "))
    }

    pub fn with_gzip(&mut self) -> &mut Self {
        self.header("Content-Encoding", "gzip")
            .vary(&["Accept-Encoding"])
    }

    pub fn with_brotli(&mut self) -> &mut Self {
        self.header("Content-Encoding", "br")
            .vary(&["Accept-Encoding"])
    }

    pub fn with_language(&mut self, lang: &str) -> &mut Self {
        self.header("Content-Language", lang)
            .vary(&["Accept-Language"])
    }

    pub fn with_api_version(&mut self, version: &str) -> &mut Self {
        self.header("X-API-Version", version)
    }
}

#[macro_export]
macro_rules! ok_json {
    ($($json:tt)+) => {{
        Response::ok(&axeon::json!($($json)+))
    }};
}

#[macro_export]
macro_rules! created_json {
   ($($json:tt)+) => {{
         Response::created(&axeon::json!($($json)+))
    }};
}