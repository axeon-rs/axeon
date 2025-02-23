# Axeon

A modern, flexible, and feature-rich web framework for Rust.

## Features

- Express-style routing with path parameters
- Middleware support (logging, security, rate limiting, etc.)
- Static file serving
- Built-in security features
- JSON request/response handling
- Async/await support

## Quick Start

Add Axeon to your `Cargo.toml`:
```toml
[dependencies]
axeon = "0.1.0"
```

Create a simple server:
```rust
use axeon::app::Application;
use axeon::ok_json;

fn main() {
    let mut app = Application::new();

    app.get("/", |_req| async {
        ok_json!({
            "message": "Hello, World!"
        })
    });

    app.listen("127.0.0.1:3000").unwrap();
}
```

## License

This project is licensed under the MIT License.