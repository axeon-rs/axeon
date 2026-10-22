//! The PostgreSQL idempotency store (when `AXEON_ORM_POSTGRES_URL` is set).

#![cfg(all(feature = "axeon", feature = "postgres"))]

use axeon::middleware::idempotency::{conformance, Begin, IdempotencyStore};
use axeon_orm::idempotency::{self, PgIdempotencyStore};
use axeon_orm::Db;
use std::time::Duration;

#[tokio::test]
async fn postgres_store_conforms_and_prunes() {
    let Ok(url) = std::env::var("AXEON_ORM_POSTGRES_URL") else {
        eprintln!("skipping: AXEON_ORM_POSTGRES_URL is not set");
        return;
    };
    let admin = Db::connect(&url).await.unwrap();
    admin.execute("DROP SCHEMA IF EXISTS axeon_idempotency_test CASCADE; CREATE SCHEMA axeon_idempotency_test").await.unwrap();
    admin.close().await;
    let separator = if url.contains('?') { '&' } else { '?' };
    let db = Db::connect(&format!("{}{}options=-c%20search_path%3Daxeon_idempotency_test", url, separator)).await.unwrap();
    idempotency::install(&db).await.unwrap();
    idempotency::install(&db).await.unwrap(); // Safe to run again.

    let store = PgIdempotencyStore::new(db.clone());
    conformance::check(&store, "").await;

    // Expired records are pruned; live ones stay.
    assert_eq!(store.begin("old", "fp", Duration::from_millis(1)).await.unwrap(), Begin::Started);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let pruned = store.prune().await.unwrap();
    assert!(pruned >= 1, "{}", pruned);
    assert_eq!(store.begin("d", "fp1", Duration::from_secs(30)).await.unwrap(), Begin::InProgress);

    let sqlite = Db::connect("sqlite::memory:").await.unwrap();
    assert!(idempotency::install(&sqlite).await.is_err());
}
