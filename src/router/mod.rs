use crate::handler::{ErrorHandler, Handler, IntoResponse};
use crate::message::request::Params;
use crate::message::{Method, Request};
use crate::middleware::{Chain, Middleware, MiddlewareManager, MiddlewareResult, Next};
use crate::ServerError;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct Route {
    pub(crate) chain: Arc<Chain>,
    pub(crate) body: BodyConfig,
    /// How long the route's middleware and handler may take.
    pub(crate) timeout: Option<Duration>,
}

/// A route's own timeout setting.
#[derive(Clone, Copy, Debug, Default)]
enum RouteTimeout {
    /// The server's `handler_timeout`.
    #[default]
    Inherit,
    After(Duration),
    Never,
}

/// How a route reads its request body.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BodyConfig {
    /// Overrides the server's `max_body_size`.
    pub(crate) limit: Option<usize>,
    /// Leave the body unread for `Request::body_stream`.
    pub(crate) stream: bool,
}

impl Route {
    pub fn handle(&self, req: Request) -> MiddlewareResult {
        let response = Next::start(Arc::clone(&self.chain)).handle(req);
        match self.timeout {
            // Dropping the future cancels the handler at its next `.await`.
            Some(limit) => Box::pin(async move {
                tokio::time::timeout(limit, response).await.unwrap_or(Err(ServerError::HandlerTimeout))
            }),
            None => response,
        }
    }
}

/// A registered handler and the middleware of the routers it was mounted from.
#[derive(Clone)]
pub(crate) struct Endpoint {
    handler: Arc<dyn Handler>,
    /// Outermost first: mounted routers' middleware, then the route's own.
    middlewares: Vec<Arc<dyn Middleware>>,
    body: BodyConfig,
    timeout: RouteTimeout,
    /// Values from [`RouteBuilder::meta`], e.g. API documentation.
    meta: http::Extensions,
}

/// A registered route, from [`Router::routes`] or
/// [`Server::routes`](crate::Server::routes): for tools that describe the
/// API, such as OpenAPI generators.
#[derive(Clone, Copy, Debug)]
pub struct RouteInfo<'a> {
    method: Method,
    path: &'a str,
    meta: &'a http::Extensions,
}

impl<'a> RouteInfo<'a> {
    /// The HTTP method.
    pub fn method(&self) -> Method {
        self.method
    }

    /// The path pattern, e.g. `/users/:id`.
    pub fn path(&self) -> &'a str {
        self.path
    }

    /// A value attached with [`RouteBuilder::meta`].
    pub fn meta<T: Send + Sync + 'static>(&self) -> Option<&'a T> {
        self.meta.get::<T>()
    }
}

/// A group of routes with shared middleware. Paths use `:name` for a
/// parameter segment and `*name` for a trailing catch-all, e.g.
/// `/users/:id` or `/files/*path`.
///
/// Middleware added with [`Router::middleware`] applies to every route of
/// this router, whether added before or after it, and runs after the
/// server's global middleware.
#[derive(Clone, Default)]
pub struct Router {
    pub(crate) middlewares: MiddlewareManager,
    pub(crate) routes: HashMap<String, HashMap<Method, Endpoint>>,
}

