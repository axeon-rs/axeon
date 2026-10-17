use crate::error::{ServerError, ServerResult};
use crate::message::Request;
use crate::message::Response;
use bytes::Bytes;
use futures::future::BoxFuture;
use serde::Serialize;
use std::any::Any;
use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;

pub(crate) type HttpResponse = ServerResult<Response>;

/// The application's error handler (`Server::on_error`).
pub(crate) type ErrorHandler = Arc<dyn Fn(ServerError) -> Response + Send + Sync>;

/// Turns an error into a response with the error handler, or the default
/// JSON error body.
pub(crate) fn error_response(handler: Option<&ErrorHandler>, error: ServerError) -> Response {
    match handler {
        Some(handler) => handler(error),
        None => Response::error(error),
    }
}

/// Logs a caught panic and turns it into a `PanicError`.
pub(crate) fn panic_error(payload: Box<dyn Any + Send>) -> ServerError {
    let message = if let Some(msg) = payload.downcast_ref::<&str>() {
        msg.to_string()
    } else if let Some(msg) = payload.downcast_ref::<String>() {
        msg.clone()
    } else {
        "Unknown panic".to_string()
    };
    tracing::error!(panic = %message, "handler panicked");
    ServerError::PanicError(message)
}

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
    /// Converts into a response, or an error for the error handler.
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
        Ok(Response::text(self))
    }
}

impl IntoResponse for String {
    fn into_response(self) -> ServerResult<Response> {
        Ok(Response::text(self))
    }
}

impl IntoResponse for Cow<'static, str> {
    fn into_response(self) -> ServerResult<Response> {
        self.into_owned().into_response()
    }
}

impl IntoResponse for Vec<u8> {
    fn into_response(self) -> ServerResult<Response> {
        Ok(Response::binary("application/octet-stream", self))
    }
}

impl IntoResponse for Bytes {
    fn into_response(self) -> ServerResult<Response> {
        Ok(Response::binary("application/octet-stream", self))
    }
}

impl IntoResponse for serde_json::Value {
    fn into_response(self) -> ServerResult<Response> {
        Json(self).into_response()
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

/// Serialization failures go to the error handler as a 500.
impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> ServerResult<Response> {
        let json = serde_json::to_vec(&self.0)
            .map_err(|e| ServerError::InternalError(format!("failed to serialize response: {}", e)))?;
        Ok(Response::new(200).with_header("Content-Type", "application/json").with_body(json))
    }
}

/// An HTML response body.
#[derive(Debug, Clone, Copy, Default)]
pub struct Html<T>(pub T);

impl<T: AsRef<str>> IntoResponse for Html<T> {
    fn into_response(self) -> ServerResult<Response> {
        Ok(Response::html(self.0.as_ref()))
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
