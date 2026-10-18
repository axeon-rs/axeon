//! Server-database behaviour, against PostgreSQL and MySQL: concurrency,
//! locking, strict typing and large eager loads. Each backend runs when its
//! URL is set, e.g. with `docker compose -f axeon-orm/docker-compose.yml up -d`:
//!
//! - `AXEON_ORM_POSTGRES_URL=postgres://postgres:postgres@localhost:55432/postgres`
//! - `AXEON_ORM_MYSQL_URL=mysql://root:mysql@localhost:53306/axeon`

#![cfg(all(any(feature = "postgres", feature = "mysql"), feature = "chrono", feature = "json"))]

use axeon_orm::migrate::{Migration, Migrator};
use axeon_orm::sqlx::types::Json;
use axeon_orm::{Backend, Db, DbOptions, Error, Model};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

/// One test's database: a fresh PostgreSQL schema or MySQL database.
struct Target {
    backend: Backend,
    url: String,
    scope: String,
}

impl Target {
    /// `None` (skip) when the backend's URL isn't set.
    async fn new(backend: Backend, name: &str) -> Option<Target> {
        let (var, prefix) = match backend {
            Backend::Postgres => ("AXEON_ORM_POSTGRES_URL", "axeon_orm_pg_"),
            _ => ("AXEON_ORM_MYSQL_URL", "axeon_orm_my_"),
        };
        let Ok(url) = std::env::var(var) else {
            eprintln!("skipping: {} is not set", var);
            return None;
        };
        let scope = format!("{}{}", prefix, name);
        let admin = Db::connect(&url).await.unwrap();
        let reset = match backend {
            Backend::Postgres => format!("DROP SCHEMA IF EXISTS {0} CASCADE; CREATE SCHEMA {0}", scope),
            _ => format!("DROP DATABASE IF EXISTS {0}; CREATE DATABASE {0}", scope),
        };
        admin.execute(&reset).await.unwrap();
        admin.close().await;
        Some(Target { backend, url, scope })
    }

    /// A new pool on this test's schema or database.
    async fn connect(&self, options: DbOptions) -> Db {
        let url = match self.backend {
            Backend::Postgres => {
                let separator = if self.url.contains('?') { '&' } else { '?' };
                format!("{}{}options=-c%20search_path%3D{}", self.url, separator, self.scope)
            }
            _ => format!("{}/{}", self.url.rsplit_once('/').unwrap().0, self.scope),
        };
        let db = Db::connect_with(&url, options).await.unwrap();
        assert_eq!(db.backend(), self.backend);
        db
    }

    async fn db(&self) -> Db {
        self.connect(DbOptions::new().max_connections(20)).await
    }

    fn mysql(&self) -> bool {
        self.backend == Backend::MySql
    }
}

#[derive(Model, Clone, Debug, PartialEq)]
#[model(has_many(Item))]
struct Owner {
    id: i64,
    name: String,
}