impl Router {
    /// An empty router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every registered route, including those of mounted routers, sorted
    /// by path, then method.
    pub fn routes(&self) -> Vec<RouteInfo<'_>> {
        let mut routes: Vec<RouteInfo<'_>> = self
            .routes
            .iter()
            .flat_map(|(path, endpoints)| {
                endpoints.iter().map(move |(method, endpoint)| RouteInfo { method: *method, path, meta: &endpoint.meta })
            })
            .collect();
        routes.sort_by(|a, b| (a.path, a.method.as_str()).cmp(&(b.path, b.method.as_str())));
        routes
    }

    /// Registers a GET handler for `path`.
    pub fn get<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::GET, path, handler);
        self
    }

    /// Registers a POST handler for `path`.
    pub fn post<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::POST, path, handler);
        self
    }

    /// Registers a PUT handler for `path`.
    pub fn put<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::PUT, path, handler);
        self
    }

    /// Registers a PATCH handler for `path`.
    pub fn patch<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::PATCH, path, handler);
        self
    }

    /// Registers a DELETE handler for `path`.
    pub fn delete<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::DELETE, path, handler);
        self
    }

    /// Registers a HEAD handler for `path`.
    pub fn head<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::HEAD, path, handler);
        self
    }

    /// Registers a CONNECT handler for `path`.
    pub fn connect<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::CONNECT, path, handler);
        self
    }

    /// Registers a OPTIONS handler for `path`.
    pub fn options<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::OPTIONS, path, handler);
        self
    }

    /// Registers a TRACE handler for `path`.
    pub fn trace<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::TRACE, path, handler);
        self
    }

    fn add<F, R>(&mut self, method: Method, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        let endpoint = Endpoint {
            handler: Arc::new(handler),
            middlewares: Vec::new(),
            body: BodyConfig::default(),
            timeout: RouteTimeout::default(),
            meta: http::Extensions::new(),
        };
        self.routes.entry(normalize(path)).or_default().insert(method, endpoint);
    }

    /// Starts configuring one path: middleware and body settings for the
    /// handlers registered through the returned builder.
    ///
    /// ```rust
    /// use axeon::{Server, ServerError};
    /// use axeon::middleware::from_fn;
    ///
    /// let mut app = Server::new();
    /// let require_admin = from_fn(|req, next| async move {
    ///     if req.get_header("x-admin").is_none() {
    ///         return Err(ServerError::Forbidden("admins only".into()));
    ///     }
    ///     next.handle(req).await
    /// });
    ///
    /// // GET /posts is public; only POST needs the middleware.
    /// app.route("/posts").get(|_req| async { "all posts" });
    /// app.route("/posts").middleware(require_admin).post(|_req| async { (201, "created") });
    ///
    /// // Uploads: up to 1 GiB, read as a stream instead of buffered.
    /// app.route("/upload")
    ///     .body_limit(1 << 30)
    ///     .stream_body()
    ///     .post(|req| async move {
    ///         let size = req.body_stream()?.save_to("/tmp/upload.bin").await?;
    ///         Ok::<_, ServerError>(format!("saved {} bytes", size))
    ///     });
    /// ```
    pub fn route(&mut self, path: &str) -> RouteBuilder<'_> {
        RouteBuilder {
            router: self,
            path: normalize(path),
            methods: Vec::new(),
            middlewares: Vec::new(),
            body: BodyConfig::default(),
            timeout: RouteTimeout::default(),
            meta: http::Extensions::new(),
        }
    }

    /// Adds a tower [`Layer`](tower_layer::Layer) as middleware for every
    /// route of this router.
    pub fn layer<L>(&mut self, layer: L) -> &mut Self
    where
        crate::tower::TowerMiddleware<L>: Middleware,
    {
        self.middleware(crate::tower::TowerMiddleware::new(layer))
    }

    /// Adds middleware for every route of this router.
    pub fn middleware(&mut self, middleware: impl Middleware + 'static) -> &mut Self {
        self.middlewares.add(middleware);
        self
    }

    /// Adds every route of `router` under the `path` prefix. This router's
    /// middleware runs before the mounted router's own.
    pub fn mount(&mut self, path: &str, router: Router) -> &mut Self {
        self.middlewares.errors.extend(router.middlewares.errors.iter().cloned());
        let prefix = path.trim_end_matches('/');
        for (key, methods) in router.routes {
            let full = normalize(&format!("{}{}", prefix, key));
            let entry = self.routes.entry(full).or_default();
            for (method, endpoint) in methods {
                let middlewares = router
                    .middlewares
                    .middlewares
                    .iter()
                    .chain(endpoint.middlewares.iter())
                    .cloned()
                    .collect();
                entry.insert(method, Endpoint { middlewares, ..endpoint });
            }
        }
        self
    }

    /// Builds the radix-tree matcher and the route chains used to serve
    /// requests.
    pub(crate) fn compile(&self, on_error: Option<ErrorHandler>, default_timeout: Option<Duration>) -> Result<CompiledRouter, String> {
        if !self.middlewares.errors.is_empty() {
            return Err(format!("invalid middleware configuration: {}", self.middlewares.errors.join("; ")));
        }
        let mut matcher = matchit::Router::new();
        let mut routes = Vec::with_capacity(self.routes.len());
        let mut patterns = Vec::with_capacity(self.routes.len());
        let mut param_names = Vec::with_capacity(self.routes.len());
        // Sorted so conflicts are reported the same way on every run.
        let mut paths: Vec<_> = self.routes.keys().collect();
        paths.sort();
        for path in paths {
            matcher
                .insert(to_matchit(path), routes.len())
                .map_err(|e| format!("invalid route {:?}: {}", path, e))?;
            let methods = self.routes[path]
                .iter()
                .map(|(method, endpoint)| {
                    let middlewares: Vec<_> = self
                        .middlewares
                        .middlewares
                        .iter()
                        .chain(endpoint.middlewares.iter())
                        .cloned()
                        .collect();
                    let chain = Chain {
                        middlewares: middlewares.into(),
                        handler: Arc::clone(&endpoint.handler),
                        on_error: on_error.clone(),
                    };
                    let timeout = match endpoint.timeout {
                        RouteTimeout::Inherit => default_timeout,
                        RouteTimeout::After(limit) => Some(limit),
                        RouteTimeout::Never => None,
                    };
                    (*method, Route { chain: Arc::new(chain), body: endpoint.body, timeout })
                })
                .collect();
            routes.push(methods);
            patterns.push(Arc::from(path.as_str()));
            param_names.push(
                path.split('/')
                    .filter_map(|segment| segment.strip_prefix(':').or_else(|| segment.strip_prefix('*')))
                    .filter(|name| !name.is_empty())
                    .map(Arc::from)
                    .collect(),
            );
        }
        Ok(CompiledRouter { matcher, routes, patterns, param_names })
    }
}

