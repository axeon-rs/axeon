use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use serde_json::json;

#[get("/plaintext")]
async fn plaintext() -> impl Responder {
    "Hello, World!"
}

#[get("/json")]
async fn json_route() -> impl Responder {
    HttpResponse::Ok().json(json!({ "message": "Hello, World!" }))
}

#[get("/users/{id}")]
async fn user(id: web::Path<u64>) -> impl Responder {
    HttpResponse::Ok().json(json!({ "id": id.into_inner(), "name": "user" }))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(|| App::new().service(plaintext).service(json_route).service(user))
        .bind(("127.0.0.1", 8003))?
        .run()
        .await
}
