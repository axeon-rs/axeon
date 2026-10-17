//! A to-do list API: the routes and data access, separate from `main` so
//! tests can run the app in-process.

use axeon::auth::jwt::{now, Jwt};
use axeon::health::Health;
use axeon::metrics::Metrics;
use axeon::middleware::{Compression, Cors, RateLimiter, RequestLogger, SecurityHeaders, SetRequestId};
use axeon::validate::{length_between, Validate, ValidationErrors};
use axeon::{Json, Router, Server, ServerError, ServerResult};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::fmt::Display;

/// Settings from environment variables (and `.env`).
#[derive(Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_database_url")]
    pub database_url: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Signs login tokens; at least 32 random bytes.
    pub jwt_secret: String,
    /// The demo account's password (user name `admin`).
    pub admin_password: String,
    /// Browser origins allowed to call the API, comma-separated.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

fn default_database_url() -> String {
    "sqlite://todos.db?mode=rwc".to_string()
}

fn default_port() -> u16 {
    3000
}

/// Opens the database and creates the table.
pub async fn connect(url: &str) -> Result<SqlitePool, sqlx::Error> {
    let options = if url.contains(":memory:") {
        // An in-memory database lives in one connection: use exactly one
        // and never close it.
        SqlitePoolOptions::new().max_connections(1).min_connections(1).idle_timeout(None).max_lifetime(None)
    } else {
        SqlitePoolOptions::new().max_connections(8)
    };
    let pool = options.connect(url).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS todos (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            owner TEXT NOT NULL,
            title TEXT NOT NULL,
            done BOOLEAN NOT NULL DEFAULT FALSE
        )",
    )
    .execute(&pool)
    .await?;
    Ok(pool)
}

/// Logs a database error and answers 500 without exposing details.
fn internal(error: impl Display) -> ServerError {
    tracing::error!(%error, "database error");
    ServerError::InternalError("something went wrong".to_string())
}

#[derive(Serialize, Deserialize, Clone)]
struct Claims {
    sub: String,
    exp: u64,
}

#[derive(Serialize, sqlx::FromRow)]
struct Todo {
    id: i64,
    title: String,
    done: bool,
}

#[derive(Deserialize)]
struct Login {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct NewTodo {
    title: String,
}

impl Validate for NewTodo {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        errors.check(length_between(self.title.trim(), 1, 200), "title", "must be 1 to 200 characters");
        errors.into_result()
    }
}

#[derive(Deserialize)]
struct UpdateTodo {
    title: Option<String>,
    done: Option<bool>,
}

impl Validate for UpdateTodo {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        if let Some(title) = &self.title {
            errors.check(length_between(title.trim(), 1, 200), "title", "must be 1 to 200 characters");
        }
        errors.check(self.title.is_some() || self.done.is_some(), "body", "nothing to update");
        errors.into_result()
    }
}

#[derive(Deserialize)]
struct ListQuery {
    done: Option<bool>,
    limit: Option<i64>,
    offset: Option<i64>,
}

impl Validate for ListQuery {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        errors.check(self.limit.is_none_or(|l| (1..=100).contains(&l)), "limit", "must be 1 to 100");
        errors.check(self.offset.is_none_or(|o| o >= 0), "offset", "must not be negative");
        errors.into_result()
    }
}

/// The signed-in user, from the token's claims.
fn owner(req: &axeon::Request) -> ServerResult<String> {
    req.extension::<Claims>()
        .map(|claims| claims.sub.clone())
        .ok_or_else(|| ServerError::Unauthorized("sign in first".to_string()))
}

fn db(req: &axeon::Request) -> SqlitePool {
    req.state::<SqlitePool>().expect("pool registered with app.state").clone()
}

