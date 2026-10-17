//! Server is the main entry point for the Axeon framework.
//!
//! This module provides the core Server struct that serves as the main
//! entry point for building web applications with Axeon.
//!
//! # Examples
//!
//! ```rust,no_run
//! use axeon::{Server, Response, ok_json};
//!
//! fn main() {
//!     let mut app = Server::new();
//!     app.get("/", |_req| async {
//!         ok_json!({ "message": "Hello!" })
//!     });
//!    app.listen("127.0.0.1:3000").unwrap();
//! }
//! ```

use crate::error::ServerError;
use crate::handler::{HttpResponse, IntoResponse};
use crate::http::codec::{self, BodyFraming, Limits, ReadError};
use crate::http::{Body, Method, Request};
use crate::http::Response;
use crate::middleware::Middleware;
use crate::plugins::Plugins;
use crate::router::{Route, Router};
use futures::FutureExt;
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::Semaphore;
use tokio::time::{timeout, timeout_at, Instant};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use std::fs::File;
use std::io::BufReader as StdBufReader;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

type ErrorHandler = Arc<dyn Fn(ServerError) -> Response + Send + Sync>;
type BoxError = Box<dyn std::error::Error + Send + Sync>;
type PathMatch<'a> = (&'a HashMap<Method, Route>, HashMap<String, String>);

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

    fn load_certs(&self) -> Result<Vec<CertificateDer<'static>>, BoxError> {
        let cert_file = File::open(&self.cert_file)?;
        let mut reader = StdBufReader::new(cert_file);
        let certs = rustls_pemfile::certs(&mut reader)
            .filter_map(|result| result.ok())
            .collect();
        Ok(certs)
    }

    fn load_key(&self) -> Result<PrivateKeyDer<'static>, BoxError> {
        let key_file = File::open(&self.key_file)?;
        let mut reader = StdBufReader::new(key_file);
        let key = rustls_pemfile::private_key(&mut reader)?
            .ok_or("No private key found")?;
        Ok(key)
    }
}

/// The main application struct that represents your web server.
///
/// # Example
///
/// ```rust,no_run
/// use axeon::{ok_json, Response, Server};
///
///
/// fn main() {
///     let mut app = Server::new();
///
///     // Add a route
///     app.get("/", |_req| async {
///     ok_json!({ "message": "Hello" })
///     });
///
///     // Start the server
///     app.listen("127.0.0.1:3000").unwrap();
/// }
/// ```
///
///

#[derive(Clone)]
pub struct Server {
    pub max_connections: usize,
    /// How long an idle keep-alive connection waits for the next request.
    pub keep_alive: Duration,
    request_timeout: Duration,
    limits: Limits,
    router: Router,
    static_dir: Option<PathBuf>,
    plugins: Plugins,
    on_error: Option<ErrorHandler>,
    tls_config: Option<Arc<TlsConfig>>,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    /// Creates a new Application instance
    pub fn new() -> Self {
        Self {
            max_connections: 1024,
            keep_alive: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            limits: Limits {
                max_header_size: 16 * 1024,
                max_headers: 100,
                max_body_size: 2 * 1024 * 1024,
            },
            router: Router::new(),
            static_dir: None,
            plugins: Plugins::new(),
            on_error: None,
            tls_config: None,
        }
    }

    /// Maximum number of concurrent connections. Further connections wait in
    /// the OS accept backlog until a slot frees up. Default: 1024.
    pub fn max_connections(&mut self, max_connections: usize) -> &mut Self {
        self.max_connections = max_connections;
        self
    }

    /// Idle timeout for keep-alive connections. Default: 5 seconds.
    pub fn keep_alive(&mut self, keep_alive: Duration) -> &mut Self {
        self.keep_alive = keep_alive;
        self
    }

    /// Time allowed to receive a full request (head and body), to complete a
    /// TLS handshake, and to write a response. Default: 30 seconds.
    pub fn request_timeout(&mut self, request_timeout: Duration) -> &mut Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Maximum request body size in bytes; larger requests get 413. Default: 2 MiB.
    pub fn max_body_size(&mut self, bytes: usize) -> &mut Self {
        self.limits.max_body_size = bytes;
        self
    }

    /// Maximum size of the request line plus headers in bytes. Default: 16 KiB.
    pub fn max_header_size(&mut self, bytes: usize) -> &mut Self {
        self.limits.max_header_size = bytes;
        self
    }

