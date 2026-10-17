# Axeon

A fast, Express-style web framework for Rust, built on [hyper](https://hyper.rs).

```rust,no_run
use axeon::Server;

fn main() {
    let mut app = Server::new();
    app.get("/", |_req| async { "Hello, World!" });
    app.listen("127.0.0.1:3000").expect("server failed to start");
}
```

## Features

- **Familiar routing**: `app.get("/users/:id", handler)`, catch-alls
  (`/files/*path`), mountable routers, matched by a radix tree whose lookup
  time doesn't depend on the number of routes.
- **Handlers return what they mean**: text, `Json(..)`, `Html(..)`, bytes,
  `(status, body)`, `Option` (404 on `None`) or `Result`.
- **Middleware as closures or types**, with typed per-request data and shared
  app state.
- **Typed requests**: `req.param::<u64>("id")`, `req.json::<T>()`,
  `req.query_as::<T>()`, `req.form::<T>()`, with the right 4xx errors.
- **HTTP/1.1 and HTTP/2**, HTTPS via rustls, streaming responses, server-sent
  events and WebSockets.
- **Production ready**: graceful shutdown, request limits and timeouts,
  backpressure, structured logging with request IDs, security headers, CORS,
  rate limiting and compression.
- **Testable**: run requests through your app in-process, without a socket.

## Installation

```toml
[dependencies]
axeon = "0.4"
tokio = { version = "1", features = ["full"] } # for #[tokio::main], tokio::spawn, etc.
```

Upgrading from 0.2? See the [migration guide](CHANGELOG.md#migrating-from-02).

## Routing

```rust
use axeon::{Router, Server};

let mut app = Server::new();

app.get("/", |_req| async { "home" });
app.post("/users", |_req| async { (201, "created") });
app.get("/users/:id", |req| async move { format!("user {}", req.params["id"]) });
app.get("/files/*path", |req| async move { format!("file {}", req.params["path"]) });

// Group routes and mount them under a prefix.
let mut api = Router::new();
api.get("/status", |_req| async { "ok" });
app.mount("/api", api); // GET /api/status
```

Static paths win over parameters (`/users/new` beats `/users/:id`), and two
routes that can never be told apart (`/users/:id` and `/users/:name`) are
reported when the server starts. `HEAD` and `OPTIONS` are answered
automatically, and a known path with the wrong method gets `405` with an
`Allow` header.

## Requests

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

Headers are available through `req.get_header("authorization")` (names are
case-insensitive) and `req.headers`; the peer address through
`req.remote_addr`.

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
app.get("/download", |_req| async {
    let mut res = Response::new(200);
    res.header("Content-Type", "text/csv")
        .header("Content-Disposition", "attachment; filename=\"report.csv\"")
        .append_header("Set-Cookie", "seen=1")
        .body("id,name\n1,Ada\n");
    res
});
```

When a handler's only result is `Ok(..)`, with no `?` or `Err`, write the error
type once, as in `Ok::<_, ServerError>(..)`, or return the value directly.

## Errors

Return `Err(ServerError::...)` (or use `?`) and Axeon answers with the matching
status and a JSON error body. Customise that with `on_error`:

```rust
use axeon::{Response, Server, ServerError};

let mut app = Server::new();
app.get("/admin", |_req| async {
    Err::<&'static str, _>(ServerError::Forbidden("admins only".into()))
});
app.on_error(|err| {
    let mut res = Response::new(err.status_code());
    res.body(format!("Something went wrong: {}", err));
    res
});
```

Panics in handlers become `500 Internal Server Error`; their messages are
logged, never sent to clients.

## Middleware

Middleware runs around every route registered after it, in order. Write it as
a closure:

```rust
use axeon::{Server, ServerError};
use axeon::middleware::from_fn;

#[derive(Clone)]
struct User(String);

let mut app = Server::new();
app.middleware(from_fn(|mut req, next| async move {
    let Some(token) = req.get_header("authorization") else {
        return Err(ServerError::Unauthorized("login required".into()));
    };
    let user = User(token.trim_start_matches("Bearer ").to_string());
    req.insert_extension(user); // typed data for later middleware and handlers
    let mut res = next.handle(req).await?;
    res.header("X-Powered-By", "axeon");
    Ok(res)
}));
app.get("/me", |req| async move { req.extension::<User>().map(|user| user.0.clone()) });
```

or as a type, which is handy when it has configuration or state:

```rust
use axeon::Request;
use axeon::middleware::{Middleware, MiddlewareResult, Next};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
struct CountRequests(AtomicU64);

impl Middleware for CountRequests {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        // One instance serves every request, so this count is global.
        self.0.fetch_add(1, Ordering::Relaxed);
        next.handle(req)
    }
}
```

Middleware added to a `Router` applies only to that router's routes. Shared
application state (database pools, configuration) is registered once and read
from any handler:

```rust
use axeon::Server;

struct Config {
    greeting: &'static str,
}

let mut app = Server::new();
app.plugins(Config { greeting: "hello" });
app.get("/", |req| async move { req.state::<Config>().map(|config| config.greeting) });
```

### Built-in middleware

| Middleware | Purpose |
|---|---|
| `SetRequestId` | Reuses or generates an `X-Request-Id` per request |
| `RequestLogger` | Logs method, path, status and latency through `tracing` |
| `SecurityHeaders` | HSTS, `X-Content-Type-Options`, `X-Frame-Options`, CSP |
| `Cors` | CORS headers and preflight responses |
| `RateLimiter` | Token-bucket limit per client IP |
| `CompressionMiddleware` | gzip/deflate, negotiated from `Accept-Encoding` |

```rust
use axeon::Server;
use axeon::middleware::{Cors, CorsConfig, RateLimitConfig, RateLimiter};

let mut app = Server::new();
app.middleware(Cors::new(CorsConfig {
    allow_origins: vec!["https://app.example.com".into()],
    allow_credentials: true,
    ..CorsConfig::default()
}));
app.middleware(RateLimiter::new(RateLimitConfig {
    requests_per_minute: 120,
    burst_size: 20,
    trust_proxy: false, // true only behind a proxy that sets X-Forwarded-For
}));
```

## Streaming, Server-Sent Events and WebSockets

```rust
use axeon::{Response, Server};
use axeon::sse::{self, Event};
use axeon::ws::{SinkExt, StreamExt};
use futures::stream;

let mut app = Server::new();

// Send chunks as they are produced, without buffering the whole body.
app.get("/numbers", |_req| async {
    let chunks = stream::iter((1..=3).map(|n| Ok::<_, std::io::Error>(format!("{}\n", n))));
    Response::stream("text/plain", chunks)
});

// Push events to the browser from any task.
app.get("/events", |_req| async {
    let (tx, response) = sse::channel(16);
    tokio::spawn(async move {
        let _ = tx.send(Event::new().event("greeting").data("hello")).await;
    });
    response
});

// Echo WebSocket messages back.
app.get("/ws", |mut req| async move {
    req.upgrade_websocket(|mut socket| async move {
        while let Some(Ok(message)) = socket.next().await {
            if socket.send(message).await.is_err() {
                break;
            }
        }
    })
});
```

`sse::Sender::send` fails once the client disconnects, so background tasks know
when to stop. WebSockets need the `ws` feature (on by default) and HTTP/1.1.

## Configuration

```rust
use axeon::Server;
use std::time::Duration;

let mut app = Server::new();
app.max_connections(10_000)                    // default 1024; extra clients wait
    .keep_alive(Duration::from_secs(15))       // idle connection timeout, default 5s
    .request_timeout(Duration::from_secs(10))  // headers, body, TLS handshake; default 30s
    .write_timeout(Duration::from_secs(10))    // clients that stop reading; default 30s
    .shutdown_timeout(Duration::from_secs(20)) // graceful shutdown limit; default 30s
    .max_body_size(1024 * 1024)                // 413 above this; default 2 MiB
    .max_header_size(16 * 1024)                // 431 above this; default 16 KiB, min 8 KiB
    .max_headers(64)                           // default 100
    .static_dir("public");                     // serve files for unmatched GET requests
```

HTTPS needs a PEM certificate chain and key; HTTP/2 is negotiated
automatically:

```rust,no_run
use axeon::Server;

let mut app = Server::new();
app.get("/", |_req| async { "secure" });
app.with_tls("cert.pem", "key.pem");
app.listen("0.0.0.0:443").unwrap();
```

## Running in Production

### Graceful shutdown

`listen()` shuts down gracefully on Ctrl+C or SIGTERM (sent by Kubernetes,
systemd and Docker): it stops accepting connections, lets in-flight requests
finish, closes idle connections and ends server-sent event streams. Anything
still open after `shutdown_timeout` is closed.

Inside your own Tokio runtime, use `serve_with_shutdown` with any future as
the signal:

```rust,no_run
use axeon::{shutdown_signal, Server};

#[tokio::main]
async fn main() {
    let mut app = Server::new();
    app.get("/", |_req| async { "Hello" });
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    app.serve_with_shutdown(listener, shutdown_signal()).await.unwrap();
}
```

Long-running handlers can watch `req.shutdown()`, for example to close
WebSockets with a "going away" frame so clients reconnect elsewhere.

### Logging

Axeon logs through [`tracing`](https://docs.rs/tracing). Install a subscriber
to see the logs, and add `SetRequestId` and `RequestLogger` for one line per
request; logs from your handlers carry the same request ID:

```rust
use axeon::Server;
use axeon::middleware::{RequestLogger, SetRequestId};

// Needs the `tracing-subscriber` crate; use `.json()` for JSON logs.
tracing_subscriber::fmt().init();

let mut app = Server::new();
app.middleware(SetRequestId::new());  // X-Request-Id in and out
app.middleware(RequestLogger::new()); // method, path, status, latency
app.get("/", |_req| async {
    tracing::info!("this line carries the request ID");
    "Hello"
});
```

### Testing

`Server::into_app()` builds an `App` that handles requests in-process, through
the same routing, middleware and error handling:

```rust
use axeon::{Method, Request, Server};

fn app() -> Server {
    let mut app = Server::new();
    app.get("/hello/:name", |req| async move { format!("Hello, {}!", req.params["name"]) });
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

Build requests with `Request::new(method, uri)` plus `.with_header(..)`,
`.with_body(..)` and `.with_json(..)`.

### Cargo features

All features are on by default. Turn off the defaults and pick what you need
to cut dependencies and build time (a minimal build compiles about half as
many crates):

```toml
axeon = { version = "0.4", default-features = false, features = ["tls"] }
```

| Feature | Enables |
|---|---|
| `tls` | HTTPS with rustls (`Server::with_tls`), HTTP/2 over ALPN |
| `compression` | gzip/deflate `CompressionMiddleware` |
| `ws` | WebSockets (`Request::upgrade_websocket`, `axeon::ws`) |
| `cache` | `axeon::cache` helpers backed by moka |

### Performance

`cargo bench` runs in-process benchmarks of routing, middleware and JSON
responses. On an Apple Silicon laptop, a request to a dynamic route takes about
0.55 µs whether the app has 10 or 1000 routes, and each middleware layer adds
about 55 ns.

## Examples

The [`examples`](examples) directory has runnable programs:

```bash
cargo run --example hello_world
cargo run --example routing
cargo run --example middleware
cargo run --example realtime    # server-sent events and WebSockets
cargo run --example production  # logging, security middleware, graceful shutdown
```

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
