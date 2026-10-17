//! The same tests against SQLite (always) and PostgreSQL (when
//! `AXEON_ORM_POSTGRES_URL` is set, e.g.
//! `postgres://postgres@localhost/postgres`). Each PostgreSQL test runs in
//! its own schema, so they can run in parallel.

#![cfg(all(feature = "chrono", feature = "json"))]

use axeon_orm::migrate::{Migration, Migrator};
use axeon_orm::{schema, Backend, Db, Error, Expr, Model};
use chrono::{DateTime, Utc};
use serde_json::json;

#[derive(Model, Clone, Debug, PartialEq)]
#[model(has_many(Post), has_one(Profile))]
struct User {
    id: i64,
    name: String,
    #[model(unique)]
    email: String,
    age: i32,
    bio: Option<String>,
    active: bool,
    #[model(created_at)]
    created_at: DateTime<Utc>,
    #[model(updated_at)]
    updated_at: DateTime<Utc>,
    #[model(skip)]
    cached: Option<String>,
}

#[derive(Model, Clone, Debug)]
struct Post {
    id: i64,
    #[model(belongs_to = User, on_delete = "cascade")]
    user_id: i64,
    title: String,
    score: f64,
    #[model(index)]
    slug: String,
    meta: Option<serde_json::Value>,
}

#[derive(Model, Clone, Debug)]
struct Profile {
    id: i32,
    #[model(belongs_to = User, on_delete = "set null")]
    user_id: Option<i64>,
    website: String,
}

#[derive(Model, Clone, Debug, PartialEq)]
#[model(table = "tag_list")]
struct Tag {
    #[model(primary_key)]
    slug: String,
    #[model(column = "label_text")]
    label: String,
    #[model(default = "0")]
    uses: i64,
}

fn user(name: &str, age: i32) -> User {
    User {
        id: 0,
        name: name.to_string(),
        email: format!("{}@example.com", name.to_lowercase()),
        age,
        bio: None,
        active: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        cached: None,
    }
}

fn post(user: &User, title: &str) -> Post {
    Post { id: 0, user_id: user.id, title: title.to_string(), score: 1.5, slug: title.to_lowercase(), meta: None }
}

async fn setup(db: &Db) {
    db.create_table::<User>().await.unwrap();
    db.create_table::<Post>().await.unwrap();
    db.create_table::<Profile>().await.unwrap();
    db.create_table::<Tag>().await.unwrap();
}

async fn crud(db: &Db) {
    setup(db).await;
    let before = Utc::now() - chrono::Duration::seconds(5);
    let ada = user("Ada", 36).insert(db).await.unwrap();
    let bob = user("Bob", 41).insert(db).await.unwrap();
    assert!(ada.id > 0 && bob.id > ada.id);
    assert!(ada.created_at > before && ada.updated_at >= ada.created_at);

    assert_eq!(User::find(db, ada.id).await.unwrap(), Some(ada.clone()));
    assert_eq!(User::find(db, 9_999).await.unwrap(), None);
    assert!(matches!(User::find_or_fail(db, 9_999).await, Err(Error::NotFound)));
    assert_eq!(User::all(db).await.unwrap().len(), 2);

    let mut changed = ada.clone();
    changed.bio = Some("mathematician".into());
    changed.cached = Some("not stored".into());
    changed.update(db).await.unwrap();
    assert_eq!(changed.bio.as_deref(), Some("mathematician"));
    assert_eq!(changed.cached, None, "update reloads the row");
    assert_eq!(changed.created_at, ada.created_at);
    assert!(changed.updated_at >= ada.updated_at);
    assert_eq!(User::find_or_fail(db, ada.id).await.unwrap().bio.as_deref(), Some("mathematician"));

    bob.clone().delete(db).await.unwrap();
    assert_eq!(User::find(db, bob.id).await.unwrap(), None);
    assert!(matches!(bob.clone().delete(db).await, Err(Error::NotFound)));
    let mut gone = bob;
    assert!(matches!(gone.update(db).await, Err(Error::NotFound)));
}

