//! The test helpers, on SQLite (always), PostgreSQL (when
//! `AXEON_ORM_POSTGRES_URL` is set) and MySQL (when `AXEON_ORM_MYSQL_URL` is).

#![cfg(all(feature = "testing", feature = "sqlite"))]

use axeon_orm::testing::{rollback, Factory, TestDb};
use axeon_orm::{Db, Model};

#[derive(Model, Clone, Debug, PartialEq)]
#[model(has_many(Post))]
struct Author {
    id: i64,
    name: String,
    #[model(unique)]
    email: String,
    active: bool,
}

#[derive(Model, Clone, Debug)]
struct Post {
    id: i64,
    #[model(belongs_to = Author)]
    author_id: i64,
    title: String,
}

impl Factory for Author {
    fn definition(n: u64) -> Self {
        Author { id: 0, name: format!("Author {}", n), email: format!("author{}@example.com", n), active: true }
    }
}

impl Factory for Post {
    fn definition(n: u64) -> Self {
        Post { id: 0, author_id: 0, title: format!("Post {}", n) }
    }
}

/// How many schemas (PostgreSQL) or databases (MySQL) have this name.
#[allow(unused_mut, unused_variables)]
async fn leftovers(url: &str, name: &str) -> i64 {
    let admin = Db::connect(url).await.unwrap();
    let mut n = -1;
    #[cfg(feature = "postgres")]
    if let Some(pool) = admin.postgres() {
        let sql = "SELECT count(*) FROM information_schema.schemata WHERE schema_name = $1";
        n = sqlx::query_scalar::<_, i64>(sql).bind(name).fetch_one(pool).await.unwrap();
    }
    #[cfg(feature = "mysql")]
    if let Some(pool) = admin.mysql() {
        let sql = "SELECT count(*) FROM information_schema.schemata WHERE schema_name = ?";
        n = sqlx::query_scalar::<_, i64>(sql).bind(name).fetch_one(pool).await.unwrap();
    }
    admin.close().await;
    n
}

async fn check(url: &str) {
    let a = TestDb::connect(url).await.unwrap();
    let b = TestDb::connect(url).await.unwrap();
    a.create_table::<Author>().await.unwrap();
    a.create_table::<Post>().await.unwrap();
    b.create_table::<Author>().await.unwrap();

    // Factories: defaults with unique sequence values, overrides, batches.
    let ada = Author::factory().with(|a| a.name = "Ada".into()).create(&*a).await.unwrap();
    assert!(ada.id > 0 && ada.name == "Ada" && ada.email.ends_with("@example.com"));
    let built = Author::factory().count(2).build_many();
    assert_ne!(built[0].email, built[1].email);
    let batch = Author::factory()
        .count(3)
        .with(|a| a.active = false)
        .each(|a, i| a.name = format!("Batch {}", i))
        .create_many(&*a)
        .await
        .unwrap();
    assert_eq!(batch.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["Batch 0", "Batch 1", "Batch 2"]);
    assert!(batch.iter().all(|a| !a.active && a.id > 0));
    let author_id = ada.id;
    let posts = Post::factory().count(2).with(move |p| p.author_id = author_id).create_many(&*a).await.unwrap();
    assert_eq!(ada.posts().count(&*a).await.unwrap(), 2);
    assert_eq!(Post::factory().count(0).create_many(&*a).await.unwrap().len(), 0);
    let _ = posts;

    // Isolation: b sees none of a's rows.
    assert_eq!(Author::query().count(&*a).await.unwrap(), 4);
    assert_eq!(Author::query().count(&*b).await.unwrap(), 0);

    // Rolled-back transactions leave nothing behind, even on success.
    let inside = rollback(&a, async |tx| {
        Author::factory().create(&mut *tx).await?;
        Author::query().count(&mut *tx).await
    })
    .await
    .unwrap();
    assert_eq!(inside, 5);
    assert_eq!(Author::query().count(&*a).await.unwrap(), 4);
    // An error inside is returned, and rolled back too.
    let failed = rollback(&a, async |tx| {
        Author::factory().create(&mut *tx).await?;
        Author::factory().with(|a| a.email = "author1@example.com".into()).create(&mut *tx).await?;
        Author::factory().with(|a| a.email = "author1@example.com".into()).create(&mut *tx).await
    })
    .await;
    assert!(matches!(failed, Err(axeon_orm::Error::UniqueViolation(_))), "{:?}", failed.map(|a| a.id));
    assert_eq!(Author::query().count(&*a).await.unwrap(), 4);

    // Schemas/databases are removed by cleanup, and by drop as a fallback.
    if !url.starts_with("sqlite") {
        let name_a = current_name(&a).await;
        let name_b = current_name(&b).await;
        assert_eq!(leftovers(url, &name_a).await, 1);
        a.cleanup().await.unwrap();
        assert_eq!(leftovers(url, &name_a).await, 0);
        drop(b);
        assert_eq!(leftovers(url, &name_b).await, 0);
    }
}

/// The schema (PostgreSQL) or database (MySQL) a TestDb uses.
#[allow(unused_mut, unused_variables)]
async fn current_name(db: &TestDb) -> String {
    let mut name = String::new();
    #[cfg(feature = "postgres")]
    if let Some(pool) = db.postgres() {
        name = sqlx::query_scalar("SELECT current_schema()").fetch_one(pool).await.unwrap();
    }
    #[cfg(feature = "mysql")]
    if let Some(pool) = db.mysql() {
        name = sqlx::query_scalar("SELECT DATABASE()").fetch_one(pool).await.unwrap();
    }
    name
}

#[tokio::test]
async fn sqlite() {
    check("sqlite::memory:").await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres() {
    match std::env::var("AXEON_ORM_POSTGRES_URL") {
        Ok(url) => check(&url).await,
        Err(_) => eprintln!("skipping: AXEON_ORM_POSTGRES_URL is not set"),
    }
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn mysql() {
    match std::env::var("AXEON_ORM_MYSQL_URL") {
        Ok(url) => check(&url).await,
        Err(_) => eprintln!("skipping: AXEON_ORM_MYSQL_URL is not set"),
    }
}

#[tokio::test]
async fn new_defaults_to_in_memory_sqlite() {
    // Only when the variable is unset; with it set, `new` uses that server.
    if std::env::var(axeon_orm::testing::DATABASE_URL_VAR).is_err() {
        let db = TestDb::new().await.unwrap();
        assert_eq!(db.backend(), axeon_orm::Backend::Sqlite);
        db.cleanup().await.unwrap();
    }
}
