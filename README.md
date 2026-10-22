# Axeon

A fast, Express-style web framework for Rust, built on [hyper](https://hyper.rs).
Write handlers as simply as in Express; get Rust's speed, memory efficiency
and safety.

```rust,no_run
use axeon::Server;

fn main() {
    let mut app = Server::new();
    app.get("/", |_req| async { "Hello, World!" });
    app.listen("127.0.0.1:3000").expect("server failed to start");
}
```

## Why Axeon

- **Simple**: Express-like routing and middleware. Handlers return plain
  values: text, `Json(..)`, `(201, body)`, `Option` (404 on `None`) or `Result`.
- **Fast and lean**: about 4-5x the throughput of Express on ~1/100th of the
  memory, within ~10% of the fastest Rust frameworks
  ([benchmarks](benchmarks/README.md)). Routing time doesn't grow with the
  number of routes.
- **Batteries included**: validation, JWT/bearer/basic auth, cookies
  (signed and encrypted), sessions, CSRF protection, typed config from env,
  templates, static files with SPA fallback, uploads streamed to disk,
  server-sent events and WebSockets.
- **Secure by default**: request size limits and timeouts, slow-client
  protection, security headers, rate limiting, CORS, panic isolation, no
  internal error details sent to clients, constant-time secret comparison.
- **Production ready**: HTTP/1.1 and HTTP/2, HTTPS, graceful shutdown with a
  drain period, health and readiness checks, Prometheus metrics, structured
  logging with request IDs, and in-process testing.
- **Interoperable**: use any [tower](https://docs.rs/tower) layer (all of
  `tower-http`) as middleware, and run an Axeon app as a tower `Service`.

## Installation

```toml
[dependencies]
axeon = "1.1"
tokio = { version = "1", features = ["full"] } # for #[tokio::main], tokio::spawn, etc.
serde = { version = "1", features = ["derive"] }
```

Upgrading from 0.x? See the [migration guide](CHANGELOG.md#migrating-from-04).

## Routing

```rust
use axeon::{Router, Server};

let mut app = Server::new();
app.get("/", |_req| async { "home" })
    .post("/users", |_req| async { (201, "created") })
    .get("/users/:id", |req| async move { format!("user {}", req.params()["id"]) })
    .get("/files/*path", |req| async move { format!("file {}", req.params()["path"]) });

// Group routes and mount them under a prefix.
let mut api = Router::new();
api.get("/status", |_req| async { "ok" });
app.mount("/api", api); // GET /api/status
```

Static paths win over parameters (`/users/new` beats `/users/:id`), and
routes that can never be told apart (`/users/:id` and `/users/:name`) are
reported when the server starts. `HEAD` and `OPTIONS` are answered
automatically, and a known path with the wrong method gets `405` with an
`Allow` header.

To configure one path, use `route`: its middleware and settings apply only
to the handlers registered through it.

```rust
use axeon::{Server, ServerError};
use axeon::middleware::from_fn;

let mut app = Server::new();
let require_admin = from_fn(|req, next| async move {
    if req.get_header("x-admin").is_none() {
        return Err(ServerError::Forbidden("admins only".into()));
    }
    next.handle(req).await
});

app.route("/posts").get(|_req| async { "all posts" }); // public
app.route("/posts")
    .middleware(require_admin) // only for POST
    .body_limit(64 * 1024)     // 64 KiB for this route
    .post(|_req| async { (201, "created") });
```

## Requests

Typed helpers parse input and answer bad input with the right 4xx status:

```rust
use axeon::{Json, Server, ServerError};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
struct NewUser {
    name: String,
    age: u8,
}

#[derive(Deserialize)]
struct Search {
    q: String,
    page: Option<u32>,
}

let mut app = Server::new();

// 400 if `id` isn't a number.
app.get("/users/:id", |req| async move {
    let id: u64 = req.param("id")?;
    Ok::<_, ServerError>(format!("user {}", id))
});

// 415 for a non-JSON body, 400 for malformed JSON, 422 for the wrong shape.
app.post("/users", |req| async move {
    let user: NewUser = req.json()?;
    Ok::<_, ServerError>((201, Json(user)))
});

// GET /search?q=rust&page=2
app.get("/search", |req| async move {
    let search: Search = req.query_as()?;
    Ok::<_, ServerError>(format!("{} (page {})", search.q, search.page.unwrap_or(1)))
});
```

Also: `req.method()`, `req.path()`, `req.query_param("page")`,
`req.get_header("authorization")`, `req.body()`, `req.text()`, `req.form()`,
`req.form_value()` (nested `user[name]` keys), `req.remote_addr()` and
`req.matched_path()` (the route pattern, e.g. `/users/:id`).

## Responses

A handler returns anything that implements `IntoResponse`:

| Return | Response |
|---|---|
| `&str`, `String` | 200 `text/plain` |
| `Json(value)`, `serde_json::Value` | 200 `application/json` |
| `Html(text)` | 200 `text/html` |
| `Vec<u8>`, `Bytes` | 200 `application/octet-stream` |
| `(status, body)` | `body` with the given status |
| `Option<T>` | `T`, or 404 for `None` |
| `()` | 204 No Content |
| `Result<T, E>` | `T`, or the error (any `E: Into<ServerError>`) |
| `Response` | full control over status, headers and body |

```rust
use axeon::{Response, Server};

let mut app = Server::new();
app.get("/report", |_req| async {
    let mut res = Response::binary("text/csv", "id,name\n1,Ada\n");
    res.attachment("report.csv").cache_control("no-store");
    res
});
app.get("/created", |_req| async { Response::text("made").with_status(201).with_header("X-Id", "7") });
```

When a handler's only result is `Ok(..)`, with no `?` or `Err`, write the
error type once: `Ok::<_, ServerError>(..)`, or return the value directly.

## Errors

Return `Err(ServerError::...)` (or use `?`): Axeon answers with the matching
status and a JSON body `{"error": {"message": ..., "status": ...}}`.
Customise every error response with `on_error`:

```rust
use axeon::{Response, Server, ServerError};

let mut app = Server::new();
app.get("/admin", |_req| async { Err::<&'static str, _>(ServerError::Forbidden("admins only".into())) });
app.on_error(|err| Response::text(format!("Something went wrong: {}", err)).with_status(err.status_code()));
```

Panics in handlers become `500 Internal Server Error`; their messages are
logged, never sent to clients.

## Middleware

Middleware wraps requests, in the order added. Server middleware covers every
request (including 404s and static files); router and route middleware cover
their own routes. Errors reach middleware as responses, so headers it adds
appear on error responses too.

```rust
use axeon::{Server, ServerError};
use axeon::middleware::from_fn;

#[derive(Clone)]
struct User(String);

let mut app = Server::new();
app.middleware(from_fn(|mut req, next| async move {
    let Some(token) = req.bearer_token() else {
        return Err(ServerError::Unauthorized("login required".into()));
    };
    let user = User(token.to_string()); // look the user up here
    req.insert_extension(user);         // typed data for handlers
    let mut res = next.handle(req).await?;
    res.header("X-Powered-By", "axeon");
    Ok(res)
}));
app.get("/me", |req| async move { req.extension::<User>().map(|user| user.0.clone()) });
```

Middleware can also be a type implementing `Middleware`, and any tower layer
works through `layer`:

```rust
use axeon::Server;
use std::time::Duration;
use tower_http::timeout::TimeoutLayer;

let mut app = Server::new();
app.layer(TimeoutLayer::with_status_code(http::StatusCode::REQUEST_TIMEOUT, Duration::from_secs(10)));
```

Shared state (database pools, clients, configuration) is registered once and
read from any handler:

```rust
use axeon::Server;

struct Config {
    greeting: &'static str,
}

let mut app = Server::new();
app.state(Config { greeting: "hello" });
app.get("/", |req| async move { req.state::<Config>().map(|config| config.greeting) });
```

### Built in

| Middleware | Purpose |
|---|---|
| `middleware::SecurityHeaders` | HSTS, nosniff, frame options, referrer and cross-origin policies, CSP |
| `middleware::Cors` | CORS headers and preflights |
| `middleware::RateLimiter` | GCRA or daily quotas per IP, API key or user, plan-based limits, `RateLimit-*` and `Retry-After` headers; shared across instances with `axeon_redis::RedisRateLimitStore` |
| `middleware::idempotency::Idempotency` | `Idempotency-Key`: retries replay the first response; memory, Redis or PostgreSQL store |
| `middleware::Compression` | gzip/deflate |
| `cache::ConditionalGet` | ETags, `Cache-Control` and `304 Not Modified` for API responses |
| `middleware::SetRequestId`, `RequestLogger` | Request IDs and structured logs |
| `auth::BearerAuth`, `auth::BasicAuth`, `auth::jwt::Jwt` | Authentication |
| `auth::api_key::ApiKeyAuth` | API keys: hashed, constant-time checked, cached, with scopes and batched `last_used_at` |
| `auth::require_scope`, `auth::require_role` | Authorization guards over the caller's `Grants` (401 / 403) |
| `cookies::CookieManager` | Cookie jar with signed and encrypted cookies |
| `session::Sessions` | Server-side sessions |
| `csrf::Csrf` | CSRF protection for forms and SPAs |
| `metrics::Metrics` | Prometheus metrics |

```rust
use axeon::Server;
use axeon::middleware::{Compression, Cors, RateLimiter, SecurityHeaders};

let mut app = Server::new();
app.middleware(SecurityHeaders::new())
    .middleware(Compression::new())
    .middleware(Cors::new().allow_origin("https://app.example.com").allow_credentials(true))
    .middleware(RateLimiter::per_minute(120).burst(20)); // .trust_proxy(true) behind a proxy
```

## Building a Backend

### Validation

```rust
use axeon::{Json, Server, ServerError};
use axeon::validate::{is_email, Validate, ValidationErrors};
use serde::Deserialize;

#[derive(Deserialize)]
struct SignUp {
    email: String,
    password: String,
}

impl Validate for SignUp {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        errors.check(is_email(&self.email), "email", "must be an email address");
        errors.check(self.password.len() >= 12, "password", "must be at least 12 characters");
        errors.into_result()
    }
}

let mut app = Server::new();
app.post("/signup", |req| async move {
    let form: SignUp = req.valid_json()?; // 422 with {"fields": {"email": [...]}}
    Ok::<_, ServerError>((201, Json(form.email)))
});
```

### Authentication

```rust
use axeon::{Json, Router, Server, ServerError};
use axeon::auth::jwt::{now, Jwt};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: u64,
}

let jwt = Jwt::hs256(b"load at least 32 random bytes from config!");
let mut app = Server::new();
app.state(jwt.clone());
app.post("/login", |req| async move {
    // check credentials here, then:
    let jwt = req.state::<Jwt>().unwrap();
    let token = jwt.issue(&Claims { sub: "ada".into(), exp: now() + 3600 })?;
    Ok::<_, ServerError>(Json(token))
});

let mut api = Router::new();
api.middleware(jwt.middleware::<Claims>()); // 401 without a valid token
api.get("/me", |req| async move { req.extension::<Claims>().map(|c| c.sub.clone()) });
app.mount("/api", api);
```

`BearerAuth` checks opaque tokens with your own async lookup, and
`BasicAuth` protects internal tools.

For public APIs, `ApiKeyAuth` handles API keys end to end:

```rust
use axeon::auth::api_key::{generate_key, ApiKey, ApiKeyAuth, ApiKeyRecord, KeyHash};
use axeon::Server;
use std::time::Duration;
# use axeon::ServerError;
# #[derive(Clone)] struct Org { id: i64 }
# struct StoredKey { hash: KeyHash, scopes: Vec<String>, org: Org }
# async fn find_key(_id: &str) -> Result<Option<StoredKey>, ServerError> { Ok(None) }
# async fn touch_last_used(_ids: Vec<String>) {}

// When a customer creates a key, show `new.key` once and store
// `new.id` and `new.hash.to_hex()`.
let new = generate_key("vs_live"); // vs_live_<id>_<secret>

let mut app = Server::new();
app.middleware(
    ApiKeyAuth::new("vs_live", |id: String| async move {
        // Look the key up by ID, not by secret; None if unknown or revoked.
        let key = find_key(&id).await?;
        Ok(key.map(|k| ApiKeyRecord { hash: k.hash, scopes: k.scopes, identity: k.org }))
    })
    .on_used(Duration::from_secs(60), |ids| async move { touch_last_used(ids).await }),
);
app.get("/v1/products", |req| async move {
    let key = req.extension::<ApiKey>().unwrap(); // id and scopes
    let org = req.extension::<Org>().unwrap();
    format!("{} for org {}", key.id, org.id)
});
```

Keys are sent as `Authorization: Bearer` or `X-API-Key` and compared by
SHA-256 hash in constant time. Lookups are cached for 60 seconds
(`cache_ttl`), which is also how long a revoked key keeps working.

Authentication attaches the caller's `Grants` (scopes and roles):
`ApiKeyAuth` does it from the key's scopes, and your own JWT or session
middleware can insert `Grants::new().scopes_from(&claims.scope).role("buyer")`.
Guards then protect routes or whole routers:

```rust
use axeon::auth::{require_role, require_scope};
use axeon::{Router, Server, ServerError};

let mut app = Server::new();
app.route("/v1/products").middleware(require_scope("catalog:read")).get(|_req| async { "[]" });

let mut ops = Router::new();
ops.middleware(require_role("operator"));
ops.get("/cases", |_req| async { "[]" });
app.mount("/ops", ops);

// Or inside a handler, when the check depends on the request:
app.post("/v1/exports", |req| async move {
    req.require_scope("catalog:export")?;
    Ok::<_, ServerError>("started")
});
```

Without `Grants` a guard answers 401; without the scope or role, 403
(with an RFC 6750 `insufficient_scope` challenge for scopes). A granted
`catalog:*` covers every `catalog:` scope.

### Caching API responses

```rust
use axeon::cache::{CacheControl, ConditionalGet, ETag};
use axeon::{Response, Server, ServerError};

let mut app = Server::new();
// Weak ETags from bodies and 304 for If-None-Match; the handler still runs.
app.middleware(ConditionalGet::new().cache_control(CacheControl::revalidate()));

// To skip the work too, compare a cheap validator such as a version column.
app.get("/v1/products/:id", |req| async move {
    let etag = ETag::strong(format!("{}-v{}", req.param::<i64>("id")?, 7));
    if req.is_fresh(Some(&etag), None) {
        return Ok(Response::not_modified(&etag));
    }
    Ok::<_, ServerError>(Response::text("...the product JSON...").with_etag(&etag))
});
```

`CacheControl` builds the header (`public`, `private`, `max-age`,
`s-maxage`, `no-cache`, `stale-while-revalidate`...), and
`with_last_modified` enables `If-Modified-Since`. Add `ConditionalGet`
after `Compression`, so ETags describe the uncompressed data.

### Cookies, sessions and CSRF

```rust
use axeon::{Server, ServerError};
use axeon::cookies::{Cookie, CookieManager, Key};
use axeon::csrf::Csrf;
use axeon::session::{MemoryStore, Sessions};

let mut app = Server::new();
app.middleware(CookieManager::new().key(Key::generate())) // load the key from config in production
    .middleware(Sessions::new(MemoryStore::new()).secure(true))
    .middleware(Csrf::new());

app.post("/login", |req| async move {
    let session = req.session()?;
    session.regenerate(); // new session ID on login
    session.insert("user_id", 42)?;
    req.cookies().signed().add(Cookie::new("theme", "dark"));
    Ok::<_, ServerError>("logged in")
});
```

Sessions use a `SessionStore` trait: `MemoryStore` is built in; implement it
for Redis or your database when running several instances.

### Configuration

```rust,no_run
use axeon::config;
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    database_url: String,         // DATABASE_URL
    port: u16,                    // PORT
    allowed_origins: Vec<String>, // ALLOWED_ORIGINS=a.com,b.com
    redis_url: Option<String>,    // optional
}

config::load_dotenv().expect("invalid .env");
let config: Config = config::from_env().expect("invalid configuration"); // lists every problem
```

### Databases

[axeon-orm](axeon-orm) is the companion ORM: models are plain structs, with
typed queries, relations (including many-to-many), aggregates, joins,
migrations and pagination, for PostgreSQL, MySQL and SQLite (built on
sqlx). It plugs into Axeon: `?` turns "not found" into a
404 and constraint violations into a 409, and `Transactional` wraps each
request in a transaction.

```rust,ignore
use axeon::{Json, Server, ServerError};
use axeon_orm::prelude::*;

#[derive(Model, serde::Serialize)]
struct Post {
    id: i64,
    title: String,
    published: bool,
}

let db = Db::connect("postgres://localhost/blog").await?;
axeon_orm::embed_migrations!().run(&db).await?;

let mut app = Server::new();
app.state(db);
app.get("/posts", |req| async move {
    let page = req.query_param("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    let posts = Post::query()
        .filter(Post::PUBLISHED.eq(true))
        .order_by(Post::ID.desc())
        .paginate(req.db()?, page, 20)
        .await?;
    Ok::<_, ServerError>(Json(posts))
});
```

Prefer plain sqlx, SeaORM or Diesel? Put the pool in `app.state(pool)` and
read it with `req.state::<PgPool>()`; Axeon doesn't depend on any database
crate.

### Companion crates

Optional crates that build on Axeon, each versioned on its own:

| Crate | What it does |
|---|---|
| [axeon-orm](axeon-orm) | Models, typed queries, relations, migrations, search and pgvector |
| [axeon-jobs](axeon-jobs) | Background jobs on PostgreSQL: retries, delays, unique jobs, cron schedules, priorities |
| [axeon-redis](axeon-redis) | Shared sessions, rate limits, idempotency keys and broadcast on Redis |
| [axeon-openapi](axeon-openapi) | OpenAPI 3.1 documents generated from your routes and Rust types |
| [axeon-client](axeon-client) | An outbound HTTP client with SSRF protection, retries and circuit breaking |
| [axeon-otel](axeon-otel) | OpenTelemetry: a server span per request, W3C trace context, OTLP export |

### Templates, static files and uploads

```rust
use axeon::{json, Server, ServerError};
use axeon::templates::Templates;

let mut app = Server::new();
app.static_dir("public")          // streamed, with range requests and 304s
    .spa_fallback("index.html")   // deep links in a single-page app
    .templates(Templates::from_dir("views"));

app.get("/hello/:name", |req| async move {
    req.render("hello.html", json!({ "name": req.params()["name"] })) // auto-escaped
});

// Large uploads: stream straight to disk instead of buffering.
app.route("/upload").body_limit(1 << 30).stream_body().post(|req| async move {
    let mut form = req.multipart()?;
    while let Some(field) = form.next_field().await? {
        if field.file_name().is_some() {
            field.save_to(std::env::temp_dir().join("upload.bin")).await?;
        }
    }
    Ok::<_, ServerError>("uploaded")
});

// Untrusted files: a per-field cap, the real type from the first bytes
// (not the client's Content-Type), and SHA-256 while streaming to any
// AsyncWrite, such as an object-store upload.
app.route("/documents").body_limit(60 << 20).stream_body().post(|req| async move {
    let mut form = req.multipart()?;
    let Some(field) = form.next_field().await? else { return Err(ServerError::BadRequest("no file".into())) };
    let upload = field
        .limit(50 << 20)                              // 413 above 50 MiB
        .accept(&["application/pdf", "image/*"])      // 415 for anything else
        .save(std::env::temp_dir().join("doc.bin"))   // or .write_to(&mut writer)
        .await?;
    Ok(json!({ "sha256": upload.sha256_hex(), "size": upload.size(), "type": upload.content_type() }))
});
```

### Realtime

```rust
use axeon::{Response, Server};
use axeon::sse::{self, Event};
use futures::stream;

let mut app = Server::new();

app.get("/numbers", |_req| async {
    let chunks = stream::iter((1..=3).map(|n| Ok::<_, std::io::Error>(format!("{}\n", n))));
    Response::stream("text/plain", chunks)
});

app.get("/events", |_req| async {
    let (tx, response) = sse::channel(16);
    tokio::spawn(async move {
        let _ = tx.send(Event::new().event("greeting").data("hello")).await;
    });
    response
});

app.get("/ws", |mut req| async move {
    req.upgrade_websocket(|mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            if socket.send(message).await.is_err() {
                break;
            }
        }
    })
});
```

With several instances, publish through a `broadcast::Hub` instead: every
instance streams the messages, and a browser that reconnects with
`Last-Event-ID` gets what it missed from whichever instance it reaches.

```rust
use axeon::broadcast::{BoxError, Hub, MemoryBackend, Message};
use axeon::Server;

// One process: MemoryBackend. Several: axeon_redis::RedisBroadcast or
// axeon_orm::broadcast::PgBroadcast.
let hub = Hub::new(MemoryBackend::new());
let mut app = Server::new();
app.state(hub.clone());
app.get("/runs/:run/events", |req| async move {
    let run: u64 = req.param("run")?;
    req.state::<Hub>().unwrap().sse(&format!("run:{}", run), req.last_event_id()).await
});

// Anywhere, on any instance, e.g. in a background job:
async fn step_done(hub: &Hub) -> Result<(), BoxError> {
    hub.publish("run:42", Message::new(r#"{"step":3}"#).event("step")).await?;
    Ok(())
}
```

If the client's last message is no longer kept, the stream starts with a
`reset` event so the client can reload its state.

## Running in Production

### Settings

```rust
use axeon::Server;
use std::time::Duration;

let mut app = Server::new();
app.max_connections(10_000)                    // default 1024; extra clients wait
    .keep_alive(Duration::from_secs(15))       // idle connections, default 5s
    .request_timeout(Duration::from_secs(10))  // headers, body, TLS handshake; default 30s
    .handler_timeout(Duration::from_secs(15))  // 503 if a handler takes longer; default none
    .write_timeout(Duration::from_secs(10))    // clients that stop reading; default 30s
    .shutdown_delay(Duration::from_secs(5))    // stay up but not-ready after SIGTERM
    .shutdown_timeout(Duration::from_secs(20)) // then wait this long for requests
    .max_body_size(1024 * 1024)                // 413 above this; default 2 MiB
    .max_header_size(16 * 1024);               // 431 above this; default 16 KiB

// Routes can override the handler timeout, longer or shorter, or opt out.
app.route("/v1/products").timeout(Duration::from_secs(2)).get(|_req| async { "[]" });
app.route("/documents").stream_body().timeout(Duration::from_secs(300)).post(|_req| async { "stored" });
app.route("/assistant").no_timeout().post(|_req| async { "streamed" });
```

A timed-out handler is cancelled at its next `.await` (an open transaction
rolls back) and the client gets 503. Global middleware such as logging and
metrics still sees the response. A streamed response only has to start in
time; a `stream_body` handler reads its body within the limit.

HTTPS is `app.with_tls("cert.pem", "key.pem")`; HTTP/2 is negotiated
automatically.

### Graceful shutdown, health and metrics

`listen()` handles Ctrl+C and SIGTERM: during `shutdown_delay` it keeps
serving while readiness reports failure (so load balancers move traffic
away), then stops accepting, lets in-flight requests finish, ends event
streams, closes WebSockets and exits.

```rust
use axeon::Server;
use axeon::health::Health;
use axeon::metrics::Metrics;
use axeon::middleware::{RequestLogger, SetRequestId};

let metrics = Metrics::new();
let mut app = Server::new();
app.middleware(SetRequestId::new())
    .middleware(RequestLogger::new()) // logs through `tracing`
    .middleware(metrics.clone())
    .mount("/", metrics.router())     // GET /metrics (Prometheus)
    .mount("/", Health::new()         // GET /health and /ready
        .check("database", || async { Ok::<_, std::io::Error>(()) })
        .router());
```

Application metrics go on the same registry and endpoint:

```rust
use axeon::metrics::{Metrics, DEFAULT_BUCKETS};

let metrics = Metrics::new(); // the one passed to `app.middleware` above
let tokens = metrics.counter_vec("llm_tokens_total", "LLM tokens used.", &["model"]);
let queue = metrics.gauge_vec("queue_depth", "Jobs waiting.", &["queue"]);
let step = metrics.histogram("workflow_step_seconds", "Step duration.", &DEFAULT_BUCKETS);

tokens.with(&["qwen-max"]).inc_by(812.0);
queue.with(&["onboarding"]).set(17.0);
let timer = step.start_timer(); // records when observed or dropped
```

Keep label values to a small, fixed set (a model, a queue name), never IDs.

Inside your own Tokio runtime (e.g. to connect to a database first), use
`app.serve_with_shutdown(listener, axeon::shutdown_signal())`.

### Testing

`Server::into_app()` runs requests through routing, middleware and error
handling in-process:

```rust
use axeon::{Method, Request, Server};

fn app() -> Server {
    let mut app = Server::new();
    app.get("/hello/:name", |req| async move { format!("Hello, {}!", req.params()["name"]) });
    app
}

#[tokio::test]
async fn says_hello() {
    let app = app().into_app().await.unwrap();
    let res = app.call(Request::new(Method::GET, "/hello/Ada")).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body_text(), "Hello, Ada!");
}
```

`App::into_service()` turns the app into a tower `Service` to mount inside
another framework or serve with your own hyper setup.

## Performance

Apple M1, 128 keep-alive connections, best of three runs
([details and caveats](benchmarks/README.md)):

| Framework | JSON | Path param | p99 latency | Memory |
|---|---|---|---|---|
| actix-web 4 | 207k req/s | 190k req/s | 5-9 ms | 10.6 MiB |
| axum 0.8 | 195k req/s | 197k req/s | 2-3 ms | 8.5 MiB |
| **Axeon** | **176k req/s** | **185k req/s** | **2 ms** | **8.9 MiB** |
| Express 5 (8 workers) | 39k req/s | 45k req/s | 14-22 ms | 877 MiB |

## Cargo Features

All features are on by default. With `default-features = false`, pick what
you need (a minimal build compiles about half as many crates):

| Feature | Enables |
|---|---|
| `tls` | HTTPS with rustls, HTTP/2 over ALPN |
| `compression` | gzip/deflate `Compression` middleware |
| `ws` | WebSockets |
| `multipart` | Streaming multipart uploads |
| `cookies` | Cookies, sessions and CSRF protection |
| `jwt` | JSON Web Tokens |
| `api-keys` | API keys (`auth::api_key`) |
| `idempotency` | `Idempotency-Key` middleware |
| `templates` | HTML templates (minijinja) |

## Examples

```bash
cargo run --example hello_world
cargo run --example routing
cargo run --example middleware
cargo run --example realtime    # server-sent events and WebSockets
cargo run --example production  # logging, security middleware, graceful shutdown
```

[`examples/todo-api`](examples/todo-api) is a complete REST API: SQLite via
axeon-orm with migrations, JWT login, validation, health checks with a
database ping, metrics, graceful shutdown, and tests.

## Stability

Axeon follows [semantic versioning](https://semver.org): no breaking
changes in 1.x. In particular:

- The minimum supported Rust version is **1.88**. Raising it is not
  considered breaking and only happens in a minor release (1.x.0), noted in
  the changelog.
- Axeon's public API exposes a few other crates' types: `http` 1, `bytes`
  1, `serde_json` 1, `futures` 0.3 (`StreamExt`, `SinkExt`), `tower` 0.3
  service traits and, with the `cookies` feature, `cookie` 0.18. Moving to
  a breaking version of any of these waits for Axeon 2.0. WebSocket and JWT
  types are Axeon's own, so their underlying crates can be upgraded freely.

## Documentation

- [API documentation](https://docs.rs/axeon)
- [Changelog and migration guide](CHANGELOG.md)

## Contributing

Contributions are welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for how to
build, test and submit changes, and [open an issue](https://github.com/axeon-rs/axeon/issues/new/choose)
for bugs and ideas. Please report security problems privately as described in
[SECURITY.md](SECURITY.md). Everyone taking part is expected to follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

## License

This project is licensed under the [MIT License](LICENSE).
