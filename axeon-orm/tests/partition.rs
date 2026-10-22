//! Partitioned tables (when `AXEON_ORM_POSTGRES_URL` is set).

#![cfg(all(feature = "postgres", feature = "chrono", feature = "sqlite"))]

use axeon_orm::partition::Partitioning;
use axeon_orm::{Db, Error, Model};
use chrono::{DateTime, TimeZone, Utc};

#[derive(Model, Clone, Debug, PartialEq)]
struct AuditEvent {
    id: i64,
    #[model(index)]
    subject: String,
    action: String,
    occurred_at: DateTime<Utc>,
}

#[derive(Model, Clone, Debug)]
struct LlmCall {
    id: i64,
    model: String,
    occurred_at: DateTime<Utc>,
}

#[derive(Model)]
struct Keyed {
    id: i64,
    #[model(unique)]
    code: String,
    occurred_at: DateTime<Utc>,
}

fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
}

#[tokio::test]
async fn monthly_and_daily_partitions() {
    // Setup mistakes need no database.
    assert!(matches!(Partitioning::monthly(Keyed::OCCURRED_AT).create_table_sql::<Keyed>(), Err(Error::Config(m)) if m.contains("unique")));
    assert!(matches!(Partitioning::monthly(AuditEvent::SUBJECT).create_table_sql::<AuditEvent>(), Err(Error::Config(_))));
    let sqlite = Db::connect("sqlite::memory:").await.unwrap();
    assert!(Partitioning::monthly(AuditEvent::OCCURRED_AT).create_table::<AuditEvent>(&sqlite).await.is_err());

    let Ok(url) = std::env::var("AXEON_ORM_POSTGRES_URL") else {
        eprintln!("skipping: AXEON_ORM_POSTGRES_URL is not set");
        return;
    };
    let admin = Db::connect(&url).await.unwrap();
    admin.execute("DROP SCHEMA IF EXISTS axeon_orm_partition CASCADE; CREATE SCHEMA axeon_orm_partition").await.unwrap();
    admin.close().await;
    // A session time zone other than UTC: bounds must still be UTC.
    let separator = if url.contains('?') { '&' } else { '?' };
    let options = "options=-c%20search_path%3Daxeon_orm_partition%20-c%20TimeZone%3DAsia%2FShanghai";
    let db = Db::connect(&format!("{}{}{}", url, separator, options)).await.unwrap();

    let audit = Partitioning::monthly(AuditEvent::OCCURRED_AT);
    audit.create_table::<AuditEvent>(&db).await.unwrap();
    audit.create_table::<AuditEvent>(&db).await.unwrap(); // Safe to run again.
    let created = audit.ensure_at(&db, at(2026, 11, 15), 2).await.unwrap();
    assert_eq!(created, ["audit_events_p2026_11", "audit_events_p2026_12", "audit_events_p2027_01"]);
    assert!(audit.ensure_at(&db, at(2026, 11, 30), 2).await.unwrap().is_empty(), "nothing new");

    let parts = audit.partitions(&db).await.unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].from, Some(Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap()));
    assert_eq!(parts[0].to, Some(Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap()));
    assert_eq!(parts[2].to, Some(Utc.with_ymd_and_hms(2027, 2, 1, 0, 0, 0).unwrap()));

    // The model works as usual; rows land in their month's partition.
    let event = |subject: &str, when| AuditEvent { id: 0, subject: subject.into(), action: "approved".into(), occurred_at: when };
    let november = event("case-1", at(2026, 11, 20)).insert(&db).await.unwrap();
    let december = event("case-2", Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap()).insert(&db).await.unwrap();
    event("case-3", at(2027, 1, 31)).insert(&db).await.unwrap();
    assert!(december.id > november.id, "ids come from one sequence");
    assert_eq!(AuditEvent::find_or_fail(&db, november.id).await.unwrap(), november);
    let mut edited = december.clone();
    edited.action = "reopened".into();
    edited.update(&db).await.unwrap();
    assert_eq!(AuditEvent::query().filter(AuditEvent::ACTION.eq("reopened")).count(&db).await.unwrap(), 1);
    let pool = db.postgres().unwrap();
    let in_december: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events_p2026_12").fetch_one(pool).await.unwrap();
    assert_eq!(in_december, 1, "midnight UTC on the 1st is December, whatever the session time zone");
    // The index exists on every partition.
    let indexed: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_indexes WHERE schemaname = 'axeon_orm_partition' AND indexdef LIKE '%(subject)%'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(indexed, 4, "the parent's index and one per partition");
    // A time without a partition can't be stored.
    assert!(event("too-late", at(2027, 3, 1)).insert(&db).await.is_err());

    // Retention: whole partitions that end by the cutoff are dropped.
    let dropped = audit.drop_before(&db, Utc.with_ymd_and_hms(2026, 12, 15, 0, 0, 0).unwrap()).await.unwrap();
    assert_eq!(dropped, ["audit_events_p2026_11"]);
    assert_eq!(AuditEvent::query().count(&db).await.unwrap(), 2);
    assert_eq!(AuditEvent::find(&db, november.id).await.unwrap(), None);
    assert_eq!(audit.partitions(&db).await.unwrap().len(), 2);

    // Daily partitions.
    let calls = Partitioning::daily(LlmCall::OCCURRED_AT);
    calls.create_table::<LlmCall>(&db).await.unwrap();
    let created = calls.ensure_at(&db, at(2026, 12, 30), 3).await.unwrap();
    assert_eq!(created, ["llm_calls_p2026_12_30", "llm_calls_p2026_12_31", "llm_calls_p2027_01_01", "llm_calls_p2027_01_02"]);
    LlmCall { id: 0, model: "qwen".into(), occurred_at: at(2027, 1, 1) }.insert(&db).await.unwrap();
    assert_eq!(calls.drop_before(&db, Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()).await.unwrap().len(), 2);
    assert_eq!(LlmCall::query().count(&db).await.unwrap(), 1);
    db.close().await;
}
