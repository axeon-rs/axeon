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
use crate::handler::{error_response, panic_error, ErrorHandler, HttpResponse, IntoResponse};
use crate::http::request::parse_query;
use crate::http::{Body, Method, Request};
use crate::http::Response;
use crate::middleware::{Chain, Middleware, MiddlewareManager, Next};
use crate::plugins::Plugins;
use crate::router::{CompiledRouter, Route, Router};
use crate::static_files::StaticFiles;
use crate::body::{body_error, RequestBody, ResponseBody};
use crate::http::convert::{normalize_path, BodyLimit, OriginalUri, PendingBody};
use crate::http::response::StreamMeta;
use bytes::Bytes;
use futures::FutureExt;
use http::header::{HeaderValue, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
#[cfg(feature = "tls")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::{watch, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use crate::lifecycle::{Shutdown, Slot, WriteTimeout};
#[cfg(feature = "ws")]
use crate::lifecycle::UpgradeContext;
use tracing::{debug, error, info, warn};
use tokio::time::{sleep_until, timeout, timeout_at, Instant};
#[cfg(feature = "tls")]
use crate::tls::TlsConfig;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type HyperResponse = http::Response<ResponseBody>;

/// hyper's HTTP/1 read buffer cannot be smaller than this, so it is also the
/// smallest header limit we can enforce.
const MIN_HEADER_LIMIT: usize = 8 * 1024;


/// Size limits applied while reading a request.
#[derive(Clone, Copy, Debug)]
struct Limits {
    max_header_size: usize,
    max_headers: usize,
    max_body_size: usize,
}

/// The main application struct that represents your web server.
///
/// Connections are served by [hyper](https://hyper.rs) and speak HTTP/1.1
/// and HTTP/2 (negotiated with ALPN over TLS, or prior knowledge in clear text).
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
    write_timeout: Duration,
    shutdown_timeout: Duration,
    limits: Limits,
    router: Router,
    /// Middleware that wraps every request.
    global: MiddlewareManager,
    static_dir: Option<PathBuf>,
    static_cache_control: String,
    plugins: Plugins,
    on_error: Option<ErrorHandler>,
    #[cfg(feature = "tls")]
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
            write_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(30),
            limits: Limits {
                max_header_size: 16 * 1024,
                max_headers: 100,
                max_body_size: 2 * 1024 * 1024,
            },
            router: Router::new(),
            global: MiddlewareManager::default(),
            static_dir: None,
            static_cache_control: "no-cache".to_string(),
            plugins: Plugins::new(),
            on_error: None,
            #[cfg(feature = "tls")]
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

    /// Time allowed to receive the request headers, and separately the
    /// request body, and to complete a TLS handshake. Default: 30 seconds.
    pub fn request_timeout(&mut self, request_timeout: Duration) -> &mut Self {
        self.request_timeout = request_timeout;
        self
    }

    /// How long a write to a client may make no progress before the
    /// connection is closed, so clients that stop reading can't hold
    /// connections open. Default: 30 seconds.
    pub fn write_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.write_timeout = timeout;
        self
    }

    /// How long a graceful shutdown waits for in-flight requests before
    /// closing the remaining connections. Default: 30 seconds.
    pub fn shutdown_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Maximum request body size in bytes; larger requests get 413. Default: 2 MiB.
    pub fn max_body_size(&mut self, bytes: usize) -> &mut Self {
        self.limits.max_body_size = bytes;
        self
    }

    /// Maximum size of the request line plus headers in bytes; larger
    /// requests get 431. Default: 16 KiB, minimum: 8 KiB.
    pub fn max_header_size(&mut self, bytes: usize) -> &mut Self {
        self.limits.max_header_size = bytes.max(MIN_HEADER_LIMIT);
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
    /// * `path` - The URL path to match, e.g. `/users/:id` or `/files/*path`
    /// * `handler` - The async handler function
    pub fn get<F, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
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
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
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
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
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
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
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
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
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
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
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
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
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
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
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
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.router.trace(path, handler);
    }

    /// Adds middleware that wraps every request: all routes (registered
    /// before or after this call), 404 and 405 responses, static files and
    /// automatic `OPTIONS` replies. Middleware runs in the order added, before
    /// any [`Router`] middleware.
    ///
    /// # Arguments
    /// * `middleware` - The middleware to add
    pub fn middleware(&mut self, middleware: impl Middleware + 'static) {
        self.global.add(middleware);
    }

    /// Starts configuring one path: per-route middleware, body limit and
    /// streaming. See [`Router::route`].
    pub fn route(&mut self, path: &str) -> crate::RouteBuilder<'_> {
        self.router.route(path)
    }

    /// Adds a tower [`Layer`](tower_layer::Layer) as global middleware, e.g.
    /// from `tower-http`. See the [`tower`](crate::tower) module.
    pub fn layer<L>(&mut self, layer: L)
    where
        crate::tower::TowerMiddleware<L>: Middleware,
    {
        self.middleware(crate::tower::TowerMiddleware::new(layer));
    }

    /// Mounts a router at a specific path
    ///
    /// # Arguments
    /// * `path` - The URL path to mount the router
    /// * `router` - The router to mount
    pub fn mount(&mut self, path: &str, router: Router) {
        self.router.mount(path, router);
    }

    /// Serve HTTPS with a PEM certificate chain and private key. HTTP/2 and
    /// HTTP/1.1 are negotiated with ALPN. Requires the `tls` feature.
    #[cfg(feature = "tls")]
    pub fn with_tls<P: AsRef<Path>>(&mut self, cert_file: P, key_file: P) -> &mut Self {
        self.tls_config = Some(Arc::new(TlsConfig::new(cert_file, key_file)));
        self
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
    ///
    /// Files are served for GET and HEAD requests that match no route. Large
    /// files are streamed from disk; range requests (206) and conditional
    /// requests (304, via `ETag` and `Last-Modified`) are supported. Hidden
    /// files such as `.env` are never served, except under `/.well-known/`.
    pub fn static_dir(&mut self, dir: &str) -> &mut Self {
        self.static_dir = Some(PathBuf::from(dir));
        self
    }

    /// The `Cache-Control` header for static files. Default: `no-cache`,
    /// which lets browsers cache files but check for changes (a cheap 304)
    /// before using them. For fingerprinted assets (`app.3f9a1c.js`) use
    /// something like `public, max-age=31536000, immutable`.
    pub fn static_cache_control(&mut self, value: &str) -> &mut Self {
        self.static_cache_control = value.to_string();
        self
    }

    /// Starts the HTTP server on its own Tokio runtime and blocks until
    /// Ctrl+C or SIGTERM, then shuts down gracefully (see
    /// [`Server::serve_with_shutdown`]).
    ///
    /// Use [`Server::serve`] instead when you already run inside a Tokio runtime.
    ///
    /// # Arguments
    /// * `addr` - Address to listen on (e.g. "127.0.0.1:3000")
    pub fn listen(self, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        let runtime = Runtime::new()?;
        runtime.block_on(async {
            let listener = TcpListener::bind(addr).await?;
            #[cfg(feature = "tls")]
            let scheme = if self.tls_config.is_some() { "https" } else { "http" };
            #[cfg(not(feature = "tls"))]
            let scheme = "http";
            println!("Server running on {}://{}", scheme, addr);
            self.serve_with_shutdown(listener, shutdown_signal())
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e })
        })
    }

    /// Serves connections from an already-bound listener on the current Tokio runtime.
    ///
    /// Fails before accepting anything if TLS files can't be loaded or two
    /// routes conflict (e.g. `/users/:id` and `/users/:name`).
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
    pub async fn serve(self, listener: TcpListener) -> Result<(), BoxError> {
        self.serve_with_shutdown(listener, std::future::pending()).await
    }

    /// Like [`Server::serve`], but shuts down gracefully when `signal`
    /// completes: it stops accepting connections, lets in-flight requests
    /// finish, closes idle connections and ends server-sent event streams,
    /// then returns. Handlers can watch [`Request::shutdown`]. Connections
    /// (including WebSockets) still open after `shutdown_timeout` are closed.
    ///
    /// ```rust,no_run
    /// use axeon::{Server, shutdown_signal};
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut app = Server::new();
    ///     app.get("/", |_req| async { "Hello" });
    ///     let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    ///     // Ctrl+C or SIGTERM (e.g. from Kubernetes or systemd)
    ///     app.serve_with_shutdown(listener, shutdown_signal()).await.unwrap();
    /// }
    /// ```
    pub async fn serve_with_shutdown<F>(self, listener: TcpListener, signal: F) -> Result<(), BoxError>
    where
        F: Future<Output = ()> + Send,
    {
        #[cfg(feature = "tls")]
        let tls_acceptor = match &self.tls_config {
            Some(tls_config) => Some(tls_config.acceptor()?),
            None => None,
        };
        let max_connections = self.max_connections.max(1);
        let shutdown_timeout = self.shutdown_timeout;
        let app = Arc::new(App::build(self).await?);
        // A permit is taken before each accept, so at the limit we simply stop
        // accepting (no busy loop) and resume when a connection finishes.
        let slots = Arc::new(Semaphore::new(max_connections));
        tokio::pin!(signal);
        if let Ok(addr) = listener.local_addr() {
            info!(%addr, "listening");
        }

        loop {
            let permit = tokio::select! {
                biased;
                _ = &mut signal => break,
                permit = Arc::clone(&slots).acquire_owned() => permit?,
            };
            let (stream, peer) = tokio::select! {
                biased;
                _ = &mut signal => break,
                accepted = listener.accept() => match accepted {
                    Ok(conn) => conn,
                    Err(e) => {
                        // e.g. EMFILE: back off instead of spinning.
                        error!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                },
            };
            let _ = stream.set_nodelay(true);
            let stream = WriteTimeout::new(stream, app.write_timeout);
            // Shared with the connection's requests so a WebSocket upgrade
            // can keep the slot after the HTTP connection ends.
            let slot: Slot = Arc::new(std::sync::Mutex::new(Some(permit)));
            let hard_stop = app.hard_stop.clone();
            let conn_app = Arc::clone(&app);
            #[cfg(feature = "tls")]
            let acceptor = tls_acceptor.clone();

            app.tasks.spawn(async move {
                let app = conn_app;
                let connection = async move {
                    #[cfg(feature = "tls")]
                    if let Some(acceptor) = acceptor {
                        match timeout(app.request_timeout, acceptor.accept(stream)).await {
                            Ok(Ok(tls_stream)) => app.serve_connection(tls_stream, peer, slot).await,
                            Ok(Err(e)) => debug!(%peer, error = %e, "TLS handshake failed"),
                            Err(_) => debug!(%peer, "TLS handshake timed out"),
                        }
                        return;
                    }
                    app.serve_connection(stream, peer, slot).await;
                };
                tokio::select! {
                    _ = connection => {}
                    _ = hard_stop.cancelled() => {}
                }
            });
        }

        drop(listener);
        info!(open_connections = app.tasks.len(), "shutting down");
        app.shutdown_tx.send_replace(true);
        app.tasks.close();
        if timeout(shutdown_timeout, app.tasks.wait()).await.is_err() {
            warn!(remaining = app.tasks.len(), "shutdown timeout elapsed; closing remaining connections");
            app.hard_stop.cancel();
            app.tasks.wait().await;
        }
        info!("shutdown complete");
        Ok(())
    }
}

