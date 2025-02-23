mod security;
mod compression;

use crate::http::Request;
pub use security::{RateLimitConfig, RateLimiter, SecurityConfig, SecurityHeaders, CorsConfig, Cors};
pub use compression::{CompressionConfig, CompressionMiddleware};

use crate::handler::{Handler, HttpResponse, IntoResponse};
use futures::future::BoxFuture;

#[derive(Clone)]
pub struct Next {
    handler: Box<dyn Handler>,
}

impl Next {
    pub fn new<F, R>(handler: F) -> Self
    where
        F: Fn(Request) -> R + Send + Sync + Clone + 'static,
        R: IntoResponse,

    {
        Self {
            handler: Box::new(handler),
        }
    }

    pub(crate) fn new_handler(handler: Box<dyn Handler>) -> Self {
        Self { handler}
    }

    pub async fn handle(&self, req: Request) -> HttpResponse {
        self.handler.handle(req).await
    }
}

pub type MiddlewareResult = BoxFuture<'static, HttpResponse>;


pub trait Middleware: Send + Sync + 'static {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult;
    fn clone_box(&self) -> Box<dyn Middleware>;
}

impl Clone for Box<dyn Middleware> {
    fn clone(&self) -> Box<dyn Middleware> {
        self.clone_box()
    }
}

#[derive(Clone)]
pub(crate) struct MiddlewareManager {
    pub(crate )middlewares: Vec<Box<dyn Middleware>>,
}


impl MiddlewareManager {
    pub fn new() -> Self {
        Self {
            middlewares: Vec::new(),
        }
    }

    pub fn add<M: Middleware + 'static>(&mut self, middleware: M) {
        self.middlewares.push(Box::new(middleware));
    }

    pub fn append(&mut self, mut other: MiddlewareManager) -> &Self {
        self.middlewares.append(&mut other.middlewares);
        self
    }

    pub async fn call(&self, req: Request, next: Next) -> HttpResponse {
        let mut next = next;
        let mut index = self.middlewares.len();
        while index > 0 {
            index -= 1;
            let middleware = self.middlewares[index].clone();
            next = Next::new_handler(Box::new(move |req| middleware.call(req, next.clone())));
        }
        next.handle(req).await
    }

}


#[macro_export]
macro_rules! middlewares {
    ($handler:expr) => {
       $handler
    };

    ($middleware:expr, $handler:expr) => {
        |req| {
            $middleware.call(req, axeon::middleware::Next::new($handler))
        }
    };

    ($middleware:expr, $($rest:tt)*) => {
        |req| {
            $middleware.call(req, axeon::middleware::Next::new(middlewares!($($rest)*)))
        }
    };

    () => {
        compile_error!("The middlewares! macro requires at least one handler")
    };
}
