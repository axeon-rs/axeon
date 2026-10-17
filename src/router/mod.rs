use crate::handler::{ErrorHandler, Handler, IntoResponse};
use crate::http::{Method, Request};
use crate::middleware::{Chain, Middleware, MiddlewareManager, MiddlewareResult, Next};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct Route {
    pub(crate) chain: Arc<Chain>,
    pub(crate) body: BodyConfig,
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
        Next::start(Arc::clone(&self.chain)).handle(req)
    }
}

/// A registered handler and the middleware of the routers it was mounted from.
#[derive(Clone)]
pub(crate) struct Endpoint {
    handler: Arc<dyn Handler>,
    /// Outermost first: mounted routers' middleware, then the route's own.
    middlewares: Vec<Arc<dyn Middleware>>,
    body: BodyConfig,
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
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::GET, path, handler);
        self
    }

    pub fn post<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::POST, path, handler);
        self
    }

    pub fn put<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::PUT, path, handler);
        self
    }

    pub fn patch<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::PATCH, path, handler);
        self
    }

    pub fn delete<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::DELETE, path, handler);
        self
    }

    pub fn head<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::HEAD, path, handler);
        self
    }

    pub fn connect<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::CONNECT, path, handler);
        self
    }

    pub fn options<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        self.add(Method::OPTIONS, path, handler);
        self
    }

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
        let endpoint = Endpoint { handler: Arc::new(handler), middlewares: Vec::new(), body: BodyConfig::default() };
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
        }
    }

    /// Adds a tower [`Layer`](tower_layer::Layer) as middleware for every
    /// route of this router.
    pub fn layer<L>(&mut self, layer: L)
    where
        crate::tower::TowerMiddleware<L>: Middleware,
    {
        self.middleware(crate::tower::TowerMiddleware::new(layer));
    }

    /// Adds middleware for every route of this router.
    pub fn middleware(&mut self, middleware: impl Middleware + 'static) {
        self.middlewares.add(middleware);
    }

    /// Adds every route of `router` under the `path` prefix. This router's
    /// middleware runs before the mounted router's own.
    pub fn mount(&mut self, path: &str, router: Router) {
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
                entry.insert(method, Endpoint { handler: endpoint.handler, middlewares, body: endpoint.body });
            }
        }
    }

    /// Builds the radix-tree matcher and the route chains used to serve
    /// requests.
    pub(crate) fn compile(&self, on_error: Option<ErrorHandler>) -> Result<CompiledRouter, String> {
        let mut matcher = matchit::Router::new();
        let mut routes = Vec::with_capacity(self.routes.len());
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
                    (*method, Route { chain: Arc::new(chain), body: endpoint.body })
                })
                .collect();
            routes.push(methods);
        }
        Ok(CompiledRouter { matcher, routes })
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
        let endpoint = Endpoint { handler, middlewares: self.middlewares.clone(), body: self.body };
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
}

/// The routes for a matched path and its decoded parameters.
pub(crate) type RouteMatch<'a> = (&'a HashMap<Method, Route>, HashMap<String, String>);

/// Routes compiled for lookup, built once when the server starts.
pub(crate) struct CompiledRouter {
    matcher: matchit::Router<usize>,
    routes: Vec<HashMap<Method, Route>>,
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
        let params = matched
            .params
            .iter()
            .map(|(name, value)| {
                let value = urlencoding::decode(value)
                    .map(|v| v.into_owned())
                    .unwrap_or_else(|_| value.to_string());
                (name.to_string(), value)
            })
            .collect();
        Some((&self.routes[*matched.value], params))
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
        let compiled = router.compile(None).unwrap();
        let (_, params) = compiled.lookup("/users/new").unwrap();
        assert!(params.is_empty());
        let (_, params) = compiled.lookup("/users/a%20b").unwrap();
        assert_eq!(params.get("id").map(String::as_str), Some("a b"));
        assert!(compiled.lookup("/users/1/extra").is_none());
    }

    #[test]
    fn conflicting_routes_are_reported() {
        let mut router = Router::new();
        router.get("/users/:id", |_req| async { crate::Response::text("a") });
        router.get("/users/:name", |_req| async { crate::Response::text("b") });
        assert!(router.compile(None).is_err());
    }
}
