//! Query metrics, the slow-query log and statement timeouts.

#![cfg(all(feature = "sqlite", feature = "axeon"))]

use axeon::metrics::Metrics;
use axeon_orm::{Db, DbOptions, Error, Model};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

#[derive(Model, Clone, Debug)]
struct Account {
    id: i64,
    #[model(unique)]
    email: String,
}

#[tokio::test]
async fn statements_are_measured() {
    let metrics = Metrics::new();
    let db = Db::connect_with("sqlite::memory:", DbOptions::new().metrics(&metrics)).await.unwrap();
    db.create_table::<Account>().await.unwrap();
    Account { id: 0, email: "a@example.com".into() }.insert(&db).await.unwrap();
    let _ = Account { id: 0, email: "a@example.com".into() }.insert(&db).await; // unique violation
    Account::query().all(&db).await.unwrap();
    Account::query().count(&db).await.unwrap();
    // Inside transactions too.
    let mut tx = db.begin().await.unwrap();
    Account::query().all(&mut tx).await.unwrap();
    tx.commit().await.unwrap();

    let text = metrics.render();
    let line = |prefix: &str| text.lines().find(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no {} in\n{}", prefix, text)).to_string();
    let count = |prefix: &str| line(prefix).rsplit(' ').next().unwrap().parse::<f64>().unwrap();
    assert_eq!(count(r#"db_query_duration_seconds_count{operation="select",table="accounts"}"#), 3.0);
    assert_eq!(count(r#"db_query_duration_seconds_count{operation="insert",table="accounts"}"#), 2.0);
    assert_eq!(count(r#"db_query_errors_total{operation="insert",table="accounts"}"#), 1.0);
}

/// Slow-query log events: (message fields as text).
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> Layer<S> for Captured {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != "axeon_orm::slow_query" {
            return;
        }
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={:?} ", field.name(), value));
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

#[tokio::test]
async fn slow_queries_are_logged_without_values() {
    let captured = Captured::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));

    let quiet = Db::connect_with("sqlite::memory:", DbOptions::new().slow_query(Duration::from_secs(60))).await.unwrap();
    quiet.create_table::<Account>().await.unwrap();
    Account::query().all(&quiet).await.unwrap();
    assert!(captured.0.lock().unwrap().is_empty(), "nothing is that slow");

    let db = Db::connect_with("sqlite::memory:", DbOptions::new().slow_query(Duration::ZERO)).await.unwrap();
    db.create_table::<Account>().await.unwrap();
    Account::query().filter(Account::EMAIL.eq("secret@example.com")).all(&db).await.unwrap();
    let logged = captured.0.lock().unwrap().clone();
    let select = logged.iter().find(|l| l.contains("operation=\"select\"")).unwrap_or_else(|| panic!("{:?}", logged));
    assert!(select.contains("table=\"accounts\"") && select.contains("elapsed_ms=") && select.contains("fingerprint="));
    assert!(select.contains(r#"WHERE \"email\" = ?"#), "{}", select);
    assert!(!logged.iter().any(|l| l.contains("secret@example.com")), "values never reach the log");
}

#[tokio::test]
async fn statement_timeouts() {
    // SQLite has none; per-transaction timeouts say so.
    let sqlite = Db::connect_with("sqlite::memory:", DbOptions::new().statement_timeout(Duration::from_millis(5))).await.unwrap();
    let mut tx = sqlite.begin().await.unwrap();
    assert!(matches!(tx.statement_timeout(Duration::from_secs(1)).await, Err(Error::Config(_))));
    drop(tx);

    #[cfg(feature = "postgres")]
    if let Ok(url) = std::env::var("AXEON_ORM_POSTGRES_URL") {
        // Pool-wide.
        let db = Db::connect_with(&url, DbOptions::new().statement_timeout(Duration::from_millis(100))).await.unwrap();
        let slow = db.execute("SELECT pg_sleep(2)").await;
        assert!(matches!(slow, Err(Error::StatementTimeout(_))), "{:?}", slow);
        db.execute("SELECT 1").await.unwrap(); // the connection is fine afterwards
        let error: axeon::ServerError = Error::StatementTimeout("x".into()).into();
        assert_eq!(error.status_code(), 503);
        db.close().await;

        // Per transaction, on a pool without one.
        let db = Db::connect(&url).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        tx.statement_timeout(Duration::from_millis(50)).await.unwrap();
        assert!(matches!(tx.execute("SELECT pg_sleep(2)").await, Err(Error::StatementTimeout(_))));
        assert!(tx.commit().await.is_err(), "the cancelled statement failed the transaction");
        // ...and only in that transaction.
        let mut tx = db.begin().await.unwrap();
        tx.execute("SELECT pg_sleep(0.2)").await.unwrap();
        tx.commit().await.unwrap();
        db.close().await;
    }

    #[cfg(feature = "mysql")]
    if let Ok(url) = std::env::var("AXEON_ORM_MYSQL_URL") {
        // MySQL's SLEEP() returns early instead of failing, so check the setting.
        let db = Db::connect_with(&url, DbOptions::new().statement_timeout(Duration::from_millis(1500))).await.unwrap();
        let ms: u64 = sqlx::query_scalar("SELECT @@SESSION.max_execution_time").fetch_one(db.mysql().unwrap()).await.unwrap();
        assert_eq!(ms, 1500);
        db.close().await;
    }
}