#[derive(Model, Clone, Debug)]
struct Item {
    id: i64,
    #[model(belongs_to = Owner, on_delete = "cascade")]
    owner_id: i64,
    #[model(unique)]
    code: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Settings {
    theme: String,
    beta: bool,
}

#[derive(Model, Clone, Debug, PartialEq)]
struct Sample {
    id: i32,
    flag: Option<bool>,
    small: Option<i32>,
    big: Option<i64>,
    ratio: Option<f64>,
    label: Option<String>,
    blob: Option<Vec<u8>>,
    at: Option<DateTime<Utc>>,
    naive: Option<NaiveDateTime>,
    day: Option<NaiveDate>,
    doc: Option<serde_json::Value>,
    settings: Option<Json<Settings>>,
}

async fn every_type_and_typed_nulls_round_trip(t: &Target) {
    let db = t.db().await;
    db.create_table::<Sample>().await.unwrap();

    // All NULL: each parameter must carry its column's type for PostgreSQL.
    let empty = Sample {
        id: 0,
        flag: None,
        small: None,
        big: None,
        ratio: None,
        label: None,
        blob: None,
        at: None,
        naive: None,
        day: None,
        doc: None,
        settings: None,
    };
    let stored = empty.clone().insert(&db).await.unwrap();
    assert_eq!(Sample { id: stored.id, ..empty }, stored);

    // All set. PostgreSQL keeps microseconds, so drop the nanoseconds first.
    let at = DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap();
    let full = Sample {
        id: 0,
        flag: Some(true),
        small: Some(i32::MIN),
        big: Some(i64::MAX),
        ratio: Some(-0.125),
        label: Some("héllo 'quoted' \"double\" ; DROP TABLE samples; --".into()),
        blob: Some(vec![0, 1, 254, 255]),
        at: Some(at),
        naive: Some(at.naive_utc()),
        day: NaiveDate::from_ymd_opt(2026, 10, 17),
        doc: Some(serde_json::json!({"nested": {"list": [1, 2, 3]}, "unicode": "✓"})),
        settings: Some(Json(Settings { theme: "dark".into(), beta: true })),
    };
    let stored = full.clone().insert(&db).await.unwrap();
    assert_eq!(Sample { id: stored.id, ..full.clone() }, stored);
    assert_eq!(Sample::find_or_fail(&db, stored.id).await.unwrap(), stored);

    // Filtering and setting NULL on typed columns.
    assert_eq!(Sample::query().filter(Sample::LABEL.is_null()).count(&db).await.unwrap(), 1);
    assert_eq!(Sample::query().filter(Sample::BIG.eq(i64::MAX)).count(&db).await.unwrap(), 1);
    assert_eq!(Sample::query().filter(Sample::DAY.lt(NaiveDate::from_ymd_opt(2027, 1, 1).unwrap())).count(&db).await.unwrap(), 1);
    let cleared = Sample::query()
        .filter(Sample::ID.eq(stored.id))
        .update(&db, [Sample::AT.set_null(), Sample::DOC.set_null(), Sample::SETTINGS.set_null(), Sample::BLOB.set_null()])
        .await
        .unwrap();
    assert_eq!(cleared, 1);
    let reloaded = Sample::find_or_fail(&db, stored.id).await.unwrap();
    assert_eq!((reloaded.at, reloaded.doc, reloaded.settings, reloaded.blob), (None, None, None, None));
}

#[cfg(feature = "uuid")]
async fn uuid_primary_keys(t: &Target) {
    #[derive(Model, Clone, Debug, PartialEq)]
    struct Token {
        id: uuid::Uuid,
        owner: Option<uuid::Uuid>,
    }
    let db = t.db().await;
    db.create_table::<Token>().await.unwrap();
    let id = uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    let token = Token { id, owner: None }.insert(&db).await.unwrap();
    assert_eq!(token.id, id, "non-integer keys are inserted, not generated");
    assert_eq!(Token::find(&db, id).await.unwrap(), Some(token.clone()));
    assert!(matches!(token.insert(&db).await, Err(Error::UniqueViolation(_))));
}

async fn concurrent_writes_through_the_pool(t: &Target) {
    let db = t.db().await;
    db.create_table::<Owner>().await.unwrap();
    db.create_table::<Item>().await.unwrap();
    let owner = Owner { id: 0, name: "pool".into() }.insert(&db).await.unwrap();

    let tasks: Vec<_> = (0..200)
        .map(|i| {
            let db = db.clone();
            tokio::spawn(async move {
                Item { id: 0, owner_id: owner.id, code: format!("item-{}", i) }.insert(&db).await.unwrap().id
            })
        })
        .collect();
    let mut ids = Vec::new();
    for task in tasks {
        ids.push(task.await.unwrap());
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 200, "every insert got its own id");
    assert_eq!(Item::query().count(&db).await.unwrap(), 200);

    // Racing on the same unique value: exactly one wins.
    let racers: Vec<_> = (0..20)
        .map(|_| {
            let db = db.clone();
            tokio::spawn(async move { Item { id: 0, owner_id: owner.id, code: "same".into() }.insert(&db).await })
        })
        .collect();
    let mut won = 0;
    for racer in racers {
        match racer.await.unwrap() {
            Ok(_) => won += 1,
            Err(Error::UniqueViolation(_)) => {}
            Err(other) => panic!("unexpected error: {}", other),
        }
    }
    assert_eq!(won, 1);
}

async fn concurrent_migrations_apply_once(t: &Target) {
    let db = t.db().await;
    let sleep = if t.mysql() { "SELECT SLEEP(0.3);" } else { "SELECT pg_sleep(0.3);" };
    let migrator = Migrator::new(vec![
        Migration::new(1, "slow", format!("CREATE TABLE ledger (id BIGINT PRIMARY KEY); {}", sleep)),
        Migration::new(2, "seed", "INSERT INTO ledger (id) VALUES (1);"),
    ]);

    // Eight "servers" start at once, each with its own pool.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let migrator = migrator.clone();
        let other = t.connect(DbOptions::new().max_connections(2)).await;
        handles.push(tokio::spawn(async move { migrator.run(&other).await }));
    }
    let mut applied = Vec::new();
    for handle in handles {
        applied.extend(handle.await.unwrap().expect("no instance fails"));
    }
    applied.sort();
    assert_eq!(applied, [1, 2], "each migration ran exactly once");
    db.execute("SELECT 1 FROM ledger").await.unwrap();
    assert!(migrator.pending(&db).await.unwrap().is_empty());
}

