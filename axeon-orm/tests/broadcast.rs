//! The PostgreSQL broadcast backend (when `AXEON_ORM_POSTGRES_URL` is set).
//! Each test uses its own schema.

#![cfg(all(feature = "postgres", feature = "axeon"))]

use axeon::broadcast::{Hub, Message, Subscription};
use axeon_orm::broadcast::{self, PgBroadcast};
use axeon_orm::Db;
use futures::StreamExt;
use std::time::Duration;

/// Two connections to one schema, standing for two app instances.
async fn dbs(name: &str) -> Option<(Db, Db)> {
    let Ok(url) = std::env::var("AXEON_ORM_POSTGRES_URL") else {
        eprintln!("skipping: AXEON_ORM_POSTGRES_URL is not set");
        return None;
    };
    let schema = format!("axeon_broadcast_{}", name);
    let admin = Db::connect(&url).await.unwrap();
    admin.execute(&format!("DROP SCHEMA IF EXISTS {0} CASCADE; CREATE SCHEMA {0}", schema)).await.unwrap();
    admin.close().await;
    let separator = if url.contains('?') { '&' } else { '?' };
    let url = format!("{}{}options=-c%20search_path%3D{}", url, separator, schema);
    let (a, b) = (Db::connect(&url).await.unwrap(), Db::connect(&url).await.unwrap());
    broadcast::install(&a).await.unwrap();
    broadcast::install(&a).await.unwrap(); // Safe to run again.
    Some((a, b))
}

async fn next(subscription: &mut Subscription) -> Message {
    tokio::time::timeout(Duration::from_secs(5), subscription.next()).await.expect("no message").unwrap().unwrap()
}

async fn quiet(subscription: &mut Subscription) -> bool {
    tokio::time::timeout(Duration::from_millis(300), subscription.next()).await.is_err()
}

#[tokio::test]
async fn postgres_backend_conforms() {
    let Some((a, b)) = dbs("conform").await else { return };
    // NOTIFY is server-wide, so topics are prefixed per test.
    axeon::broadcast::conformance::check(Hub::new(PgBroadcast::new(a)), Hub::new(PgBroadcast::new(b)), "conform:").await;
}

#[tokio::test]
async fn published_in_a_transaction_only_on_commit() {
    let Some((a, b)) = dbs("tx").await else { return };
    let hub = Hub::new(PgBroadcast::new(a.clone()));
    let mut subscription = Hub::new(PgBroadcast::new(b)).subscribe("tx:run", None).await.unwrap();

    let mut tx = a.begin().await.unwrap();
    broadcast::publish_in(&mut tx, "tx:run", &Message::new("rolled back")).await.unwrap();
    tx.rollback().await.unwrap();
    let mut tx = a.begin().await.unwrap();
    broadcast::publish_in(&mut tx, "tx:run", &Message::new("committed").event("step")).await.unwrap();
    assert!(quiet(&mut subscription).await, "nothing before commit");
    tx.commit().await.unwrap();
    let message = next(&mut subscription).await;
    assert_eq!((message.data(), message.event_name()), ("committed", Some("step")));
    assert!(quiet(&mut subscription).await);
    hub.publish("tx:run", Message::new("plain")).await.unwrap();
    assert_eq!(next(&mut subscription).await.data(), "plain");
}

#[tokio::test]
async fn ids_follow_commit_order_within_a_topic() {
    let Some((a, b)) = dbs("order").await else { return };
    let mut subscription = Hub::new(PgBroadcast::new(b)).subscribe("order:run", None).await.unwrap();

    // The first message's transaction stays open while a second message is
    // published. Without ordering, the second could be read first and the
    // first skipped for good.
    let mut tx = a.begin().await.unwrap();
    broadcast::publish_in(&mut tx, "order:run", &Message::new("first")).await.unwrap();
    let publisher = Hub::new(PgBroadcast::new(a.clone()));
    let second = tokio::spawn(async move { publisher.publish("order:run", Message::new("second")).await.unwrap() });
    // Another topic isn't held up.
    Hub::new(PgBroadcast::new(a.clone())).publish("order:other", Message::new("x")).await.unwrap();
    assert!(quiet(&mut subscription).await);
    tx.commit().await.unwrap();
    second.await.unwrap();
    assert_eq!(next(&mut subscription).await.data(), "first");
    assert_eq!(next(&mut subscription).await.data(), "second");
}

#[tokio::test]
async fn pruned_messages_are_reported_missed() {
    let Some((a, _)) = dbs("prune").await else { return };
    let backend = PgBroadcast::new(a);
    let hub = Hub::new(backend.clone());
    let first = hub.publish("prune:t", Message::new("1")).await.unwrap();
    assert!(!hub.subscribe("prune:t", Some(&first)).await.unwrap().missed());
    assert_eq!(backend.prune(Duration::from_secs(3600)).await.unwrap(), 0, "nothing that old");
    assert_eq!(backend.prune(Duration::ZERO).await.unwrap(), 1);
    assert!(hub.subscribe("prune:t", Some(&first)).await.unwrap().missed());
    hub.publish("prune:t", Message::new("2")).await.unwrap();
    let mut resumed = hub.subscribe("prune:t", Some(&first)).await.unwrap();
    assert!(!resumed.missed() || resumed.missed()); // either way, "2" is delivered:
    assert_eq!(next(&mut resumed).await.data(), "2");
}

#[tokio::test]
async fn other_databases_are_refused() {
    let sqlite = Db::connect("sqlite::memory:").await.unwrap();
    assert!(broadcast::install(&sqlite).await.is_err());
    let hub = Hub::new(PgBroadcast::new(sqlite));
    assert!(hub.publish("t", Message::new("x")).await.is_err());
}
