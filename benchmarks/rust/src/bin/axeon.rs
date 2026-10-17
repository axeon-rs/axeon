use axeon::{json, Json, Server, ServerError};

fn main() {
    let mut app = Server::new();
    app.max_connections(10_000)
        .get("/plaintext", |_req| async { "Hello, World!" })
        .get("/json", |_req| async { Json(json!({ "message": "Hello, World!" })) })
        .get("/users/:id", |req| async move {
            let id: u64 = req.param("id")?;
            Ok::<_, ServerError>(Json(json!({ "id": id, "name": "user" })))
        });
    app.listen("127.0.0.1:8001").unwrap();
}
