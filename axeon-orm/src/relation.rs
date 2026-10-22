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
//!
//! # Many-to-many
//!
//! `belongs_to_many` links two models through a pivot table holding both
//! keys. The defaults follow the model names: for `Post` and `Tag`, a
//! `post_tag` table with `post_id` and `tag_id` columns.
//!
//! ```
//! use axeon_orm::Model;
//!
//! #[derive(Model, Clone)]
//! #[model(belongs_to_many(Tag))]      // Post::TAGS, post.tags()
//! struct Post { id: i64, title: String }
//!
//! #[derive(Model, Clone)]
//! #[model(belongs_to_many(Post))]     // Tag::POSTS, tag.posts()
//! struct Tag { id: i64, name: String }
//! ```
//!
//! Link records with [`BelongsToMany::attach`], [`detach`](BelongsToMany::detach)
//! and [`sync`](BelongsToMany::sync), and create the pivot table with
//! [`Db::create_pivot_table`](crate::Db::create_pivot_table) or a migration.
//!
//! # Filtering by relations
//!
//! [`Query::has`], [`Query::doesnt_have`] and [`Query::where_has`] keep rows
//! by their relatives, with any relation type:
//!
//! ```no_run
//! # use axeon_orm::{Db, Model};
//! # #[derive(Model, Clone)] #[model(has_many(Post))] struct User { id: i64 }
//! # #[derive(Model, Clone)] struct Post { id: i64, #[model(belongs_to = User)] user_id: i64, published: bool }
//! # async fn run(db: Db) -> axeon_orm::Result<()> {
//! let authors = User::query().where_has(User::POSTS, Post::PUBLISHED.eq(true)).all(&db).await?;
//! # Ok(()) }
//! ```

use crate::db::{Backend, Conn, Executor};
use crate::error::Result;
use crate::model::{primary_key, Model};
use crate::query::{Expr, Query};
use crate::sql::{Cond, Sql, Subquery};
use crate::value::{SqlType, Value};
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

/// A condition on a model by its relatives, from [`Related::related_cond`].
#[doc(hidden)]
pub struct RelatedCond(pub(crate) Cond);

/// A relation usable with [`Query::has`] and [`Query::where_has`].
pub trait Related<M: Model>: Send + Sync {
    /// The related model.
    type Target: Model;

    #[doc(hidden)]
    fn related_cond(&self, filter: Option<Expr<Self::Target>>, negated: bool) -> RelatedCond;
}

/// A relation usable with [`Query::join`]: `belongs_to`, `has_one` and
/// `has_many`.
pub trait Join<M: Model>: Send + Sync {
    /// The joined model.
    type Target: Model;

    /// The column of `M` and the column of the target that must be equal.
    #[doc(hidden)]
    fn on(&self) -> (&'static str, &'static str);
}

fn filter_conds<T>(filter: Option<Expr<T>>) -> Vec<Cond> {
    filter.map(|expr| expr.cond).into_iter().collect()
}

impl<P: Model, C: Model> Related<P> for HasMany<P, C> {
    type Target = C;
    fn related_cond(&self, filter: Option<Expr<C>>, negated: bool) -> RelatedCond {
        let sub = Subquery { table: C::TABLE, column: self.foreign_key, conds: filter_conds(filter), tenant: C::TENANT };
        RelatedCond(Cond::InSubquery(P::PRIMARY_KEY, Box::new(sub), negated))
    }
}

impl<P: Model, C: Model> Related<P> for HasOne<P, C> {
    type Target = C;
    fn related_cond(&self, filter: Option<Expr<C>>, negated: bool) -> RelatedCond {
        let sub = Subquery { table: C::TABLE, column: self.foreign_key, conds: filter_conds(filter), tenant: C::TENANT };
        RelatedCond(Cond::InSubquery(P::PRIMARY_KEY, Box::new(sub), negated))
    }
}

impl<C: Model, P: Model> Related<C> for BelongsTo<C, P> {
    type Target = P;
    fn related_cond(&self, filter: Option<Expr<P>>, negated: bool) -> RelatedCond {
        let sub = Subquery { table: P::TABLE, column: P::PRIMARY_KEY, conds: filter_conds(filter), tenant: P::TENANT };
        RelatedCond(Cond::InSubquery(self.foreign_key, Box::new(sub), negated))
    }
}

impl<P: Model, C: Model> Join<P> for HasMany<P, C> {
    type Target = C;
    fn on(&self) -> (&'static str, &'static str) {
        (P::PRIMARY_KEY, self.foreign_key)
    }
}

impl<P: Model, C: Model> Join<P> for HasOne<P, C> {
    type Target = C;
    fn on(&self) -> (&'static str, &'static str) {
        (P::PRIMARY_KEY, self.foreign_key)
    }
}

