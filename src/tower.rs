//! Interoperability with [tower](https://docs.rs/tower): use tower layers
//! (such as everything in [`tower-http`](https://docs.rs/tower-http)) as
//! Axeon middleware, and run an Axeon app as a tower `Service`.
//!
//! ```rust
//! use axeon::Server;
//! use std::time::Duration;
//! use tower_http::timeout::TimeoutLayer;
//!
//! let mut app = Server::new();
//! // Any tower layer works where middleware does: globally, on a Router,
//! // or on one route.
//! app.layer(TimeoutLayer::with_status_code(http::StatusCode::REQUEST_TIMEOUT, Duration::from_secs(10)));
//! app.get("/", |_req| async { "hello" });
//! ```

use crate::app::App;
use crate::body::{BoxError, RequestBody, ResponseBody};
use crate::error::ServerError;
use crate::message::{Request, Response};
use crate::middleware::{Middleware, MiddlewareResult, Next};
use bytes::Bytes;
use futures::future::BoxFuture;
use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};
use tower_layer::Layer;
use tower_service::Service;

/// The rest of the middleware chain, as a tower `Service`. Tower layers used
/// with [`Server::layer`](crate::Server::layer) wrap this.
#[derive(Clone)]
pub struct NextService {
    next: Next,
}

impl<B> Service<http::Request<B>> for NextService
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    type Response = http::Response<ResponseBody>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let next = self.next.clone();
        Box::pin(async move {
            let response = match Request::from_http(request).await {
                Ok(request) => next.handle(request).await.unwrap_or_else(Response::error),
                Err(error) => Response::error(error),
            };
            Ok(response.into_http())
        })
    }
}

/// Runs a tower [`Layer`] as Axeon middleware. Usually created through
/// [`Server::layer`](crate::Server::layer),
/// [`Router::layer`](crate::Router::layer) or
/// [`RouteBuilder::layer`](crate::RouteBuilder::layer).
pub struct TowerMiddleware<L> {
    layer: L,
}

impl<L> TowerMiddleware<L> {
    /// Wraps a tower layer.
    pub fn new(layer: L) -> Self {
        Self { layer }
    }
}

fn service_error(error: BoxError) -> ServerError {
    match error.downcast::<ServerError>() {
        Ok(error) => *error,
        Err(error) => {
            tracing::error!(error = %error, "tower service failed");
            ServerError::InternalError(error.to_string())
        }
    }
}

impl<L, S, ResBody> Middleware for TowerMiddleware<L>
where
    L: Layer<NextService, Service = S> + Send + Sync + 'static,
    S: Service<http::Request<RequestBody>, Response = http::Response<ResBody>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<BoxError>,
    ResBody: http_body::Body<Data = Bytes> + Send + 'static,
    ResBody::Error: Into<BoxError>,
{
    fn call(&self, request: Request, next: Next) -> MiddlewareResult {
        let mut service = self.layer.layer(NextService { next });
        Box::pin(async move {
            futures::future::poll_fn(|cx| service.poll_ready(cx))
                .await
                .map_err(|e| service_error(e.into()))?;
            let response = service.call(request.into_http()).await.map_err(|e| service_error(e.into()))?;
            Ok(Response::from_http(response))
        })
    }
}

/// An Axeon app as a tower `Service`, from [`App::into_service`]: mount it
/// in another framework (e.g. axum's `Router::nest_service`) or serve it
/// with your own hyper setup. Request size limits, timeouts on the body,
/// middleware and routing all apply; `req.remote_addr` is `None`.
#[derive(Clone)]
pub struct AppService {
    app: Arc<App>,
}

impl App {
    /// Converts the app into a tower [`Service`].
    ///
    /// ```rust
    /// use axeon::Server;
    /// use tower::ServiceExt;
    ///
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// let mut app = Server::new();
    /// app.get("/", |_req| async { "hello" });
    /// let service = app.into_app().await.unwrap().into_service();
    ///
    /// let request = http::Request::get("/").body(String::new()).unwrap();
    /// let response = service.oneshot(request).await.unwrap();
    /// assert_eq!(response.status(), 200);
    /// # });
    /// ```
    pub fn into_service(self) -> AppService {
        AppService { app: Arc::new(self) }
    }
}

impl<B> Service<http::Request<B>> for AppService
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    type Response = http::Response<ResponseBody>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let app = Arc::clone(&self.app);
        Box::pin(async move { Ok(app.respond(request, None, None).await) })
    }
}