async fn queries(db: &Db) {
    setup(db).await;
    for (i, age) in [10, 20, 30, 40, 50].into_iter().enumerate() {
        let mut u = user(&format!("u{}", i + 1), age);
        u.bio = (age >= 30).then(|| format!("bio {}", age));
        u.insert(db).await.unwrap();
    }
    let count = |q: axeon_orm::Query<User>| q.count(db);

    assert_eq!(count(User::query().filter(User::AGE.gte(30))).await.unwrap(), 3);
    assert_eq!(count(User::query().filter(User::AGE.ne(30))).await.unwrap(), 4);
    assert_eq!(count(User::query().filter(User::AGE.gt(10)).filter(User::AGE.lt(50))).await.unwrap(), 3);
    assert_eq!(count(User::query().filter(User::NAME.like("u%"))).await.unwrap(), 5);
    assert_eq!(count(User::query().filter(User::AGE.is_in([10, 50]))).await.unwrap(), 2);
    assert_eq!(count(User::query().filter(User::AGE.is_in(Vec::<i32>::new()))).await.unwrap(), 0);
    assert_eq!(count(User::query().filter(User::AGE.not_in(Vec::<i32>::new()))).await.unwrap(), 5);
    assert_eq!(count(User::query().filter(User::BIO.is_null())).await.unwrap(), 2);
    assert_eq!(count(User::query().filter(User::BIO.is_not_null())).await.unwrap(), 3);
    assert_eq!(count(User::query().filter(User::AGE.eq(10) | User::AGE.eq(20))).await.unwrap(), 2);
    assert_eq!(count(User::query().filter(!(User::AGE.eq(10) | User::AGE.eq(20)))).await.unwrap(), 3);
    assert_eq!(count(User::query().filter(User::AGE.gte(20) & User::BIO.is_null())).await.unwrap(), 1);
    assert_eq!(count(User::query().filter(Expr::all([]))).await.unwrap(), 5);
    assert_eq!(count(User::query().filter(Expr::any([]))).await.unwrap(), 0);
    assert_eq!(count(User::query().filter(User::BIO.eq("bio 40"))).await.unwrap(), 1);

    let names = |users: Vec<User>| users.into_iter().map(|u| u.name).collect::<Vec<_>>();
    let page = User::query().order_by(User::AGE.desc()).limit(2).offset(1).all(db).await.unwrap();
    assert_eq!(names(page), ["u4", "u3"]);
    let rest = User::query().order_by(User::AGE.asc()).offset(3).all(db).await.unwrap();
    assert_eq!(names(rest), ["u4", "u5"]);
    let first = User::query().order_by(User::AGE.asc()).first(db).await.unwrap().unwrap();
    assert_eq!(first.name, "u1");
    assert!(User::query().filter(User::AGE.eq(40)).exists(db).await.unwrap());
    assert!(!User::query().filter(User::AGE.eq(41)).exists(db).await.unwrap());
    assert!(matches!(User::query().filter(User::AGE.eq(41)).first_or_fail(db).await, Err(Error::NotFound)));

    let paged = User::query().order_by(User::AGE.asc()).paginate(db, 2, 2).await.unwrap();
    assert_eq!((names(paged.items), paged.page, paged.total, paged.last_page), (vec!["u3".into(), "u4".into()], 2, 5, 3));
    let paged = User::query().paginate(db, 0, 10).await.unwrap();
    assert_eq!((paged.items.len(), paged.page, paged.last_page), (5, 1, 1));
    let paged = User::query().paginate(db, 9, 10).await.unwrap();
    assert!(paged.items.is_empty());

    let n = User::query().filter(User::AGE.lt(30)).update(db, [User::ACTIVE.set(false), User::BIO.set("young")]).await.unwrap();
    assert_eq!(n, 2);
    assert_eq!(count(User::query().filter(User::ACTIVE.eq(false))).await.unwrap(), 2);
    User::query().filter(User::AGE.eq(10)).update(db, [User::BIO.set_null()]).await.unwrap();
    assert_eq!(count(User::query().filter(User::BIO.is_null())).await.unwrap(), 1);
    assert_eq!(User::query().filter(User::AGE.gte(50)).delete(db).await.unwrap(), 1);
    assert_eq!(User::query().count(db).await.unwrap(), 4);
}

