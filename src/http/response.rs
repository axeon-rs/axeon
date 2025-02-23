use crate::error::ServerError;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: String,
    pub headers: HashMap<String, String>,
}

impl Response {
    pub fn new(status: u16) -> Response {
        Response {
            status,
            headers: HashMap::new(),
            body: String::new(),
        }
    }

    // Chainable status setter
    pub fn status(&mut self, status: u16) -> &mut Self {
        self.status = status;
        self
    }

    // Generic body setter
    pub fn body<T: AsRef<str>>(&mut self, body: T) -> &mut Self {
        self.body = body.as_ref().to_string();
        self
    }

    // Generic header setter
    pub fn header<K: AsRef<str>, V: AsRef<str>>(&mut self, name: K, value: V) -> &mut Self {
        self.headers.insert(name.as_ref().to_string(), value.as_ref().to_string());
        self
    }

    // Set multiple headers at once
    pub fn headers(&mut self, headers: HashMap<String, String>) -> &mut Self {
        self.headers.extend(headers);
        self
    }

    // Enhanced JSON response handling
    pub fn json<T: Serialize>(&mut self, value: &T) -> Result<&mut Self, ServerError> {
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
        let error_message = err.to_string();
        let mut response = Response::new(status);
        response.json(&serde_json::json!({
            "error": {
                "message": error_message,
                "status": status
            }
        })).expect("Error creating JSON response");
        response
    }

    // Helper method for streaming responses
    pub fn stream(&mut self, content_type: &str) -> &mut Self {
        self.header("Transfer-Encoding", "chunked")
            .header("Content-Type", content_type)
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
            println!("{}: {}", name, value);
        }
        println!("\r\n{}", self.body);
    }

    // New convenience methods
    pub fn text<T: AsRef<str>>(content: T) -> Response {
        let mut response = Response::new(200);
        response
            .header("Content-Type", "text/plain")
            .body(content);
        response
    }

    pub fn html<T: AsRef<str>>(content: T) -> Response {
        let mut response = Response::new(200);
        response
            .header("Content-Type", "text/html")
            .body(content);
        response
    }

    pub fn xml<T: AsRef<str>>(content: T) -> Response {
        let mut response = Response::new(200);
        response
            .header("Content-Type", "application/xml")
            .body(content);
        response
    }

    pub fn redirect(location: &str) -> Response {
        let mut response = Response::new(302);
        response.header("Location", location);
        response
    }

    pub fn permanent_redirect(location: &str) -> Response {
        let mut response = Response::new(301);
        response.header("Location", location);
        response
    }

    pub fn method_not_allowed(allowed_methods: &[&str]) -> Response {
        let mut response = Response::new(405);
        response
            .header("Allow", allowed_methods.join(", "))
            .json(&serde_json::json!({
                "error": {
                    "message": "Method not allowed",
                    "allowed_methods": allowed_methods
                }
            })).expect("Error creating JSON response");
        response
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
        let mut response = axeon::http::Response::new(200);
        response.json(&axeon::json!($($json)+)).expect("Error creating JSON response");
        Ok(response)
    }};
}

#[macro_export]
macro_rules! created_json {
   ($($json:tt)+) => {{
        let mut response =  axeon::http::Response::new(200);
        response.json(&axeon::json!($($json)+)).expect("Error creating JSON response");
        Ok(response)
    }};
}