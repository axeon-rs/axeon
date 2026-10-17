use std::io;
use std::fmt;

#[derive(Debug)]
#[non_exhaustive]
pub enum ServerError {
    IoError(io::Error),
    ParseError(String),
    ValidationError(String),
    NotFound,
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    InternalError(String),
    Conflict(String),
    PanicError(String),
    TooManyRequests,
    /// The path exists but not for this method. Holds the value of the `Allow` header.
    MethodNotAllowed(String),
    RequestTimeout,
    PayloadTooLarge,
    UriTooLong,
    HeadersTooLarge,
    NotImplemented(String),
}

impl ServerError {
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
            ServerError::UriTooLong => 414,
            ServerError::ParseError(_) => 422,
            ServerError::ValidationError(_) => 422,
            ServerError::TooManyRequests => 429,
            ServerError::HeadersTooLarge => 431,
            ServerError::IoError(_)
            | ServerError::InternalError(_)
            | ServerError::PanicError(_) => 500,
            ServerError::NotImplemented(_) => 501,
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
            ServerError::ParseError(msg) => write!(f, "Parse error: {}", msg),
            ServerError::ValidationError(msg) => write!(f, "Validation error: {}", msg),
            ServerError::NotFound => write!(f, "Not found"),
            ServerError::BadRequest(msg) => write!(f, "Bad request: {}", msg),
            ServerError::Unauthorized(msg) => write!(f, "Unauthorized: {}", msg),
            ServerError::Forbidden(msg) => write!(f, "Forbidden: {}", msg),
            ServerError::Conflict(msg) => write!(f, "Conflict: {}", msg),
            ServerError::InternalError(msg) => write!(f, "Internal error: {}", msg),
            ServerError::PanicError(msg) => write!(f, "Panic: {}", msg),
            ServerError::TooManyRequests => write!(f, "Too many requests"),
            ServerError::MethodNotAllowed(_) => write!(f, "Method not allowed"),
            ServerError::RequestTimeout => write!(f, "Request timeout"),
            ServerError::PayloadTooLarge => write!(f, "Payload too large"),
            ServerError::UriTooLong => write!(f, "URI too long"),
            ServerError::HeadersTooLarge => write!(f, "Request header fields too large"),
            ServerError::NotImplemented(msg) => write!(f, "Not implemented: {}", msg),
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

pub type ServerResult<T> = Result<T, ServerError>;