    /// Maximum number of request headers. Default: 100.
    pub fn max_headers(&mut self, count: usize) -> &mut Self {
        self.limits.max_headers = count;
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

    /// Starts the HTTP server on its own Tokio runtime and blocks forever.
    ///
    /// Use [`Server::serve`] instead when you already run inside a Tokio runtime.
    ///
    /// # Arguments
    /// * `addr` - Address to listen on (e.g. "127.0.0.1:3000")
    pub fn listen(self, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        let runtime = Runtime::new()?;
        runtime.block_on(async {
            let listener = TcpListener::bind(addr).await?;
            println!("Server running on {}", if self.tls_config.is_some() {
                format!("https://{}", addr)
            } else {
                format!("http://{}", addr)
            });
            self.serve(listener).await.map_err(|e| -> Box<dyn std::error::Error> { e })
        })
    }

    /// Serves connections from an already-bound listener on the current Tokio runtime.
    ///
    /// ```rust,no_run
    /// use axeon::{Response, Server};
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut app = Server::new();
    ///     app.get("/", |_req| async { Response::text("Hello") });
    ///     let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    ///     app.serve(listener).await.unwrap();
    /// }
    /// ```
    pub async fn serve(mut self, listener: TcpListener) -> Result<(), BoxError> {
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

        // Resolve the static root once instead of on every request.
        if let Some(dir) = self.static_dir.take() {
            match tokio::fs::canonicalize(&dir).await {
                Ok(root) => self.static_dir = Some(root),
                Err(e) => eprintln!("Static directory {:?} is unavailable: {}", dir, e),
            }
        }

        // One shared, immutable app for every connection.
        let app = Arc::new(self);
        // A permit is taken before each accept, so at the limit we simply stop
        // accepting (no busy loop) and resume when a connection finishes.
        let slots = Arc::new(Semaphore::new(app.max_connections.max(1)));

        loop {
            let permit = Arc::clone(&slots).acquire_owned().await?;
            let (stream, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    // e.g. EMFILE: back off instead of spinning.
                    eprintln!("Accept failed: {}", e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let _ = stream.set_nodelay(true);
            let app = Arc::clone(&app);
            let acceptor = tls_acceptor.clone();

            tokio::spawn(async move {
                let _permit = permit;
                let result = if let Some(acceptor) = acceptor {
                    match timeout(app.request_timeout, acceptor.accept(stream)).await {
                        Ok(Ok(tls_stream)) => app.handle_connection(tls_stream, Some(peer)).await,
                        Ok(Err(e)) => {
                            eprintln!("TLS handshake failed: {}", e);
                            Ok(())
                        }
                        Err(_) => Ok(()),
                    }
                } else {
                    app.handle_connection(stream, Some(peer)).await
                };

                if let Err(e) = result {
                    if !is_disconnect(&e) {
                        eprintln!("Connection error: {}", e);
                    }
                }
            });
        }
    }

    /// Serves HTTP/1.x requests on one connection until it closes, keeping it
    /// alive between requests when the client allows it.
    async fn handle_connection<S>(&self, stream: S, peer: Option<SocketAddr>) -> Result<(), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut reader = BufReader::with_capacity(8 * 1024, stream);
        let mut first_request = true;

        loop {
            if !first_request {
                // Idle keep-alive: wait for the first byte of the next request.
                match timeout(self.keep_alive, reader.fill_buf()).await {
                    Err(_) => return Ok(()),
                    Ok(Ok([])) => return Ok(()),
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => return Err(e),
                }
            }
            first_request = false;

            let deadline = Instant::now() + self.request_timeout;
            let head = match timeout_at(deadline, codec::read_head(&mut reader, &self.limits)).await {
                Err(_) => return self.reject(reader.get_mut(), ServerError::RequestTimeout).await,
                Ok(Err(ReadError::Eof)) => return Ok(()),
                Ok(Err(ReadError::Io(e))) => return Err(e),
                Ok(Err(ReadError::Reject(err))) => return self.reject(reader.get_mut(), err).await,
                Ok(Ok(head)) => head,
            };

            let http10 = head.version_minor == 0;
            let connection = head.headers.get("connection").map(String::as_str).unwrap_or("");
            let mut keep_alive = if http10 {
                codec::has_token(connection, "keep-alive")
            } else {
                !codec::has_token(connection, "close")
            };

            let framing = match codec::body_framing(&head.headers, self.limits.max_body_size) {
                Ok(framing) => framing,
                Err(err) => return self.reject(reader.get_mut(), err).await,
            };
            if framing != BodyFraming::None
                && !http10
                && head.headers.get("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
            {
                let stream = reader.get_mut();
                stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
                stream.flush().await?;
            }
            let body = match timeout_at(deadline, codec::read_body(&mut reader, framing, &self.limits)).await {
                Err(_) => return self.reject(reader.get_mut(), ServerError::RequestTimeout).await,
                Ok(Err(ReadError::Eof)) => return Ok(()),
                Ok(Err(ReadError::Io(e))) => return Err(e),
                Ok(Err(ReadError::Reject(err))) => return self.reject(reader.get_mut(), err).await,
                Ok(Ok(body)) => body,
            };

            let is_head = head.method == Method::HEAD;
            let request = Self::build_request(head, body, peer, self.plugins.clone());
            let response = self.dispatch(request).await;

            if response
                .get_header("connection")
                .is_some_and(|v| codec::has_token(v, "close"))
            {
                keep_alive = false;
            }
            timeout(self.request_timeout, Self::write_response(reader.get_mut(), &response, is_head, keep_alive, http10))
                .await
                .map_err(|_| Error::new(ErrorKind::TimedOut, "response write timed out"))??;

            if !keep_alive {
                return Ok(());
            }
        }
    }

    fn build_request(
        head: codec::RequestHead,
        body: Vec<u8>,
        peer: Option<SocketAddr>,
        plugins: Plugins,
    ) -> Request {
        // Absolute-form targets ("http://host/path") are reduced to the path.
        let mut target = head.target.as_str();
        if let Some(rest) = target.strip_prefix("http://").or_else(|| target.strip_prefix("https://")) {
            target = rest.find('/').map(|i| &rest[i..]).unwrap_or("/");
        }
        let (path, query) = match target.split_once('?') {
            Some((path, query)) => (path, Self::parse_query(query)),
            None => (target, HashMap::new()),
        };
        let path = path.trim_end_matches('/');
        let path = if path.is_empty() { "/".to_string() } else { path.to_string() };

        let content_type = head
            .headers
            .get("content-type")
            .cloned()
            .unwrap_or_else(|| "none".to_owned());

        Request {
            method: head.method,
            path,
            query,
            headers: head.headers,
            body: Body {
                content_type,
                data: body,
            },
            params: HashMap::new(),
            data: HashMap::new(),
            plugins,
            remote_addr: peer,
        }
    }

    /// Runs routing and handlers, converting panics and errors into responses.
    async fn dispatch(&self, request: Request) -> Response {
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
                eprintln!("Handler panicked: {}", panic_msg);
                Err(ServerError::PanicError(panic_msg))
            },
        };
        match response {
            Ok(response) => response,
            Err(err) => self.handle_error(err),
        }
    }

    /// Sends an error response for a request that could not be read, then closes.
    async fn reject<W>(&self, stream: &mut W, err: ServerError) -> Result<(), Error>
    where
        W: AsyncWrite + Unpin,
    {
        let response = self.handle_error(err);
        let write = Self::write_response(stream, &response, false, false, false);
        // Best effort: the client may already be gone.
        let _ = timeout(Duration::from_secs(1), write).await;
        Ok(())
    }

    async fn write_response<W>(
        stream: &mut W,
        response: &Response,
        is_head: bool,
        keep_alive: bool,
        http10: bool,
    ) -> Result<(), Error>
    where
        W: AsyncWrite + Unpin,
    {
        let mut out = codec::encode_head(response, keep_alive, http10);
        let send_body = !is_head && codec::status_has_body(response.status);
        if send_body && response.body.len() <= 16 * 1024 {
            // One write for small responses.
            out.extend_from_slice(&response.body);
            stream.write_all(&out).await?;
        } else {
            stream.write_all(&out).await?;
            if send_body {
                stream.write_all(&response.body).await?;
            }
        }
        stream.flush().await
    }

    /// Sets the directory for serving static files
    ///
    /// # Arguments
    /// * `dir` - Path to the static files directory
    ///
    /// # Example
    /// ```rust
    ///
    /// use axeon::Server;
    ///
    /// let mut app = Server::new();
    /// app.static_dir("public");
    /// ```
    pub fn static_dir(&mut self, dir: &str) -> &mut Self {
        self.static_dir = Some(PathBuf::from(dir));
        self
    }

    async fn handle(&self, mut req: Request) -> HttpResponse {
        let method = req.method;
        // A path that exists but lacks a handler for this method.
        let mut path_match: Option<PathMatch> = None;

        // Keys containing ':' are dynamic patterns and are matched below.
        if !req.path.contains(':') {
            if let Some(routes) = self.router.routes.get(&req.path) {
                if let Some((route, as_get)) = Self::select(routes, method) {
                    return Self::run(route, as_get, req).await;
                }
                path_match = Some((routes, HashMap::new()));
            }
        }

        for dynamic_path in &self.router.dynamic_routes {
            if let Some(params) = Self::match_dynamic_path(dynamic_path, &req.path) {
                if let Some(routes) = self.router.routes.get(dynamic_path) {
                    if let Some((route, as_get)) = Self::select(routes, method) {
                        req.params = params;
                        return Self::run(route, as_get, req).await;
                    }
                    if path_match.is_none() {
                        path_match = Some((routes, params));
                    }
                }
            }
        }

        if let Some((routes, params)) = path_match {
            req.params = params;
            let allow = Self::allow_header(routes);
            if method == Method::OPTIONS {
                return Self::handle_options(routes, allow, req).await;
            }
            return Err(ServerError::MethodNotAllowed(allow));
        }

        if method == Method::GET || method == Method::HEAD {
            if let Some(response) = self.handle_static_file(&req.path).await {
                return Ok(response);
            }
        }
        Err(ServerError::NotFound)
    }

    /// The route for `method`, falling back to GET for HEAD requests. The
    /// flag is true when a GET handler serves a HEAD request.
    fn select(routes: &HashMap<Method, Route>, method: Method) -> Option<(&Route, bool)> {
        if let Some(route) = routes.get(&method) {
            return Some((route, false));
        }
        if method == Method::HEAD {
            return routes.get(&Method::GET).map(|route| (route, true));
        }
        None
    }

    async fn run(route: &Route, as_get: bool, mut req: Request) -> HttpResponse {
        if as_get {
            // The body is dropped when the response is written.
            req.method = Method::GET;
        }
        route.handle(req).await
    }

    fn allow_header(routes: &HashMap<Method, Route>) -> String {
        Method::ALL
            .iter()
            .filter(|m| {
                routes.contains_key(m)
                    || (**m == Method::HEAD && routes.contains_key(&Method::GET))
                    || **m == Method::OPTIONS
            })
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Automatic OPTIONS response. Runs the path's middleware so CORS
    /// preflights work for routes without an explicit OPTIONS handler.
    async fn handle_options(routes: &HashMap<Method, Route>, allow: String, req: Request) -> HttpResponse {
        let middlewares = routes
            .get(&Method::GET)
            .or_else(|| routes.values().next())
            .map(|route| route.middlewares.clone())
            .unwrap_or_else(crate::middleware::MiddlewareManager::new);
        let route = Route {
            middlewares,
            handler: Box::new(move |_| {
                let allow = allow.clone();
                async move {
                    let mut response = Response::new(204);
                    response.header("Allow", allow);
                    Ok(response)
                }
            }),
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

    async fn handle_static_file(&self, path: &str) -> Option<Response> {
        // `static_dir` is canonicalized when the server starts.
        let static_root = self.static_dir.as_ref()?;
        let decoded = urlencoding::decode(path.trim_start_matches('/')).ok()?;
        let file_path = static_root.join(decoded.as_ref());
        let canonical_path = tokio::fs::canonicalize(&file_path).await.ok()?;
        if !canonical_path.starts_with(static_root) {
            return None;
        }
        let metadata = tokio::fs::metadata(&canonical_path).await.ok()?;
        if !metadata.is_file() {
            return None;
        }
        Self::serve_file(&canonical_path, &metadata).await
    }

    async fn serve_file(path: &Path, metadata: &std::fs::Metadata) -> Option<Response> {
        let contents = tokio::fs::read(path).await.ok()?;
        let mut response = Response::new(200);

        // Set content type based on file extension
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        let content_type = match ext.as_str() {
            "html" | "htm" => "text/html; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "js" | "mjs" => "text/javascript; charset=utf-8",
            "json" => "application/json",
            "txt" => "text/plain; charset=utf-8",
            "xml" => "application/xml",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "avif" => "image/avif",
            "svg" => "image/svg+xml",
            "ico" => "image/x-icon",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            "wasm" => "application/wasm",
            "pdf" => "application/pdf",
            "mp4" => "video/mp4",
            "webm" => "video/webm",
            "mp3" => "audio/mpeg",
            _ => "application/octet-stream",
        };
        response.header("Content-Type", content_type);

        // Set cache control headers
        response.header("Cache-Control", "public, max-age=31536000");

        if let Ok(modified) = metadata.modified() {
            response.header("Last-Modified", httpdate::fmt_http_date(modified));
            // ETag from size and modification time
            let secs = modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            response.header("ETag", format!("\"{}-{}\"", metadata.len(), secs));
        }

        response.body = contents;
        Some(response)
    }

    /// Parses a query string, percent-decoding keys and values ('+' is a space).
    fn parse_query(query: &str) -> HashMap<String, String> {
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

    fn match_dynamic_path(pattern: &str, path: &str) -> Option<HashMap<String, String>> {
        let mut pattern_parts = pattern.split('/');
        let mut path_parts = path.split('/');
        let mut params = HashMap::new();

        loop {
            match (pattern_parts.next(), path_parts.next()) {
                (None, None) => return Some(params),
                (Some(pattern_part), Some(path_part)) => {
                    if let Some(name) = pattern_part.strip_prefix(':') {
                        let value = urlencoding::decode(path_part)
                            .map(|v| v.into_owned())
                            .unwrap_or_else(|_| path_part.to_string());
                        params.insert(name.to_string(), value);
                    } else if pattern_part != path_part {
                        return None;
                    }
                }
                _ => return None,
            }
        }
    }
}

/// Errors that just mean the client went away.
fn is_disconnect(e: &Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::TimedOut
    )
}
