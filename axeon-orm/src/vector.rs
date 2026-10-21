//! [pgvector](https://github.com/pgvector/pgvector) columns, distances and
//! indexes (feature `pgvector`, PostgreSQL only).
//!
//! Enable the extension once per database (`CREATE EXTENSION IF NOT EXISTS
//! vector`, e.g. in a migration), give the model a [`Vector`] field with
//! its dimensions, and search by distance:
//!
//! ```no_run
//! use axeon_orm::{Db, Model};
//! use axeon_orm::vector::{self, Vector, VectorIndex};
//!
//! #[derive(Model)]
//! struct Product {
//!     id: i64,
//!     category: String,
//!     #[model(dimensions = 3)]
//!     embedding: Vector,
//! }
//!
//! # async fn run(db: Db) -> axeon_orm::Result<()> {
//! db.execute("CREATE EXTENSION IF NOT EXISTS vector").await?;
//! db.create_table::<Product>().await?;
//! db.execute(&vector::create_index::<Product>("embedding", VectorIndex::hnsw_cosine())).await?;
//!
//! let query = Vector::from(vec![0.1, 0.2, 0.3]);
//! // The 10 nearest in a category, with their distances.
//! let hits: Vec<(Product, f64)> = Product::query()
//!     .filter(Product::CATEGORY.eq("lighting"))
//!     .nearest(&db, Product::EMBEDDING.cosine_distance(&query), 10)
//!     .await?;
//! // Or as an ordering and a filter in any query.
//! let close = Product::query()
//!     .filter(Product::EMBEDDING.cosine_distance(&query).lt(0.3))
//!     .order_by(Product::EMBEDDING.cosine_distance(&query).asc())
//!     .all(&db)
//!     .await?;
//! # Ok(()) }
//! ```

use crate::db::Row;
use crate::error::{Error, Result};
use crate::model::Model;
use crate::query::{Column, Expr, Order};
use crate::value::{Kind, SqlType, Value};
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use std::fmt;

/// A vector of `f32`s, stored in a pgvector `vector(n)` column. Give the
/// field `#[model(dimensions = n)]` so tables and indexes know its size.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Vector(Vec<f32>);

impl Vector {
    /// The values.
    pub fn as_slice(&self) -> &[f32] {
        &self.0
    }

    /// The values, owned.
    pub fn into_vec(self) -> Vec<f32> {
        self.0
    }

    /// The number of dimensions.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it has no dimensions.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<f32>> for Vector {
    fn from(values: Vec<f32>) -> Self {
        Vector(values)
    }
}

impl From<&[f32]> for Vector {
    fn from(values: &[f32]) -> Self {
        Vector(values.to_vec())
    }
}

impl From<Vector> for Vec<f32> {
    fn from(vector: Vector) -> Self {
        vector.0
    }
}

/// pgvector's text form: `[1,2.5,3]`.
impl fmt::Display for Vector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[")?;
        for (i, value) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{}", value)?;
        }
        f.write_str("]")
    }
}

impl std::str::FromStr for Vector {
    type Err = std::num::ParseFloatError;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        let inner = text.trim().trim_start_matches('[').trim_end_matches(']');
        if inner.trim().is_empty() {
            return Ok(Vector(Vec::new()));
        }
        inner.split(',').map(|v| v.trim().parse()).collect::<std::result::Result<_, _>>().map(Vector)
    }
}

impl sqlx::Type<sqlx::Postgres> for Vector {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        sqlx::postgres::PgTypeInfo::with_name("vector")
    }
}

/// pgvector's binary form: dimensions (i16), unused (i16), then each value
/// as a big-endian `f32`.
impl sqlx::Encode<'_, sqlx::Postgres> for Vector {
    fn encode_by_ref(&self, buf: &mut sqlx::postgres::PgArgumentBuffer) -> std::result::Result<IsNull, BoxDynError> {
        let dimensions = i16::try_from(self.0.len()).map_err(|_| "a pgvector vector has at most 16000 dimensions")?;
        buf.extend(&dimensions.to_be_bytes());
        buf.extend(&0i16.to_be_bytes());
        for value in &self.0 {
            buf.extend(&value.to_be_bytes());
        }
        Ok(IsNull::No)
    }
}

