//! Middleware example for Axeon
//!
//! This example demonstrates how to create and use middleware for:
//! - Logging requests
//! - Authentication
//! - CORS handling

use std::time::Instant;
use axeon::{Server, Request, Response, Router, ServerError, ok_json};
use axeon::middleware::{Middleware, MiddlewareResult, Next};

// Logger middleware that tracks request duration
struct Logger;

impl Middleware for Logger {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        Box::pin(async move {
            let start = Instant::now();
            let url = req.path.clone();
            let method = req.method.clone();
            let res = next.handle(req).await;
            let status = match &res {
                Ok(res) => res.status,
                Err(err) => err.status_code(),
            };
            let duration = start.elapsed().as_millis();
            println!("[{}] {:?} {} - {}ms", status, method, url, duration);
            res
        })
    }

    fn clone_box(&self) -> Box<dyn Middleware> {
        Box::new(Self)
    }
}

// Simple auth middleware
struct AuthMiddleware;

impl Middleware for AuthMiddleware {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        Box::pin(async move {
            // Check for token in Authorization header
            match req.get_header("Authorization") {
                Some(token) if token.starts_with("Bearer ") => next.handle(req).await,
                _ => Err(ServerError::Unauthorized("Authentication required".to_string())),
            }
        })
    }


    fn clone_box(&self) -> Box<dyn Middleware> {
        Box::new(Self)
    }
}

fn main() {
    let mut app = Server::new();

    // Apply logger middleware globally
    app.middleware(Logger);

    // Public route - no auth required
    app.get("/public", |_req| async  {
        Response::text("This is a public endpoint")
    });

    // Protected routes with auth middleware
    let mut protected = Router::new();
    protected.middleware(AuthMiddleware);

    protected.get("/profile", |_req| async  {
        ok_json!({
            "name": "User",
            "email": "user@example.com"
        })
    });

    app.mount("/api", protected);

    app.listen("127.0.0.1:3000")
        .expect("Server failed to start");
}
