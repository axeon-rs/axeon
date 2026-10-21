//! LISTEN / NOTIFY against PostgreSQL (when `AXEON_ORM_POSTGRES_URL` is
//! set). Channels are shared by the whole server, so each test uses its own.

#![cfg(feature = "postgres")]

use axeon_orm::notify::Notification;
use axeon_orm::{Db, Error};
use futures::StreamExt;
use std::time::Duration;

async fn postgres() -> Option<Db> {
    let Ok(url) = std::env::var("AXEON_ORM_POSTGRES_URL") else {
        eprintln!("skipping: AXEON_ORM_POSTGRES_URL is not set");
        return None;
    };
    Some(Db::connect(&url).await.unwrap())
}

/// The next message, failing the test after a second.
async fn next(listener: &mut axeon_orm::notify::Listener) -> Notification {
    tokio::time::timeout(Duration::from_secs(1), listener.recv()).await.expect("no notification").unwrap()
}

/// Whether nothing arrives for a moment.
async fn silent(listener: &mut axeon_orm::notify::Listener) -> bool {
    tokio::time::timeout(Duration::from_millis(200), listener.recv()).await.is_err()
}

#[tokio::test]
async fn notify_reaches_listeners() {
    let Some(db) = postgres().await else { return };
    let mut listener = db.listen(["t1_orders", "T1 Mixed \"Case\""]).await.unwrap();

    db.notify("t1_orders", "42").await.unwrap();
    let message = next(&mut listener).await;
    assert_eq!((message.channel.as_str(), message.payload.as_str()), ("t1_orders", "42"));
    assert!(message.process_id > 0);

    // Any channel name and payload is passed through as-is.
    db.notify("T1 Mixed \"Case\"", "it's \u{1F680}; DROP TABLE x").await.unwrap();
    let message = next(&mut listener).await;
    assert_eq!((message.channel.as_str(), message.payload.as_str()), ("T1 Mixed \"Case\"", "it's \u{1F680}; DROP TABLE x"));
    db.notify("t1_orders", "").await.unwrap();
    assert_eq!(next(&mut listener).await.payload, "");

    // Names are case-sensitive, and other channels aren't heard.
    db.notify("T1_ORDERS", "loud").await.unwrap();
    db.notify("t1_other", "x").await.unwrap();
    assert!(silent(&mut listener).await);

    listener.listen("t1_other").await.unwrap();
    listener.unlisten("t1_orders").await.unwrap();
    db.notify("t1_orders", "gone").await.unwrap();
    db.notify("t1_other", "here").await.unwrap();
    assert_eq!(next(&mut listener).await.payload, "here");
    assert!(silent(&mut listener).await);
}

#[tokio::test]
async fn transactions_notify_on_commit_only() {
    let Some(db) = postgres().await else { return };
    let mut listener = db.listen(["t2_jobs"]).await.unwrap();

    let mut tx = db.begin().await.unwrap();
    tx.notify("t2_jobs", "rolled back").await.unwrap();
    tx.rollback().await.unwrap();
    assert!(silent(&mut listener).await);

    let mut tx = db.begin().await.unwrap();
    tx.notify("t2_jobs", "a").await.unwrap();
    tx.notify("t2_jobs", "b").await.unwrap();
    assert!(silent(&mut listener).await, "held until commit");
    tx.commit().await.unwrap();
    assert_eq!(next(&mut listener).await.payload, "a");
    assert_eq!(next(&mut listener).await.payload, "b");

    let too_big = "x".repeat(8000);
    assert!(matches!(db.notify("t2_jobs", &too_big).await, Err(Error::Database(_))));
}

#[tokio::test]
async fn streams_messages() {
    let Some(db) = postgres().await else { return };
    let stream = db.listen(["t3_events"]).await.unwrap().into_stream();
    for i in 0..3 {
        db.notify("t3_events", &i.to_string()).await.unwrap();
    }
    let payloads: Vec<String> = tokio::time::timeout(Duration::from_secs(1), stream.take(3).map(|n| n.unwrap().payload).collect())
        .await
        .unwrap();
    assert_eq!(payloads, ["0", "1", "2"]);
}

#[tokio::test]
async fn try_recv_reports_a_lost_connection() {
    let Some(db) = postgres().await else { return };
    let mut listener = db.listen(["t4_wake"]).await.unwrap();
    db.notify("t4_wake", "before").await.unwrap();
    assert_eq!(listener.try_recv().await.unwrap().unwrap().payload, "before");

    // Kill the listener's connection from another one.
    let killed: Option<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE query LIKE 'LISTEN \"t4_wake\"%' AND pid <> pg_backend_pid()",
    )
    .fetch_optional(db.postgres().unwrap())
    .await
    .unwrap();
    assert_eq!(killed, Some(true));

    let lost = tokio::time::timeout(Duration::from_secs(5), listener.try_recv()).await.expect("no reconnect");
    assert!(matches!(lost, Ok(None)), "{:?}", lost);
    // Reconnected and listening again.
    db.notify("t4_wake", "after").await.unwrap();
    assert_eq!(next(&mut listener).await.payload, "after");
}

#[tokio::test]
async fn other_databases_are_refused() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    assert!(matches!(db.listen(["x"]).await, Err(Error::Config(_))));
    assert!(matches!(db.notify("x", "y").await, Err(Error::Config(_))));
    let mut tx = db.begin().await.unwrap();
    assert!(matches!(tx.notify("x", "y").await, Err(Error::Config(_))));
}
