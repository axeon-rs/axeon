//! A to-do list API: the routes and data access, separate from `main` so
//! tests can run the app in-process.

use axeon::auth::jwt::{now, Jwt};
use axeon::health::Health;
use axeon::metrics::Metrics;
use axeon::middleware::{Compression, Cors, RateLimiter, RequestLogger, SecurityHeaders, SetRequestId};
use axeon::validate::{length_between, Validate, ValidationErrors};
use axeon::{Json, Router, Server, ServerError, ServerResult};
use axeon_orm::prelude::*;
use serde::{Deserialize, Serialize};

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

/// Opens the database and applies any pending migrations.
pub async fn connect(url: &str) -> axeon_orm::Result<Db> {
    let db = Db::connect(url).await?;
    axeon_orm::embed_migrations!().run(&db).await?;
    Ok(db)
}

#[derive(Serialize, Deserialize, Clone)]
struct Claims {
    sub: String,
    exp: u64,
}

#[derive(Model, Serialize)]
struct Todo {
    id: i64,
    #[serde(skip)]
    owner: String,
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
    limit: Option<u64>,
    offset: Option<u64>,
}

impl Validate for ListQuery {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        errors.check(self.limit.is_none_or(|l| (1..=100).contains(&l)), "limit", "must be 1 to 100");
        errors.into_result()
    }
}

/// The signed-in user, from the token's claims.
fn owner(req: &axeon::Request) -> ServerResult<String> {
    req.extension::<Claims>()
        .map(|claims| claims.sub.clone())
        .ok_or_else(|| ServerError::Unauthorized("sign in first".to_string()))
}

/// The signed-in user's to-do with this id, if any.
fn mine(req: &axeon::Request) -> ServerResult<axeon_orm::Query<Todo>> {
    let id: i64 = req.param("id")?;
    Ok(Todo::query().filter(Todo::ID.eq(id) & Todo::OWNER.eq(owner(req)?)))
}

/// Builds the whole application.
pub fn app(db: Db, config: Config) -> Server {
    let jwt = Jwt::hs256(config.jwt_secret.as_bytes()).issuer("todo-api");
    let metrics = Metrics::new();

    let mut app = Server::new();
    app.state(db.clone()).state(jwt.clone()).state(config.clone());

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
    app.mount("/", Health::new().database(&db).router());
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
        let mut todos = Todo::query().filter(Todo::OWNER.eq(owner(&req)?));
        if let Some(done) = query.done {
            todos = todos.filter(Todo::DONE.eq(done));
        }
        let todos = todos
            .order_by(Todo::ID.asc())
            .limit(query.limit.unwrap_or(20))
            .offset(query.offset.unwrap_or(0))
            .all(req.db()?)
            .await?;
        Ok::<_, ServerError>(Json(todos))
    });

    todos.post("/", |req| async move {
        let new: NewTodo = req.valid_json()?;
        let todo = Todo { id: 0, owner: owner(&req)?, title: new.title.trim().to_string(), done: false };
        let todo = todo.insert(req.db()?).await?;
        Ok::<_, ServerError>((201, Json(todo)))
    });

    todos.get("/:id", |req| async move {
        let todo = mine(&req)?.first(req.db()?).await?;
        Ok::<_, ServerError>(todo.map(Json)) // None becomes 404
    });

    todos.patch("/:id", |req| async move {
        let update: UpdateTodo = req.valid_json()?;
        let mut todo = mine(&req)?.first_or_fail(req.db()?).await?;
        if let Some(title) = update.title {
            todo.title = title.trim().to_string();
        }
        if let Some(done) = update.done {
            todo.done = done;
        }
        todo.update(req.db()?).await?;
        Ok::<_, ServerError>(Json(todo))
    });

    todos.delete("/:id", |req| async move {
        match mine(&req)?.delete(req.db()?).await? {
            0 => Err(ServerError::NotFound),
            _ => Ok(()),
        }
    });

    app.mount("/todos", todos);
    app
}
