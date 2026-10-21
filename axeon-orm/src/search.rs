//! PostgreSQL full-text search, and scores that combine text rank with
//! vector similarity for hybrid search (feature `postgres`).
//!
//! ```no_run
//! use axeon_orm::search::{self, FullText};
//! use axeon_orm::{Db, Model};
//!
//! #[derive(Model)]
//! struct Product { id: i64, name: String, description: Option<String> }
//!
//! # async fn run(db: Db) -> axeon_orm::Result<()> {
//! let fts = FullText::columns([Product::NAME, Product::DESCRIPTION]).config("english");
//! db.execute(&search::create_index(&fts, "products_fts")).await?; // a GIN index the queries use
//!
//! // Web-search syntax: words, "quoted phrases", OR, -excluded.
//! let found = Product::query()
//!     .filter(fts.matches("led lamp -solar"))
//!     .order_by(fts.rank("led lamp -solar").desc())
//!     .all(&db)
//!     .await?;
//!
//! // Scored results, best first, with their scores.
//! let top: Vec<(Product, f64)> = Product::query()
//!     .filter(fts.matches("led lamp"))
//!     .ranked(&db, fts.rank("led lamp"), 20)
//!     .await?;
//! # Ok(()) }
//! ```
//!
//! With the `pgvector` feature, add vector similarity to the score for
//! hybrid search:
//!
//! ```ignore
//! let score = fts.rank(&q) * 0.6 + Product::EMBEDDING.cosine_distance(&v).similarity() * 0.4;
//! Product::query().filter(fts.matches(&q) | Product::EMBEDDING.cosine_distance(&v).lt(0.5)).ranked(&db, score, 20)
//! ```
//!
//! The search text is always a bound parameter. Text-search configurations
//! (`english`, `simple`, ...) decide how words are split and stemmed; the
//! default is `simple`, which only lower-cases. PostgreSQL can't split
//! Chinese into words on its own: `simple` treats a run of Chinese
//! characters as one word. For Chinese, install a parser such as zhparser
//! or pg_jieba and pass its configuration name.

use crate::model::Model;
use crate::query::{Column, Expr, Order};
use crate::sql::{Cond, FtsSource, ScoreTerm, SortTerm};
use crate::value::Value;
use std::marker::PhantomData;

/// Text to search, in a model's text columns or a stored `tsvector`
/// column; see the [module docs](self).
pub struct FullText<M> {
    source: FtsSource,
    config: String,
    _marker: PhantomData<fn() -> M>,
}

impl<M> Clone for FullText<M> {
    fn clone(&self) -> Self {
        Self { source: self.source.clone(), config: self.config.clone(), _marker: PhantomData }
    }
}

impl<M> std::fmt::Debug for FullText<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FullText").field("source", &self.source).field("config", &self.config).finish()
    }
}

impl<M: Model> FullText<M> {
    /// The text of these columns (`NULL`s count as empty), turned into
    /// search terms as each query runs. Index it with [`create_index`].
    pub fn columns<const N: usize>(columns: [Column<M, String>; N]) -> Self {
        Self { source: FtsSource::Columns(columns.iter().map(|c| c.name()).collect()), config: "simple".into(), _marker: PhantomData }
    }

    /// A stored `tsvector` column, e.g. one from [`generated_column`]. Not
    /// a model field: leave it out of the struct.
    pub fn stored(column: &'static str) -> Self {
        Self { source: FtsSource::Stored(column), config: "simple".into(), _marker: PhantomData }
    }

    /// The text-search configuration, e.g. `english`. Default: `simple`.
    ///
    /// # Panics
    ///
    /// When `config` isn't a plain name (letters, digits, `_`, an optional
    /// `schema.` prefix): it goes into the SQL as a literal so indexes can
    /// match it.
    pub fn config(mut self, config: &str) -> Self {
        let valid = !config.is_empty()
            && config.split('.').count() <= 2
            && config.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(valid, "invalid text-search configuration name {:?}", config);
        self.config = config.to_string();
        self
    }

    /// Rows matching `query`, in web-search syntax (`websearch_to_tsquery`):
    /// words must all appear, `"quoted phrases"` in order, `a OR b`,
    /// `-word` excluded. Never fails on odd input.
    pub fn matches(&self, query: impl Into<String>) -> Expr<M> {
        Expr::new(Cond::FullText(self.source.clone(), self.config.clone(), Value::Text(query.into())))
    }

    /// How well rows match `query`, from 0 to 1 (`ts_rank` normalized as
    /// `rank / (rank + 1)`): sort by it, or combine it into a [`Score`].
    pub fn rank(&self, query: impl Into<String>) -> Rank<M> {
        Rank::new(ScoreTerm::TextRank(self.source.clone(), self.config.clone(), Value::Text(query.into())))
    }
}