/// Completes on Ctrl+C, or SIGTERM on Unix. Pass it to
/// [`Server::serve_with_shutdown`].
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// A built server that handles requests in-process, without a socket:
/// for tests of your routes and middleware, and for benchmarks.
///
/// Routing, middleware, error handlers and static files work as when
/// serving; network-level behaviour (body size limits, timeouts, HEAD body
/// stripping) does not apply.
///
/// ```rust
/// use axeon::{Method, Request, Server};
///
/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
/// let mut app = Server::new();
/// app.get("/users/:id", |req| async move { format!("user {}", req.params["id"]) });
/// let app = app.into_app().await.unwrap();
///
/// let response = app.call(Request::new(Method::GET, "/users/7")).await;
/// assert_eq!(response.status, 200);
/// assert_eq!(response.body_text(), "user 7");
/// # });
/// ```
pub struct App {
    routing: Arc<Routing>,
    /// Global middleware around routing, if any; requests start here.
    entry: Option<Arc<Chain>>,
    plugins: Plugins,
    on_error: Option<ErrorHandler>,
    limits: Limits,
    request_timeout: Duration,
    write_timeout: Duration,
    keep_alive: Duration,
    http: auto::Builder<TokioExecutor>,
    /// Set to true when shutdown starts.
    shutdown_tx: watch::Sender<bool>,
    /// Every connection task, including upgraded WebSockets.
    tasks: TaskTracker,
    /// Cancelled when shutdown times out, closing what is left.
    hard_stop: CancellationToken,
}

