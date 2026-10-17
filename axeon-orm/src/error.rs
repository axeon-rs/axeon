use std::fmt;

/// A `Result` with [`Error`] as the error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong talking to the database.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// No row matched, e.g. from `find_or_fail`, `update` or `delete`.
    NotFound,
    /// A unique constraint rejected the write; holds the database message.
    UniqueViolation(String),
    /// A foreign key constraint rejected the write; holds the database message.
    ForeignKeyViolation(String),
    /// A column couldn't be read into its Rust type.
    Decode {
        /// The column name.
        column: String,
        /// Why decoding failed.
        message: String,
    },
    /// A migration failed or doesn't match what was applied before.
    Migration(String),
    /// Bad configuration, e.g. an unsupported database URL.
    Config(String),
    /// The transaction was already committed, rolled back, or is in use.
    Transaction(String),
    /// Any other database or connection error.
    Database(sqlx::Error),
}

impl Error {
    pub(crate) fn decode(column: &str, error: impl fmt::Display) -> Self {
        Error::Decode { column: column.to_string(), message: error.to_string() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => f.write_str("record not found"),
            Error::UniqueViolation(msg) => write!(f, "unique constraint violated: {}", msg),
            Error::ForeignKeyViolation(msg) => write!(f, "foreign key constraint violated: {}", msg),
            Error::Decode { column, message } => write!(f, "cannot decode column `{}`: {}", column, message),
            Error::Migration(msg) => write!(f, "migration error: {}", msg),
            Error::Config(msg) => write!(f, "configuration error: {}", msg),
            Error::Transaction(msg) => write!(f, "transaction error: {}", msg),
            Error::Database(error) => write!(f, "database error: {}", error),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        match error {
            sqlx::Error::RowNotFound => Error::NotFound,
            sqlx::Error::Database(db) if db.is_unique_violation() => Error::UniqueViolation(db.message().to_string()),
            sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
                Error::ForeignKeyViolation(db.message().to_string())
            }
            sqlx::Error::ColumnDecode { index, source } => Error::Decode { column: index, message: source.to_string() },
            error => Error::Database(error),
        }
    }
}
