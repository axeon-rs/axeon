//! Application is the main entry point for the Axeon framework.
//!
//! This module provides the core Application struct that serves as the main
//! entry point for building web applications with Axeon.
//!
//! # Examples
//!
//! ```rust
//! use axeon::app::Application;
//! use axeon::ok_json;
//!
//! let mut app = Application::new();
//! app.get("/", |_req| async {
//!     ok_json!({ "message": "Hello!" })
//! });
//! ```

use crate::error::ServerError;
use crate::handler::{HttpResponse, IntoResponse};
use crate::http::{Body, Method, Request};
use crate::http::Response;
use crate::middleware::Middleware;
use crate::plugins::Plugins;
use crate::router::{Route, Router};
use futures::{FutureExt};
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use std::fs;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener};
use tokio::runtime::Runtime;
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use std::fs::File;
use std::io::BufReader as StdBufReader;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

type ErrorHandler = Arc<dyn Fn(ServerError) -> Response + Send + Sync>;

/// TLS configuration for HTTPS support
pub struct TlsConfig {
    cert_file: PathBuf,
    key_file: PathBuf,
}

impl TlsConfig {
    pub fn new<P: AsRef<Path>>(cert_file: P, key_file: P) -> Self {
        Self {
            cert_file: cert_file.as_ref().to_path_buf(),
            key_file: key_file.as_ref().to_path_buf(),
        }
    }

    fn load_certs(&self) -> Result<Vec<CertificateDer<'static>>, Box<dyn std::error::Error>> {
        let cert_file = File::open(&self.cert_file)?;
        let mut reader = StdBufReader::new(cert_file);
        let certs = rustls_pemfile::certs(&mut reader)
            .filter_map(|result| result.ok())
            .collect();
        Ok(certs)
    }

    fn load_key(&self) -> Result<PrivateKeyDer<'static>, Box<dyn std::error::Error>> {
        let key_file = File::open(&self.key_file)?;
        let mut reader = StdBufReader::new(key_file);
        let key = rustls_pemfile::private_key(&mut reader)?
            .ok_or_else(|| "No private key found")?;
        Ok(key)
    }
}

/// The main application struct that represents your web server.
///
/// # Example
///
/// ```rust
/// use axeon::app::Application;
/// use axeon::ok_json;
/// use axeon::http::Response;
///
/// let mut app = Application::new();
///
/// // Add a route
/// app.get("/", |_req| async {
///     ok_json!({ "message": "Hello" })
/// });
///
/// // Start the server
/// app.listen("127.0.0.1:3000").unwrap();
/// ```
///
///

#[derive(Clone)]
pub struct Application {
    pub max_connections: usize,
    pub keep_alive: Duration,
    router: Router,
    static_dir: Option<PathBuf>,
    plugins: Plugins,
    on_error: Option<ErrorHandler>,
    tls_config: Option<Arc<TlsConfig>>,
}

impl Application {
    /// Creates a new Application instance
    pub fn new() -> Self {
        Self {
            max_connections: 256,
            keep_alive: Duration::from_secs(5),
            router: Router::new(),
            static_dir: None,
            plugins: Plugins::new(),
            on_error: None,
            tls_config: None,
        }
    }

    pub fn max_connections(&mut self, max_connections: usize) -> &mut Self {
        self.max_connections = max_connections;
        self
    }

    pub fn keep_alive(&mut self, keep_alive: Duration) -> &mut Self {
        self.keep_alive = keep_alive;
        self
    }

    pub fn plugins<T>(&mut self, plugin: T) -> &mut Self
    where
        T: Send + Sync + 'static,
    {
        self.plugins.insert(plugin);
        self
    }

    pub fn on_error<F>(&mut self, handler: F) -> &mut Self
    where
        F: Fn(ServerError) -> Response + Send + Sync + 'static,
    {
        self.on_error = Some(Arc::new(handler));
        self
    }

