use std::io;
use std::fmt;

/// An error that becomes an HTTP error response.
///
/// Return it from handlers and middleware (`Err(ServerError::NotFound)`, or
/// with `?`). The default response is the status below with a JSON body
/// `{"error": {"message": ..., "status": ...}}`; customize it with
/// [`Server::on_error`](crate::Server::on_error). Messages of 5xx errors
/// caused by IO or panics are never sent to clients.
#[derive(Debug)]
#[non_exhaustive]
pub enum ServerError {
    /// 500: an IO error (details are logged, not sent).
    IoError(io::Error),
    /// 422: input that parsed but doesn't fit the expected shape.
    ValidationError(String),
    /// 404 Not Found.
    NotFound,
    /// 400 Bad Request.
    BadRequest(String),
    /// 401 Unauthorized.
    Unauthorized(String),
    /// 403 Forbidden.
    Forbidden(String),
    /// 500 Internal Server Error.
    InternalError(String),
    /// 409 Conflict.
    Conflict(String),
    /// 500: a handler panicked (details are logged, not sent).
    PanicError(String),
    /// 429, with how long the client should wait (sent as `Retry-After`).
    TooManyRequests {
        /// When the client may retry.
        retry_after: Option<std::time::Duration>,
    },
    /// The path exists but not for this method. Holds the value of the `Allow` header.
    MethodNotAllowed(String),
    /// 408: the client sent the request too slowly.
    RequestTimeout,
    /// 503: the handler didn't respond within its
    /// [timeout](crate::Server::handler_timeout).
    HandlerTimeout,
    /// 413: the body exceeds the size limit.
    PayloadTooLarge,
    /// 414: the request URI is too long.
    UriTooLong,
    /// 431: the request headers are too large.
    HeadersTooLarge,
    /// 501: e.g. an unknown HTTP method.
    NotImplemented(String),
    /// 415: the body's content type isn't accepted.
    UnsupportedMediaType(String),
    /// Any other status (e.g. 402, 410, 503) with a message.
    Status(u16, String),
    /// 422 with error messages per field, from [`Validate`](crate::validate::Validate).
    Invalid(crate::validate::ValidationErrors),
}

impl ServerError {
    /// The HTTP status code for this error.
    pub fn status_code(&self) -> u16 {
        match self {
            ServerError::BadRequest(_) => 400,
            ServerError::Unauthorized(_) => 401,
            ServerError::Forbidden(_) => 403,
            ServerError::NotFound => 404,
            ServerError::MethodNotAllowed(_) => 405,
            ServerError::RequestTimeout => 408,
            ServerError::Conflict(_) => 409,
            ServerError::PayloadTooLarge => 413,
            ServerError::UnsupportedMediaType(_) => 415,
            ServerError::UriTooLong => 414,
            ServerError::ValidationError(_) | ServerError::Invalid(_) => 422,
            ServerError::TooManyRequests { .. } => 429,
            ServerError::HeadersTooLarge => 431,
            ServerError::IoError(_)
            | ServerError::InternalError(_)
            | ServerError::PanicError(_) => 500,
            ServerError::NotImplemented(_) => 501,
            ServerError::HandlerTimeout => 503,
            ServerError::Status(status, _) if (400..600).contains(status) => *status,
            ServerError::Status(..) => 500,
        }
    }

    /// The message that is safe to send to clients. Panic and IO details stay server-side.
    pub(crate) fn public_message(&self) -> String {
        match self {
            ServerError::IoError(_) | ServerError::PanicError(_) => "Internal server error".to_string(),
            other => other.to_string(),
        }
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerError::IoError(err) => write!(f, "IO error: {}", err),
            ServerError::ValidationError(msg) => write!(f, "Validation error: {}", msg),
            ServerError::NotFound => write!(f, "Not found"),
            ServerError::BadRequest(msg) => write!(f, "Bad request: {}", msg),
            ServerError::Unauthorized(msg) => write!(f, "Unauthorized: {}", msg),
            ServerError::Forbidden(msg) => write!(f, "Forbidden: {}", msg),
            ServerError::Conflict(msg) => write!(f, "Conflict: {}", msg),
            ServerError::InternalError(msg) => write!(f, "Internal error: {}", msg),
            ServerError::PanicError(msg) => write!(f, "Panic: {}", msg),
            ServerError::TooManyRequests { .. } => write!(f, "Too many requests"),
            ServerError::MethodNotAllowed(_) => write!(f, "Method not allowed"),
            ServerError::RequestTimeout => write!(f, "Request timeout"),
            ServerError::HandlerTimeout => write!(f, "The server took too long to respond"),
            ServerError::PayloadTooLarge => write!(f, "Payload too large"),
            ServerError::UriTooLong => write!(f, "URI too long"),
            ServerError::HeadersTooLarge => write!(f, "Request header fields too large"),
            ServerError::NotImplemented(msg) => write!(f, "Not implemented: {}", msg),
            ServerError::UnsupportedMediaType(msg) => write!(f, "Unsupported media type: {}", msg),
            ServerError::Invalid(errors) => write!(f, "Validation failed: {}", errors),
            ServerError::Status(_, msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ServerError::IoError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for ServerError {
    fn from(err: io::Error) -> Self {
        ServerError::IoError(err)
    }
}

/// A `Result` whose error is a [`ServerError`].
pub type ServerResult<T> = Result<T, ServerError>;