impl Server {
    /// Builds the server into an [`App`] for in-process requests. Fails if
    /// routes conflict, like [`Server::serve`].
    pub async fn into_app(self) -> Result<App, BoxError> {
        App::build(self).await
    }
}

impl App {
    /// Handles one request through routing and middleware and returns the
    /// response, with errors already converted by the error handler.
    pub async fn call(&self, mut request: Request) -> Response {
        request.plugins = self.plugins.clone();
        request.extensions.insert(Shutdown::new(self.shutdown_tx.subscribe()));
        self.dispatch(request).await
    }

    async fn build(server: Server) -> Result<Self, BoxError> {
        let on_error = server.on_error;
        let router = server.router.compile(on_error.clone())?;
        if HeaderValue::from_str(&server.static_cache_control).is_err() {
            return Err(format!("invalid static Cache-Control value {:?}", server.static_cache_control).into());
        }

        // Resolve the static root once instead of on every request.
        let static_files = match server.static_dir {
            Some(dir) => match tokio::fs::canonicalize(&dir).await {
                Ok(root) => Some(StaticFiles::new(root, server.static_cache_control)),
                Err(e) => {
                    warn!(dir = %dir.display(), error = %e, "static directory is unavailable");
                    None
                }
            },
            None => None,
        };

        let routing = Arc::new(Routing { router, static_files });
        let entry = (!server.global.middlewares.is_empty()).then(|| {
            let terminal = Arc::clone(&routing);
            Arc::new(Chain {
                middlewares: server.global.middlewares.into(),
                handler: Arc::new(move |req: Request| {
                    let routing = Arc::clone(&terminal);
                    async move { routing.handle(req).await }
                }),
                on_error: on_error.clone(),
            })
        });

        let limits = server.limits;
        let mut http = auto::Builder::new(TokioExecutor::new());
        http.http1()
            .timer(TokioTimer::new())
            .header_read_timeout(server.request_timeout)
            // The read buffer bounds the request line plus headers.
            .max_buf_size(limits.max_header_size.max(MIN_HEADER_LIMIT))
            .max_headers(limits.max_headers);
        http.http2()
            .timer(TokioTimer::new())
            .max_header_list_size(limits.max_header_size.min(u32::MAX as usize) as u32)
            .max_concurrent_streams(256);

        Ok(Self {
            routing,
            entry,
            plugins: server.plugins,
            on_error,
            limits,
            request_timeout: server.request_timeout,
            write_timeout: server.write_timeout,
            keep_alive: server.keep_alive,
            http,
            shutdown_tx: watch::channel(false).0,
            tasks: TaskTracker::new(),
            hard_stop: CancellationToken::new(),
        })
    }