impl<C: Model, P: Model> Join<C> for BelongsTo<C, P> {
    type Target = P;
    fn on(&self) -> (&'static str, &'static str) {
        (self.foreign_key, P::PRIMARY_KEY)
    }
}

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

/// `P` and `C` are linked through a pivot table holding both keys; see
/// [many-to-many](self#many-to-many).
pub struct BelongsToMany<P, C> {
    pivot: &'static str,
    foreign_key: &'static str,
    related_key: &'static str,
    _marker: PhantomData<fn() -> (P, C)>,
}

impl<P, C> Clone for BelongsToMany<P, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<P, C> Copy for BelongsToMany<P, C> {}

impl<P, C> std::fmt::Debug for BelongsToMany<P, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BelongsToMany({}.{} -> {})", self.pivot, self.foreign_key, self.related_key)
    }
}

impl<P, C> BelongsToMany<P, C> {
    #[doc(hidden)]
    pub const fn new(pivot: &'static str, foreign_key: &'static str, related_key: &'static str) -> Self {
        Self { pivot, foreign_key, related_key, _marker: PhantomData }
    }

    /// The pivot table.
    pub const fn pivot(&self) -> &'static str {
        self.pivot
    }

    /// The pivot column holding `P`'s key.
    pub const fn foreign_key(&self) -> &'static str {
        self.foreign_key
    }

    /// The pivot column holding `C`'s key.
    pub const fn related_key(&self) -> &'static str {
        self.related_key
    }
}

impl<P: Model, C: Model> BelongsToMany<P, C> {
    fn parent_key(parent: &P) -> Value {
        parent.get(P::PRIMARY_KEY).unwrap_or(Value::Null(primary_key::<P>().kind))
    }

    /// The records linked to one parent.
    pub fn of(&self, parent: &P) -> Query<C> {
        let sub = Subquery {
            table: self.pivot,
            column: self.related_key,
            conds: vec![Cond::Compare(self.foreign_key, "=", Self::parent_key(parent))],
            tenant: None,
        };
        Query::new().cond(Cond::InSubquery(C::PRIMARY_KEY, Box::new(sub), false))
    }

    /// Links `related` to `parent`; linking twice is not an error.
    pub async fn attach<'c>(&self, ex: impl Executor<'c>, parent: &P, related: &C) -> Result<()> {
        let key = related.get(C::PRIMARY_KEY).unwrap_or(Value::Null(primary_key::<C>().kind));
        self.insert_links(&mut ex.into_conn(), Self::parent_key(parent), vec![key]).await
    }

    /// Links the record with this key to `parent`.
    pub async fn attach_key<'c>(&self, ex: impl Executor<'c>, parent: &P, key: impl SqlType) -> Result<()> {
        self.insert_links(&mut ex.into_conn(), Self::parent_key(parent), vec![key.to_value()]).await
    }

    /// Unlinks `related` from `parent`; returns whether a link existed.
    pub async fn detach<'c>(&self, ex: impl Executor<'c>, parent: &P, related: &C) -> Result<bool> {
        let mut conn = ex.into_conn();
        conn.writable()?;
        let key = related.get(C::PRIMARY_KEY).unwrap_or(Value::Null(primary_key::<C>().kind));
        let mut sql = Sql::new(conn.backend());
        sql.push("DELETE FROM ").ident(self.pivot);
        sql.where_clause(&[Cond::Compare(self.foreign_key, "=", Self::parent_key(parent)), Cond::Compare(self.related_key, "=", key)]);
        let (sql, args) = sql.finish();
        Ok(conn.execute(sql, args).await? > 0)
    }

    /// Unlinks everything from `parent`; returns how many links were removed.
    pub async fn detach_all<'c>(&self, ex: impl Executor<'c>, parent: &P) -> Result<u64> {
        let mut conn = ex.into_conn();
        conn.writable()?;
        let mut sql = Sql::new(conn.backend());
        sql.push("DELETE FROM ").ident(self.pivot);
        sql.where_clause(&[Cond::Compare(self.foreign_key, "=", Self::parent_key(parent))]);
        let (sql, args) = sql.finish();
        conn.execute(sql, args).await
    }

    /// Makes `parent` linked to exactly the records with these keys,
    /// adding and removing links as needed. Pass a transaction to make it
    /// atomic.
    pub async fn sync<'c, V: SqlType>(&self, ex: impl Executor<'c>, parent: &P, keys: impl IntoIterator<Item = V>) -> Result<()> {
        let mut conn = ex.into_conn();
        conn.writable()?;
        let keys: Vec<Value> = keys.into_iter().map(|k| k.to_value()).collect();
        let mut sql = Sql::new(conn.backend());
        sql.push("DELETE FROM ").ident(self.pivot);
        sql.where_clause(&[
            Cond::Compare(self.foreign_key, "=", Self::parent_key(parent)),
            Cond::In(self.related_key, keys.clone(), true),
        ]);
        let (sql, args) = sql.finish();
        conn.execute(sql, args).await?;
        self.insert_links(&mut conn, Self::parent_key(parent), keys).await
    }

    /// Inserts pivot rows, skipping links that already exist.
    async fn insert_links(&self, conn: &mut Conn<'_>, parent: Value, keys: Vec<Value>) -> Result<()> {
        conn.writable()?;
        for chunk in keys.chunks(CHUNK / 2) {
            let backend = conn.backend();
            let mut sql = Sql::new(backend);
            sql.push("INSERT INTO ").ident(self.pivot).push(" (").ident(self.foreign_key).push(", ");
            sql.ident(self.related_key).push(") VALUES ");
            for (i, key) in chunk.iter().enumerate() {
                if i > 0 {
                    sql.push(", ");
                }
                sql.push("(").arg(parent.clone()).push(", ").arg(key.clone()).push(")");
            }
            if backend == Backend::MySql {
                // Not INSERT IGNORE, which would also hide foreign key errors.
                sql.push(" ON DUPLICATE KEY UPDATE ").ident(self.foreign_key).push(" = ").ident(self.foreign_key);
            } else {
                sql.push(" ON CONFLICT DO NOTHING");
            }
            let (sql, args) = sql.finish();
            conn.execute(sql, args).await?;
        }
        Ok(())
    }
}