/// Configures the handlers of one path; see [`Router::route`].
///
/// Middleware and body settings apply to every handler registered through
/// this builder, whether added before or after them, and to no other route.
/// Route middleware runs after global and router middleware.
pub struct RouteBuilder<'a> {
    router: &'a mut Router,
    path: String,
    methods: Vec<Method>,
    middlewares: Vec<Arc<dyn Middleware>>,
    body: BodyConfig,
    timeout: RouteTimeout,
    meta: http::Extensions,
}

macro_rules! route_methods {
    ($($name:ident => $method:ident),* $(,)?) => {
        $(
            #[doc = concat!("Registers a ", stringify!($method), " handler for this path.")]
            pub fn $name<F, R>(&mut self, handler: F) -> &mut Self
            where
                F: Fn(Request) -> R + Send + Sync + 'static,
                R: Future + Send + 'static,
                R::Output: IntoResponse,
            {
                self.add(Method::$method, Arc::new(handler))
            }
        )*
    };
}

impl RouteBuilder<'_> {
    route_methods! {
        get => GET,
        post => POST,
        put => PUT,
        patch => PATCH,
        delete => DELETE,
        head => HEAD,
        options => OPTIONS,
        connect => CONNECT,
        trace => TRACE,
    }

    fn add(&mut self, method: Method, handler: Arc<dyn Handler>) -> &mut Self {
        let endpoint = Endpoint {
            handler,
            middlewares: self.middlewares.clone(),
            body: self.body,
            timeout: self.timeout,
            meta: self.meta.clone(),
        };
        self.router.routes.entry(self.path.clone()).or_default().insert(method, endpoint);
        if !self.methods.contains(&method) {
            self.methods.push(method);
        }
        self
    }

    /// Applies `f` to the endpoints already registered through this builder.
    fn update(&mut self, f: impl Fn(&mut Endpoint)) {
        if let Some(endpoints) = self.router.routes.get_mut(&self.path) {
            for method in &self.methods {
                if let Some(endpoint) = endpoints.get_mut(method) {
                    f(endpoint);
                }
            }
        }
    }

    /// Adds middleware for this builder's handlers.
    pub fn middleware(&mut self, middleware: impl Middleware + 'static) -> &mut Self {
        if let Err(error) = middleware.validate() {
            self.router.middlewares.errors.push(error);
        }
        let middleware: Arc<dyn Middleware> = Arc::new(middleware);
        self.middlewares.push(Arc::clone(&middleware));
        self.update(|endpoint| endpoint.middlewares.push(Arc::clone(&middleware)));
        self
    }

    /// Adds a tower [`Layer`](tower_layer::Layer) as middleware for this
    /// builder's handlers.
    pub fn layer<L>(&mut self, layer: L) -> &mut Self
    where
        crate::tower::TowerMiddleware<L>: Middleware,
    {
        self.middleware(crate::tower::TowerMiddleware::new(layer))
    }

    /// The maximum request body size for these handlers, overriding
    /// `Server::max_body_size`. Larger requests get 413.
    pub fn body_limit(&mut self, bytes: usize) -> &mut Self {
        self.body.limit = Some(bytes);
        self.update(|endpoint| endpoint.body.limit = Some(bytes));
        self
    }

    /// Don't buffer the request body: read it in the handler with
    /// [`Request::body_stream`] or [`Request::multipart`] as it arrives.
    /// `req.body` stays empty, also for middleware.
    pub fn stream_body(&mut self) -> &mut Self {
        self.body.stream = true;
        self.update(|endpoint| endpoint.body.stream = true);
        self
    }

    /// How long these handlers, with their route and router middleware,
    /// may take to produce a response, overriding
    /// [`Server::handler_timeout`](crate::Server::handler_timeout). After
    /// that the handler is cancelled and the client gets 503
    /// ([`ServerError::HandlerTimeout`]).
    ///
    /// The time runs from when routing hands over the request: a buffered
    /// body has already been read (under `request_timeout`), while a
    /// [`stream_body`](RouteBuilder::stream_body) handler reads the body
    /// within it. A streamed response, such as [`sse`](crate::sse), only
    /// has to *start* in time.
    ///
    /// ```rust
    /// use axeon::Server;
    /// use std::time::Duration;
    ///
    /// let mut app = Server::new();
    /// app.handler_timeout(Duration::from_secs(10));
    /// app.route("/v1/products").timeout(Duration::from_secs(2)).get(|_req| async { "[]" });
    /// app.route("/documents").stream_body().timeout(Duration::from_secs(300)).post(|_req| async { "stored" });
    /// ```
    pub fn timeout(&mut self, limit: Duration) -> &mut Self {
        self.set_timeout(RouteTimeout::After(limit))
    }

    /// No time limit for these handlers, even with a
    /// [`Server::handler_timeout`](crate::Server::handler_timeout).
    pub fn no_timeout(&mut self) -> &mut Self {
        self.set_timeout(RouteTimeout::Never)
    }

    /// Attaches a value to these handlers, replacing one of the same type,
    /// for tools that read [`Router::routes`]: e.g. axeon-openapi stores
    /// each route's API documentation this way. It doesn't change how
    /// requests are handled.
    pub fn meta<T: Clone + Send + Sync + 'static>(&mut self, value: T) -> &mut Self {
        self.meta.insert(value.clone());
        self.update(|endpoint| {
            endpoint.meta.insert(value.clone());
        });
        self
    }

    fn set_timeout(&mut self, timeout: RouteTimeout) -> &mut Self {
        self.timeout = timeout;
        self.update(|endpoint| endpoint.timeout = timeout);
        self
    }
}