    /// Serves HTTP/1.1 or HTTP/2 on one connection, closing it once it has
    /// been idle for `keep_alive`, or when the server shuts down.
    async fn serve_connection<S>(self: Arc<Self>, stream: S, peer: SocketAddr, slot: Slot)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut shutdown = self.shutdown_tx.subscribe();
        let activity = Arc::new(Activity::new());
        #[cfg(feature = "ws")]
        let upgrade = UpgradeContext { tasks: self.tasks.clone(), hard_stop: self.hard_stop.clone(), slot };
        #[cfg(not(feature = "ws"))]
        let _slot = slot; // held until the connection ends
        let service = {
            let app = Arc::clone(&self);
            let activity = Arc::clone(&activity);
            service_fn(move |mut req: hyper::Request<Incoming>| {
                let app = Arc::clone(&app);
                let busy = activity.begin();
                req.extensions_mut().insert(Shutdown::new(app.shutdown_tx.subscribe()));
                #[cfg(feature = "ws")]
                req.extensions_mut().insert(upgrade.clone());
                async move {
                    let response = app.respond(req, Some(peer)).await;
                    drop(busy);
                    Ok::<_, Infallible>(response)
                }
            })
        };

        // With upgrades, so WebSocket handshakes can take over the connection.
        let conn = self.http.serve_connection_with_upgrades(TokioIo::new(stream), service);
        tokio::pin!(conn);
        let mut closing = false;
        loop {
            let idle_deadline = activity.last() + self.keep_alive;
            tokio::select! {
                result = conn.as_mut() => {
                    if let Err(e) = result {
                        if is_disconnect(e.as_ref()) {
                            debug!(%peer, error = %e, "connection closed");
                        } else {
                            warn!(%peer, error = %e, "connection error");
                        }
                    }
                    return;
                }
                // Also fires if the server is gone (sender dropped).
                _ = shutdown.wait_for(|stop| *stop), if !closing => {
                    conn.as_mut().graceful_shutdown();
                    closing = true;
                }
                _ = sleep_until(idle_deadline), if !closing => {
                    if activity.idle_for(self.keep_alive) {
                        // Finishes any in-flight response, then closes.
                        conn.as_mut().graceful_shutdown();
                        closing = true;
                    }
                }
            }
        }
    }

    /// Reads the body under the limits, then routes the request.
    /// Reads the body under the route's limits (or leaves it unread for
    /// streaming routes), then runs middleware and routing.
    pub(crate) async fn respond<B>(&self, req: http::Request<B>, peer: Option<SocketAddr>) -> HyperResponse
    where
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        let Some(method) = Method::parse(req.method().as_str()) else {
            let err = ServerError::NotImplemented(format!("method {}", req.method()));
            return self.to_hyper(self.handle_error(err));
        };
        let (mut parts, body) = req.into_parts();
        let path = normalize_path(parts.uri.path());
        let config = self.routing.router.body_config(&path, method);
        let limit = config.limit.unwrap_or(self.limits.max_body_size);

        // Refuse a declared oversized body before reading any of it.
        let declared = parts
            .headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if declared.is_some_and(|len| len > limit as u64) {
            return self.reject(ServerError::PayloadTooLarge);
        }

        let data = if config.stream {
            let body = RequestBody::limited(body, limit, self.request_timeout);
            parts.extensions.insert(PendingBody::new(body));
            Bytes::new()
        } else {
            let deadline = Instant::now() + self.request_timeout;
            match timeout_at(deadline, Limited::new(body, limit).collect()).await {
                Err(_) => return self.reject(ServerError::RequestTimeout),
                Ok(Err(e)) => return self.reject(body_error(e)),
                Ok(Ok(collected)) => collected.to_bytes(),
            }
        };

        let query = parts.uri.query().map(parse_query).unwrap_or_default();
        let content_type = parts
            .headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("none")
            .to_string();
        parts.extensions.insert(OriginalUri(parts.uri));
        parts.extensions.insert(BodyLimit(limit));

        let request = Request {
            method,
            path,
            query,
            headers: parts.headers,
            body: Body { content_type, data },
            params: HashMap::new(),
            data: HashMap::new(),
            plugins: self.plugins.clone(),
            remote_addr: peer,
            extensions: parts.extensions,
        };
        let response = self.dispatch(request).await;
        self.to_hyper(response)
    }

    /// An error response for a request whose body was not read; the
    /// connection is closed afterwards so unread bytes can't be misparsed.
    fn reject(&self, err: ServerError) -> HyperResponse {
        let mut response = self.to_hyper(self.handle_error(err));
        response.headers_mut().insert(CONNECTION, HeaderValue::from_static("close"));
        response
    }

    fn to_hyper(&self, response: Response) -> HyperResponse {
        let response = response.into_http();
        if !response.extensions().get::<StreamMeta>().is_some_and(|m| m.end_on_shutdown) {
            return response;
        }
        // Event streams end cleanly at shutdown so clients reconnect elsewhere.
        let mut shutdown = self.shutdown_tx.subscribe();
        let stopping = async move {
            let _ = shutdown.wait_for(|stopping| *stopping).await;
        };
        response.map(|body| body.take_until(stopping))
    }

    /// Runs global middleware, routing and handlers. Errors and panics are
    /// already turned into responses inside the chain; this is a backstop.
    async fn dispatch(&self, request: Request) -> Response {
        let response = match &self.entry {
            Some(entry) => AssertUnwindSafe(Next::start(Arc::clone(entry)).handle(request)).catch_unwind().await,
            None => AssertUnwindSafe(self.routing.handle(request)).catch_unwind().await,
        };
        match response {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => self.handle_error(err),
            Err(payload) => self.handle_error(panic_error(payload)),
        }
    }

    fn handle_error(&self, error: ServerError) -> Response {
        error_response(self.on_error.as_ref(), error)
    }
}

