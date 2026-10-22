//! Read replicas. Each "replica" here is a separate database holding a
//! marker row, so tests can see where a read went.

#![cfg(all(feature = "sqlite", feature = "axeon"))]

use axeon_orm::select::{Agg, Select};
use axeon_orm::{Db, DbOptions, Error, Model};
use futures::TryStreamExt;

#[derive(Model, Clone, Debug, PartialEq)]
struct Marker {
    id: i64,
    source: String,
}

#[derive(Model, Clone, Debug)]
struct Owned {
    id: i64,
    #[model(tenant)]
    org_id: i64,
}

async fn database(source: &str) -> Db {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    db.create_table::<Marker>().await.unwrap();
    db.create_table::<Owned>().await.unwrap();
    Marker { id: 0, source: source.into() }.insert(&db).await.unwrap();
    db
}

async fn source(db: &Db) -> String {
    Marker::query().first(db).await.unwrap().unwrap().source
}

#[tokio::test]
async fn reads_rotate_over_replicas_and_writes_are_refused() {
    // Without replicas, replica() reads the primary (and still refuses writes).
    let alone = database("primary").await;
    assert_eq!(alone.replica_count(), 0);
    assert_eq!(source(&alone.replica()).await, "primary");
    assert!(alone.replica().is_replica() && !alone.is_replica());

    let metrics = axeon::metrics::Metrics::new();
    let primary = Db::connect_with("sqlite::memory:", DbOptions::new().metrics(&metrics)).await.unwrap();
    primary.create_table::<Marker>().await.unwrap();
    Marker { id: 0, source: "primary".into() }.insert(&primary).await.unwrap();
    let db = primary.with_replicas(vec![database("replica-1").await, database("replica-2").await]);
    assert_eq!(db.replica_count(), 2);

    // In turn; the primary itself is untouched.
    let mut order = Vec::new();
    for _ in 0..4 {
        order.push(source(&db.replica()).await);
    }
    assert_eq!(order, ["replica-1", "replica-2", "replica-1", "replica-2"]);
    assert_eq!(source(&db).await, "primary");

    // Every read path works through a handle.
    let replica = db.replica();
    assert_eq!(Marker::find(&replica, 1).await.unwrap().unwrap().source, "replica-1");
    assert_eq!(Marker::query().stream(&replica).try_collect::<Vec<_>>().await.unwrap().len(), 1);
    assert_eq!(Marker::query().cursor_paginate(&replica, None, 10).await.unwrap().items.len(), 1);
    let report = Select::from::<Marker>().column(Marker::SOURCE).aggregate(Agg::count(), "n").group_by(Marker::SOURCE).fetch_json(&replica).await.unwrap();
    assert_eq!(report, [serde_json::json!({ "source": "replica-1", "n": 1 })]);
    // Writes through a replica handle are refused before reaching it.
    fn refused<T>(r: Result<T, Error>) -> bool {
        matches!(r, Err(Error::Config(m)) if m.contains("primary"))
    }
    // (In-memory SQLite has one connection per database: finish with the
    // transaction before using that replica again.)
    let mut tx = replica.begin().await.unwrap();
    assert_eq!(source_tx(&mut tx).await, "replica-1");
    assert!(refused(Marker { id: 0, source: "x".into() }.insert(&mut tx).await), "in a replica transaction too");
    drop(tx);
    let marker = Marker::query().first(&replica).await.unwrap().unwrap();
    assert!(refused(Marker { id: 0, source: "x".into() }.insert(&replica).await));
    assert!(refused(Marker::insert_many(&replica, &[Marker { id: 0, source: "x".into() }]).await));
    assert!(refused(marker.clone().update(&replica).await));
    assert!(refused(marker.clone().delete(&replica).await));
    assert!(refused(Marker::query().update(&replica, [Marker::SOURCE.set("x")]).await));
    assert!(refused(Marker::query().delete(&replica).await));
    assert_eq!(Marker::query().count(&db.replica()).await.unwrap(), 1);
    // The primary still takes writes.
    Marker { id: 0, source: "new".into() }.insert(&db).await.unwrap();
    assert_eq!(Marker::query().count(&db).await.unwrap(), 2);

    // Handles keep the tenant scope (and refuse tenant models without one)...
    assert!(matches!(Owned::query().all(&db.replica()).await, Err(Error::TenantScope(_))));
    assert!(Owned::query().all(&db.for_tenant(1i64).replica()).await.is_ok());
    // ...and the primary's metrics.
    assert!(metrics.render().contains(r#"db_query_duration_seconds_count{operation="select",table="markers"}"#));
}

async fn source_tx(tx: &mut axeon_orm::Transaction) -> String {
    Marker::query().first(&mut *tx).await.unwrap().unwrap().source
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn replicas_must_match_the_primary() {
    let Ok(url) = std::env::var("AXEON_ORM_POSTGRES_URL") else { return };
    let postgres = Db::connect(&url).await.unwrap();
    let sqlite = Db::connect("sqlite::memory:").await.unwrap();
    let mixed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sqlite.with_replicas(vec![postgres.clone()])));
    assert!(mixed.is_err());
    postgres.close().await;
}