async fn eager_loading_beyond_the_parameter_chunk(t: &Target) {
    let db = t.db().await;
    db.create_table::<Owner>().await.unwrap();
    db.create_table::<Item>().await.unwrap();
    // 12,000 owners (more than two 5,000-key chunks), one item per third.
    let seed = if t.mysql() {
        "SET SESSION cte_max_recursion_depth = 20000;
         INSERT INTO owners (name) WITH RECURSIVE seq (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 12000)
             SELECT CONCAT('owner ', n) FROM seq;
         INSERT INTO items (owner_id, code) SELECT id, CONCAT('code ', id) FROM owners WHERE id % 3 = 0;"
    } else {
        "INSERT INTO owners (name) SELECT 'owner ' || n FROM generate_series(1, 12000) AS n;
         INSERT INTO items (owner_id, code) SELECT id, 'code ' || id FROM owners WHERE id % 3 = 0;"
    };
    db.execute(seed).await.unwrap();

    let owners = Owner::query().order_by(Owner::ID.asc()).with(Owner::ITEMS).all(&db).await.unwrap();
    assert_eq!(owners.len(), 12_000);
    assert!(owners.iter().all(|(owner, items)| items.len() == usize::from(owner.id % 3 == 0)));

    let items = Item::query().with(Item::OWNER).all(&db).await.unwrap();
    assert_eq!(items.len(), 4_000);
    assert!(items.iter().all(|(item, owner)| owner.as_ref().map(|o| o.id) == Some(item.owner_id)));

    let ids: Vec<i64> = (1..=12_000).collect();
    assert_eq!(Owner::query().filter(Owner::ID.is_in(ids)).count(&db).await.unwrap(), 12_000);
}

async fn errors_inside_transactions(t: &Target) {
    let db = t.db().await;
    db.create_table::<Owner>().await.unwrap();
    db.create_table::<Item>().await.unwrap();
    let owner = Owner { id: 0, name: "tx".into() }.insert(&db).await.unwrap();
    Item { id: 0, owner_id: owner.id, code: "taken".into() }.insert(&db).await.unwrap();

    // After an error PostgreSQL rejects further statements in the
    // transaction (MySQL carries on); rolling back must still work and
    // leave nothing behind.
    let mut tx = db.begin().await.unwrap();
    Item { id: 0, owner_id: owner.id, code: "new".into() }.insert(&mut tx).await.unwrap();
    let duplicate = Item { id: 0, owner_id: owner.id, code: "taken".into() }.insert(&mut tx).await;
    assert!(matches!(duplicate, Err(Error::UniqueViolation(_))));
    let after = Owner::all(&mut tx).await;
    if t.mysql() {
        assert!(after.is_ok());
    } else {
        assert!(matches!(after, Err(Error::Database(_))), "transaction is aborted");
    }
    tx.rollback().await.unwrap();
    assert_eq!(Item::query().count(&db).await.unwrap(), 1);

    // Committing after a failed statement reports the rollback instead of
    // succeeding silently, and keeps nothing, even the writes before the
    // error (the same on both databases).
    let mut tx = db.begin().await.unwrap();
    Item { id: 0, owner_id: owner.id, code: "before".into() }.insert(&mut tx).await.unwrap();
    let orphan = Item { id: 0, owner_id: 9_999, code: "orphan".into() }.insert(&mut tx).await;
    assert!(matches!(orphan, Err(Error::ForeignKeyViolation(_))));
    assert!(matches!(tx.commit().await, Err(Error::Transaction(_))));
    assert_eq!(Item::query().count(&db).await.unwrap(), 1);

    // The pool is healthy afterwards.
    db.ping().await.unwrap();
}

