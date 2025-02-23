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

pub trait Handler: Send + Sync + 'static {
    fn handle(&self, req: Request) -> BoxFuture<'static, HttpResponse>;

    fn dyn_clone<'s>(&self) -> Box<dyn Handler + 's>
    where
        Self: 's;
}

impl Clone for Box<dyn Handler> {
    fn clone(&self) -> Box<dyn Handler> {
        self.dyn_clone()
    }
}

impl<F, R> Handler for F
where
    F: Fn(Request) -> R + Send + Sync + Clone + 'static,
    R: IntoResponse,
{
    fn handle(&self, req: Request) -> BoxFuture<'static, HttpResponse> {
        (self)(req).into_response_future()
    }

    fn dyn_clone<'s>(&self) -> Box<dyn Handler + 's>
    where
        Self: 's,
    {
        Box::new((*self).clone())
    }
}
