use std::io;
use std::fmt;

#[derive(Debug)]
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
}

impl ServerError {
    pub fn status_code(&self) -> u16 {
        match self {
            ServerError::BadRequest(_) => 400,
            ServerError::Unauthorized(_) => 401,
            ServerError::Forbidden(_) => 403,
            ServerError::NotFound => 404,
            ServerError::Conflict(_) => 409,
            ServerError::ParseError(_) => 422,
            ServerError::ValidationError(_) => 422,
            ServerError::TooManyRequests => 429,
            ServerError::IoError(_)
            | ServerError::InternalError(_)
            | ServerError::PanicError(_) => 500,
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