    /// Registers a GET route handler
    ///
    /// # Arguments
    /// * `path` - The URL path to match
    /// * `handler` - The async handler function
    pub fn get<F, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.router.get(path, handler);
    }

    /// Registers a POST route handler
    ///
    /// # Arguments
    /// * `path` - The URL path to match
    /// * `handler` - The async handler function
    pub fn post<F, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.router.post(path, handler);
    }

    /// Registers a PUT route handler
    ///
    /// # Arguments
    /// * `path` - The URL path to match
    /// * `handler` - The async handler function
    pub fn put<F, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.router.put(path, handler);
    }

    /// Registers a PATCH route handler
    ///
    /// # Arguments
    /// * `path` - The URL path to match
    /// * `handler` - The async handler function
    pub fn patch<F, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.router.patch(path, handler);
    }

    /// Registers a DELETE route handler
    ///
    /// # Arguments
    /// * `path` - The URL path to match
    /// * `handler` - The async handler function
    pub fn delete<F, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.router.delete(path, handler);
    }

    /// Registers a HEAD route handler
    ///
    /// # Arguments
    /// * `path` - The URL path to match
    /// * `handler` - The async handler function
    pub fn head<F, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.router.head(path, handler);
    }

    /// Registers a CONNECT route handler
    ///
    /// # Arguments
    /// * `path` - The URL path to match
    /// * `handler` - The async handler function
    pub fn connect<F, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.router.connect(path, handler);
    }

    /// Registers an OPTIONS route handler
    ///
    /// # Arguments
    /// * `path` - The URL path to match
    /// * `handler` - The async handler function
    pub fn options<F, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.router.options(path, handler);
    }

    /// Registers a TRACE route handler
    ///
    /// # Arguments
    /// * `path` - The URL path to match
    /// * `handler` - The async handler function
    pub fn trace<F, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.router.trace(path, handler);
    }

    /// Adds a middleware to the application
    ///
    /// # Arguments
    /// * `middleware` - The middleware to add
    pub fn middleware(&mut self, middleware: impl Middleware + 'static) {
        self.router.middleware(middleware);
    }

    /// Mounts a router at a specific path
    ///
    /// # Arguments
    /// * `path` - The URL path to mount the router
    /// * `router` - The router to mount
    pub fn mount(&mut self, path: &str, router: Router) {
        self.router.mount(path, router);
    }

    /// Configure TLS for HTTPS support
    pub fn with_tls<P: AsRef<Path>>(&mut self, cert_file: P, key_file: P) -> &mut Self {
        self.tls_config = Some(Arc::new(TlsConfig::new(cert_file, key_file)));
        self
    }

    /// Starts the HTTP server
    ///
    /// # Arguments
    /// * `addr` - Address to listen on (e.g. "127.0.0.1:3000")
    pub fn listen(self, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        let runtime = Runtime::new()?;
        runtime.block_on(async {
            let listener = TcpListener::bind(addr).await?;
            let connection_counter = Arc::new(AtomicUsize::new(0));

            println!("Server running on {}", if self.tls_config.is_some() {
                format!("https://{}", addr)
            } else {
                format!("http://{}", addr)
            });

            let tls_acceptor = if let Some(tls_config) = &self.tls_config {
                let certs = tls_config.load_certs()?;
                let key = tls_config.load_key()?;
                let config = ServerConfig::builder()
                    .with_no_client_auth()
                    .with_single_cert(certs, key)?;
                Some(TlsAcceptor::from(Arc::new(config)))
            } else {
                None
            };

            loop {
                let counter = Arc::clone(&connection_counter);
                if counter.load(Ordering::Relaxed) >= self.max_connections {
                    eprintln!("Max connections reached");
                    continue;
                }

                match listener.accept().await {
                    Ok((stream, _)) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                        let app = self.clone();
                        let counter = Arc::clone(&counter);
                        let acceptor = tls_acceptor.clone();

                        tokio::spawn(async move {
                            let result = if let Some(acceptor) = acceptor {
                                match acceptor.accept(stream).await {
                                    Ok(tls_stream) => app.handle_connection(tls_stream).await,
                                    Err(e) => {
                                        eprintln!("TLS handshake failed: {}", e);
                                        Ok(())
                                    }
                                }
                            } else {
                                app.handle_connection(stream).await
                            };

                            if let Err(e) = result {
                                eprintln!("Connection error: {}", e);
                            }
                            counter.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Err(e) => eprintln!("Connection failed: {}", e),
                }
            }
        })
    }

    async fn handle_connection<S>(&self, mut stream: S) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut buf_reader = BufReader::new(&mut stream);
        let mut request_line = String::new();
        buf_reader.read_line(&mut request_line).await?;

        if request_line.is_empty() {
            return Ok(());
        }

        // Parse the request line
        let mut parts = request_line.trim().split_whitespace();
        let method = parts
            .next()
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "Invalid request line"))?
            .to_string();

        let full_path = parts
            .next()
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "Invalid request line"))?;

        // Split path and query
        let mut path_parts = full_path.split('?');
        let path = path_parts.next().unwrap_or("/").to_string();
        let path = path.trim_end_matches('/').to_string();
        let path = if path.is_empty() { "/".to_string() } else { path };
        let query = path_parts
            .next()
            .map(|query| Self::parse_query(query))
            .unwrap_or_default();

        // Parse headers efficiently
        let mut headers = HashMap::new();
        loop {
            let mut line = String::new();
            buf_reader.read_line(&mut line).await?;

            if line.trim().is_empty() {
                break;
            }

            if let Some((key, value)) = line.trim().split_once(':') {
                headers.insert(key.trim().to_lowercase(), value.trim().to_string());
            }
        }

        // Read body if Content-Length is present
        let mut body = Vec::new();
        let mut content_type = "none".to_owned();
        if headers.contains_key("content-type") {
            content_type = headers["content-type"].clone();
        }
        if let Some(content_length) = headers.get("content-length") {
            if let Ok(length) = content_length.parse::<usize>() {
                body.reserve(length);
                let mut take = buf_reader.take(length as u64);
                take.read_to_end(&mut body).await?;
            }
        }

        let request = Request {
            method: Method::from_string(&method),
            path,
            query,
            headers,
            body: Body {
                content_type: content_type.to_string(),
                data: body,
            },
            params: HashMap::new(),
            data: HashMap::new(),
            plugins: self.plugins.clone(),
        };

        let response = AssertUnwindSafe(self.handle(request)).catch_unwind().await;
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                let panic_msg = if let Some(msg) = err.downcast_ref::<&str>() {
                    msg.to_string()
                } else if let Some(msg) = err.downcast_ref::<String>() {
                    msg.clone()
                } else {
                    "Unknown panic".to_string()
                };
                Err(ServerError::PanicError(panic_msg))
            },
        };
        let response = match response {
            Ok(response) => response,
            Err(err) => self.handle_error(err),
        };
        let mut response_line = format!("HTTP/1.1 {}\r\n", response.status);
        response.headers.iter().for_each(|(name, value)| {
            response_line += &format!("{}: {}\r\n", name, value);
        });

        let contents = &response.body;
        let length = contents.len();
        response_line += &format!("Content-Length: {}\r\n\r\n{}", length, contents);
        stream.write_all(response_line.as_bytes()).await?;
        Ok(())
    }

    /// Sets the directory for serving static files
    ///
    /// # Arguments
    /// * `dir` - Path to the static files directory
    ///
    /// # Example
    /// ```rust
    /// app.static_dir("public");
    /// ```
    pub fn static_dir(&mut self, dir: &str) -> &mut Self {
        self.static_dir = Some(PathBuf::from(dir));
        self
    }

    async fn handle(&self, mut req: Request) -> HttpResponse {
        let path = req.path.clone();
        let method = req.method.clone();
        if let Some(routes) = self.router.routes.get(&path) {
            if let Some(route) = routes.get(&method) {
                return route.handle(req).await;
            } else {
                if method == Method::HEAD {
                    if let Some(route) = routes.get(&Method::GET) {
                        return Self::handle_head(route.clone(), req).await;
                    }
                }
                if method == Method::OPTIONS {
                    if let Some(route) = routes.get(&Method::GET) {
                        return Self::handle_options(route.clone(), req).await;
                    }
                }
            }
        }

        for dynamic_path in &self.router.dynamic_routes {
            if let Some(params) = self.match_dynamic_path(dynamic_path, &path) {
                if let Some(routes) = self.router.routes.get(dynamic_path) {
                    let method = req.method.clone();
                    if let Some(route) = routes.get(&method) {
                        req.params = params;
                        return route.handle(req).await;
                    } else {
                        if method == Method::HEAD {
                            if let Some(route) = routes.get(&Method::GET) {
                                return Self::handle_head(route.clone(), req).await;
                            }
                        }
                        if method == Method::OPTIONS {
                            if let Some(route) = routes.get(&Method::GET) {
                                req.params = params;
                                return Self::handle_options(route.clone(), req).await;
                            }
                        }
                    }
                }
            }
        }
        if let Some(response) = self.handle_static_file(&req.path) {
            Ok(response)
        } else {
            Err(ServerError::NotFound)
        }
    }

    async fn handle_head(route: Route, req: Request) -> HttpResponse {
        let mut req = req;
        req.method = Method::GET;
        let response = route.handle(req).await;
        match response {
            Ok(mut response) => {
                response.body = "".to_string();
                Ok(response)
            }
            Err(err) => Err(err),
        }
    }

    async fn handle_options(route: Route, req: Request) -> HttpResponse {
        let route = Route {
            middlewares: route.middlewares.clone(),
            handler: Box::new(|_| async { Ok(Response::new(200)) }),
        };
        route.handle(req).await
    }

    fn handle_error(&self, error: ServerError) -> Response {
        if let Some(handler) = &self.on_error {
            handler(error)
        } else {
            Response::error(error)
        }
    }

    fn handle_static_file(&self, path: &str) -> Option<Response> {
        if let Some(static_dir) = &self.static_dir {
            let file_path = static_dir.join(path.trim_start_matches('/'));
            if let Ok(canonical_path) = fs::canonicalize(&file_path) {
                if canonical_path.starts_with(fs::canonicalize(static_dir).ok()?)
                    && canonical_path.is_file()
                {
                    return self.serve_file(&canonical_path);
                }
            }
        }
        None
    }

    fn serve_file(&self, path: &Path) -> Option<Response> {
        if let Ok(contents) = fs::read(path) {
            let mut response = Response::new(200);

            // Set content type based on file extension
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let content_type = match ext {
                    "html" => "text/html",
                    "css" => "text/css",
                    "js" => "text/javascript",
                    "png" => "image/png",
                    "jpg" | "jpeg" => "image/jpeg",
                    "gif" => "image/gif",
                    "svg" => "image/svg+xml",
                    "ico" => "image/x-icon",
                    _ => "application/octet-stream",
                };
                response.header("Content-Type", content_type);
            }

            // Set cache control headers
            response.header("Cache-Control", "public, max-age=31536000");

            // Set Last-Modified
            if let Ok(metadata) = fs::metadata(path) {
                if let Ok(modified) = metadata.modified() {
                    if let Ok(duration) = modified.duration_since(SystemTime::UNIX_EPOCH) {
                        response.header(
                            "Last-Modified",
                            &httpdate::fmt_http_date(std::time::UNIX_EPOCH + duration),
                        );
                    }
                }
            }

            // Set ETag (using file size and modification time as a simple hash)
            if let Ok(metadata) = fs::metadata(path) {
                let etag = format!(
                    "\"{}-{}\"",
                    metadata.len(),
                    metadata
                        .modified()
                        .map(|m| m.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs())
                        .unwrap_or(0)
                );
                response.header("ETag", &etag);
            }

            response.body = String::from_utf8_lossy(&contents).to_string();
            Some(response)
        } else {
            None
        }
    }

    fn parse_query(query: &str) -> HashMap<String, String> {
        query
            .split('&')
            .filter(|s| !s.is_empty())
            .filter_map(|pair| {
                let mut parts = pair.split('=');
                Some((
                    parts.next()?.to_string(),
                    parts.next().unwrap_or("").to_string(),
                ))
            })
            .collect()
    }

    fn match_dynamic_path(&self, pattern: &str, path: &str) -> Option<HashMap<String, String>> {
        let pattern_parts: Vec<&str> = pattern.split('/').collect();
        let path_parts: Vec<&str> = path.split('/').collect();

        if pattern_parts.len() != path_parts.len() {
            return None;
        }

        let mut params = HashMap::new();

        for (pattern_part, path_part) in pattern_parts.iter().zip(path_parts.iter()) {
            if pattern_part.starts_with(':') {
                params.insert(pattern_part[1..].to_string(), path_part.to_string());
            } else if pattern_part != path_part {
                return None;
            }
        }

        Some(params)
    }
}
