//! Relations between models and eager loading.
//!
//! Declared on the derive, each relation becomes a constant and an accessor:
//!
//! ```
//! use axeon_orm::Model;
//!
//! #[derive(Model, Clone)]
//! #[model(has_many(Post))]            // User::POSTS, user.posts()
//! struct User { id: i64, name: String }
//!
//! #[derive(Model, Clone)]
//! struct Post {
//!     id: i64,
//!     #[model(belongs_to = User)]     // Post::USER, post.user()
//!     user_id: i64,
//!     title: String,
//! }
//! ```
//!
//! The accessor returns a [`Query`] for one record's relatives
//! (`user.posts().all(&db)`). To load a relation for many records without
//! a query per record (the "N+1" problem), use [`Query::with`] or the
//! relation's `load`, which fetch all relatives in one query.

use crate::db::{Conn, Executor};
use crate::error::Result;
use crate::model::Model;
use crate::query::Query;
use crate::sql::Cond;
use crate::value::Value;
use futures::future::BoxFuture;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;

/// Keys per `IN (...)` query, well under every database's parameter limit.
const CHUNK: usize = 5_000;

/// A relation that can be loaded for a list of `M`; implemented by
/// [`HasMany`], [`HasOne`] and [`BelongsTo`].
pub trait Relation<M: Model>: Send + Sync {
    /// What each `M` gets: `Vec<C>` or `Option<C>`.
    type Output: Send;

    #[doc(hidden)]
    fn load_for<'a, 'c: 'a>(&'a self, conn: &'a mut Conn<'c>, models: &'a [M]) -> BoxFuture<'a, Result<Vec<Self::Output>>>;
}

/// Fetches every `T` whose `column` is one of `keys`.
async fn fetch_in<T: Model>(conn: &mut Conn<'_>, column: &'static str, keys: Vec<Value>) -> Result<Vec<T>> {
    let mut rows = Vec::new();
    for chunk in keys.chunks(CHUNK) {
        rows.extend(Query::<T>::new().cond(Cond::In(column, chunk.to_vec(), false)).fetch(conn).await?);
    }
    Ok(rows)
}

/// The distinct non-null values of `column` across `models`.
fn keys<T: Model>(models: &[T], column: &'static str) -> Vec<Value> {
    let mut seen = HashSet::new();
    models
        .iter()
        .filter_map(|m| m.get(column))
        .filter(|v| !v.is_null())
        .filter(|v| seen.insert(v.clone().key()))
        .collect()
}

fn key_of<T: Model>(model: &T, column: &str) -> Option<Value> {
    model.get(column).filter(|v| !v.is_null()).map(Value::key)
}

macro_rules! relation_type {
    ($(#[$doc:meta])* $name:ident<$a:ident, $b:ident>) => {
        $(#[$doc])*
        pub struct $name<$a, $b> {
            foreign_key: &'static str,
            _marker: PhantomData<fn() -> ($a, $b)>,
        }

        impl<$a, $b> Clone for $name<$a, $b> {
            fn clone(&self) -> Self {
                *self
            }
        }

        impl<$a, $b> Copy for $name<$a, $b> {}

        impl<$a, $b> std::fmt::Debug for $name<$a, $b> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.foreign_key)
            }
        }

        impl<$a, $b> $name<$a, $b> {
            #[doc(hidden)]
            pub const fn new(foreign_key: &'static str) -> Self {
                Self { foreign_key, _marker: PhantomData }
            }

            /// The foreign key column.
            pub const fn foreign_key(&self) -> &'static str {
                self.foreign_key
            }
        }
    };
}

relation_type!(
    /// `P` has many `C`: `C`'s foreign key holds `P`'s primary key.
    HasMany<P, C>
);
relation_type!(
    /// `P` has at most one `C`: `C`'s foreign key holds `P`'s primary key.
    HasOne<P, C>
);
relation_type!(
    /// `C` belongs to `P`: `C`'s foreign key holds `P`'s primary key.
    BelongsTo<C, P>
);

/// Rows of `T` grouped by the key in `column`.
fn group<T: Model>(rows: Vec<T>, column: &str) -> HashMap<Value, Vec<T>> {
    let mut groups: HashMap<Value, Vec<T>> = HashMap::new();
    for row in rows {
        if let Some(key) = key_of(&row, column) {
            groups.entry(key).or_default().push(row);
        }
    }
    groups
}

impl<P: Model, C: Model> HasMany<P, C> {
    /// The children of one parent.
    pub fn of(&self, parent: &P) -> Query<C> {
        let key = parent.get(P::PRIMARY_KEY).unwrap_or(Value::Null(crate::Kind::I64));
        Query::new().cond(Cond::Compare(self.foreign_key, "=", key))
    }

