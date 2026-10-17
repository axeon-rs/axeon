//! Routing example for Axeon
//!
//! This example demonstrates:
//! - Basic routes and path parameters
//! - Query parameters and JSON bodies with typed helpers
//! - Route groups mounted under a prefix
//!
//! Try it:
//!   curl http://127.0.0.1:3000/users/42
//!   curl "http://127.0.0.1:3000/search?q=rust&page=2"
//!   curl -X POST http://127.0.0.1:3000/users -H 'content-type: application/json' -d '{"name":"Ada","role":"admin"}'

use axeon::{json, Json, Router, Server, ServerError};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct User {
    name: String,
    role: String,
}

#[derive(Deserialize)]
struct Search {
    q: String,
    page: Option<u32>,
}

fn main() {
    let mut app = Server::new();

    app.get("/", |_req| async { "Welcome to the Axeon API!" });

    // Path parameter, parsed into a number (400 if it isn't one).
    app.get("/users/:id", |req| async move {
        let id: u64 = req.param("id")?;
        Ok::<_, ServerError>(Json(json!({ "id": id })))
    });

    // Query string into a struct.
    app.get("/search", |req| async move {
        let search: Search = req.query_as()?;
        Ok::<_, ServerError>(format!("searching {:?}, page {}", search.q, search.page.unwrap_or(1)))
    });

    // JSON body: 415 if not JSON, 400 if malformed, 422 if fields don't match.
    app.post("/users", |req| async move {
        let user: User = req.json()?;
        Ok::<_, ServerError>((201, Json(user)))
    });

    // A group of routes under /api.
    let mut api = Router::new();
    api.get("/status", |_req| async { Json(json!({ "status": "operational" })) });
    app.mount("/api", api);

    app.listen("127.0.0.1:3000").expect("server failed to start");
}
