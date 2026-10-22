//! Test helpers (feature `testing`, for `[dev-dependencies]`): an isolated
//! database per test, rolled-back transactions, and model factories.
//!
//! ```no_run
//! use axeon_orm::testing::{Factory, TestDb};
//! use axeon_orm::Model;
//!
//! #[derive(Model, Clone)]
//! struct User { id: i64, name: String, email: String, active: bool }
//!
//! impl Factory for User {
//!     fn definition(n: u64) -> Self {
//!         User { id: 0, name: format!("User {}", n), email: format!("user{}@example.com", n), active: true }
//!     }
//! }
//!
//! # async fn test() -> axeon_orm::Result<()> {
//! // A fresh schema on $AXEON_TEST_DATABASE_URL, or in-memory SQLite.
//! let db = TestDb::new().await?;
//! db.create_table::<User>().await?; // or run your migrations on `&*db`
//!
//! let ada = User::factory().with(|u| u.name = "Ada".into()).create(&*db).await?;
//! let inactive = User::factory().count(3).with(|u| u.active = false).create_many(&*db).await?;
//! assert_eq!(User::query().count(&*db).await?, 4);
//!
//! db.cleanup().await?; // drops the schema (also done, best effort, on drop)
//! # Ok(()) }
//! ```

use crate::{Db, Executor, Model, Result, Transaction};
use std::any::TypeId;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// The environment variable [`TestDb::new`] reads.
pub const DATABASE_URL_VAR: &str = "AXEON_TEST_DATABASE_URL";

/// A database for one test, isolated from other tests running at the same
/// time: its own schema on PostgreSQL, its own database on MySQL, a private
/// in-memory database on SQLite. Derefs to [`Db`].
///
/// Call [`cleanup`](TestDb::cleanup) at the end of the test. If the test
/// panics first, dropping the `TestDb` removes the schema or database on a
/// helper thread.
pub struct TestDb {
    db: Db,
    cleanup: Option<Cleanup>,
}

/// What to drop afterwards, and how to connect to drop it.
struct Cleanup {
    admin_url: String,
    statement: String,
}

impl TestDb {
    /// A database on `$AXEON_TEST_DATABASE_URL` (PostgreSQL, MySQL or
    /// SQLite), or in-memory SQLite when it isn't set.
    pub async fn new() -> Result<TestDb> {
        match std::env::var(DATABASE_URL_VAR) {
            Ok(url) if !url.trim().is_empty() => Self::connect(url.trim()).await,
            _ => Self::connect("sqlite::memory:").await,
        }
    }

    /// A database on the server at `url`. On PostgreSQL the user needs to
    /// create schemas, on MySQL databases. A SQLite file URL is used as it
    /// is, so tests sharing it aren't isolated; prefer `sqlite::memory:`.
    pub async fn connect(url: &str) -> Result<TestDb> {
        if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let schema = unique_name();
            let admin = Db::connect(url).await?;
            let created = admin.execute(&format!("CREATE SCHEMA {}", schema)).await;
            admin.close().await;
            created?;
            let separator = if url.contains('?') { '&' } else { '?' };
            let db = Db::connect(&format!("{}{}options=-c%20search_path%3D{}", url, separator, schema)).await?;
            let statement = format!("DROP SCHEMA IF EXISTS {} CASCADE", schema);
            return Ok(TestDb { db, cleanup: Some(Cleanup { admin_url: url.to_string(), statement }) });
        }
        if url.starts_with("mysql://") || url.starts_with("mariadb://") {
            let database = unique_name();
            let admin = Db::connect(url).await?;
            let created = admin.execute(&format!("CREATE DATABASE {}", database)).await;
            admin.close().await;
            created?;
            let db = Db::connect(&with_database(url, &database)).await?;
            let statement = format!("DROP DATABASE IF EXISTS {}", database);
            return Ok(TestDb { db, cleanup: Some(Cleanup { admin_url: url.to_string(), statement }) });
        }
        Ok(TestDb { db: Db::connect(url).await?, cleanup: None })
    }

    /// The database handle (also reachable through `Deref`).
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Closes the pool and drops the test's schema or database.
    pub async fn cleanup(mut self) -> Result<()> {
        self.db.close().await;
        if let Some(cleanup) = self.cleanup.take() {
            let admin = Db::connect(&cleanup.admin_url).await?;
            let dropped = admin.execute(&cleanup.statement).await;
            admin.close().await;
            dropped?;
        }
        Ok(())
    }
}

impl Deref for TestDb {
    type Target = Db;

    fn deref(&self) -> &Db {
        &self.db
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let Some(cleanup) = self.cleanup.take() else { return };
        // Drop can't await, and may run inside the test's runtime: clean up
        // on a thread with a runtime of its own.
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
            runtime.block_on(async {
                let admin = Db::connect(&cleanup.admin_url).await.ok()?;
                let _ = admin.execute(&cleanup.statement).await;
                admin.close().await;
                Some(())
            })
        });
        let _ = worker.join();
    }
}