    /// The children of each parent, in the parents' order, in one query.
    pub async fn load<'c>(&self, ex: impl Executor<'c>, parents: &[P]) -> Result<Vec<Vec<C>>> {
        self.load_for(&mut ex.into_conn(), parents).await
    }
}

impl<P: Model, C: Model> Relation<P> for HasMany<P, C> {
    type Output = Vec<C>;

    fn load_for<'a, 'c: 'a>(&'a self, conn: &'a mut Conn<'c>, parents: &'a [P]) -> BoxFuture<'a, Result<Vec<Vec<C>>>> {
        Box::pin(async move {
            let children = fetch_in::<C>(conn, self.foreign_key, keys(parents, P::PRIMARY_KEY)).await?;
            let mut groups = group(children, self.foreign_key);
            Ok(parents
                .iter()
                .map(|p| key_of(p, P::PRIMARY_KEY).and_then(|k| groups.remove(&k)).unwrap_or_default())
                .collect())
        })
    }
}

impl<P: Model, C: Model> HasOne<P, C> {
    /// The child of one parent.
    pub fn of(&self, parent: &P) -> Query<C> {
        let key = parent.get(P::PRIMARY_KEY).unwrap_or(Value::Null(crate::Kind::I64));
        Query::new().cond(Cond::Compare(self.foreign_key, "=", key))
    }

    /// The child of each parent, in the parents' order, in one query.
    pub async fn load<'c>(&self, ex: impl Executor<'c>, parents: &[P]) -> Result<Vec<Option<C>>> {
        self.load_for(&mut ex.into_conn(), parents).await
    }
}

impl<P: Model, C: Model> Relation<P> for HasOne<P, C> {
    type Output = Option<C>;

    fn load_for<'a, 'c: 'a>(&'a self, conn: &'a mut Conn<'c>, parents: &'a [P]) -> BoxFuture<'a, Result<Vec<Option<C>>>> {
        Box::pin(async move {
            let children = fetch_in::<C>(conn, self.foreign_key, keys(parents, P::PRIMARY_KEY)).await?;
            let mut groups = group(children, self.foreign_key);
            Ok(parents
                .iter()
                .map(|p| key_of(p, P::PRIMARY_KEY).and_then(|k| groups.remove(&k)).and_then(|v| v.into_iter().next()))
                .collect())
        })
    }
}

impl<C: Model, P: Model> BelongsTo<C, P> {
    /// The parent of one child (no rows if the foreign key is `NULL`).
    pub fn of(&self, child: &C) -> Query<P> {
        let key = child.get(self.foreign_key).unwrap_or(Value::Null(crate::Kind::I64));
        Query::new().cond(Cond::Compare(P::PRIMARY_KEY, "=", key))
    }
}

impl<C: Model, P: Model + Clone> BelongsTo<C, P> {
    /// The parent of each child, in the children's order, in one query.
    pub async fn load<'c>(&self, ex: impl Executor<'c>, children: &[C]) -> Result<Vec<Option<P>>> {
        self.load_for(&mut ex.into_conn(), children).await
    }
}

impl<C: Model, P: Model + Clone> Relation<C> for BelongsTo<C, P> {
    type Output = Option<P>;

    fn load_for<'a, 'c: 'a>(&'a self, conn: &'a mut Conn<'c>, children: &'a [C]) -> BoxFuture<'a, Result<Vec<Option<P>>>> {
        Box::pin(async move {
            let parents = fetch_in::<P>(conn, P::PRIMARY_KEY, keys(children, self.foreign_key)).await?;
            let by_key: HashMap<Value, P> =
                parents.into_iter().filter_map(|p| key_of(&p, P::PRIMARY_KEY).map(|k| (k, p))).collect();
            Ok(children.iter().map(|c| key_of(c, self.foreign_key).and_then(|k| by_key.get(&k).cloned())).collect())
        })
    }
}

/// A query that also loads a relation, from [`Query::with`].
pub struct WithQuery<M, R> {
    query: Query<M>,
    relation: R,
}

impl<M: Model, R: Relation<M>> WithQuery<M, R> {
    pub(crate) fn new(query: Query<M>, relation: R) -> Self {
        Self { query, relation }
    }

    /// Every matching row with its relatives, in two queries.
    pub async fn all<'c>(self, ex: impl Executor<'c>) -> Result<Vec<(M, R::Output)>> {
        let mut conn = ex.into_conn();
        let models = self.query.fetch(&mut conn).await?;
        let related = self.relation.load_for(&mut conn, &models).await?;
        Ok(models.into_iter().zip(related).collect())
    }

    /// The first matching row with its relatives.
    pub async fn first<'c>(self, ex: impl Executor<'c>) -> Result<Option<(M, R::Output)>> {
        let query = self.query.limit(1);
        Ok(WithQuery { query, relation: self.relation }.all(ex).await?.into_iter().next())
    }
}
