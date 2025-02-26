# Axeon

A modern, flexible, and feature-rich web framework for Rust.

## Features

- Express-style routing with support for path parameters and query parameters
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
axeon = "0.2.0"
```

Create a simple server:

```rust
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
use axeon::{Server, middleware::{Middleware, Next}};

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

    fn clone_box(&self) -> Box<dyn Middleware> {
        Box::new(Self)
    }
}

let mut app = Server::new();
app.middleware(Logger);
```

### Router Groups

```rust
use axeon::{Router, Server};

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

## Documentation

For detailed documentation and more examples, check out:
- [API Documentation](https://docs.rs/axeon)
- [Examples Directory](https://github.com/axeon-rs/axeon/tree/master/examples)

## License

This project is licensed under the MIT License.