/// The routes for a matched path and its decoded parameters.
pub(crate) type RouteMatch<'a> = (&'a HashMap<Method, Route>, Params, Arc<str>);

/// Routes compiled for lookup, built once when the server starts.
pub(crate) struct CompiledRouter {
    matcher: matchit::Router<usize>,
    routes: Vec<HashMap<Method, Route>>,
    /// The registered path of each route, e.g. `/users/:id`.
    patterns: Vec<Arc<str>>,
    /// Each route's parameter names, in path order (shared, not allocated per request).
    param_names: Vec<Vec<Arc<str>>>,
}

/// The route pattern a request matched (e.g. `/users/:id`), for global
/// middleware, which runs before routing: watch the request on the way in,
/// read the pattern once the response is back. For metrics and tracing,
/// where the pattern keeps label cardinality low.
///
/// ```rust
/// use axeon::middleware::from_fn;
/// use axeon::{MatchedRoute, Server};
///
/// let mut app = Server::new();
/// app.middleware(from_fn(|mut req, next| async move {
///     let route = MatchedRoute::watch(&mut req);
///     let response = next.handle(req).await?;
///     println!("{} -> {}", route.get().unwrap_or("(no route)"), response.status);
///     Ok(response)
/// }));
/// app.get("/users/:id", |_req| async { "user" });
/// ```
#[derive(Clone, Default)]
pub struct MatchedRoute(pub(crate) Arc<std::sync::OnceLock<Arc<str>>>);

impl MatchedRoute {
    /// The request's slot, created if no earlier middleware watches it, so
    /// several middleware share one.
    pub fn watch(req: &mut Request) -> MatchedRoute {
        if let Some(route) = req.extension::<MatchedRoute>() {
            return route.clone();
        }
        let route = MatchedRoute::default();
        req.insert_extension(route.clone());
        route
    }

