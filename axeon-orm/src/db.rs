//! Connections, transactions and rows.

use crate::error::{Error, Result};
use crate::model::Model;
use crate::tenant::Scope;
use crate::value::{Kind, SqlType, Value};
use sqlx::AssertSqlSafe;
use std::time::Duration;

#[cfg(not(any(feature = "postgres", feature = "sqlite", feature = "mysql")))]
compile_error!("axeon-orm needs at least one database feature: `postgres`, `sqlite` or `mysql`");

/// Which database a [`Db`] talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Backend {
    /// PostgreSQL (feature `postgres`).
    Postgres,
    /// SQLite (feature `sqlite`).
    Sqlite,
    /// MySQL 8 (feature `mysql`). MariaDB 10.5+ should work but isn't tested.
    MySql,
}

/// Connection pool settings for [`Db::connect_with`].
#[derive(Clone, Debug)]
pub struct DbOptions {
    max_connections: u32,
    min_connections: u32,
    acquire_timeout: Duration,
    idle_timeout: Option<Duration>,
}

impl Default for DbOptions {
    fn default() -> Self {
        Self {
            max_connections: 10,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(30),
            idle_timeout: Some(Duration::from_secs(600)),
        }
    }
}

impl DbOptions {
    /// 10 connections at most, a 30-second acquire timeout.
    pub fn new() -> Self {
        Self::default()
    }

    /// The most connections the pool opens. Default: 10.
    pub fn max_connections(mut self, n: u32) -> Self {
        self.max_connections = n.max(1);
        self
    }

    /// Connections kept open even when idle. Default: 0.
    pub fn min_connections(mut self, n: u32) -> Self {
        self.min_connections = n;
        self
    }

    /// How long a query waits for a free connection. Default: 30 seconds.
    pub fn acquire_timeout(mut self, timeout: Duration) -> Self {
        self.acquire_timeout = timeout;
        self
    }

    /// How long an unused connection stays open. Default: 10 minutes.
    pub fn idle_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.idle_timeout = timeout;
        self
    }
}

/// A database connection pool. Cheap to clone; clones share the pool.
///
/// ```no_run
/// # async fn run() -> axeon_orm::Result<()> {
/// use axeon_orm::Db;
///
/// let db = Db::connect("postgres://localhost/app").await?;
/// let db = Db::connect("sqlite://app.db").await?; // created if missing
/// let db = Db::connect("sqlite::memory:").await?;
/// let db = Db::connect("mysql://root:secret@localhost/app").await?; // feature `mysql`
/// # Ok(()) }
/// ```
#[derive(Clone, Debug)]
pub struct Db {
    inner: Pool,
    scope: Scope,
}

#[derive(Clone, Debug)]
enum Pool {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::SqlitePool),
    #[cfg(feature = "mysql")]
    MySql(sqlx::MySqlPool),
}

impl Db {
    /// Connects with default [`DbOptions`]. The URL scheme picks the
    /// database: `postgres://` / `postgresql://`, `sqlite:`, or
    /// `mysql://` / `mariadb://`.
    pub async fn connect(url: &str) -> Result<Db> {
        Self::connect_with(url, DbOptions::default()).await
    }

