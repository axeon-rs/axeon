//! Axeon integration (feature `axeon`).
//!
//! - `?` on ORM results inside handlers: [`Error`] converts to
//!   [`ServerError`] (not found becomes 404, constraint violations 409,
//!   anything else a 500 that doesn't leak details).
//! - [`DbRequest::db`]: the [`Db`] registered with `app.state(db)`.
//! - [`Transactional`]: one transaction per request, committed when the
//!   response succeeds and rolled back otherwise; reach it with
//!   [`DbRequest::tx`].
//! - [`TenantScope`]: scopes `req.db()` and [`Transactional`] to the
//!   request's tenant (see [`Db::for_tenant`]).
//! - [`HealthExt::database`]: a readiness check that pings the database.
//!
//! ```no_run
//! use axeon::{Json, Server, ServerError};
//! use axeon::health::Health;
//! use axeon_orm::{Db, Model};
//! use axeon_orm::server::{DbRequest, HealthExt};
//!
//! #[derive(Model, serde::Serialize)]
//! struct User { id: i64, name: String }
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let db = Db::connect("sqlite://app.db").await?;
//!
//! let mut app = Server::new();
//! app.state(db.clone());
//! app.mount("/", Health::new().database(&db).router());
//! app.get("/users/:id", |req| async move {
//!     let user = User::find_or_fail(req.db()?, req.param::<i64>("id")?).await?;
//!     Ok::<_, ServerError>(Json(user))
//! });
//! # Ok(()) }
//! ```

use crate::db::{Db, Transaction};
use crate::error::Error;
use crate::value::Value;
use axeon::health::Health;
use axeon::middleware::{Middleware, MiddlewareResult, Next};
use axeon::{Request, ServerError};
use std::sync::Arc;
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};

impl From<Error> for ServerError {
    fn from(error: Error) -> Self {
        match error {
            Error::NotFound => ServerError::NotFound,
            Error::InvalidCursor(msg) => ServerError::BadRequest(format!("invalid cursor: {}", msg)),
            Error::StatementTimeout(_) => ServerError::Status(503, "the database took too long".to_string()),
            Error::StaleRecord(_) => {
                ServerError::Conflict("the record was changed by someone else; reload it and try again".to_string())
            }
            Error::WrongTenant(_) => ServerError::Forbidden("the record belongs to another tenant".to_string()),
            Error::UniqueViolation(_) => ServerError::Conflict("record already exists".to_string()),
            Error::ForeignKeyViolation(_) => {
                ServerError::Conflict("related record is missing or still referenced".to_string())
            }
            other => ServerError::InternalError(other.to_string()),
        }
    }
}

/// The request's transaction, from [`DbRequest::tx`]. Pass `&mut *tx`
/// where a query takes an executor.
pub type TxGuard<'a> = MappedMutexGuard<'a, Transaction>;

#[derive(Clone)]
struct TxSlot(Arc<Mutex<Option<Transaction>>>);

/// The request's tenant-scoped `Db`, set by [`TenantScope`].
#[derive(Clone)]
struct ScopedDb(Db);

/// Database access from a request.
pub trait DbRequest {
    /// The [`Db`] registered with `app.state(db)`, scoped to the request's
    /// tenant when [`TenantScope`] picked one.
    fn db(&self) -> Result<&Db, ServerError>;

    /// The request's transaction, when the [`Transactional`] middleware
    /// runs. Only one guard can be held at a time.
    fn tx(&self) -> Result<TxGuard<'_>, ServerError>;
}

impl DbRequest for Request {
    fn db(&self) -> Result<&Db, ServerError> {
        if let Some(ScopedDb(db)) = self.extension::<ScopedDb>() {
            return Ok(db);
        }
        self.state::<Db>()
            .ok_or_else(|| ServerError::InternalError("no Db registered; call app.state(db)".to_string()))
    }

    fn tx(&self) -> Result<TxGuard<'_>, ServerError> {
        let slot = self.extension::<TxSlot>().ok_or_else(|| {
            ServerError::InternalError("no transaction; add the Transactional middleware".to_string())
        })?;
        let guard = slot.0.try_lock().map_err(|_| ServerError::InternalError("transaction already in use".to_string()))?;
        MutexGuard::try_map(guard, Option::as_mut)
            .map_err(|_| ServerError::InternalError("transaction already finished".to_string()))
    }
}

