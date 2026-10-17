use crate::error::{ServerError, ServerResult};
use crate::http::Request;
use crate::http::Response;
use bytes::Bytes;
use futures::future::BoxFuture;
use serde::Serialize;
use std::borrow::Cow;
use std::future::Future;

pub(crate) type HttpResponse = ServerResult<Response>;

/// Anything a handler or middleware can return.
///
/// ```rust
/// use axeon::{Html, Json, Server, ServerError, json};
///
/// let mut app = Server::new();
/// app.get("/text", |_req| async { "plain text" });
/// app.get("/json", |_req| async { Json(json!({ "ok": true })) });
/// app.get("/html", |_req| async { Html("<h1>Hi</h1>") });
/// app.post("/items", |_req| async { (201, "created") });
/// app.get("/maybe", |_req| async { None::<String> }); // 404
/// app.get("/fallible", |req| async move {
///     let id: u32 = req.param("id")?;
///     Ok::<_, ServerError>(format!("item {}", id))
/// });
/// ```
pub trait IntoResponse {
    fn into_response(self) -> ServerResult<Response>;
}

impl IntoResponse for Response {
    fn into_response(self) -> ServerResult<Response> {
        Ok(self)
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> ServerResult<Response> {
        Err(self)
    }
}

/// `Ok` is converted as usual; `Err` goes through the error handler.
impl<T, E> IntoResponse for Result<T, E>
where
    T: IntoResponse,
    E: Into<ServerError>,
{
    fn into_response(self) -> ServerResult<Response> {
        self.map_err(Into::into)?.into_response()
    }
}

/// `None` becomes 404 Not Found.
impl<T: IntoResponse> IntoResponse for Option<T> {
    fn into_response(self) -> ServerResult<Response> {
        self.ok_or(ServerError::NotFound)?.into_response()
    }
}

/// An empty 204 No Content response.
impl IntoResponse for () {
    fn into_response(self) -> ServerResult<Response> {
        Ok(Response::no_content())
    }
}

impl IntoResponse for &'static str {
    fn into_response(self) -> ServerResult<Response> {
        Response::text(self)
    }
}

impl IntoResponse for String {
    fn into_response(self) -> ServerResult<Response> {
        let mut response = Response::new(200);
        response.header("Content-Type", "text/plain; charset=utf-8").body(self);
        Ok(response)
    }
}

impl IntoResponse for Cow<'static, str> {
    fn into_response(self) -> ServerResult<Response> {
        self.into_owned().into_response()
    }
}

impl IntoResponse for Vec<u8> {
    fn into_response(self) -> ServerResult<Response> {
        Response::binary("application/octet-stream", self)
    }
}

impl IntoResponse for Bytes {
    fn into_response(self) -> ServerResult<Response> {
        let mut response = Response::new(200);
        response.header("Content-Type", "application/octet-stream").body_bytes(self);
        Ok(response)
    }
}

impl IntoResponse for serde_json::Value {
    fn into_response(self) -> ServerResult<Response> {
        Response::ok(&self)
    }
}

/// Overrides the status of the inner response, e.g. `(201, Json(user))`.
impl<T: IntoResponse> IntoResponse for (u16, T) {
    fn into_response(self) -> ServerResult<Response> {
        let (status, inner) = self;
        let mut response = inner.into_response()?;
        response.status = status;
        Ok(response)
    }
}

/// A JSON response body: `Json(value)` serializes `value` with serde.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> ServerResult<Response> {
        Response::ok(&self.0)
    }
}

/// An HTML response body.
#[derive(Debug, Clone, Copy, Default)]
pub struct Html<T>(pub T);

impl<T: AsRef<str>> IntoResponse for Html<T> {
    fn into_response(self) -> ServerResult<Response> {
        Response::html(self.0)
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
    R: Future + Send + 'static,
    R::Output: IntoResponse,
{
    fn handle(&self, req: Request) -> BoxFuture<'static, HttpResponse> {
        let future = (self)(req);
        Box::pin(async move { future.await.into_response() })
    }
}
