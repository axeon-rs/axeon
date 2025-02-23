use crate::handler::{Handler, HttpResponse, IntoResponse};
use crate::http::{Method, Request};
use crate::middleware::{Middleware, MiddlewareManager, Next};
use std::collections::HashMap;

#[derive(Clone)]
pub(crate) struct Route {
    pub(crate) middlewares: MiddlewareManager,
    pub(crate) handler: Box<dyn Handler>,
}

impl Route {
    pub async fn handle(&self, req: Request) -> HttpResponse {
        self.middlewares.call(req, Next::new_handler(self.handler.clone())).await
    }
}

#[derive(Clone)]
pub struct Router {
    pub(crate) middlewares: MiddlewareManager,
    pub(crate) routes: HashMap<String, HashMap<Method, Route>>,
    pub(crate) dynamic_routes: Vec<String>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            middlewares: MiddlewareManager::new(),
            routes: HashMap::new(),
            dynamic_routes: Vec::new(),
        }
    }

    pub fn get<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::GET, path, handler);
        self
    }

    pub fn post<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::POST, path, handler);
        self
    }

    pub fn put<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::PUT, path, handler);
        self
    }

    pub fn patch<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::PATCH, path, handler);
        self
    }

    pub fn delete<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::DELETE, path, handler);
        self
    }

    pub fn head<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::HEAD, path, handler);
        self
    }

    pub fn connect<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::CONNECT, path, handler);
        self
    }

    pub fn options<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::OPTIONS, path, handler);
        self
    }

    pub fn trace<F, R>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn(Request) -> R + Send + Clone + Sync + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::TRACE, path, handler);
        self
    }

    fn add<F, R>(&mut self, method: Method, path: &str, handler: F)
    where
        F: Fn(Request) -> R + Send + Sync + Clone + 'static,
        R: IntoResponse,
    {
        let path = path.trim_end_matches('/').to_owned();
        let path = if path.is_empty() { "/".to_owned() } else { path };
        if !self.routes.contains_key(&path) {
            self.routes.insert(path.clone(), HashMap::new());
        }
        if path.contains(":") {
            self.dynamic_routes.push(path.clone());
        }
        self.routes
            .get_mut(&path)
            .unwrap()
            .insert(method, Route {
                middlewares: self.middlewares.clone(),
                handler: Box::new(handler),
            });
    }

    pub fn middleware(&mut self, middleware: impl Middleware + 'static) {
        self.middlewares.add(middleware);
    }

    pub fn mount(&mut self, path: &str, router: Router) {
        for (key, value) in router.routes.into_iter() {
            let path = (path.to_owned() + &key).trim_end_matches('/').to_owned();

            for (method, handler) in value {
                if !self.routes.contains_key(&path) {
                    self.routes.insert(path.clone(), HashMap::new());
                }

                if path.contains(":") {
                    self.dynamic_routes.push(path.clone());
                }

                self.routes
                    .get_mut(&path)
                    .unwrap()
                    .insert(method, Route {
                        middlewares: self.middlewares.clone().append(handler.middlewares.clone()).clone(),
                        handler: handler.handler,
                    });
            }
        }
    }
}
