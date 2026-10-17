use axum::{extract::Path, routing::get, Json, Router};
use serde_json::{json, Value};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/plaintext", get(|| async { "Hello, World!" }))
        .route("/json", get(|| async { Json(json!({ "message": "Hello, World!" })) }))
        .route("/users/{id}", get(|Path(id): Path<u64>| async move { Json::<Value>(json!({ "id": id, "name": "user" })) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8002").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
