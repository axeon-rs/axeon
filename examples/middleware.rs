//! Middleware example for Axeon
//!
//! This example demonstrates:
//! - Closure middleware with `from_fn` (logging)
//! - Middleware as a type (an API key check)
//! - Scoping middleware to a router, and passing data to handlers
//!
//! Try it:
//!   curl http://127.0.0.1:3000/public
//!   curl http://127.0.0.1:3000/api/profile -H 'x-api-key: secret'

use axeon::middleware::{from_fn, Middleware, MiddlewareResult, Next};
use axeon::{json, Json, Request, Router, Server, ServerError};
use std::time::Instant;

/// The caller identified by the API key check.
#[derive(Clone)]
struct Caller(String);

/// Rejects requests without the right `X-Api-Key`.
struct ApiKey {
    key: String,
}

impl Middleware for ApiKey {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        if req.get_header("x-api-key") != Some(self.key.as_str()) {
            return Box::pin(async { Err(ServerError::Unauthorized("missing or wrong API key".into())) });
        }
        req.insert_extension(Caller("demo-user".into()));
        next.handle(req)
    }
}

fn main() {
    let mut app = Server::new();

    // Logs every request, including 404s.
    app.middleware(from_fn(|req, next| async move {
        let start = Instant::now();
        let (method, path) = (req.method(), req.path().to_string());
        let res = next.handle(req).await?;
        println!("[{}] {} {} - {:?}", res.status, method, path, start.elapsed());
        Ok(res)
    }));

    app.get("/public", |_req| async { "This is a public endpoint" });

    // Everything under /api needs the API key.
    let mut api = Router::new();
    api.middleware(ApiKey { key: "secret".into() });
    api.get("/profile", |req| async move {
        let caller = req.extension::<Caller>().map(|c| c.0.clone());
        Json(json!({ "user": caller }))
    });
    app.mount("/api", api);

    app.listen("127.0.0.1:3000").expect("server failed to start");
}