async fn bad_credentials_are_an_error_not_a_panic(t: &Target) {
    let (scheme, rest) = t.url.split_once("://").unwrap();
    let (user, host) = rest.split_once('@').unwrap();
    let user = user.split(':').next().unwrap();
    let wrong = format!("{}://{}:wrong-password@{}", scheme, user, host);
    let options = DbOptions::new().acquire_timeout(std::time::Duration::from_secs(5));
    match Db::connect_with(&wrong, options).await {
        Err(Error::Database(_)) => {}
        Err(other) => panic!("unexpected error: {}", other),
        Ok(_) => eprintln!("server accepts any password (trust auth); nothing to check"),
    }
}

#[cfg(feature = "axeon")]
async fn axeon_requests(t: &Target) {
    use axeon::{Method, Request, Server, ServerError};
    use axeon_orm::server::{DbRequest, Transactional};

    let db = t.db().await;
    db.create_table::<Owner>().await.unwrap();
    db.create_table::<Item>().await.unwrap();
    let owner = Owner { id: 0, name: "web".into() }.insert(&db).await.unwrap();

    let mut app = Server::new();
    app.state(db.clone());
    app.middleware(Transactional::new(db.clone()));
    app.post("/items/:code", move |req| async move {
        let mut tx = req.tx()?;
        let code = req.params()["code"].clone();
        Item { id: 0, owner_id: owner.id, code: code.clone() }.insert(&mut *tx).await?;
        if code.starts_with("bad") {
            return Err(ServerError::BadRequest("rejected after writing".into()));
        }
        Ok::<_, ServerError>("created")
    });
    app.post("/swallow", move |req| async move {
        let mut tx = req.tx()?;
        let _ignored = Item { id: 0, owner_id: owner.id, code: "a".into() }.insert(&mut *tx).await;
        Ok::<_, ServerError>("looks fine")
    });
    app.get("/owners/:id", |req| async move {
        let owner = Owner::find_or_fail(req.db()?, req.param::<i64>("id")?).await?;
        Ok::<_, ServerError>(owner.name)
    });
    let app = app.into_app().await.unwrap();
    let call = |method, path: &str| app.call(Request::new(method, path));

    assert_eq!(call(Method::POST, "/items/a").await.status, 200);
    assert_eq!(call(Method::POST, "/items/bad-1").await.status, 400);
    assert_eq!(call(Method::POST, "/items/a").await.status, 409);
    // A handler that swallows a database error and still succeeds gets a
    // 500, because the transaction can't be committed.
    assert_eq!(call(Method::POST, "/swallow").await.status, 500);

    // Many requests at once, each in its own transaction.
    let responses = futures::future::join_all((0..50).map(|i| call(Method::POST, &format!("/items/c{}", i)))).await;
    assert!(responses.iter().all(|r| r.status == 200));

    let codes: Vec<String> = Item::all(&db).await.unwrap().into_iter().map(|i| i.code).collect();
    assert_eq!(codes.len(), 51, "rolled-back and duplicate requests left nothing: {:?}", codes);
    assert!(!codes.iter().any(|c| c.starts_with("bad")));
    assert_eq!(call(Method::GET, &format!("/owners/{}", owner.id)).await.body_text(), "web");
    assert_eq!(call(Method::GET, "/owners/0").await.status, 404);
}

macro_rules! server_tests {
    ($($(#[$cfg:meta])* $name:ident),* $(,)?) => {
        #[cfg(feature = "postgres")]
        mod postgres {
            $($(#[$cfg])*
            #[tokio::test]
            async fn $name() {
                if let Some(target) = super::Target::new(axeon_orm::Backend::Postgres, stringify!($name)).await {
                    super::$name(&target).await;
                }
            })*
        }
        #[cfg(feature = "mysql")]
        mod mysql {
            $($(#[$cfg])*
            #[tokio::test]
            async fn $name() {
                if let Some(target) = super::Target::new(axeon_orm::Backend::MySql, stringify!($name)).await {
                    super::$name(&target).await;
                }
            })*
        }
    };
}

server_tests!(
    every_type_and_typed_nulls_round_trip,
    #[cfg(feature = "uuid")]
    uuid_primary_keys,
    concurrent_writes_through_the_pool,
    concurrent_migrations_apply_once,
    eager_loading_beyond_the_parameter_chunk,
    errors_inside_transactions,
    bad_credentials_are_an_error_not_a_panic,
    #[cfg(feature = "axeon")]
    axeon_requests,
);
