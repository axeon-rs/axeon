//! Middleware: code that runs around handlers, for logging, security,
//! authentication, compression and more.
//!
//! Write middleware as a closure with [`from_fn`] or as a type implementing
//! [`Middleware`], and add it with [`Server::middleware`](crate::Server::middleware)
//! (every request), [`Router::middleware`](crate::Router::middleware) (that
//! router's routes) or [`RouteBuilder::middleware`](crate::RouteBuilder::middleware)
//! (one route). Tower layers work too, via `layer`.
//!
//! Built in: [`SecurityHeaders`], [`Cors`], [`RateLimiter`] (with shared
//! stores, see [`rate_limit`]), `idempotency::Idempotency` (feature
//! `idempotency`), [`Compression`]
//! (feature `compression`), [`SetRequestId`] and [`RequestLogger`]. More live
//! in [`auth`](crate::auth), [`cookies`](crate::cookies),
//! [`session`](crate::session), [`csrf`](crate::csrf) and
//! [`metrics`](crate::metrics).

mod security;
pub mod rate_limit;
#[cfg(feature = "idempotency")]
pub mod idempotency;
#[cfg(feature = "compression")]
mod compression;
mod observe;

use crate::message::Request;
pub use rate_limit::RateLimiter;
pub use security::{Cors, SecurityHeaders};
#[cfg(feature = "compression")]
pub use compression::Compression;
pub use observe::{RequestId, RequestLogger, SetRequestId};

use crate::handler::{error_response, panic_error, ErrorHandler, Handler, HttpResponse, IntoResponse};
use futures::future::BoxFuture;
use futures::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

/// A middleware stack and the handler at its end, built once when the
/// server starts and shared by every request.
pub(crate) struct Chain {
    pub(crate) middlewares: Arc<[Arc<dyn Middleware>]>,
    pub(crate) handler: Arc<dyn Handler>,
    pub(crate) on_error: Option<ErrorHandler>,
}

/// The rest of the middleware chain. Calling [`Next::handle`] runs the next
/// middleware, or the route handler after the last one.
///
/// Errors and panics further down the chain arrive as responses, already
/// rendered by the error handler (`Server::on_error`), so middleware can add
/// headers to every response, including 404s and 500s.
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
        R: Future + Send + 'static,
        R::Output: IntoResponse,
    {
        Self::start(Arc::new(Chain {
            middlewares: Arc::new([]),
            handler: Arc::new(handler),
            on_error: None,
        }))
    }

    pub(crate) fn start(chain: Arc<Chain>) -> Self {
        Self { chain, index: 0 }
    }

    /// Runs the rest of the chain and returns its response.
    pub fn handle(&self, req: Request) -> MiddlewareResult {
        let started = std::panic::catch_unwind(AssertUnwindSafe(|| match self.chain.middlewares.get(self.index) {
            Some(middleware) => middleware.call(
                req,
                Next {
                    chain: Arc::clone(&self.chain),
                    index: self.index + 1,
                },
            ),
            None => self.chain.handler.handle(req),
        }));
        let future = match started {
            Ok(future) => future,
            Err(payload) => {
                let error = panic_error(payload);
                return Box::pin(async move { Err(error) });
            }
        };
        // The start of a chain returns to routing or to the server, which
        // handle errors themselves; only middleware awaiting `next` needs
        // them rendered, so the conversion is skipped there.
        if self.index == 0 {
            return future;
        }
        let on_error = self.chain.on_error.clone();
        if self.index == self.chain.middlewares.len() {
            // The handler: catch its panics so middleware sees a 500 response.
            return Box::pin(async move {
                let result = AssertUnwindSafe(future).catch_unwind().await.unwrap_or_else(|p| Err(panic_error(p)));
                Ok(result.unwrap_or_else(|error| error_response(on_error.as_ref(), error)))
            });
        }
        Box::pin(async move { Ok(future.await.unwrap_or_else(|error| error_response(on_error.as_ref(), error))) })
    }
}

/// The future a middleware returns: the response, or an error.
pub type MiddlewareResult = BoxFuture<'static, HttpResponse>;

/// Request/response middleware. One instance is shared by all requests, so
/// any state it keeps (counters, caches) is shared too.
pub trait Middleware: Send + Sync + 'static {
    /// Handles a request, usually by calling `next.handle(req)` and
    /// adjusting the request before or the response after.
    fn call(&self, req: Request, next: Next) -> MiddlewareResult;

    /// Checks the configuration. Called when the middleware is added; an
    /// error makes [`Server::serve`](crate::Server::serve) and
    /// [`Server::into_app`](crate::Server::into_app) fail before accepting
    /// requests. The default accepts everything.
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Builds middleware from an async closure, Express style.
///
/// ```rust
/// use axeon::{Server, Response, ServerError};
/// use axeon::middleware::from_fn;
///
/// let mut app = Server::new();
/// // Wraps every request, including 404s and static files.
/// app.middleware(from_fn(|req, next| async move {
///     if req.get_header("x-api-key").is_none() {
///         return Err(ServerError::Unauthorized("missing API key".into()));
///     }
///     let mut res = next.handle(req).await?;
///     res.header("X-Powered-By", "axeon");
///     Ok(res)
/// }));
/// app.get("/", |_req| async { "hello" });
/// ```
pub fn from_fn<F, R>(f: F) -> FromFn<F>
where
    F: Fn(Request, Next) -> R + Send + Sync + 'static,
    R: Future<Output = HttpResponse> + Send + 'static,
{
    FromFn(f)
}

/// Middleware made from a closure by [`from_fn`].
pub struct FromFn<F>(F);

impl<F, R> Middleware for FromFn<F>
where
    F: Fn(Request, Next) -> R + Send + Sync + 'static,
    R: Future<Output = HttpResponse> + Send + 'static,
{
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        Box::pin((self.0)(req, next))
    }
}

/// Middleware registered on the server or a router.
#[derive(Clone, Default)]
pub(crate) struct MiddlewareManager {
    pub(crate) middlewares: Vec<Arc<dyn Middleware>>,
    /// Configuration errors, reported when the server is built.
    pub(crate) errors: Vec<String>,
}

impl MiddlewareManager {
    pub fn add<M: Middleware + 'static>(&mut self, middleware: M) {
        if let Err(error) = middleware.validate() {
            self.errors.push(error);
        }
        self.middlewares.push(Arc::new(middleware));
    }
}
