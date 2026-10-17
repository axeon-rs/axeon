//! An Eloquent-style ORM for Rust, built on [sqlx], for PostgreSQL and
//! SQLite, with first-class [Axeon](https://docs.rs/axeon) integration.
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
//! | `has_many(Post, foreign_key = "author_id", name = "articles")` | Custom foreign key or accessor name |
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
//! # Features
//!
//! | Feature | Default | Enables |
//! |---|---|---|
//! | `postgres` | yes | PostgreSQL |
//! | `sqlite` | yes | SQLite |
//! | `chrono` | yes | `DateTime<Utc>`, `NaiveDateTime`, `NaiveDate` columns and timestamps |
//! | `json` | yes | `serde_json::Value` and `sqlx::types::Json<T>` columns |
//! | `axeon` | yes | The [`server`] module |
//! | `uuid` | no | `Uuid` columns |
//!
//! When the ORM can't express a query, use sqlx directly through
//! [`Db::postgres`] or [`Db::sqlite`]; sqlx is re-exported as
//! [`axeon_orm::sqlx`](sqlx).

#![warn(missing_docs)]

mod db;
mod error;
mod model;
mod query;
mod sql;
mod value;

pub mod migrate;
pub mod relation;
pub mod schema;
#[cfg(feature = "axeon")]
pub mod server;

pub use crate::db::{Backend, Db, DbOptions, Executor, Row, Transaction};
pub use crate::error::{Error, Result};
pub use crate::model::{ColumnDef, Model, Reference, Timestamp};
pub use crate::query::{Assignment, Column, Expr, Order, Page, Query};
pub use crate::relation::{BelongsTo, HasMany, HasOne, Relation};
pub use crate::value::{Kind, SqlType, Value};

/// Derives [`Model`](trait@Model); see the [crate docs](crate#defining-models).
pub use axeon_orm_macros::Model;

/// Embeds a folder of migrations in the binary at compile time and returns
/// a [`Migrator`](migrate::Migrator). The path is relative to the crate's
/// `Cargo.toml` and defaults to `"migrations"`; see [`migrate`].
pub use axeon_orm_macros::embed_migrations;

pub use sqlx;

/// The items most code needs: `use axeon_orm::prelude::*;`.
pub mod prelude {
    pub use crate::{Db, Executor, Model, Query, Transaction};
    #[cfg(feature = "axeon")]
    pub use crate::server::{DbRequest, HealthExt, Transactional};
}

#[doc(hidden)]
pub mod __private {
    pub use crate::db::Conn;
    pub use crate::model::{delete, find, has_column, insert, update};
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
#[doc(hidden)]
pub mod compile_fail_tests {}
