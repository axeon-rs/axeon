#![cfg(all(feature = "axeon", feature = "sqlite"))]

use axeon::health::Health;
use axeon::{Json, Method, Request, Server, ServerError};
use axeon_orm::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Model, Clone, Debug, Serialize, Deserialize)]
struct Note {
    id: i64,
    #[model(unique)]
    title: String,
}

async fn db() -> Db {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    db.create_table::<Note>().await.unwrap();
    db
}

fn post(path: &str, title: &str) -> Request {
    Request::new(Method::POST, path).with_json(&serde_json::json!({ "id": 0, "title": title }))
}

#[tokio::test]
async fn handlers_use_the_db_and_map_errors() {
    let db = db().await;
    let mut app = Server::new();
    app.state(db.clone());
    app.get("/notes/:id", |req| async move {
        let note = Note::find_or_fail(req.db()?, req.param::<i64>("id")?).await?;
        Ok::<_, ServerError>(Json(note))
    });
    app.post("/notes", |req| async move {
        let note = req.json::<Note>()?.insert(req.db()?).await?;
        Ok::<_, ServerError>((201, Json(note)))
    });
    app.mount("/", Health::new().database(&db).router());
    let app = app.into_app().await.unwrap();

    let created = app.call(post("/notes", "first")).await;
    assert_eq!(created.status, 201, "{}", created.body_text());
    let note: Note = serde_json::from_str(&created.body_text()).unwrap();
    assert!(note.id > 0);

    let found = app.call(Request::new(Method::GET, &format!("/notes/{}", note.id))).await;
    assert_eq!((found.status, found.body_text().contains("first")), (200, true));
    assert_eq!(app.call(Request::new(Method::GET, "/notes/999")).await.status, 404);

    let duplicate = app.call(post("/notes", "first")).await;
    assert_eq!(duplicate.status, 409);
    assert!(!duplicate.body_text().contains("UNIQUE"), "database details stay private");

    assert_eq!(app.call(Request::new(Method::GET, "/ready")).await.status, 200);
    db.close().await;
    assert_eq!(app.call(Request::new(Method::GET, "/ready")).await.status, 503);
}

#[tokio::test]
async fn transactional_commits_on_success_and_rolls_back_on_error() {
    let db = db().await;
    let mut app = Server::new();
    app.middleware(Transactional::new(db.clone()));
    app.post("/ok", |req| async move {
        let mut tx = req.tx()?;
        req.json::<Note>()?.insert(&mut *tx).await?;
        Ok::<_, ServerError>("saved")
    });
    app.post("/fail", |req| async move {
        let mut tx = req.tx()?;
        let note = req.json::<Note>()?.insert(&mut *tx).await?;
        assert!(Note::find(&mut *tx, note.id).await?.is_some());
        Err::<&str, _>(ServerError::BadRequest("changed my mind".into()))
    });
    app.post("/twice", |req| async move {
        let _first = req.tx()?;
        let second = req.tx().err().map(|e| e.to_string());
        Ok::<_, ServerError>(second.unwrap_or_default())
    });
    let app = app.into_app().await.unwrap();

    assert_eq!(app.call(post("/ok", "kept")).await.status, 200);
    assert_eq!(app.call(post("/fail", "dropped")).await.status, 400);
    let titles: Vec<String> = Note::all(&db).await.unwrap().into_iter().map(|n| n.title).collect();
    assert_eq!(titles, ["kept"]);

    // A unique violation inside the transaction is a 409, and rolls back.
    assert_eq!(app.call(post("/ok", "kept")).await.status, 409);
    assert!(app.call(post("/twice", "x")).await.body_text().contains("in use"));
}

#[tokio::test]
async fn helpers_explain_missing_setup() {
    let mut app = Server::new();
    app.get("/db", |req| async move { req.db().map(|_| "ok") });
    app.get("/tx", |req| async move { req.tx().map(|_| "ok") });
    let app = app.into_app().await.unwrap();
    assert_eq!(app.call(Request::new(Method::GET, "/db")).await.status, 500);
    assert_eq!(app.call(Request::new(Method::GET, "/tx")).await.status, 500);
}
