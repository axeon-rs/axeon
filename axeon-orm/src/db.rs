//! Connections, transactions and rows.

use crate::error::{Error, Result};
use crate::model::Model;
use crate::value::{Kind, SqlType, Value};
use sqlx::AssertSqlSafe;
use std::time::Duration;

#[cfg(not(any(feature = "postgres", feature = "sqlite")))]
compile_error!("axeon-orm needs at least one database feature: `postgres` or `sqlite`");

/// Which database a [`Db`] talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Backend {
    /// PostgreSQL (feature `postgres`).
    Postgres,
    /// SQLite (feature `sqlite`).
    Sqlite,
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
/// # Ok(()) }
/// ```
#[derive(Clone, Debug)]
pub struct Db {
    inner: Pool,
}

#[derive(Clone, Debug)]
enum Pool {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::SqlitePool),
}

impl Db {
    /// Connects with default [`DbOptions`]. The URL scheme picks the
    /// database: `postgres://` / `postgresql://` or `sqlite:`.
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
                return Ok(Db { inner: Pool::Postgres(pool) });
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
                return Ok(Db { inner: Pool::Sqlite(pool.connect_with(connect).await?) });
            }
            #[cfg(not(feature = "sqlite"))]
            return Err(Error::Config("SQLite support needs the `sqlite` feature".into()));
        }
        Err(Error::Config(format!(
            "unsupported database URL `{}`: expected postgres:// or sqlite:",
            url.split('@').next_back().unwrap_or_default()
        )))
    }

    /// Which database this pool talks to.
    pub fn backend(&self) -> Backend {
        match &self.inner {
            #[cfg(feature = "postgres")]
            Pool::Postgres(_) => Backend::Postgres,
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(_) => Backend::Sqlite,
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
        };
        Ok(Transaction { inner, failed: false })
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

    /// Drops the model's table if it exists.
    pub async fn drop_table<M: Model>(&self) -> Result<()> {
        self.execute(&format!("DROP TABLE IF EXISTS {}", crate::sql::ident(M::TABLE))).await
    }

    /// Closes every connection; later queries fail.
    pub async fn close(&self) {
        match &self.inner {
            #[cfg(feature = "postgres")]
            Pool::Postgres(pool) => pool.close().await,
            #[cfg(feature = "sqlite")]
            Pool::Sqlite(pool) => pool.close().await,
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
}

#[cfg(feature = "postgres")]
impl From<sqlx::PgPool> for Db {
    fn from(pool: sqlx::PgPool) -> Self {
        Db { inner: Pool::Postgres(pool) }
    }
}

#[cfg(feature = "sqlite")]
impl From<sqlx::SqlitePool> for Db {
    fn from(pool: sqlx::SqlitePool) -> Self {
        Db { inner: Pool::Sqlite(pool) }
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
}

#[derive(Debug)]
enum Tx {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
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
        }
    }

    /// Runs raw SQL inside the transaction; see [`Db::execute`].
    pub async fn execute(&mut self, sql: &str) -> Result<()> {
        Conn::Tx(self).execute_raw(sql.to_string()).await
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
        }
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
            Conn::Db(Db { inner: Pool::Postgres(pool) }) => {
                let $q = bind_all!(sqlx::query::<sqlx::Postgres>(AssertSqlSafe($sql)), $args);
                let $ex = pool;
                $body
            }
            #[cfg(feature = "sqlite")]
            Conn::Db(Db { inner: Pool::Sqlite(pool) }) => {
                let $q = bind_all!(sqlx::query::<sqlx::Sqlite>(AssertSqlSafe($sql)), $args);
                let $ex = pool;
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

    /// Runs SQL without parameters; may hold several statements.
    pub(crate) async fn execute_raw(&mut self, sql: String) -> Result<()> {
        let result = self.execute_raw_inner(sql).await;
        self.track(result)
    }

    async fn execute_raw_inner(&mut self, sql: String) -> Result<()> {
        let raw = sqlx::raw_sql(AssertSqlSafe(sql));
        match self {
            #[cfg(feature = "postgres")]
            Conn::Db(Db { inner: Pool::Postgres(pool) }) => {
                raw.execute(pool).await?;
            }
            #[cfg(feature = "sqlite")]
            Conn::Db(Db { inner: Pool::Sqlite(pool) }) => {
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

#[cfg(feature = "sqlite")]
impl From<sqlx::sqlite::SqliteRow> for Row {
    fn from(row: sqlx::sqlite::SqliteRow) -> Self {
        Row::Sqlite(row)
    }
}