async fn constraints(db: &Db) {
    setup(db).await;
    let ada = user("Ada", 36).insert(db).await.unwrap();
    assert!(matches!(user("Ada", 20).insert(db).await, Err(Error::UniqueViolation(_))));
    let orphan = Post { user_id: 9_999, ..post(&ada, "Orphan") };
    assert!(matches!(orphan.insert(db).await, Err(Error::ForeignKeyViolation(_))));
}

async fn relations(db: &Db) {
    setup(db).await;
    let ada = user("Ada", 36).insert(db).await.unwrap();
    let bob = user("Bob", 41).insert(db).await.unwrap();
    let cy = user("Cy", 25).insert(db).await.unwrap();
    for title in ["One", "Two"] {
        post(&ada, title).insert(db).await.unwrap();
    }
    post(&bob, "Three").insert(db).await.unwrap();
    Profile { id: 0, user_id: Some(ada.id), website: "ada.dev".into() }.insert(db).await.unwrap();
    Profile { id: 0, user_id: None, website: "nobody".into() }.insert(db).await.unwrap();

    // Lazy.
    assert_eq!(ada.posts().count(db).await.unwrap(), 2);
    let two = ada.posts().filter(Post::TITLE.eq("Two")).first(db).await.unwrap().unwrap();
    assert_eq!(two.user().first(db).await.unwrap().unwrap().name, "Ada");
    assert_eq!(ada.profile().first(db).await.unwrap().unwrap().website, "ada.dev");
    assert!(cy.profile().first(db).await.unwrap().is_none());

    // Eager, in the parents' order.
    let users = User::query().order_by(User::ID.asc()).with(User::POSTS).all(db).await.unwrap();
    let counts: Vec<(String, usize)> = users.iter().map(|(u, p)| (u.name.clone(), p.len())).collect();
    assert_eq!(counts, [("Ada".into(), 2), ("Bob".into(), 1), ("Cy".into(), 0)]);
    let profiles = User::query().order_by(User::ID.asc()).with(User::PROFILE).all(db).await.unwrap();
    assert_eq!(profiles.iter().map(|(_, p)| p.is_some()).collect::<Vec<_>>(), [true, false, false]);
    let posts = Post::query().order_by(Post::ID.asc()).with(Post::USER).all(db).await.unwrap();
    let authors: Vec<String> = posts.into_iter().map(|(_, u)| u.unwrap().name).collect();
    assert_eq!(authors, ["Ada", "Ada", "Bob"]);
    let owners = Profile::query().order_by(Profile::ID.asc()).with(Profile::USER).all(db).await.unwrap();
    assert_eq!(owners.iter().map(|(_, u)| u.is_some()).collect::<Vec<_>>(), [true, false]);
    let (first, posts) = User::query().filter(User::NAME.eq("Bob")).with(User::POSTS).first(db).await.unwrap().unwrap();
    assert_eq!((first.name.as_str(), posts.len()), ("Bob", 1));

    let loaded = User::POSTS.load(db, &[cy.clone(), ada.clone()]).await.unwrap();
    assert_eq!(loaded.iter().map(Vec::len).collect::<Vec<_>>(), [0, 2]);
    assert!(User::POSTS.load(db, &[]).await.unwrap().is_empty());

    // ON DELETE CASCADE and SET NULL.
    ada.delete(db).await.unwrap();
    assert_eq!(Post::query().count(db).await.unwrap(), 1);
    assert_eq!(Profile::query().filter(Profile::USER_ID.is_null()).count(db).await.unwrap(), 2);
}

