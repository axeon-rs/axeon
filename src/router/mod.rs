use crate::handler::{ErrorHandler, Handler, IntoResponse};
use crate::http::{Method, Request};
use crate::middleware::{Chain, Middleware, MiddlewareManager, MiddlewareResult, Next};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct Route {
    pub(crate) chain: Arc<Chain>,
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
    middlewares: Vec<Arc<dyn Middleware>>,
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
        let endpoint = Endpoint { handler: Arc::new(handler), middlewares: Vec::new() };
        self.routes.entry(normalize(path)).or_default().insert(method, endpoint);
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
                entry.insert(method, Endpoint { handler: endpoint.handler, middlewares });
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
                    (*method, Route { chain: Arc::new(chain) })
                })
                .collect();
            routes.push(methods);
        }
        Ok(CompiledRouter { matcher, routes })
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