    /// Connects with custom pool settings.
    pub async fn connect_with(url: &str, options: DbOptions) -> Result<Db> {
        if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            #[cfg(feature = "postgres")]
            {
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(options.max_connections)
                    .min_connections(options.min_connections)
                    .acquire_timeout(options.acquire_timeout)
                    .idle_timeout(options.idle_timeout)
                    .connect(url)
                    .await?;
                return Ok(Db { inner: Pool::Postgres(pool), scope: Scope::Unset });
            }
            #[cfg(not(feature = "postgres"))]
            return Err(Error::Config("PostgreSQL support needs the `postgres` feature".into()));
        }
        if url.starts_with("sqlite:") {
            #[cfg(feature = "sqlite")]
            {
                use std::str::FromStr;
                let connect = sqlx::sqlite::SqliteConnectOptions::from_str(url)?
                    .create_if_missing(true)
                    .foreign_keys(true);
                let mut pool = sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(options.max_connections)
                    .min_connections(options.min_connections)
                    .acquire_timeout(options.acquire_timeout)
                    .idle_timeout(options.idle_timeout);
                if url.contains(":memory:") || url.contains("mode=memory") {
                    // Every connection to `:memory:` is a separate, empty
                    // database that vanishes when it closes, so keep one
                    // connection open forever.
                    pool = pool.max_connections(1).min_connections(1).idle_timeout(None).max_lifetime(None);
                }
                return Ok(Db { inner: Pool::Sqlite(pool.connect_with(connect).await?), scope: Scope::Unset });
            }
            #[cfg(not(feature = "sqlite"))]
            return Err(Error::Config("SQLite support needs the `sqlite` feature".into()));
        }
        if url.starts_with("mysql://") || url.starts_with("mariadb://") {
            #[cfg(feature = "mysql")]
            {
                use std::str::FromStr;
                let url = url.replacen("mariadb://", "mysql://", 1);
                let connect = sqlx::mysql::MySqlConnectOptions::from_str(&url)?;
                let pool = sqlx::mysql::MySqlPoolOptions::new()
                    .max_connections(options.max_connections)
                    .min_connections(options.min_connections)
                    .acquire_timeout(options.acquire_timeout)
                    .idle_timeout(options.idle_timeout)
                    .connect_with(connect)
                    .await?;
                return Ok(Db { inner: Pool::MySql(pool), scope: Scope::Unset });
            }
            #[cfg(not(feature = "mysql"))]
            return Err(Error::Config("MySQL support needs the `mysql` feature".into()));
        }
        Err(Error::Config(format!(
            "unsupported database URL `{}`: expected postgres://, sqlite: or mysql://",
            url.split('@').next_back().unwrap_or_default()
        )))
    }

    /// The same pool, scoped to one tenant: queries on models with a
    /// `#[model(tenant)]` column see only rows whose tenant column equals
    /// `tenant`, and writes of other tenants' rows fail with
    /// [`Error::WrongTenant`]. Transactions begun on it keep the scope.
    /// Cheap: the pool is shared.
    ///
    /// ```no_run
    /// # use axeon_orm::{Db, Model};
    /// #[derive(Model)]
    /// struct Selection {
    ///     id: i64,
    ///     #[model(tenant)]
    ///     org_id: i64,
    ///     product_id: i64,
    /// }
    ///
    /// # async fn run(db: Db, org_id: i64) -> axeon_orm::Result<()> {
    /// let org = db.for_tenant(org_id);
    /// let mine = Selection::query().all(&org).await?; // WHERE org_id = $1
    /// // Without a scope, tenant-scoped models refuse to run:
    /// assert!(Selection::query().all(&db).await.is_err());
    /// # Ok(()) }
    /// ```
    ///
    /// Not covered: raw SQL (`execute`, `postgres()` and the like), pivot
    /// tables, and models without a tenant column.
    pub fn for_tenant(&self, tenant: impl Into<Value>) -> Db {
        Db { inner: self.inner.clone(), scope: Scope::Tenant(tenant.into()) }
    }

    /// The same pool, seeing every tenant's rows: for background jobs,
    /// migrations and operator tools. The explicit opt-out from tenant
    /// scopes; a plain `Db` refuses to touch tenant-scoped models.
    pub fn all_tenants(&self) -> Db {
        Db { inner: self.inner.clone(), scope: Scope::All }
    }

    /// The tenant this handle is scoped to, if any.
    pub fn tenant(&self) -> Option<&Value> {
        match &self.scope {
            Scope::Tenant(tenant) => Some(tenant),
            _ => None,
        }
    }

    /// Which database this pool talks to.
    pub fn backend(&self) -> Backend {
        match &self.inner {
            #[cfg(feature = "postgres")]
            Pool::Postgres(_) => Backend::Postgres,
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(_) => Backend::Sqlite,
            #[cfg(feature = "mysql")]
            Pool::MySql(_) => Backend::MySql,
        }
    }

    /// Starts a transaction. Pass `&mut tx` wherever a query takes `&db`,
    /// then [`commit`](Transaction::commit); dropping it rolls back.
    pub async fn begin(&self) -> Result<Transaction> {
        let inner = match &self.inner {
            #[cfg(feature = "postgres")]
            Pool::Postgres(pool) => Tx::Postgres(pool.begin().await?),
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(pool) => Tx::Sqlite(pool.begin().await?),
            #[cfg(feature = "mysql")]
            Pool::MySql(pool) => Tx::MySql(pool.begin().await?),
        };
        Ok(Transaction { inner, failed: false, scope: self.scope.clone() })
    }

    /// Checks the database answers, e.g. for a readiness probe.
    pub async fn ping(&self) -> Result<()> {
        Conn::Db(self).execute_raw("SELECT 1".to_string()).await
    }

    /// Runs raw SQL, which may hold several statements separated by `;`.
    /// Takes no parameters: never build it from user input.
    pub async fn execute(&self, sql: &str) -> Result<()> {
        Conn::Db(self).execute_raw(sql.to_string()).await
    }

    /// Creates the model's table and indexes if they don't exist, from its
    /// definition. Handy for tests and prototypes; use
    /// [migrations](crate::migrate) in production.
    pub async fn create_table<M: Model>(&self) -> Result<()> {
        self.execute(&crate::schema::create_table::<M>(self.backend())).await
    }

    /// Creates a many-to-many pivot table if it doesn't exist:
    /// `db.create_pivot_table(&Post::TAGS)`. See
    /// [`schema::create_pivot_table`](crate::schema::create_pivot_table).
    pub async fn create_pivot_table<P: Model, C: Model>(&self, relation: &crate::BelongsToMany<P, C>) -> Result<()> {
        self.execute(&crate::schema::create_pivot_table(relation, self.backend())).await
    }

    /// Drops the model's table if it exists.
    pub async fn drop_table<M: Model>(&self) -> Result<()> {
        self.execute(&format!("DROP TABLE IF EXISTS {}", crate::sql::ident(self.backend(), M::TABLE))).await
    }

    /// Closes every connection; later queries fail.
    pub async fn close(&self) {
        match &self.inner {
            #[cfg(feature = "postgres")]
            Pool::Postgres(pool) => pool.close().await,
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(pool) => pool.close().await,
            #[cfg(feature = "mysql")]
            Pool::MySql(pool) => pool.close().await,
        }
    }

    /// The underlying sqlx pool, for queries the ORM can't express.
    #[cfg(feature = "postgres")]
    pub fn postgres(&self) -> Option<&sqlx::PgPool> {
        match &self.inner {
            Pool::Postgres(pool) => Some(pool),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The underlying sqlx pool, for queries the ORM can't express.
    #[cfg(feature = "sqlite")]
    pub fn sqlite(&self) -> Option<&sqlx::SqlitePool> {
        match &self.inner {
            Pool::Sqlite(pool) => Some(pool),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The underlying sqlx pool, for queries the ORM can't express.
    #[cfg(feature = "mysql")]
    pub fn mysql(&self) -> Option<&sqlx::MySqlPool> {
        match &self.inner {
            Pool::MySql(pool) => Some(pool),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

#[cfg(feature = "mysql")]
impl From<sqlx::MySqlPool> for Db {
    fn from(pool: sqlx::MySqlPool) -> Self {
        Db { inner: Pool::MySql(pool), scope: Scope::Unset }
    }
}

#[cfg(feature = "postgres")]
impl From<sqlx::PgPool> for Db {
    fn from(pool: sqlx::PgPool) -> Self {
        Db { inner: Pool::Postgres(pool), scope: Scope::Unset }
    }
}

#[cfg(feature = "sqlite")]
impl From<sqlx::SqlitePool> for Db {
    fn from(pool: sqlx::SqlitePool) -> Self {
        Db { inner: Pool::Sqlite(pool), scope: Scope::Unset }
    }
}

/// A database transaction from [`Db::begin`]. Rolls back when dropped
/// without [`commit`](Transaction::commit).
#[derive(Debug)]
pub struct Transaction {
    inner: Tx,
    /// A statement failed. PostgreSQL then aborts the transaction and turns
    /// `COMMIT` into a silent rollback, so `commit` must report it.
    /// Tracked for SQLite too, so both databases behave alike.
    failed: bool,
    /// The tenant scope, from the `Db` it began on.
    scope: Scope,
}

#[derive(Debug)]
enum Tx {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
    #[cfg(feature = "mysql")]
    MySql(sqlx::Transaction<'static, sqlx::MySql>),
}

impl Transaction {
    /// Makes the transaction's changes permanent.
    ///
    /// If any statement in the transaction failed, `commit` rolls back and
    /// returns [`Error::Transaction`]. PostgreSQL discards such a
    /// transaction anyway (while reporting `COMMIT` as successful), and
    /// SQLite is treated the same way so code behaves alike on both.
    pub async fn commit(self) -> Result<()> {
        if self.failed {
            self.rollback().await?;
            return Err(Error::Transaction(
                "an earlier statement failed, so the database rolled the transaction back".to_string(),
            ));
        }
        match self.inner {
            #[cfg(feature = "postgres")]
            Tx::Postgres(tx) => tx.commit().await?,
            #[cfg(feature = "sqlite")]
            Tx::Sqlite(tx) => tx.commit().await?,
            #[cfg(feature = "mysql")]
            Tx::MySql(tx) => tx.commit().await?,
        }
        Ok(())
    }

    /// Discards the transaction's changes.
    pub async fn rollback(self) -> Result<()> {
        match self.inner {
            #[cfg(feature = "postgres")]
            Tx::Postgres(tx) => tx.rollback().await?,
            #[cfg(feature = "sqlite")]
            Tx::Sqlite(tx) => tx.rollback().await?,
            #[cfg(feature = "mysql")]
            Tx::MySql(tx) => tx.rollback().await?,
        }
        Ok(())
    }

    /// Which database this transaction runs on.
    pub fn backend(&self) -> Backend {
        match &self.inner {
            #[cfg(feature = "postgres")]
            Tx::Postgres(_) => Backend::Postgres,
            #[cfg(feature = "sqlite")]
            Tx::Sqlite(_) => Backend::Sqlite,
            #[cfg(feature = "mysql")]
            Tx::MySql(_) => Backend::MySql,
        }
    }

    /// Runs raw SQL inside the transaction; see [`Db::execute`].
    pub async fn execute(&mut self, sql: &str) -> Result<()> {
        Conn::Tx(self).execute_raw(sql.to_string()).await
    }

    /// The underlying sqlx connection, for queries the ORM can't express,
    /// run inside this transaction. [`commit`](Transaction::commit) doesn't
    /// see whether statements run here failed: after an error on
    /// PostgreSQL, which aborts the transaction, roll back.
    #[cfg(feature = "postgres")]
    pub fn postgres(&mut self) -> Option<&mut sqlx::PgConnection> {
        match &mut self.inner {
            Tx::Postgres(tx) => Some(&mut **tx),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The underlying sqlx connection; see [`postgres`](Transaction::postgres).
    #[cfg(feature = "sqlite")]
    pub fn sqlite(&mut self) -> Option<&mut sqlx::SqliteConnection> {
        match &mut self.inner {
            Tx::Sqlite(tx) => Some(&mut **tx),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The underlying sqlx connection; see [`postgres`](Transaction::postgres).
    #[cfg(feature = "mysql")]
    pub fn mysql(&mut self) -> Option<&mut sqlx::MySqlConnection> {
        match &mut self.inner {
            Tx::MySql(tx) => Some(&mut **tx),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

/// A row returned by the database.
#[derive(Debug)]
#[non_exhaustive]
pub enum Row {
    #[doc(hidden)]
    #[cfg(feature = "postgres")]
    Postgres(sqlx::postgres::PgRow),
    #[doc(hidden)]
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::sqlite::SqliteRow),
    #[doc(hidden)]
    #[cfg(feature = "mysql")]
    MySql(sqlx::mysql::MySqlRow),
}

impl Row {
    /// Reads the named column.
    pub fn get<T: SqlType>(&self, column: &str) -> Result<T> {
        T::decode(self, column)
    }

    /// Whether the named column is `NULL`.
    pub fn is_null(&self, column: &str) -> Result<bool> {
        use sqlx::{Row as _, ValueRef as _};
        match self {
            #[cfg(feature = "postgres")]
            Row::Postgres(row) => row.try_get_raw(column).map(|v| v.is_null()).map_err(|e| Error::decode(column, e)),
            #[cfg(feature = "sqlite")]
            Row::Sqlite(row) => row.try_get_raw(column).map(|v| v.is_null()).map_err(|e| Error::decode(column, e)),
            #[cfg(feature = "mysql")]
            Row::MySql(row) => row.try_get_raw(column).map(|v| v.is_null()).map_err(|e| Error::decode(column, e)),
        }
    }

    /// Reads a column as a [`Value`] of the given kind.
    pub(crate) fn get_value(&self, column: &str, kind: Kind) -> Result<Value> {
        if self.is_null(column)? {
            return Ok(Value::Null(kind));
        }
        Ok(match kind {
            Kind::Bool => Value::Bool(self.get(column)?),
            Kind::I32 => Value::I32(self.get(column)?),
            Kind::I64 => Value::I64(self.get(column)?),
            Kind::F64 => Value::F64(self.get(column)?),
            Kind::Text => Value::Text(self.get(column)?),
            Kind::Bytes => Value::Bytes(self.get(column)?),
            #[cfg(feature = "chrono")]
            Kind::DateTime => Value::DateTime(self.get(column)?),
            #[cfg(feature = "chrono")]
            Kind::NaiveDateTime => Value::NaiveDateTime(self.get(column)?),
            #[cfg(feature = "chrono")]
            Kind::Date => Value::Date(self.get(column)?),
            #[cfg(feature = "uuid")]
            Kind::Uuid => Value::Uuid(self.get(column)?),
            #[cfg(feature = "json")]
            Kind::Json => Value::Json(self.get(column)?),
            #[cfg(feature = "pgvector")]
            Kind::Vector => Value::Vector(self.get(column)?),
        })
    }
}

/// Where queries run: `&db` (a pooled connection) or `&mut tx` (inside a
/// transaction).
pub trait Executor<'c>: Send {
    #[doc(hidden)]
    fn into_conn(self) -> Conn<'c>;
}

impl<'c> Executor<'c> for &'c Db {
    fn into_conn(self) -> Conn<'c> {
        Conn::Db(self)
    }
}

impl<'c> Executor<'c> for &'c mut Transaction {
    fn into_conn(self) -> Conn<'c> {
        Conn::Tx(self)
    }
}

impl<'c, 'a: 'c> Executor<'c> for &'c mut Conn<'a> {
    fn into_conn(self) -> Conn<'c> {
        match self {
            Conn::Db(db) => Conn::Db(db),
            Conn::Tx(tx) => Conn::Tx(tx),
        }
    }
}

#[doc(hidden)]
pub enum Conn<'c> {
    Db(&'c Db),
    Tx(&'c mut Transaction),
}

/// A connection with a different tenant scope, from [`Conn::rescoped`];
/// a transaction gets its own scope back when this is dropped.
pub(crate) enum Rescoped<'a> {
    Db(Db),
    Tx(&'a mut Transaction, Option<Scope>),
}

impl Rescoped<'_> {
    pub(crate) fn conn(&mut self) -> Conn<'_> {
        match self {
            Rescoped::Db(db) => Conn::Db(db),
            Rescoped::Tx(tx, _) => Conn::Tx(tx),
        }
    }
}

impl Drop for Rescoped<'_> {
    fn drop(&mut self) {
        if let Rescoped::Tx(tx, previous) = self {
            if let Some(previous) = previous.take() {
                tx.scope = previous;
            }
        }
    }
}

macro_rules! bind_all {
    ($query:expr, $args:expr) => {{
        let mut query = $query;
        for value in $args {
            query = match value {
                Value::Null(kind) => match kind {
                    Kind::Bool => query.bind(None::<bool>),
                    Kind::I32 => query.bind(None::<i32>),
                    Kind::I64 => query.bind(None::<i64>),
                    Kind::F64 => query.bind(None::<f64>),
                    Kind::Text => query.bind(None::<String>),
                    Kind::Bytes => query.bind(None::<Vec<u8>>),
                    #[cfg(feature = "chrono")]
                    Kind::DateTime => query.bind(None::<chrono::DateTime<chrono::Utc>>),
                    #[cfg(feature = "chrono")]
                    Kind::NaiveDateTime => query.bind(None::<chrono::NaiveDateTime>),
                    #[cfg(feature = "chrono")]
                    Kind::Date => query.bind(None::<chrono::NaiveDate>),
                    #[cfg(feature = "uuid")]
                    Kind::Uuid => query.bind(None::<uuid::Uuid>),
                    #[cfg(feature = "json")]
                    Kind::Json => query.bind(None::<serde_json::Value>),
                    #[cfg(feature = "pgvector")]
                    Kind::Vector => query.bind(None::<crate::vector::Vector>),
                },
                Value::Bool(v) => query.bind(v),
                Value::I32(v) => query.bind(v),
                Value::I64(v) => query.bind(v),
                Value::F64(v) => query.bind(v),
                Value::Text(v) => query.bind(v),
                Value::Bytes(v) => query.bind(v),
                #[cfg(feature = "chrono")]
                Value::DateTime(v) => query.bind(v),
                #[cfg(feature = "chrono")]
                Value::NaiveDateTime(v) => query.bind(v),
                #[cfg(feature = "chrono")]
                Value::Date(v) => query.bind(v),
                #[cfg(feature = "uuid")]
                Value::Uuid(v) => query.bind(v),
                #[cfg(feature = "json")]
                Value::Json(v) => query.bind(v),
                #[cfg(feature = "pgvector")]
                Value::Vector(v) => query.bind(v),
            };
        }
        query
    }};
}

/// Runs `$method` on a bound query against whichever connection `$conn` is.
macro_rules! run {
    ($conn:expr, $sql:expr, $args:expr, |$q:ident, $ex:ident| $body:expr) => {
        match $conn {
            #[cfg(feature = "postgres")]
            Conn::Db(Db { inner: Pool::Postgres(pool), .. }) => {
                let $q = bind_all!(sqlx::query::<sqlx::Postgres>(AssertSqlSafe($sql)), $args);
                let $ex = pool;
                $body
            }
            #[cfg(feature = "sqlite")]
            Conn::Db(Db { inner: Pool::Sqlite(pool), .. }) => {
                let $q = bind_all!(sqlx::query::<sqlx::Sqlite>(AssertSqlSafe($sql)), $args);
                let $ex = pool;
                $body
            }
            #[cfg(feature = "mysql")]
            Conn::Db(Db { inner: Pool::MySql(pool), .. }) => {
                let $q = bind_all!(sqlx::query::<sqlx::MySql>(AssertSqlSafe($sql)), $args);
                let $ex = pool;
                $body
            }
            #[cfg(feature = "mysql")]
            Conn::Tx(Transaction { inner: Tx::MySql(tx), .. }) => {
                let $q = bind_all!(sqlx::query::<sqlx::MySql>(AssertSqlSafe($sql)), $args);
                let $ex = &mut **tx;
                $body
            }
            #[cfg(feature = "postgres")]
            Conn::Tx(Transaction { inner: Tx::Postgres(tx), .. }) => {
                let $q = bind_all!(sqlx::query::<sqlx::Postgres>(AssertSqlSafe($sql)), $args);
                let $ex = &mut **tx;
                $body
            }
            #[cfg(feature = "sqlite")]
            Conn::Tx(Transaction { inner: Tx::Sqlite(tx), .. }) => {
                let $q = bind_all!(sqlx::query::<sqlx::Sqlite>(AssertSqlSafe($sql)), $args);
                let $ex = &mut **tx;
                $body
            }
        }
    };
}

impl Conn<'_> {
    pub(crate) fn scope(&self) -> &Scope {
        match self {
            Conn::Db(db) => &db.scope,
            Conn::Tx(tx) => &tx.scope,
        }
    }

    /// Runs with another tenant scope, e.g. for a query that opted out.
    pub(crate) fn rescoped(&mut self, scope: Scope) -> Rescoped<'_> {
        match self {
            Conn::Db(db) => Rescoped::Db(Db { inner: db.inner.clone(), scope }),
            Conn::Tx(tx) => {
                let previous = std::mem::replace(&mut tx.scope, scope);
                Rescoped::Tx(tx, Some(previous))
            }
        }
    }

    pub(crate) fn backend(&self) -> Backend {
        match self {
            Conn::Db(db) => db.backend(),
            Conn::Tx(tx) => tx.backend(),
        }
    }

    /// Marks the transaction failed when a statement in it fails.
    fn track<T>(&mut self, result: Result<T>) -> Result<T> {
        if let (Err(_), Conn::Tx(tx)) = (&result, self) {
            tx.failed = true;
        }
        result
    }

    pub(crate) async fn fetch_all(&mut self, sql: String, args: Vec<Value>) -> Result<Vec<Row>> {
        let result = run!(self, sql, args, |q, ex| {
            q.fetch_all(ex).await.map(|rows| rows.into_iter().map(Into::into).collect()).map_err(Error::from)
        });
        self.track(result)
    }

    pub(crate) async fn fetch_optional(&mut self, sql: String, args: Vec<Value>) -> Result<Option<Row>> {
        let result = run!(self, sql, args, |q, ex| q.fetch_optional(ex).await.map(|r| r.map(Into::into)).map_err(Error::from));
        self.track(result)
    }

    pub(crate) async fn execute(&mut self, sql: String, args: Vec<Value>) -> Result<u64> {
        let result = run!(self, sql, args, |q, ex| q.execute(ex).await.map(|r| r.rows_affected()).map_err(Error::from));
        self.track(result)
    }

    /// Runs an `INSERT` on MySQL, which has no `RETURNING`; returns the
    /// auto-increment id it assigned.
    #[cfg(feature = "mysql")]
    pub(crate) async fn insert_mysql(&mut self, sql: String, args: Vec<Value>) -> Result<u64> {
        let result = self.insert_mysql_inner(sql, args).await;
        self.track(result)
    }

    /// Like [`insert_mysql`](Conn::insert_mysql), but a unique violation
    /// is `None`. It doesn't fail a transaction: MySQL rolls back only the
    /// failed statement.
    #[cfg(feature = "mysql")]
    pub(crate) async fn insert_mysql_unless_duplicate(&mut self, sql: String, args: Vec<Value>) -> Result<Option<u64>> {
        match self.insert_mysql_inner(sql, args).await {
            Err(Error::UniqueViolation(_)) => Ok(None),
            result => self.track(result).map(Some),
        }
    }

    #[cfg(feature = "mysql")]
    async fn insert_mysql_inner(&mut self, sql: String, args: Vec<Value>) -> Result<u64> {
        let query = bind_all!(sqlx::query::<sqlx::MySql>(AssertSqlSafe(sql)), args);
        let result = match self {
            Conn::Db(Db { inner: Pool::MySql(pool), .. }) => query.execute(pool).await.map_err(Error::from),
            Conn::Tx(Transaction { inner: Tx::MySql(tx), .. }) => query.execute(&mut **tx).await.map_err(Error::from),
            #[allow(unreachable_patterns)]
            _ => Err(Error::Config("insert_mysql on another database".into())),
        };
        result.map(|r| r.last_insert_id())
    }

    /// Runs SQL without parameters; may hold several statements.
    pub(crate) async fn execute_raw(&mut self, sql: String) -> Result<()> {
        let result = self.execute_raw_inner(sql).await;
        self.track(result)
    }

    async fn execute_raw_inner(&mut self, sql: String) -> Result<()> {
        let raw = sqlx::raw_sql(AssertSqlSafe(sql));
        match self {
            #[cfg(feature = "postgres")]
            Conn::Db(Db { inner: Pool::Postgres(pool), .. }) => {
                raw.execute(pool).await?;
            }
            #[cfg(feature = "sqlite")]
            Conn::Db(Db { inner: Pool::Sqlite(pool), .. }) => {
                raw.execute(pool).await?;
            }
            #[cfg(feature = "postgres")]
            Conn::Tx(Transaction { inner: Tx::Postgres(tx), .. }) => {
                raw.execute(&mut **tx).await?;
            }
            #[cfg(feature = "sqlite")]
            Conn::Tx(Transaction { inner: Tx::Sqlite(tx), .. }) => {
                raw.execute(&mut **tx).await?;
            }
            #[cfg(feature = "mysql")]
            Conn::Db(Db { inner: Pool::MySql(pool), .. }) => {
                raw.execute(pool).await?;
            }
            #[cfg(feature = "mysql")]
            Conn::Tx(Transaction { inner: Tx::MySql(tx), .. }) => {
                raw.execute(&mut **tx).await?;
            }
        };
        Ok(())
    }
}

#[cfg(feature = "postgres")]
impl From<sqlx::postgres::PgRow> for Row {
    fn from(row: sqlx::postgres::PgRow) -> Self {
        Row::Postgres(row)
    }
}

#[cfg(feature = "mysql")]
impl From<sqlx::mysql::MySqlRow> for Row {
    fn from(row: sqlx::mysql::MySqlRow) -> Self {
        Row::MySql(row)
    }
}

#[cfg(feature = "sqlite")]
impl From<sqlx::sqlite::SqliteRow> for Row {
    fn from(row: sqlx::sqlite::SqliteRow) -> Self {
        Row::Sqlite(row)
    }
}