impl sqlx::Decode<'_, sqlx::Postgres> for Vector {
    fn decode(value: sqlx::postgres::PgValueRef<'_>) -> std::result::Result<Self, BoxDynError> {
        match value.format() {
            sqlx::postgres::PgValueFormat::Binary => {
                let bytes = value.as_bytes()?;
                if bytes.len() < 4 {
                    return Err("vector value too short".into());
                }
                let dimensions = i16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                let data = &bytes[4..];
                if data.len() != dimensions * 4 {
                    return Err("vector value has the wrong length".into());
                }
                Ok(Vector(data.as_chunks::<4>().0.iter().map(|c| f32::from_be_bytes(*c)).collect()))
            }
            sqlx::postgres::PgValueFormat::Text => Ok(value.as_str()?.parse()?),
        }
    }
}

// Other databases have no vector type; the text form keeps binding valid.
#[cfg(feature = "sqlite")]
impl sqlx::Type<sqlx::Sqlite> for Vector {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <String as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
}

#[cfg(feature = "sqlite")]
impl<'q> sqlx::Encode<'q, sqlx::Sqlite> for Vector {
    fn encode_by_ref(&self, buf: &mut <sqlx::Sqlite as sqlx::Database>::ArgumentBuffer) -> std::result::Result<IsNull, BoxDynError> {
        <String as sqlx::Encode<'q, sqlx::Sqlite>>::encode(self.to_string(), buf)
    }
}

#[cfg(feature = "mysql")]
impl sqlx::Type<sqlx::MySql> for Vector {
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        <String as sqlx::Type<sqlx::MySql>>::type_info()
    }
}

#[cfg(feature = "mysql")]
impl<'q> sqlx::Encode<'q, sqlx::MySql> for Vector {
    fn encode_by_ref(&self, buf: &mut <sqlx::MySql as sqlx::Database>::ArgumentBuffer) -> std::result::Result<IsNull, BoxDynError> {
        <String as sqlx::Encode<'q, sqlx::MySql>>::encode(self.to_string(), buf)
    }
}

impl SqlType for Vector {
    const KIND: Kind = Kind::Vector;

    fn to_value(&self) -> Value {
        Value::Vector(self.clone())
    }

    fn decode(row: &Row, column: &str) -> Result<Self> {
        match row {
            Row::Postgres(row) => sqlx::Row::try_get(row, column).map_err(|e| Error::decode(column, e)),
            #[allow(unreachable_patterns)]
            _ => Err(Error::decode(column, "vector columns need PostgreSQL with pgvector")),
        }
    }
}

impl From<Vector> for Value {
    fn from(vector: Vector) -> Self {
        Value::Vector(vector)
    }
}

/// A pgvector distance operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Metric {
    /// `<->`
    L2,
    /// `<=>`
    Cosine,
    /// `<#>`: the negative inner product, so smaller is closer.
    InnerProduct,
}

impl Metric {
    pub(crate) fn operator(self) -> &'static str {
        match self {
            Metric::L2 => "<->",
            Metric::Cosine => "<=>",
            Metric::InnerProduct => "<#>",
        }
    }

    fn ops(self) -> &'static str {
        match self {
            Metric::L2 => "vector_l2_ops",
            Metric::Cosine => "vector_cosine_ops",
            Metric::InnerProduct => "vector_ip_ops",
        }
    }
}

/// The distance between a vector column and a query vector, from
/// [`Column::cosine_distance`] and friends: sort by it, filter on it, or
/// get it back with [`Query::nearest`](crate::Query::nearest).
pub struct Distance<M> {
    pub(crate) column: &'static str,
    pub(crate) metric: Metric,
    pub(crate) target: Vector,
    _marker: std::marker::PhantomData<fn() -> M>,
}

impl<M> Clone for Distance<M> {
    fn clone(&self) -> Self {
        Self { column: self.column, metric: self.metric, target: self.target.clone(), _marker: std::marker::PhantomData }
    }
}

impl<M> fmt::Debug for Distance<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Distance({} {} {})", self.column, self.metric.operator(), self.target)
    }
}

