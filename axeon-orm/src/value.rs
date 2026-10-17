//! Column types and the values bound to queries.

use crate::db::Row;
use crate::error::{Error, Result};
use std::hash::{Hash, Hasher};

/// The SQL type of a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Kind {
    /// `BOOLEAN`
    Bool,
    /// `INTEGER` (32-bit)
    I32,
    /// `BIGINT` (64-bit)
    I64,
    /// `DOUBLE PRECISION` / `REAL`
    F64,
    /// `TEXT`
    Text,
    /// `BYTEA` / `BLOB`
    Bytes,
    /// `TIMESTAMPTZ`; `TEXT` in SQLite. Feature `chrono`.
    #[cfg(feature = "chrono")]
    DateTime,
    /// `TIMESTAMP`; `TEXT` in SQLite. Feature `chrono`.
    #[cfg(feature = "chrono")]
    NaiveDateTime,
    /// `DATE`; `TEXT` in SQLite. Feature `chrono`.
    #[cfg(feature = "chrono")]
    Date,
    /// `UUID`; `BLOB` in SQLite. Feature `uuid`.
    #[cfg(feature = "uuid")]
    Uuid,
    /// `JSONB`; `TEXT` in SQLite. Feature `json`.
    #[cfg(feature = "json")]
    Json,
}

impl Kind {
    /// Whether this is an integer type, which makes a primary key
    /// auto-incrementing.
    pub const fn is_integer(self) -> bool {
        matches!(self, Kind::I32 | Kind::I64)
    }
}

/// A value bound to a query or read from a model.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// SQL `NULL` of the given type.
    Null(Kind),
    /// A boolean.
    Bool(bool),
    /// A 32-bit integer.
    I32(i32),
    /// A 64-bit integer.
    I64(i64),
    /// A float.
    F64(f64),
    /// Text.
    Text(String),
    /// Raw bytes.
    Bytes(Vec<u8>),
    /// A UTC timestamp.
    #[cfg(feature = "chrono")]
    DateTime(chrono::DateTime<chrono::Utc>),
    /// A timestamp without time zone.
    #[cfg(feature = "chrono")]
    NaiveDateTime(chrono::NaiveDateTime),
    /// A date.
    #[cfg(feature = "chrono")]
    Date(chrono::NaiveDate),
    /// A UUID.
    #[cfg(feature = "uuid")]
    Uuid(uuid::Uuid),
    /// A JSON document.
    #[cfg(feature = "json")]
    Json(serde_json::Value),
}

impl Value {
    /// Whether this is `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null(_))
    }

    /// The value used to match keys across tables: integers of either width
    /// compare equal, so an `i32` foreign key finds an `i64` primary key.
    pub(crate) fn key(self) -> Value {
        match self {
            Value::I32(v) => Value::I64(v.into()),
            other => other,
        }
    }
}

// Keys are grouped in hash maps for eager loading; floats hash by bits.
impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Value::Null(kind) => kind.hash(state),
            Value::Bool(v) => v.hash(state),
            Value::I32(v) => v.hash(state),
            Value::I64(v) => v.hash(state),
            Value::F64(v) => v.to_bits().hash(state),
            Value::Text(v) => v.hash(state),
            Value::Bytes(v) => v.hash(state),
            #[cfg(feature = "chrono")]
            Value::DateTime(v) => v.hash(state),
            #[cfg(feature = "chrono")]
            Value::NaiveDateTime(v) => v.hash(state),
            #[cfg(feature = "chrono")]
            Value::Date(v) => v.hash(state),
            #[cfg(feature = "uuid")]
            Value::Uuid(v) => v.hash(state),
            #[cfg(feature = "json")]
            Value::Json(v) => v.to_string().hash(state),
        }
    }
}

