//! Routing example for Axeon
//!
//! This example demonstrates different routing techniques including:
//! - Basic routes
//! - Path parameters
//! - Query parameters
//! - Different HTTP methods

use axeon::{Response, Router, Server, ServerError};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct User {
    name: String,
    role: String,
}

fn main() {
    let mut app = Server::new();

    // Basic GET route
    app.get("/", |_req| async {
        Response::text("Welcome to Axeon API server!")
    });

    // Route with path parameter
    app.get("/users/:id", |req| async move {
        let user_id = req.params.get("id").unwrap();
        Response::text(format!("User ID: {}", user_id))
    });

    // POST request with JSON body
    app.post("/users", |req| async move {
        match req.body.json::<User>() {
            Some(user) => Response::ok(&user),
            None => Err(ServerError::BadRequest("Invalid JSON body".to_string())),
        }
    });

    // Group routes under /api prefix
    let mut api = Router::new();
    api.get("/status", |_req| async {
        Response::ok(&serde_json::json!({
            "status": "operational",
            "version": "1.0.0"
        }))
    });

    // Mount the API router to the main server
    app.mount("/api", api);

    app.listen("127.0.0.1:3000")
        .expect("Server failed to start")
}