impl<M> Column<M, Vector> {
    fn distance(&self, metric: Metric, target: impl Into<Vector>) -> Distance<M> {
        Distance { column: self.name(), metric, target: target.into(), _marker: std::marker::PhantomData }
    }

    /// Cosine distance (`<=>`): 0 for the same direction, up to 2. Usual
    /// for text embeddings.
    pub fn cosine_distance(&self, target: impl Into<Vector>) -> Distance<M> {
        self.distance(Metric::Cosine, target)
    }

    /// Euclidean distance (`<->`).
    pub fn l2_distance(&self, target: impl Into<Vector>) -> Distance<M> {
        self.distance(Metric::L2, target)
    }

    /// The negative inner product (`<#>`), so that smaller is closer; for
    /// normalized vectors it ranks like cosine distance, a little faster.
    pub fn inner_product(&self, target: impl Into<Vector>) -> Distance<M> {
        self.distance(Metric::InnerProduct, target)
    }
}

impl From<&Vector> for Vector {
    fn from(vector: &Vector) -> Self {
        vector.clone()
    }
}

impl<M> Distance<M> {
    /// Nearest first. With a matching index, PostgreSQL finds the nearest
    /// rows without comparing every row (approximately, for HNSW and
    /// IVFFlat).
    pub fn asc(&self) -> Order<M> {
        Order::distance(self.clone(), false)
    }

    /// Farthest first.
    pub fn desc(&self) -> Order<M> {
        Order::distance(self.clone(), true)
    }

    /// Closer than `max`.
    pub fn lt(&self, max: f64) -> Expr<M> {
        Expr::distance(self.clone(), "<", max)
    }

    /// At most `max` away.
    pub fn lte(&self, max: f64) -> Expr<M> {
        Expr::distance(self.clone(), "<=", max)
    }
}

/// An approximate nearest-neighbour index, for [`create_index`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VectorIndex {
    method: &'static str,
    metric: Metric,
}

impl VectorIndex {
    /// HNSW for cosine distance: the usual choice. Good recall and speed,
    /// and it can be built on an empty table.
    pub fn hnsw_cosine() -> Self {
        Self { method: "hnsw", metric: Metric::Cosine }
    }

    /// HNSW for Euclidean distance.
    pub fn hnsw_l2() -> Self {
        Self { method: "hnsw", metric: Metric::L2 }
    }

    /// HNSW for inner product.
    pub fn hnsw_inner_product() -> Self {
        Self { method: "hnsw", metric: Metric::InnerProduct }
    }

    /// IVFFlat for cosine distance: faster to build, less memory; build it
    /// after loading data.
    pub fn ivfflat_cosine() -> Self {
        Self { method: "ivfflat", metric: Metric::Cosine }
    }
}

/// `CREATE INDEX IF NOT EXISTS` for an approximate nearest-neighbour index
/// on `M`'s vector column `column`. Queries use it when they sort by the
/// same distance. The column needs `#[model(dimensions = n)]`.
///
/// # Panics
///
/// When `M` has no column named `column`.
pub fn create_index<M: Model>(column: &str, index: VectorIndex) -> String {
    let def = M::COLUMNS.iter().find(|c| c.name == column).unwrap_or_else(|| panic!("{} has no column `{}`", M::TABLE, column));
    let backend = crate::Backend::Postgres;
    format!(
        "CREATE INDEX IF NOT EXISTS {} ON {} USING {} ({} {})",
        crate::sql::ident(backend, &format!("{}_{}_{}", M::TABLE, def.name, index.method)),
        crate::sql::ident(backend, M::TABLE),
        index.method,
        crate::sql::ident(backend, def.name),
        index.metric.ops()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_form_round_trips() {
        let vector = Vector::from(vec![1.0, -2.5, 0.125]);
        assert_eq!(vector.to_string(), "[1,-2.5,0.125]");
        assert_eq!("[1, -2.5,0.125]".parse::<Vector>().unwrap(), vector);
        assert_eq!("[]".parse::<Vector>().unwrap(), Vector::default());
        assert!("[1,x]".parse::<Vector>().is_err());
    }
}