    /// The matched pattern, once routing has run; `None` for requests no
    /// route matched (404s, static files).
    pub fn get(&self) -> Option<&str> {
        self.0.get().map(|pattern| &**pattern)
    }
}

impl CompiledRouter {
    /// The body settings of the route that will handle this request.
    pub(crate) fn body_config(&self, path: &str, method: Method) -> BodyConfig {
        let Ok(matched) = self.matcher.at(path) else { return BodyConfig::default() };
        let routes = &self.routes[*matched.value];
        routes
            .get(&method)
            .or_else(|| (method == Method::HEAD).then(|| routes.get(&Method::GET)).flatten())
            .map(|route| route.body)
            .unwrap_or_default()
    }

    /// The routes for `path` and its percent-decoded parameters.
    pub(crate) fn lookup(&self, path: &str) -> Option<RouteMatch<'_>> {
        let matched = self.matcher.at(path).ok()?;
        let index = *matched.value;
        let names = &self.param_names[index];
        let entries = matched
            .params
            .iter()
            .enumerate()
            .map(|(i, (key, value))| {
                let name = match names.get(i) {
                    Some(name) if &**name == key => Arc::clone(name),
                    _ => Arc::from(key),
                };
                let value = if value.contains('%') {
                    urlencoding::decode(value).map(|v| v.into_owned()).unwrap_or_else(|_| value.to_string())
                } else {
                    value.to_string()
                };
                (name, value)
            })
            .collect();
        Some((&self.routes[index], Params::from_entries(entries), Arc::clone(&self.patterns[index])))
    }
}

fn normalize(path: &str) -> String {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    }
}

/// Converts `/users/:id/*rest` to matchit's `/users/{id}/{*rest}`, escaping
/// literal braces.
fn to_matchit(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if let Some(name) = segment.strip_prefix(':').filter(|n| !n.is_empty()) {
                format!("{{{}}}", name)
            } else if let Some(name) = segment.strip_prefix('*').filter(|n| !n.is_empty()) {
                format!("{{*{}}}", name)
            } else {
                segment.replace('{', "{{").replace('}', "}}")
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_paths_to_matchit_syntax() {
        assert_eq!(to_matchit("/"), "/");
        assert_eq!(to_matchit("/users/:id"), "/users/{id}");
        assert_eq!(to_matchit("/files/*path"), "/files/{*path}");
        assert_eq!(to_matchit("/a/{b}"), "/a/{{b}}");
        assert_eq!(to_matchit("/a:b"), "/a:b");
    }

    #[test]
    fn static_routes_win_over_params() {
        let mut router = Router::new();
        router.get("/users/:id", |_req| async { crate::Response::text("param") });
        router.get("/users/new", |_req| async { crate::Response::text("static") });
        let compiled = router.compile(None, None).unwrap();
        let (_, params, pattern) = compiled.lookup("/users/new").unwrap();
        assert_eq!(&*pattern, "/users/new");
        assert!(params.is_empty());
        let (_, params, pattern) = compiled.lookup("/users/a%20b").unwrap();
        assert_eq!(&*pattern, "/users/:id");
        assert_eq!(params.get("id"), Some("a b"));
        assert!(compiled.lookup("/users/1/extra").is_none());
    }

    #[test]
    fn conflicting_routes_are_reported() {
        let mut router = Router::new();
        router.get("/users/:id", |_req| async { crate::Response::text("a") });
        router.get("/users/:name", |_req| async { crate::Response::text("b") });
        assert!(router.compile(None, None).is_err());
    }

    mod properties {
        use super::super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(1000))]

            /// Arbitrary route patterns either compile or report an error,
            /// and arbitrary request paths never panic during lookup.
            #[test]
            fn compile_and_lookup_never_panic(
                patterns in proptest::collection::vec(r"(/[:*]?[a-z{}.]{0,5}){1,4}", 1..8),
                paths in proptest::collection::vec(r"(/[a-z%0-9.{}]{0,6}){0,5}", 1..16),
            ) {
                let mut router = Router::new();
                for pattern in &patterns {
                    router.get(pattern, |_req| async { "ok" });
                }
                if let Ok(compiled) = router.compile(None, None) {
                    for path in &paths {
                        let _ = compiled.lookup(path);
                        let _ = compiled.body_config(path, Method::GET);
                    }
                }
            }
        }
    }
}