/// A schema or database name unique to this process and moment.
fn unique_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("axeon_test_{}_{}_{}", std::process::id(), nanos % 1_000_000_000_000, COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// `url` with its database name replaced by `database`, keeping any query.
fn with_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let host = match base[scheme_end..].find('/') {
        Some(slash) => &base[..scheme_end + slash],
        None => base,
    };
    match query {
        Some(query) => format!("{}/{}?{}", host, database, query),
        None => format!("{}/{}", host, database),
    }
}

/// Runs `body` in a transaction that is always rolled back, so nothing it
/// writes outlives it; returns what `body` returns.
///
/// ```no_run
/// # use axeon_orm::{Db, Model};
/// # #[derive(Model)] struct User { id: i64, name: String }
/// # async fn test(db: Db) -> axeon_orm::Result<()> {
/// let seen = axeon_orm::testing::rollback(&db, async |tx| {
///     User { id: 0, name: "temp".into() }.insert(&mut *tx).await?;
///     User::query().count(&mut *tx).await
/// })
/// .await?;
/// assert_eq!(seen, 1);
/// assert_eq!(User::query().count(&db).await?, 0); // rolled back
/// # Ok(()) }
/// ```
pub async fn rollback<T, F>(db: &Db, body: F) -> Result<T>
where
    F: AsyncFnOnce(&mut Transaction) -> Result<T>,
{
    let mut tx = db.begin().await?;
    let result = body(&mut tx).await;
    tx.rollback().await?;
    result
}

/// Default values for test records of a model: implement
/// [`definition`](Factory::definition), then build or insert records with
/// [`factory`](Factory::factory).
///
/// The sequence number counts up per model across the whole test process,
/// so values made from it (emails, slugs) stay unique even when tests run
/// in parallel against one database.
pub trait Factory: Model {
    /// A valid record. Leave auto-increment keys at `0`.
    fn definition(sequence: u64) -> Self;

    /// Starts building records.
    fn factory() -> FactoryBuilder<Self> {
        FactoryBuilder { count: 1, edits: Vec::new() }
    }
}

type Edit<M> = Box<dyn Fn(&mut M, usize) + Send + Sync>;

/// Builds records from a [`Factory`]: change fields, choose how many, then
/// [`build`](FactoryBuilder::build) them or
/// [`create`](FactoryBuilder::create) them in the database.
pub struct FactoryBuilder<M> {
    count: usize,
    edits: Vec<Edit<M>>,
}

impl<M: Factory> FactoryBuilder<M> {
    /// How many records [`build_many`](FactoryBuilder::build_many) and
    /// [`create_many`](FactoryBuilder::create_many) make. Default: 1.
    pub fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    /// Changes every record, e.g. `.with(|u| u.active = false)` or a
    /// foreign key: `.with(move |p| p.user_id = ada.id)`.
    pub fn with<F>(mut self, edit: F) -> Self
    where
        F: Fn(&mut M) + Send + Sync + 'static,
    {
        self.edits.push(Box::new(move |model, _| edit(model)));
        self
    }

    /// Changes each record knowing its position in this batch (from 0).
    pub fn each<F>(mut self, edit: F) -> Self
    where
        F: Fn(&mut M, usize) + Send + Sync + 'static,
    {
        self.edits.push(Box::new(edit));
        self
    }

    fn make(&self, index: usize) -> M {
        let mut model = M::definition(next_sequence::<M>());
        for edit in &self.edits {
            edit(&mut model, index);
        }
        model
    }

    /// One record, not saved.
    pub fn build(&self) -> M {
        self.make(0)
    }

    /// [`count`](FactoryBuilder::count) records, not saved.
    pub fn build_many(&self) -> Vec<M> {
        (0..self.count).map(|i| self.make(i)).collect()
    }

    /// Inserts one record; returns it as stored (with its new ID).
    pub async fn create<'c>(&self, ex: impl Executor<'c>) -> Result<M> {
        crate::model::insert(ex, &self.build()).await
    }

    /// Inserts [`count`](FactoryBuilder::count) records in bulk; returns
    /// them as stored, in order.
    pub async fn create_many<'c>(&self, ex: impl Executor<'c>) -> Result<Vec<M>> {
        if self.count == 0 {
            return Ok(Vec::new());
        }
        crate::model::insert_many(ex, &self.build_many()).await
    }
}

/// The next sequence number for model `M`, from 1.
fn next_sequence<M: 'static>() -> u64 {
    static SEQUENCES: Mutex<Option<HashMap<TypeId, u64>>> = Mutex::new(None);
    let mut sequences = SEQUENCES.lock().unwrap_or_else(|e| e.into_inner());
    let next = sequences.get_or_insert_with(HashMap::new).entry(TypeId::of::<M>()).or_insert(0);
    *next += 1;
    *next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_urls() {
        assert_eq!(with_database("mysql://root:pw@localhost:3306/axeon", "t1"), "mysql://root:pw@localhost:3306/t1");
        assert_eq!(with_database("mysql://root@db/axeon?ssl-mode=disabled", "t1"), "mysql://root@db/t1?ssl-mode=disabled");
        assert_eq!(with_database("mysql://root@db", "t1"), "mysql://root@db/t1");
    }

    #[test]
    fn names_are_unique_identifiers() {
        let (a, b) = (unique_name(), unique_name());
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_'));
    }
}