/// Routes, handlers and static files: the end of the global middleware chain.
struct Routing {
    router: CompiledRouter,
    static_files: Option<StaticFiles>,
}

impl Routing {
    async fn handle(&self, mut req: Request) -> HttpResponse {
        let method = req.method;
        let Some((routes, params)) = self.router.lookup(&req.path) else {
            if method == Method::GET || method == Method::HEAD {
                if let Some(static_files) = &self.static_files {
                    if let Some(response) = static_files.serve(&req).await {
                        return Ok(response);
                    }
                }
            }
            return Err(ServerError::NotFound);
        };
        req.params = params;

        if let Some((route, as_get)) = select(routes, method) {
            if as_get {
                // A GET handler serving HEAD; the body is dropped when sending.
                req.method = Method::GET;
            }
            return route.handle(req).await;
        }
        let allow = allow_header(routes);
        if method == Method::OPTIONS {
            return handle_options(routes, allow, req).await;
        }
        Err(ServerError::MethodNotAllowed(allow))
    }
}

/// Tracks in-flight requests and the last activity on a connection.
struct Activity {
    in_flight: AtomicUsize,
    last: Mutex<Instant>,
}

impl Activity {
    fn new() -> Self {
        Self { in_flight: AtomicUsize::new(0), last: Mutex::new(Instant::now()) }
    }

