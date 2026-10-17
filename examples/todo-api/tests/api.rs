//! The API end to end, in-process (no network), against an in-memory database.

use axeon::{App, Method, Request, Response};
use serde_json::{json, Value};
use todo_api::{app, connect, Config};

async fn test_app() -> App {
    let pool = connect("sqlite::memory:").await.unwrap();
    let config = Config {
        database_url: "sqlite::memory:".into(),
        port: 0,
        jwt_secret: "test secret that is at least 32 bytes long".into(),
        admin_password: "s3cret".into(),
        allowed_origins: vec![],
    };
    app(pool, config).into_app().await.unwrap()
}

fn body(res: &Response) -> Value {
    serde_json::from_slice(&res.body).unwrap_or(Value::Null)
}

async fn token(app: &App) -> String {
    let res = app
        .call(Request::new(Method::POST, "/auth/token").with_json(&json!({ "username": "admin", "password": "s3cret" })))
        .await;
    assert_eq!(res.status, 200);
    body(&res)["token"].as_str().unwrap().to_string()
}

fn authed(method: Method, uri: &str, token: &str) -> Request {
    Request::new(method, uri).with_header("authorization", &format!("Bearer {}", token))
}

#[tokio::test]
async fn crud_flow() {
    let app = test_app().await;
    let token = token(&app).await;

    let res = app.call(authed(Method::POST, "/todos", &token).with_json(&json!({ "title": "  write docs  " }))).await;
    assert_eq!(res.status, 201);
    assert_eq!(body(&res), json!({ "id": 1, "title": "write docs", "done": false }));
    app.call(authed(Method::POST, "/todos", &token).with_json(&json!({ "title": "ship it" }))).await;

    let res = app.call(authed(Method::PATCH, "/todos/1", &token).with_json(&json!({ "done": true }))).await;
    assert_eq!(body(&res), json!({ "id": 1, "title": "write docs", "done": true }));

    let res = app.call(authed(Method::GET, "/todos?done=false", &token)).await;
    assert_eq!(body(&res), json!([{ "id": 2, "title": "ship it", "done": false }]));

    assert_eq!(app.call(authed(Method::DELETE, "/todos/2", &token)).await.status, 204);
    assert_eq!(app.call(authed(Method::GET, "/todos/2", &token)).await.status, 404);
    assert_eq!(app.call(authed(Method::DELETE, "/todos/2", &token)).await.status, 404);
}

#[tokio::test]
async fn input_is_validated() {
    let app = test_app().await;
    let token = token(&app).await;

    let res = app.call(authed(Method::POST, "/todos", &token).with_json(&json!({ "title": "   " }))).await;
    assert_eq!(res.status, 422);
    assert_eq!(body(&res)["error"]["fields"]["title"][0], "must be 1 to 200 characters");
    assert_eq!(app.call(authed(Method::GET, "/todos?limit=1000", &token)).await.status, 422);
    assert_eq!(app.call(authed(Method::GET, "/todos/abc", &token)).await.status, 400);
    let empty = authed(Method::PATCH, "/todos/1", &token).with_json(&json!({}));
    assert_eq!(app.call(empty).await.status, 422);
}

#[tokio::test]
async fn authentication_is_required() {
    let app = test_app().await;
    assert_eq!(app.call(Request::new(Method::GET, "/todos")).await.status, 401);
    assert_eq!(app.call(authed(Method::GET, "/todos", "not-a-token")).await.status, 401);
    let wrong = Request::new(Method::POST, "/auth/token").with_json(&json!({ "username": "admin", "password": "nope" }));
    assert_eq!(app.call(wrong).await.status, 401);
}

#[tokio::test]
async fn operations_endpoints() {
    let app = test_app().await;
    let ready = app.call(Request::new(Method::GET, "/ready")).await;
    assert_eq!(ready.status, 200);
    assert_eq!(body(&ready)["checks"]["database"]["status"], "up");

    let res = app.call(Request::new(Method::GET, "/health")).await;
    assert_eq!(res.get_header("x-content-type-options"), Some("nosniff"));
    assert!(res.get_header("x-request-id").is_some());
    let metrics = app.call(Request::new(Method::GET, "/metrics")).await;
    assert!(metrics.body_text().contains("route=\"/health\""));
}
