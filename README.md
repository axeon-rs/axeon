# Axeon

A modern, flexible, and feature-rich web framework for Rust.

## Features

- Built on [hyper](https://hyper.rs): HTTP/1.1 and HTTP/2 (ALPN over TLS)
- Express-style routing with path parameters (`/users/:id`), catch-alls (`/files/*path`) and query parameters, matched by a radix tree
- Modular router system with mounting capabilities
- Powerful middleware system for request/response processing
- Built-in security features and authentication support
- JSON request/response handling with type safety
- Async/await support throughout the framework
- Built-in logging and error handling

## Quick Start

Add Axeon to your `Cargo.toml`:

```toml
[dependencies]
axeon = "0.3.0"
```

Create a simple server:

```rust,no_run
use axeon::{Response, Server};

fn main() {
    let mut app = Server::new();
    app.get("/", |_req| async { 
        Response::text("Hello, World!") 
    });
    app.listen("127.0.0.1:3000")
        .expect("Server failed to start");
}
```

## Examples

### Basic Routing

```rust
use axeon::{Response, Server};

let mut app = Server::new();

// Basic GET route
app.get("/", |_req| async {
    Response::text("Welcome!")
});

// Route with path parameter
app.get("/users/:id", |req| async move {
    let user_id = req.params.get("id").unwrap();
    Response::text(format!("User ID: {}", user_id))
});
```

### JSON Handling

```rust
use axeon::{Response, Server, ServerError};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct User {
    name: String,
    role: String,
}

let mut app = Server::new();

// POST endpoint with JSON handling
app.post("/users", |req| async move {
    match req.body.json::<User>() {
        Some(user) => Response::ok(&user),
        None => Err(ServerError::BadRequest("Invalid JSON body".to_string())),
    }
});
```

### Middleware

```rust
use axeon::{Request, Server, middleware::{Middleware, MiddlewareResult, Next}};

struct Logger;
impl Middleware for Logger {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        Box::pin(async move {
            // Process request...
            let res = next.handle(req).await;
            // Process response...
            res
        })
    }
}

let mut app = Server::new();
app.middleware(Logger);
```

### Router Groups

```rust
use axeon::{Response, Router, Server};

let mut app = Server::new();
let mut api = Router::new();

// Define routes for the API group
api.get("/status", |_req| async {
    Response::ok(&serde_json::json!({
        "status": "operational"
    }))
});

// Mount the API router with a prefix
app.mount("/api", api);
```

### Handlers Return Anything

Handlers can return `&str`, `String`, `Json(..)`, `Html(..)`, `Vec<u8>`,
`(status, body)`, `Option<T>` (`None` is 404), `()` (204), `Response`, or a
`Result` of any of these.

```rust
use axeon::{Json, Server, ServerError, json};

let mut app = Server::new();
app.get("/", |_req| async { "Hello!" });
app.post("/users", |req| async move {
    let user: serde_json::Value = req.json()?; // 415 / 400 / 422 on bad input
    Ok::<_, ServerError>((201, Json(user)))
});
app.get("/users/:id", |req| async move {
    let id: u64 = req.param("id")?; // 400 if not a number
    Ok::<_, ServerError>(Json(json!({ "id": id })))
});
```

When a handler's only result is `Ok(..)` (no `?` or `Err`), write the error
type once: `Ok::<_, ServerError>(..)`, or return the value directly.

### Closure Middleware

```rust
use axeon::{Server, ServerError};
use axeon::middleware::from_fn;

let mut app = Server::new();
app.middleware(from_fn(|req, next| async move {
    if req.get_header("authorization").is_none() {
        return Err(ServerError::Unauthorized("login required".into()));
    }
    let mut res = next.handle(req).await?;
    res.header("X-Powered-By", "axeon");
    Ok(res)
}));
```

Middleware can pass typed values to handlers with `req.insert_extension(value)`
and `req.extension::<T>()`. Shared app state registered with
`app.plugins(value)` is available as `req.state::<T>()`.

### Streaming, Server-Sent Events and WebSockets

```rust
use axeon::Server;
use axeon::sse::{self, Event};
use axeon::ws::{SinkExt, StreamExt};

let mut app = Server::new();

app.get("/events", |_req| async {
    let (tx, response) = sse::channel(16);
    tokio::spawn(async move {
        let _ = tx.send(Event::new().event("greeting").data("hello")).await;
    });
    response
});

app.get("/ws", |mut req| async move {
    req.upgrade_websocket(|mut socket| async move {
        while let Some(Ok(msg)) = socket.next().await {
            let _ = socket.send(msg).await;
        }
    })
});
```

`Response::stream(content_type, stream)` sends any `Stream` of chunks without
buffering it. WebSockets need the default `ws` feature.

## Running in Production

### Graceful Shutdown

`listen()` shuts down gracefully on Ctrl+C or SIGTERM: it stops accepting
connections, lets in-flight requests finish (up to `shutdown_timeout`, default
30s) and closes idle keep-alive connections. Inside your own runtime, use
`serve_with_shutdown` with any future as the signal:

```rust,no_run
use axeon::{Server, shutdown_signal};
use std::time::Duration;

#[tokio::main]
async fn main() {
    let mut app = Server::new();
    app.shutdown_timeout(Duration::from_secs(20));
    app.get("/", |_req| async { "Hello" });
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    app.serve_with_shutdown(listener, shutdown_signal()).await.unwrap();
}
```

### Logging

Axeon logs through [`tracing`](https://docs.rs/tracing). Install a subscriber
to see the logs, and add `SetRequestId` and `RequestLogger` for one line per
request tagged with its ID:

```rust
use axeon::Server;
use axeon::middleware::{RequestLogger, SetRequestId};

// Needs the `tracing-subscriber` crate in your app; `.json()` for JSON logs.
tracing_subscriber::fmt().init();

let mut app = Server::new();
app.middleware(SetRequestId::new());   // X-Request-Id in and out
app.middleware(RequestLogger::new());  // method, path, status, latency
app.get("/", |_req| async {
    tracing::info!("this line carries the request ID");
    "Hello"
});
```

### Testing Without a Socket

`Server::into_app()` builds an `App` that handles requests in-process:

```rust
use axeon::{Method, Request, Server};

# tokio::runtime::Runtime::new().unwrap().block_on(async {
let mut app = Server::new();
app.get("/hello/:name", |req| async move { format!("Hello, {}!", req.params["name"]) });
let app = app.into_app().await.unwrap();

let res = app.call(Request::new(Method::GET, "/hello/Ada")).await;
assert_eq!(res.body_text(), "Hello, Ada!");
# });
```

### Cargo Features

All features are on by default. To trim dependencies and build time, turn off
the defaults and pick what you need:

```toml
axeon = { version = "0.3", default-features = false, features = ["tls"] }
```

| Feature | Enables |
|---|---|
| `tls` | HTTPS with rustls (`Server::with_tls`), HTTP/2 over ALPN |
| `compression` | gzip/deflate `CompressionMiddleware` |
| `ws` | WebSockets (`Request::upgrade_websocket`) |
| `cache` | `axeon::cache` helpers backed by moka |

### Benchmarks

`cargo bench` runs in-process benchmarks of routing, middleware and JSON
responses (criterion). On an Apple Silicon laptop a dynamic route costs about
0.55 µs whether the app has 10 or 1000 routes, and each middleware layer adds
about 55 ns.

## Documentation

For detailed documentation and more examples, check out:
- [API Documentation](https://docs.rs/axeon)
- [Examples Directory](https://github.com/axeon-rs/axeon/tree/master/examples)

## License

This project is licensed under the MIT License.