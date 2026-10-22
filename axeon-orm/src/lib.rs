//! An Eloquent-style ORM for Rust, built on [sqlx], for PostgreSQL, MySQL
//! and SQLite, with first-class [Axeon](https://docs.rs/axeon) integration.
//!
//! Define a model as a plain struct, and get typed queries, relations,
//! migrations and pagination without writing SQL:
//!
//! ```
//! use axeon_orm::{Db, Model};
//!
//! #[derive(Model, Clone, Debug)]
//! #[model(has_many(Post))]
//! struct User {
//!     id: i64,
//!     name: String,
//!     #[model(unique)]
//!     email: String,
//! }
//!
//! #[derive(Model, Clone, Debug)]
//! struct Post {
//!     id: i64,
//!     #[model(belongs_to = User, on_delete = "cascade")]
//!     user_id: i64,
//!     title: String,
//!     published: bool,
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> axeon_orm::Result<()> {
//! let db = Db::connect("sqlite::memory:").await?;
//! db.create_table::<User>().await?;
//! db.create_table::<Post>().await?;
//!
//! // Insert returns the stored row, with its new id.
//! let ada = User { id: 0, name: "Ada".into(), email: "ada@example.com".into() }
//!     .insert(&db)
//!     .await?;
//! Post { id: 0, user_id: ada.id, title: "Hello".into(), published: true }.insert(&db).await?;
//!
//! // Typed queries: columns are constants on the model.
//! let found = User::query().filter(User::EMAIL.eq("ada@example.com")).first(&db).await?;
//! assert_eq!(found.unwrap().name, "Ada");
//!
//! // Relations, lazily or eagerly (one extra query for all rows).
//! let posts = ada.posts().filter(Post::PUBLISHED.eq(true)).all(&db).await?;
//! assert_eq!(posts.len(), 1);
//! for (user, posts) in User::query().with(User::POSTS).all(&db).await? {
//!     println!("{} wrote {} posts", user.name, posts.len());
//! }
//!
//! // Update and delete.
//! let mut ada = User::find_or_fail(&db, ada.id).await?;
//! ada.name = "Ada Lovelace".into();
//! ada.update(&db).await?;
//! ada.delete(&db).await?;
//! # Ok(()) }
//! ```
//!
//! # Defining models
//!
//! `#[derive(Model)]` maps each field to a column of the same name. The
//! table defaults to the struct name in snake case, pluralized (`User` →
//! `users`, `Category` → `categories`), and the primary key to the `id`
//! field. Integer primary keys are assigned by the database on insert;
//! the value in the struct is ignored, so use `0`.
//!
//! On the struct:
//!
//! | Attribute | Meaning |
//! |---|---|
//! | `#[model(table = "people")]` | Table name |
//! | `#[model(has_many(Post))]` | `User::POSTS`, `user.posts()`; foreign key `user_id` on `Post` |
//! | `#[model(has_one(Profile))]` | `User::PROFILE`, `user.profile()` |
//! | `#[model(belongs_to_many(Tag))]` | `Post::TAGS`, `post.tags()`, through a `post_tag` pivot table ([more](relation#many-to-many)) |
//! | `has_many(Post, foreign_key = "author_id", name = "articles")` | Custom foreign key or accessor name |
//! | `belongs_to_many(Tag, pivot = "taggings", foreign_key = "post_id", related_key = "tag_id")` | Custom pivot table and columns |
//!
//! On fields:
//!
//! | Attribute | Meaning |
//! |---|---|
//! | `#[model(primary_key)]` | The primary key, if not `id` |
//! | `#[model(column = "full_name")]` | Column name, if not the field name |
//! | `#[model(unique)]`, `#[model(index)]` | Unique constraint, index |
//! | `#[model(belongs_to = User)]` | `Post::USER`, `post.user()`; a foreign key to `users.id` |
//! | `on_delete = "cascade"` | With `belongs_to`: the `ON DELETE` action |
//! | `#[model(created_at)]`, `#[model(updated_at)]` | Set to now on insert / every save (feature `chrono`) |
//! | `#[model(default)]` | The database fills it on insert; `default = "SQL"` sets the expression for `create_table` |
//! | `#[model(skip)]` | Not a column; `Default::default()` when loaded |
//!
//! Supported field types are those implementing [`SqlType`]: `bool`, `i32`,
//! `i64`, `f64`, `String`, `Vec<u8>`, `Option<T>` for nullable columns,
//! and with features `chrono`, `uuid` and `json` their types. Match the
//! integer width to the column: `i32` for `INTEGER`, `i64` for `BIGINT`.
//!
//! # Enums
//!
//! `#[derive(SqlEnum)]` lets a fieldless enum be a column. It is stored as
//! text, as the variant name in snake case unless renamed, so it works the
//! same on every database:
//!
//! ```
//! use axeon_orm::{Db, Model, SqlEnum};
//!
//! #[derive(SqlEnum, Clone, Copy, Debug, PartialEq)]
//! enum Status {
//!     Pending,                 // "pending"
//!     InReview,                // "in_review"
//!     #[sql_enum(rename = "ok")]
//!     Approved,                // "ok"
//! }
//!
//! #[derive(Model)]
//! struct Case {
//!     id: i64,
//!     status: Status,
//!     previous: Option<Status>,
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> axeon_orm::Result<()> {
//! # let db = Db::connect("sqlite::memory:").await?;
//! # db.create_table::<Case>().await?;
//! Case { id: 0, status: Status::InReview, previous: None }.insert(&db).await?;
//! let open = Case::query()
//!     .filter(Case::STATUS.is_in([Status::Pending, Status::InReview]))
//!     .count(&db)
//!     .await?;
//! assert_eq!(open, 1);
//! assert_eq!(Status::InReview.as_str(), "in_review");
//! assert_eq!("ok".parse::<Status>().unwrap(), Status::Approved);
//! # Ok(()) }
//! ```
//!
//! `#[sql_enum(rename_all = "...")]` on the enum picks another case:
//! `SCREAMING_SNAKE_CASE`, `kebab-case`, `lowercase`, `UPPERCASE`,
//! `camelCase` or `PascalCase`. The derive also gives `Status::VALUES`,
//! `as_str()`, `Display` and `FromStr`. [`schema::create_table`] adds a
//! `CHECK` constraint listing the values. Reading a value the enum doesn't
//! have (say, written by a newer release) is an [`Error::Decode`], not a
//! panic.
//!
//! Native PostgreSQL enum types (`CREATE TYPE ... AS ENUM`) aren't
//! supported: use a `TEXT` column, where adding a value is a code change
//! (and a new `CHECK`), not an `ALTER TYPE`.
//!
//! # Features
//!
//! | Feature | Default | Enables |
//! |---|---|---|
//! | `postgres` | yes | PostgreSQL |
//! | `sqlite` | yes | SQLite |
//! | `mysql` | no | MySQL 8 (MariaDB 10.5+ should work but isn't tested) |
//! | `tls` | yes | Encrypted connections (rustls), needed by most managed databases and MySQL 8's default login |
//! | `chrono` | yes | `DateTime<Utc>`, `NaiveDateTime`, `NaiveDate` columns and timestamps |
//! | `json` | yes | `serde_json::Value` and `sqlx::types::Json<T>` columns |
//! | `axeon` | yes | The [`server`] module |
//! | `uuid` | no | `Uuid` columns |
//! | `pgvector` | no | [`vector`] columns, distances and indexes (PostgreSQL) |
//!
//! # Beyond simple queries
//!
//! - **Aggregates:** [`Query::sum`], [`avg`](Query::avg), [`min`](Query::min),
//!   [`max`](Query::max) and [`count_by`](Query::count_by).
//! - **Filtering by relations:** [`Query::has`], [`doesnt_have`](Query::doesnt_have)
//!   and [`where_has`](Query::where_has).
//! - **Joins:** [`Query::join`] and [`left_join`](Query::left_join) give
//!   `(Post, User)` pairs from one query, filtered and sorted by either side.
//! - **Full-text and hybrid search:** [`search`] (PostgreSQL), with
//!   [`Query::ranked`].
//! - **Vector search:** [`vector::Vector`] columns, distances to sort and
//!   filter by, and [`Query::nearest`] (feature `pgvector`).
//! - **JSON filters:** [`Column::contains`], [`has_key`](Column::has_key)
//!   and [`key`](Column::key) paths on JSON columns.
//! - **Raw SQL fragments:** [`Expr::raw`] and [`Order::raw`] mix SQL
//!   (JSONB, full-text search, pgvector) into a typed query, with bound
//!   parameters.
//! - **Upserts:** `upsert` and `insert_or_ignore` with an [`OnConflict`]
//!   target, for idempotent writes.
//! - **Row locks:** [`Query::for_update`], [`skip_locked`](Query::skip_locked)
//!   and [`nowait`](Query::nowait), for claiming rows in a transaction.
//! - **LISTEN / NOTIFY:** [`Db::notify`] and [`Db::listen`] on PostgreSQL;
//!   see [`notify`].
//! - **Migrations:** [`migrate`], and the `axeon-orm` command-line tool
//!   (`cargo install axeon-orm-cli`).
//!
//! When the ORM can't express a query, use sqlx directly through
//! [`Db::postgres`], `Db::mysql` or [`Db::sqlite`]; sqlx is re-exported as
//! [`axeon_orm::sqlx`](sqlx).

#![warn(missing_docs)]

mod db;
mod error;
mod cursor;
mod model;
mod query;
mod sql;
mod tenant;
mod value;

pub mod migrate;
#[cfg(feature = "postgres")]
pub mod search;
#[cfg(feature = "pgvector")]
pub mod vector;
#[cfg(all(feature = "axeon", feature = "postgres"))]
pub mod broadcast;
#[cfg(all(feature = "axeon", feature = "postgres"))]
pub mod idempotency;
#[cfg(feature = "postgres")]
pub mod notify;
pub mod relation;
pub mod schema;
#[cfg(feature = "axeon")]
pub mod server;

pub use crate::db::{Backend, Db, DbOptions, Executor, Row, Transaction};
pub use crate::cursor::CursorPage;
pub use crate::error::{Error, Result};
pub use crate::model::{ColumnDef, Model, OnConflict, Reference, Timestamp};
pub use crate::query::{Assignment, Column, Expr, JoinQuery, Numeric, Order, Page, Query, RawArgs};
#[cfg(feature = "json")]
pub use crate::query::{JsonColumn, JsonPath};
pub use crate::relation::{BelongsTo, BelongsToMany, HasMany, HasOne, Join, Related, Relation};
pub use crate::value::{Kind, SqlType, UnknownVariant, Value};

/// Derives [`Model`](trait@Model); see the [crate docs](crate#defining-models).
pub use axeon_orm_macros::Model;

/// Derives [`SqlType`] for a fieldless enum, stored as text; see the
/// [crate docs](crate#enums).
pub use axeon_orm_macros::SqlEnum;

/// Embeds a folder of migrations in the binary at compile time and returns
/// a [`Migrator`](migrate::Migrator). The path is relative to the crate's
/// `Cargo.toml` and defaults to `"migrations"`; see [`migrate`].
pub use axeon_orm_macros::embed_migrations;

pub use sqlx;

/// The items most code needs: `use axeon_orm::prelude::*;`.
pub mod prelude {
    pub use crate::{Db, Executor, Model, Query, Transaction};
    #[cfg(feature = "axeon")]
    pub use crate::server::{DbRequest, HealthExt, TenantScope, Transactional};
}

#[cfg(feature = "json")]
mod sealed {
    pub trait Sealed {}
    #[cfg(feature = "json")]
    impl Sealed for serde_json::Value {}
    #[cfg(feature = "json")]
    impl<T> Sealed for sqlx::types::Json<T> {}
}

#[doc(hidden)]
pub mod __private {
    pub use crate::db::Conn;
    pub use crate::model::{column_name, delete, find, has_column, insert, insert_many, insert_or_ignore, update, upsert, upsert_many};

    pub fn decode_error(column: &str, error: impl std::fmt::Display) -> crate::Error {
        crate::Error::decode(column, error)
    }
}

/// Mistakes the derive and the typed API reject at compile time.
///
/// A relation whose foreign key doesn't exist:
///
/// ```compile_fail
/// #[derive(axeon_orm::Model)]
/// #[model(has_many(Post))]
/// struct User { id: i64 }
/// #[derive(axeon_orm::Model)]
/// struct Post { id: i64, author_id: i64 } // not `user_id`
/// ```
///
/// No primary key:
///
/// ```compile_fail
/// #[derive(axeon_orm::Model)]
/// struct Log { message: String }
/// ```
///
/// Comparing a column with the wrong type:
///
/// ```compile_fail
/// #[derive(axeon_orm::Model)]
/// struct User { id: i64, age: i32 }
/// let _ = User::query().filter(User::AGE.eq("old"));
/// ```
///
/// Filtering one model by another's column:
///
/// ```compile_fail
/// #[derive(axeon_orm::Model)]
/// struct User { id: i64 }
/// #[derive(axeon_orm::Model)]
/// struct Post { id: i64 }
/// let _ = User::query().filter(Post::ID.eq(1));
/// ```
///
/// A field type that isn't a column type:
///
/// ```compile_fail
/// #[derive(axeon_orm::Model)]
/// struct User { id: i64, tags: std::collections::HashMap<String, String> }
/// ```
///
/// An enum variant holding data:
///
/// ```compile_fail
/// #[derive(axeon_orm::SqlEnum)]
/// enum Status { Open, Closed(String) }
/// ```
///
/// Two variants stored as the same value:
///
/// ```compile_fail
/// #[derive(axeon_orm::SqlEnum)]
/// enum Status { Open, #[sql_enum(rename = "open")] Reopened }
/// ```
///
/// An unknown case:
///
/// ```compile_fail
/// #[derive(axeon_orm::SqlEnum)]
/// #[sql_enum(rename_all = "Title Case")]
/// enum Status { Open }
/// ```
///
/// Comparing an enum column with a string:
///
/// ```compile_fail
/// #[derive(axeon_orm::SqlEnum)]
/// enum Status { Open }
/// #[derive(axeon_orm::Model)]
/// struct Case { id: i64, status: Status }
/// let _ = Case::query().filter(Case::STATUS.eq("open"));
/// ```
#[doc(hidden)]
pub mod compile_fail_tests {}
