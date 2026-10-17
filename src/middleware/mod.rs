mod security;
mod compression;

use crate::http::Request;
pub use security::{RateLimitConfig, RateLimiter, SecurityConfig, SecurityHeaders, CorsConfig, Cors};
pub use compression::{CompressionConfig, CompressionMiddleware};

use crate::handler::{Handler, HttpResponse, IntoResponse};
use futures::future::BoxFuture;
use std::sync::Arc;

/// The middleware stack and handler for one route, built once at
/// registration and shared by every request.
pub(crate) struct Chain {
    pub(crate) middlewares: Arc<[Arc<dyn Middleware>]>,
    pub(crate) handler: Arc<dyn Handler>,
}

/// The rest of the middleware chain. Calling [`Next::handle`] runs the next
/// middleware, or the route handler after the last one.
///
/// Cloning is cheap: it is a reference to the route's chain plus a position.
#[derive(Clone)]
pub struct Next {
    chain: Arc<Chain>,
    index: usize,
}

impl Next {
    /// A `Next` that calls `handler` directly, with no middleware.
    pub fn new<F, R>(handler: F) -> Self
    where
        F: Fn(Request) -> R + Send + Sync + 'static,
        R: IntoResponse,
    {
        Self::start(Arc::new(Chain {
            middlewares: Arc::new([]),
            handler: Arc::new(handler),
        }))
    }

    pub(crate) fn start(chain: Arc<Chain>) -> Self {
        Self { chain, index: 0 }
    }

    pub fn handle(&self, req: Request) -> MiddlewareResult {
        match self.chain.middlewares.get(self.index) {
            Some(middleware) => middleware.call(
                req,
                Next {
                    chain: Arc::clone(&self.chain),
                    index: self.index + 1,
                },
            ),
            None => self.chain.handler.handle(req),
        }
    }
}

pub type MiddlewareResult = BoxFuture<'static, HttpResponse>;

/// Request/response middleware. One instance is shared by all requests, so
/// any state it keeps (counters, caches) is shared too.
pub trait Middleware: Send + Sync + 'static {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult;
}

/// Middleware registered on a router, applied to routes added after it.
#[derive(Clone, Default)]
pub(crate) struct MiddlewareManager {
    pub(crate) middlewares: Vec<Arc<dyn Middleware>>,
}

impl MiddlewareManager {
    pub fn add<M: Middleware + 'static>(&mut self, middleware: M) {
        self.middlewares.push(Arc::new(middleware));
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
