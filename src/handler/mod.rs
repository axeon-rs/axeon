use crate::error::ServerResult;
use crate::http::Request;
use crate::http::Response;
use futures::future::BoxFuture;
use std::future::Future;

pub(crate) type HttpResponse = ServerResult<Response>;

pub trait IntoResponse {
    fn into_response_future(self) -> BoxFuture<'static, HttpResponse>;
}

impl<F: Future<Output = HttpResponse> + Send + 'static> IntoResponse for F {
    fn into_response_future(self) -> BoxFuture<'static, HttpResponse> {
        Box::pin(self)
    }
}

/// A route handler. Stored behind an `Arc` and shared by every request, so
/// handlers don't need to be `Clone`.
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, req: Request) -> BoxFuture<'static, HttpResponse>;
}

impl<F, R> Handler for F
where
    F: Fn(Request) -> R + Send + Sync + 'static,
    R: IntoResponse,
{
    fn handle(&self, req: Request) -> BoxFuture<'static, HttpResponse> {
        (self)(req).into_response_future()
    }
}