/// Middleware that wraps each request in a transaction: committed when the
/// handler returns a status below 400, rolled back otherwise.
///
/// ```no_run
/// # use axeon::{Server, ServerError};
/// # use axeon_orm::{Db, Model};
/// use axeon_orm::server::{DbRequest, Transactional};
/// # #[derive(Model)] struct Account { id: i64, balance: i64 }
/// # async fn run(db: Db) {
/// let mut app = Server::new();
/// app.middleware(Transactional::new(db));
/// app.post("/transfer", |req| async move {
///     let mut tx = req.tx()?;
///     let mut from = Account::find_or_fail(&mut *tx, 1).await?;
///     from.balance -= 10;
///     from.update(&mut *tx).await?;
///     Ok::<_, ServerError>("done")
/// });
/// # }
/// ```
#[derive(Clone)]
pub struct Transactional {
    db: Db,
}

impl Transactional {
    /// Transactions on `db`.
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Middleware for Transactional {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        // After TenantScope, the transaction is scoped to the tenant too.
        let db = match req.extension::<ScopedDb>() {
            Some(ScopedDb(scoped)) => scoped.clone(),
            None => self.db.clone(),
        };
        Box::pin(async move {
            let slot = TxSlot(Arc::new(Mutex::new(Some(db.begin().await?))));
            req.insert_extension(slot.clone());
            let result = next.handle(req).await;
            let succeeded = matches!(&result, Ok(response) if response.status < 400);
            let tx = slot.0.lock().await.take();
            if let Some(tx) = tx {
                if succeeded {
                    tx.commit().await?;
                } else {
                    let _ = tx.rollback().await;
                }
            }
            result
        })
    }
}

type TenantFn = Arc<dyn Fn(&Request) -> Option<Value> + Send + Sync>;

/// Middleware that scopes the database to the request's tenant: after it,
/// [`req.db()`](DbRequest::db) and [`Transactional`] transactions are
/// [`Db::for_tenant`] handles, so models with a `#[model(tenant)]` column
/// only see and write that tenant's rows.
///
/// Add it after the authentication middleware that identifies the tenant,
/// and before [`Transactional`]. Requests with no tenant keep the unscoped
/// `Db`, on which tenant-scoped models refuse to run, so a route that
/// forgot to authenticate fails instead of leaking.
///
/// ```no_run
/// # use axeon::{Json, Server, ServerError};
/// # use axeon_orm::{Db, Model};
/// use axeon_orm::server::{DbRequest, TenantScope, Transactional};
///
/// #[derive(Clone)]
/// struct Member { org_id: i64 }
///
/// #[derive(Model, serde::Serialize)]
/// struct Selection { id: i64, #[model(tenant)] org_id: i64, product_id: i64 }
///
/// # async fn run(db: Db) {
/// let mut app = Server::new();
/// app.state(db.clone());
/// // ... authentication middleware that inserts a `Member` extension ...
/// app.middleware(TenantScope::new(|req| req.extension::<Member>().map(|m| m.org_id)));
/// app.get("/selections", |req| async move {
///     let mine = Selection::query().all(req.db()?).await?; // this org's only
///     Ok::<_, ServerError>(Json(mine))
/// });
/// # }
/// ```
#[derive(Clone)]
pub struct TenantScope {
    tenant: TenantFn,
}

impl TenantScope {
    /// Scopes each request to the tenant `tenant` returns, if any.
    pub fn new<F, T>(tenant: F) -> Self
    where
        F: Fn(&Request) -> Option<T> + Send + Sync + 'static,
        T: Into<Value>,
    {
        Self { tenant: Arc::new(move |req| tenant(req).map(Into::into)) }
    }
}

impl Middleware for TenantScope {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        if let Some(tenant) = (self.tenant)(&req) {
            match req.state::<Db>() {
                Some(db) => {
                    let scoped = ScopedDb(db.for_tenant(tenant));
                    req.insert_extension(scoped);
                }
                None => {
                    let error = ServerError::InternalError("no Db registered; call app.state(db)".to_string());
                    return Box::pin(async move { Err(error) });
                }
            }
        }
        next.handle(req)
    }
}

/// Adds a database check to [`Health`].
pub trait HealthExt {
    /// A readiness check named `database` that pings `db`.
    fn database(self, db: &Db) -> Self;
}

impl HealthExt for Health {
    fn database(self, db: &Db) -> Self {
        let db = db.clone();
        self.check("database", move || {
            let db = db.clone();
            async move { db.ping().await }
        })
    }
}