/// A numeric score of a row: a text [rank](FullText::rank), or a vector
/// similarity. Sort by it, or weight and add several into a [`Score`]:
/// `rank * 0.6 + similarity * 0.4`.
pub struct Rank<M> {
    term: ScoreTerm,
    _marker: PhantomData<fn() -> M>,
}

impl<M> Clone for Rank<M> {
    fn clone(&self) -> Self {
        Self { term: self.term.clone(), _marker: PhantomData }
    }
}

impl<M> Rank<M> {
    pub(crate) fn new(term: ScoreTerm) -> Self {
        Self { term, _marker: PhantomData }
    }

    /// Highest first.
    pub fn desc(&self) -> Order<M> {
        Score::from(self.clone()).desc()
    }

    /// Lowest first.
    pub fn asc(&self) -> Order<M> {
        Score::from(self.clone()).asc()
    }
}

/// A weighted sum of [`Rank`]s, for [`Query::ranked`](crate::Query::ranked)
/// or sorting.
pub struct Score<M> {
    pub(crate) terms: Vec<(f64, ScoreTerm)>,
    _marker: PhantomData<fn() -> M>,
}

impl<M> Clone for Score<M> {
    fn clone(&self) -> Self {
        Self { terms: self.terms.clone(), _marker: PhantomData }
    }
}

impl<M> Score<M> {
    /// Highest first.
    pub fn desc(&self) -> Order<M> {
        Order::score(SortTerm::Score(self.terms.clone()), true)
    }

    /// Lowest first.
    pub fn asc(&self) -> Order<M> {
        Order::score(SortTerm::Score(self.terms.clone()), false)
    }
}

impl<M> From<Rank<M>> for Score<M> {
    fn from(rank: Rank<M>) -> Self {
        Score { terms: vec![(1.0, rank.term)], _marker: PhantomData }
    }
}

impl<M> std::ops::Mul<f64> for Rank<M> {
    type Output = Score<M>;
    fn mul(self, weight: f64) -> Score<M> {
        Score { terms: vec![(weight, self.term)], _marker: PhantomData }
    }
}

impl<M> std::ops::Add for Score<M> {
    type Output = Score<M>;
    fn add(mut self, other: Score<M>) -> Score<M> {
        self.terms.extend(other.terms);
        self
    }
}

impl<M> std::ops::Add<Rank<M>> for Score<M> {
    type Output = Score<M>;
    fn add(self, other: Rank<M>) -> Score<M> {
        self + Score::from(other)
    }
}

impl<M> std::ops::Add<Score<M>> for Rank<M> {
    type Output = Score<M>;
    fn add(self, other: Score<M>) -> Score<M> {
        Score::from(self) + other
    }
}

impl<M> std::ops::Add for Rank<M> {
    type Output = Score<M>;
    fn add(self, other: Rank<M>) -> Score<M> {
        Score::from(self) + Score::from(other)
    }
}

/// The search expression of `fts` in SQL, as queries write it.
fn vector_sql<M>(fts: &FullText<M>) -> String {
    let mut sql = crate::sql::Sql::new(crate::Backend::Postgres);
    sql.fts_vector(&fts.source, &fts.config);
    sql.finish().0
}

/// `CREATE INDEX IF NOT EXISTS` for a GIN index named `name` on exactly
/// the expression `fts` searches, so PostgreSQL uses it for
/// [`matches`](FullText::matches).
pub fn create_index<M: Model>(fts: &FullText<M>, name: &str) -> String {
    let backend = crate::Backend::Postgres;
    format!(
        "CREATE INDEX IF NOT EXISTS {} ON {} USING GIN (({}))",
        crate::sql::ident(backend, name),
        crate::sql::ident(backend, M::TABLE),
        vector_sql(fts)
    )
}

/// `ALTER TABLE ... ADD COLUMN` for a stored, generated `tsvector` column
/// named `column` from `columns`, kept up to date by PostgreSQL. Search it
/// with [`FullText::stored`]; index it with [`create_index`].
pub fn generated_column<M: Model, const N: usize>(column: &str, columns: [Column<M, String>; N], config: &str) -> String {
    let fts = FullText::columns(columns).config(config);
    format!(
        "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} tsvector GENERATED ALWAYS AS ({}) STORED",
        crate::sql::ident(crate::Backend::Postgres, M::TABLE),
        crate::sql::ident(crate::Backend::Postgres, column),
        vector_sql(&fts)
    )
}