    fn touch(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    fn last(&self) -> Instant {
        *self.last.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn begin(self: &Arc<Self>) -> BusyGuard {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.touch();
        BusyGuard(Arc::clone(self))
    }

    fn idle_for(&self, duration: Duration) -> bool {
        self.in_flight.load(Ordering::SeqCst) == 0 && self.last().elapsed() >= duration
    }
}

/// Marks a request as in flight until dropped.
struct BusyGuard(Arc<Activity>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

/// The route for `method`, falling back to GET for HEAD requests. The flag
/// is true when a GET handler serves a HEAD request.
fn select(routes: &HashMap<Method, Route>, method: Method) -> Option<(&Route, bool)> {
    if let Some(route) = routes.get(&method) {
        return Some((route, false));
    }
    if method == Method::HEAD {
        return routes.get(&Method::GET).map(|route| (route, true));
    }
    None
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
    let template = routes.get(&Method::GET).or_else(|| routes.values().next());
    let middlewares = template.map(|route| Arc::clone(&route.chain.middlewares)).unwrap_or_else(|| Arc::new([]));
    let on_error = template.and_then(|route| route.chain.on_error.clone());
    let route = Route {
        body: Default::default(),
        chain: Arc::new(Chain {
            middlewares,
            on_error,
            handler: Arc::new(move |_req: Request| {
                let allow = allow.clone();
                async move {
                    let mut response = Response::new(204);
                    response.header("Allow", allow);
                    response
                }
            }),
        }),
    };
    route.handle(req).await
}

/// Connection errors that just mean the client went away or timed out.
fn is_disconnect(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(e);
    while let Some(err) = source {
        if let Some(hyper_err) = err.downcast_ref::<hyper::Error>() {
            if hyper_err.is_incomplete_message() || hyper_err.is_canceled() || hyper_err.is_timeout() {
                return true;
            }
        }
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            if matches!(io.kind(), UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe | TimedOut | NotConnected) {
                return true;
            }
        }
        source = err.source();
    }
    false
}