/// A Rust type that maps to a column.
///
/// Implemented for `bool`, `i32`, `i64`, `f64`, `String`, `Vec<u8>`,
/// `Option<T>` (nullable), and with features `chrono` (`DateTime<Utc>`,
/// `NaiveDateTime`, `NaiveDate`), `uuid` (`Uuid`) and `json`
/// (`serde_json::Value`, `sqlx::types::Json<T>`).
pub trait SqlType: Sized {
    /// The column's SQL type.
    const KIND: Kind;
    /// Whether the column allows `NULL`.
    const NULLABLE: bool = false;
    /// This value as a query parameter.
    fn to_value(&self) -> Value;
    /// Reads the named column from a row.
    fn decode(row: &Row, column: &str) -> Result<Self>;
}

macro_rules! sql_type {
    ($ty:ty, $kind:ident, |$v:ident| $to:expr) => {
        impl SqlType for $ty {
            const KIND: Kind = Kind::$kind;
            fn to_value(&self) -> Value {
                let $v = self;
                $to
            }
            fn decode(row: &Row, column: &str) -> Result<Self> {
                match row {
                    #[cfg(feature = "postgres")]
                    Row::Postgres(row) => sqlx::Row::try_get(row, column).map_err(|e| Error::decode(column, e)),
                    #[cfg(feature = "sqlite")]
                    Row::Sqlite(row) => sqlx::Row::try_get(row, column).map_err(|e| Error::decode(column, e)),
                }
            }
        }
    };
}

sql_type!(bool, Bool, |v| Value::Bool(*v));
sql_type!(i32, I32, |v| Value::I32(*v));
sql_type!(i64, I64, |v| Value::I64(*v));
sql_type!(f64, F64, |v| Value::F64(*v));
sql_type!(String, Text, |v| Value::Text(v.clone()));
sql_type!(Vec<u8>, Bytes, |v| Value::Bytes(v.clone()));
#[cfg(feature = "chrono")]
sql_type!(chrono::DateTime<chrono::Utc>, DateTime, |v| Value::DateTime(*v));
#[cfg(feature = "chrono")]
sql_type!(chrono::NaiveDateTime, NaiveDateTime, |v| Value::NaiveDateTime(*v));
#[cfg(feature = "chrono")]
sql_type!(chrono::NaiveDate, Date, |v| Value::Date(*v));
#[cfg(feature = "uuid")]
sql_type!(uuid::Uuid, Uuid, |v| Value::Uuid(*v));
#[cfg(feature = "json")]
sql_type!(serde_json::Value, Json, |v| Value::Json(v.clone()));

#[cfg(feature = "json")]
impl<T: serde::Serialize + serde::de::DeserializeOwned> SqlType for sqlx::types::Json<T> {
    const KIND: Kind = Kind::Json;
    fn to_value(&self) -> Value {
        Value::Json(serde_json::to_value(&self.0).unwrap_or(serde_json::Value::Null))
    }
    fn decode(row: &Row, column: &str) -> Result<Self> {
        let value = serde_json::Value::decode(row, column)?;
        serde_json::from_value(value).map(sqlx::types::Json).map_err(|e| Error::decode(column, e))
    }
}

impl<T: SqlType> SqlType for Option<T> {
    const KIND: Kind = T::KIND;
    const NULLABLE: bool = true;
    fn to_value(&self) -> Value {
        match self {
            Some(value) => value.to_value(),
            None => Value::Null(T::KIND),
        }
    }
    fn decode(row: &Row, column: &str) -> Result<Self> {
        if row.is_null(column)? {
            Ok(None)
        } else {
            T::decode(row, column).map(Some)
        }
    }
}

macro_rules! from_value {
    ($($ty:ty => $variant:ident),* $(,)?) => {
        $(impl From<$ty> for Value {
            fn from(v: $ty) -> Self {
                Value::$variant(v.into())
            }
        })*
    };
}

from_value!(bool => Bool, i32 => I32, i64 => I64, f64 => F64, String => Text, &str => Text, Vec<u8> => Bytes);