/// Builds the whole application.
pub fn app(pool: SqlitePool, config: Config) -> Server {
    let jwt = Jwt::hs256(config.jwt_secret.as_bytes()).issuer("todo-api");
    let metrics = Metrics::new();

    let mut app = Server::new();
    app.state(pool.clone()).state(jwt.clone()).state(config.clone());

    // Every request: IDs, logs, metrics, security headers, compression, limits.
    let cors = config.allowed_origins.iter().fold(Cors::new(), |cors, origin| cors.allow_origin(origin.as_str()));
    app.middleware(SetRequestId::new())
        .middleware(RequestLogger::new())
        .middleware(metrics.clone())
        .middleware(SecurityHeaders::new())
        .middleware(Compression::new())
        .middleware(cors)
        .middleware(RateLimiter::per_minute(600).burst(100));

    // Operations: /health, /ready (checks the database) and /metrics.
    let check_pool = pool.clone();
    app.mount("/", Health::new()
        .check("database", move || {
            let pool = check_pool.clone();
            async move { sqlx::query("SELECT 1").execute(&pool).await.map(|_| ()) }
        })
        .router());
    app.mount("/", metrics.router());

    // POST /auth/token: exchange the demo credentials for a token.
    app.post("/auth/token", |req| async move {
        let login: Login = req.json()?;
        let config = req.state::<Config>().expect("config");
        let password_ok = axeon::auth::constant_time_eq(login.password.as_bytes(), config.admin_password.as_bytes());
        if login.username != "admin" || !password_ok {
            return Err(ServerError::Unauthorized("wrong user name or password".to_string()));
        }
        let jwt = req.state::<Jwt>().expect("jwt");
        let token = jwt.issue(&Claims { sub: login.username, exp: now() + 3600 })?;
        Ok(Json(axeon::json!({ "token": token, "expires_in": 3600 })))
    });

    // /todos: everything here needs a valid token.
    let mut todos = Router::new();
    todos.middleware(jwt.middleware::<Claims>());

    todos.get("/", |req| async move {
        let query: ListQuery = req.valid_query()?;
        let todos: Vec<Todo> = sqlx::query_as(
            "SELECT id, title, done FROM todos WHERE owner = ? AND (? IS NULL OR done = ?)
             ORDER BY id LIMIT ? OFFSET ?",
        )
        .bind(owner(&req)?)
        .bind(query.done)
        .bind(query.done)
        .bind(query.limit.unwrap_or(20))
        .bind(query.offset.unwrap_or(0))
        .fetch_all(&db(&req))
        .await
        .map_err(internal)?;
        Ok::<_, ServerError>(Json(todos))
    });

    todos.post("/", |req| async move {
        let new: NewTodo = req.valid_json()?;
        let todo: Todo = sqlx::query_as("INSERT INTO todos (owner, title) VALUES (?, ?) RETURNING id, title, done")
            .bind(owner(&req)?)
            .bind(new.title.trim())
            .fetch_one(&db(&req))
            .await
            .map_err(internal)?;
        Ok::<_, ServerError>((201, Json(todo)))
    });

    todos.get("/:id", |req| async move {
        let id: i64 = req.param("id")?;
        let todo: Option<Todo> = sqlx::query_as("SELECT id, title, done FROM todos WHERE id = ? AND owner = ?")
            .bind(id)
            .bind(owner(&req)?)
            .fetch_optional(&db(&req))
            .await
            .map_err(internal)?;
        Ok::<_, ServerError>(todo.map(Json)) // None becomes 404
    });

    todos.patch("/:id", |req| async move {
        let id: i64 = req.param("id")?;
        let update: UpdateTodo = req.valid_json()?;
        let todo: Option<Todo> = sqlx::query_as(
            "UPDATE todos SET title = COALESCE(?, title), done = COALESCE(?, done)
             WHERE id = ? AND owner = ? RETURNING id, title, done",
        )
        .bind(update.title.as_deref().map(str::trim))
        .bind(update.done)
        .bind(id)
        .bind(owner(&req)?)
        .fetch_optional(&db(&req))
        .await
        .map_err(internal)?;
        Ok::<_, ServerError>(todo.map(Json))
    });

    todos.delete("/:id", |req| async move {
        let id: i64 = req.param("id")?;
        let result = sqlx::query("DELETE FROM todos WHERE id = ? AND owner = ?")
            .bind(id)
            .bind(owner(&req)?)
            .execute(&db(&req))
            .await
            .map_err(internal)?;
        if result.rows_affected() == 0 {
            return Err(ServerError::NotFound);
        }
        Ok(())
    });

    app.mount("/todos", todos);
    app
}