async fn types(db: &Db) {
    setup(db).await;
    let ada = user("Ada", 36).insert(db).await.unwrap();
    let meta = json!({"tags": ["rust", "orm"], "draft": false});
    let stored = Post { meta: Some(meta.clone()), score: -2.25, ..post(&ada, "Typed") }.insert(db).await.unwrap();
    let loaded = Post::find_or_fail(db, stored.id).await.unwrap();
    assert_eq!((loaded.meta, loaded.score), (Some(meta), -2.25));

    // Text primary key, renamed column, database default.
    let tag = Tag { slug: "rust".into(), label: "Rust".into(), uses: 99 }.insert(db).await.unwrap();
    assert_eq!(tag, Tag { slug: "rust".into(), label: "Rust".into(), uses: 0 });
    assert_eq!(Tag::find(db, "rust").await.unwrap(), Some(tag.clone()));
    assert!(matches!(tag.clone().insert(db).await, Err(Error::UniqueViolation(_))));
    let mut tag = tag;
    tag.uses = 3;
    tag.update(db).await.unwrap();
    assert_eq!(Tag::query().filter(Tag::LABEL.eq("Rust")).first(db).await.unwrap().unwrap().uses, 3);
}

async fn transactions(db: &Db) {
    setup(db).await;
    let mut tx = db.begin().await.unwrap();
    let ada = user("Ada", 36).insert(&mut tx).await.unwrap();
    assert!(User::find(&mut tx, ada.id).await.unwrap().is_some());
    tx.rollback().await.unwrap();
    assert!(User::find(db, ada.id).await.unwrap().is_none());

    let mut tx = db.begin().await.unwrap();
    let bob = user("Bob", 41).insert(&mut tx).await.unwrap();
    post(&bob, "In tx").insert(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(bob.posts().count(db).await.unwrap(), 1);

    {
        let mut tx = db.begin().await.unwrap();
        user("Cy", 25).insert(&mut tx).await.unwrap();
    } // dropped: rolled back
    assert_eq!(User::query().count(db).await.unwrap(), 1);
}

async fn migrations(db: &Db) {
    let migrator = axeon_orm::embed_migrations!("tests/migrations");
    assert_eq!(migrator.migrations().len(), 2);
    assert_eq!(migrator.pending(db).await.unwrap().len(), 2);
    assert_eq!(migrator.run(db).await.unwrap(), [1, 2]);
    assert!(migrator.run(db).await.unwrap().is_empty());
    assert!(migrator.pending(db).await.unwrap().is_empty());

    let from_disk = Migrator::from_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/migrations")).unwrap();
    assert!(from_disk.run(db).await.unwrap().is_empty());

    // Editing an applied migration is an error.
    let edited = Migrator::new(vec![Migration::new(1, "create_accounts", "SELECT 1")]);
    assert!(matches!(edited.run(db).await, Err(Error::Migration(m)) if m.contains("changed")));

    // A failing migration leaves nothing behind.
    let mut list = migrator.migrations().to_vec();
    list.push(Migration::new(3, "good", "CREATE TABLE extra (id BIGINT PRIMARY KEY)"));
    list.push(Migration::new(4, "bad", "CREATE TABLE broken (id BIGINT PRIMARY KEY"));
    let failing = Migrator::new(list);
    assert!(matches!(failing.run(db).await, Err(Error::Migration(m)) if m.contains("migration 4")));
    assert_eq!(failing.pending(db).await.unwrap().len(), 2);
    assert!(db.execute("SELECT * FROM extra").await.is_err());

    let duplicate = Migrator::new(vec![Migration::new(7, "a", "SELECT 1"), Migration::new(7, "b", "SELECT 1")]);
    assert!(matches!(duplicate.run(db).await, Err(Error::Migration(m)) if m.contains("version 7")));
}

#[cfg(feature = "sqlite")]
async fn sqlite(_name: &str) -> Option<Db> {
    Some(Db::connect("sqlite::memory:").await.unwrap())
}

#[cfg(feature = "postgres")]
async fn postgres(name: &str) -> Option<Db> {
    let Ok(url) = std::env::var("AXEON_ORM_POSTGRES_URL") else {
        eprintln!("skipping: AXEON_ORM_POSTGRES_URL is not set");
        return None;
    };
    let schema = format!("axeon_orm_{}", name);
    let admin = Db::connect(&url).await.unwrap();
    admin.execute(&format!("DROP SCHEMA IF EXISTS {0} CASCADE; CREATE SCHEMA {0}", schema)).await.unwrap();
    admin.close().await;
    let separator = if url.contains('?') { '&' } else { '?' };
    let db = Db::connect(&format!("{}{}options=-c%20search_path%3D{}", url, separator, schema)).await.unwrap();
    assert_eq!(db.backend(), Backend::Postgres);
    Some(db)
}

macro_rules! backend_tests {
    ($($name:ident),* $(,)?) => {
        #[cfg(feature = "sqlite")]
        mod sqlite {
            $(#[tokio::test]
            async fn $name() {
                let db = super::sqlite(stringify!($name)).await.unwrap();
                super::$name(&db).await;
            })*
        }
        #[cfg(feature = "postgres")]
        mod postgres {
            $(#[tokio::test]
            async fn $name() {
                if let Some(db) = super::postgres(stringify!($name)).await {
                    super::$name(&db).await;
                }
            })*
        }
    };
}

backend_tests!(crud, queries, constraints, relations, types, transactions, migrations);

#[test]
fn generated_schema() {
    assert_eq!(User::TABLE, "users");
    assert_eq!(Tag::TABLE, "tag_list");
    assert_eq!(Tag::PRIMARY_KEY, "slug");
    assert_eq!(User::COLUMNS.len(), 8, "skip fields aren't columns");

    let pg = schema::create_table::<Post>(Backend::Postgres);
    assert!(pg.contains(r#""id" BIGSERIAL PRIMARY KEY"#), "{}", pg);
    assert!(pg.contains(r#""user_id" BIGINT NOT NULL REFERENCES "users" ("id") ON DELETE CASCADE"#), "{}", pg);
    assert!(pg.contains(r#""score" DOUBLE PRECISION NOT NULL"#), "{}", pg);
    assert!(pg.contains(r#""meta" JSONB,"#) || pg.contains(r#""meta" JSONB"#), "{}", pg);
    assert!(pg.contains(r#"CREATE INDEX IF NOT EXISTS "posts_slug_index" ON "posts" ("slug");"#), "{}", pg);

    let lite = schema::create_table::<User>(Backend::Sqlite);
    assert!(lite.contains(r#""id" INTEGER PRIMARY KEY AUTOINCREMENT"#), "{}", lite);
    assert!(lite.contains(r#""email" TEXT NOT NULL UNIQUE"#), "{}", lite);
    assert!(lite.contains(r#""bio" TEXT,"#), "{}", lite);
    assert!(lite.contains(r#""created_at" TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"#), "{}", lite);
    assert!(schema::create_table::<Profile>(Backend::Postgres).contains(r#""id" SERIAL PRIMARY KEY"#));
    assert!(schema::create_table::<Tag>(Backend::Sqlite).contains(r#""uses" INTEGER NOT NULL DEFAULT 0"#));
}

#[tokio::test]
async fn rejects_unknown_urls() {
    assert!(matches!(Db::connect("mysql://localhost/app").await, Err(Error::Config(_))));
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn failed_statement_poisons_transaction_on_sqlite_too() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    setup(&db).await;
    user("Ada", 36).insert(&db).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    user("Bob", 41).insert(&mut tx).await.unwrap();
    assert!(matches!(user("Ada", 20).insert(&mut tx).await, Err(Error::UniqueViolation(_))));
    assert!(matches!(tx.commit().await, Err(Error::Transaction(_))));
    assert_eq!(User::query().count(&db).await.unwrap(), 1);
}