impl<P: Model, C: Model + Clone> BelongsToMany<P, C> {
    /// The linked records of each parent, in the parents' order, in two
    /// queries.
    pub async fn load<'c>(&self, ex: impl Executor<'c>, parents: &[P]) -> Result<Vec<Vec<C>>> {
        self.load_for(&mut ex.into_conn(), parents).await
    }
}

impl<P: Model, C: Model + Clone> Relation<P> for BelongsToMany<P, C> {
    type Output = Vec<C>;

    fn load_for<'a, 'c: 'a>(&'a self, conn: &'a mut Conn<'c>, parents: &'a [P]) -> BoxFuture<'a, Result<Vec<Vec<C>>>> {
        Box::pin(async move {
            let (parent_kind, related_kind) = (primary_key::<P>().kind, primary_key::<C>().kind);
            let mut links: HashMap<Value, Vec<Value>> = HashMap::new();
            let mut related_keys = Vec::new();
            let mut seen = HashSet::new();
            for chunk in keys(parents, P::PRIMARY_KEY).chunks(CHUNK) {
                let mut sql = Sql::new(conn.backend());
                sql.push("SELECT ").ident(self.foreign_key).push(", ").ident(self.related_key);
                sql.push(" FROM ").ident(self.pivot);
                sql.where_clause(&[Cond::In(self.foreign_key, chunk.to_vec(), false)]);
                sql.push(" ORDER BY ").ident(self.related_key);
                let (sql, args) = sql.finish();
                for row in conn.fetch_all(sql, args).await? {
                    let parent = row.get_value(self.foreign_key, parent_kind)?.key();
                    let related = row.get_value(self.related_key, related_kind)?;
                    if seen.insert(related.clone().key()) {
                        related_keys.push(related.clone());
                    }
                    links.entry(parent).or_default().push(related.key());
                }
            }
            let by_key: HashMap<Value, C> = fetch_in::<C>(conn, C::PRIMARY_KEY, related_keys)
                .await?
                .into_iter()
                .filter_map(|c| key_of(&c, C::PRIMARY_KEY).map(|k| (k, c)))
                .collect();
            Ok(parents
                .iter()
                .map(|p| {
                    key_of(p, P::PRIMARY_KEY)
                        .and_then(|k| links.get(&k))
                        .map(|keys| keys.iter().filter_map(|k| by_key.get(k).cloned()).collect())
                        .unwrap_or_default()
                })
                .collect())
        })
    }
}

impl<P: Model, C: Model> Related<P> for BelongsToMany<P, C> {
    type Target = C;
    fn related_cond(&self, filter: Option<Expr<C>>, negated: bool) -> RelatedCond {
        let mut conds = Vec::new();
        if let Some(expr) = filter {
            let related = Subquery { table: C::TABLE, column: C::PRIMARY_KEY, conds: vec![expr.cond], tenant: C::TENANT };
            conds.push(Cond::InSubquery(self.related_key, Box::new(related), false));
        }
        let sub = Subquery { table: self.pivot, column: self.foreign_key, conds, tenant: None };
        RelatedCond(Cond::InSubquery(P::PRIMARY_KEY, Box::new(sub), negated))
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
        // A query that opted out of tenant scopes loads its relations
        // unscoped too.
        let scope = self.query.scope(&conn);
        let mut rescoped = conn.rescoped(scope);
        let mut conn = rescoped.conn();
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
