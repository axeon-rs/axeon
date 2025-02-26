//! A minimal "Hello, World!" server using Axeon
//!
//! This example demonstrates how to create a basic server that responds with
//! "Hello, World!" for all requests.

use axeon::{Response, Server};

fn main() {
    let mut app = Server::new();

    // Add a route that handles GET requests to "/"
    app.get("/", |_req| async { Response::text("Hello, World!") });

    app.listen("127.0.0.1:3000")
        .expect("Server failed to start");
}
