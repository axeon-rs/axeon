//! The same tests against SQLite (always), PostgreSQL (when
//! `AXEON_ORM_POSTGRES_URL` is set, e.g.
//! `postgres://postgres:postgres@localhost:55432/postgres`) and MySQL (when
//! `AXEON_ORM_MYSQL_URL` is set, e.g. `mysql://root:mysql@localhost:53306/axeon`).
//! Each PostgreSQL test runs in its own schema and each MySQL test in its
//! own database, so they can run in parallel.

#![cfg(all(feature = "chrono", feature = "json"))]

use axeon_orm::migrate::{Migration, Migrator};
use axeon_orm::{schema, Backend, Db, Error, Expr, Model, OnConflict, Order, SqlEnum};
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
#[model(belongs_to_many(Tag))]
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
#[model(table = "tag_list", belongs_to_many(Post))]
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

    // A failing migration leaves nothing behind, except on MySQL, which
    // commits each schema change on its own.
    let mut list = migrator.migrations().to_vec();
    list.push(Migration::new(3, "good", "CREATE TABLE extra (id BIGINT PRIMARY KEY)"));
    list.push(Migration::new(4, "bad", "CREATE TABLE broken (id BIGINT PRIMARY KEY"));
    let failing = Migrator::new(list);
    assert!(matches!(failing.run(db).await, Err(Error::Migration(m)) if m.contains("migration 4")));
    if db.backend() == Backend::MySql {
        assert_eq!(failing.pending(db).await.unwrap().len(), 1);
        assert!(db.execute("SELECT * FROM extra").await.is_ok());
    } else {
        assert_eq!(failing.pending(db).await.unwrap().len(), 2);
        assert!(db.execute("SELECT * FROM extra").await.is_err());
    }

    let status = failing.status(db).await.unwrap();
    let applied: Vec<bool> = status.iter().map(|s| s.applied_at.is_some()).collect();
    assert_eq!(applied.len(), 4);
    assert!(applied[0] && applied[1] && !applied[3]);
    assert!(status.iter().all(|s| !s.changed && !s.missing));
    let edited_status = edited.status(db).await.unwrap();
    assert!(edited_status[0].changed, "checksum mismatch is reported");
    assert!(edited_status.iter().skip(1).all(|s| s.missing), "unknown applied migrations are reported");

    let duplicate = Migrator::new(vec![Migration::new(7, "a", "SELECT 1"), Migration::new(7, "b", "SELECT 1")]);
    assert!(matches!(duplicate.run(db).await, Err(Error::Migration(m)) if m.contains("version 7")));
}

async fn many_to_many(db: &Db) {
    setup(db).await;
    db.create_pivot_table(&Post::TAGS).await.unwrap();
    let ada = user("Ada", 36).insert(db).await.unwrap();
    let one = post(&ada, "One").insert(db).await.unwrap();
    let two = post(&ada, "Two").insert(db).await.unwrap();
    let tag = |slug: &str| Tag { slug: slug.into(), label: slug.to_uppercase(), uses: 0 };
    let rust = tag("rust").insert(db).await.unwrap();
    let orm = tag("orm").insert(db).await.unwrap();
    let web = tag("web").insert(db).await.unwrap();

    Post::TAGS.attach(db, &one, &rust).await.unwrap();
    Post::TAGS.attach(db, &one, &rust).await.unwrap(); // already linked: no error, no duplicate
    Post::TAGS.attach(db, &one, &orm).await.unwrap();
    Post::TAGS.attach_key(db, &two, "rust".to_string()).await.unwrap();
    let slugs = |tags: Vec<Tag>| tags.into_iter().map(|t| t.slug).collect::<Vec<_>>();
    assert_eq!(slugs(one.tags().order_by(Tag::SLUG.asc()).all(db).await.unwrap()), ["orm", "rust"]);
    assert_eq!(rust.posts().count(db).await.unwrap(), 2, "the inverse side sees both links");
    assert!(matches!(Post::TAGS.attach_key(db, &one, "missing".to_string()).await, Err(Error::ForeignKeyViolation(_))));

    // Eager loading, both directions.
    let posts = Post::query().order_by(Post::ID.asc()).with(Post::TAGS).all(db).await.unwrap();
    assert_eq!(posts.iter().map(|(_, tags)| slugs(tags.clone())).collect::<Vec<_>>(), [vec!["orm", "rust"], vec!["rust"]]);
    let tags = Tag::query().order_by(Tag::SLUG.asc()).with(Tag::POSTS).all(db).await.unwrap();
    assert_eq!(tags.iter().map(|(_, posts)| posts.len()).collect::<Vec<_>>(), [1, 2, 0]);

    // Filtering by linked records.
    assert_eq!(Post::query().where_has(Post::TAGS, Tag::SLUG.eq("orm")).count(db).await.unwrap(), 1);
    assert_eq!(Post::query().has(Post::TAGS).count(db).await.unwrap(), 2);
    assert_eq!(slugs(Tag::query().doesnt_have(Tag::POSTS).all(db).await.unwrap()), ["web"]);

    // Detach and sync.
    assert!(Post::TAGS.detach(db, &one, &orm).await.unwrap());
    assert!(!Post::TAGS.detach(db, &one, &orm).await.unwrap());
    Post::TAGS.sync(db, &one, ["web".to_string(), "rust".to_string()]).await.unwrap();
    assert_eq!(slugs(one.tags().order_by(Tag::SLUG.asc()).all(db).await.unwrap()), ["rust", "web"]);
    let mut tx = db.begin().await.unwrap();
    Post::TAGS.sync(&mut tx, &one, Vec::<String>::new()).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(one.tags().count(db).await.unwrap(), 0);
    Post::TAGS.attach(db, &one, &web).await.unwrap();
    assert_eq!(Post::TAGS.detach_all(db, &one).await.unwrap(), 1);

    // Deleting a record deletes its links.
    two.delete(db).await.unwrap();
    assert_eq!(rust.posts().count(db).await.unwrap(), 0);
}

async fn aggregates(db: &Db) {
    setup(db).await;
    let mut users = Vec::new();
    for (name, age) in [("u1", 10), ("u2", 20), ("u3", 30)] {
        let mut u = user(name, age);
        u.bio = (age > 10).then(|| format!("bio {}", age));
        u.active = age != 20;
        users.push(u.insert(db).await.unwrap());
    }
    for score in [1.5, 2.5, -1.0] {
        Post { score, ..post(&users[0], &format!("p{}", score)) }.insert(db).await.unwrap();
    }

    assert_eq!(User::query().sum(db, User::AGE).await.unwrap(), Some(60));
    assert_eq!(User::query().avg(db, User::AGE).await.unwrap(), Some(20.0));
    assert_eq!(User::query().min(db, User::AGE).await.unwrap(), Some(10));
    assert_eq!(User::query().max(db, User::NAME).await.unwrap().as_deref(), Some("u3"));
    assert_eq!(User::query().filter(User::AGE.gt(15)).sum(db, User::ID).await.unwrap(), Some(users[1].id + users[2].id));
    assert_eq!(Post::query().sum(db, Post::SCORE).await.unwrap(), Some(3.0));
    assert!(User::query().max(db, User::CREATED_AT).await.unwrap().is_some());

    let none = || User::query().filter(User::AGE.gt(100));
    assert_eq!(none().sum(db, User::AGE).await.unwrap(), None);
    assert_eq!(none().avg(db, User::AGE).await.unwrap(), None);
    assert_eq!(none().min(db, User::AGE).await.unwrap(), None);

    assert_eq!(User::query().count_by(db, User::ACTIVE).await.unwrap(), [(false, 1), (true, 2)]);
    let bios = User::query().count_by(db, User::BIO).await.unwrap();
    assert_eq!(bios, [("bio 20".to_string(), 1), ("bio 30".to_string(), 1)], "NULL is left out");
}

async fn relation_filters_and_joins(db: &Db) {
    setup(db).await;
    let ada = user("Ada", 36).insert(db).await.unwrap();
    let bob = user("Bob", 41).insert(db).await.unwrap();
    let cy = user("Cy", 25).insert(db).await.unwrap();
    post(&ada, "Rust").insert(db).await.unwrap();
    post(&ada, "Go").insert(db).await.unwrap();
    post(&bob, "Zig").insert(db).await.unwrap();
    Profile { id: 0, user_id: Some(ada.id), website: "ada.dev".into() }.insert(db).await.unwrap();
    // A profile without a user must not break NOT IN.
    Profile { id: 0, user_id: None, website: "nobody".into() }.insert(db).await.unwrap();

    let names = |users: Vec<User>| users.into_iter().map(|u| u.name).collect::<Vec<_>>();
    let by_id = || User::query().order_by(User::ID.asc());
    assert_eq!(names(by_id().has(User::POSTS).all(db).await.unwrap()), ["Ada", "Bob"]);
    assert_eq!(names(by_id().doesnt_have(User::POSTS).all(db).await.unwrap()), ["Cy"]);
    assert_eq!(names(by_id().where_has(User::POSTS, Post::TITLE.eq("Rust")).all(db).await.unwrap()), ["Ada"]);
    assert_eq!(names(by_id().where_doesnt_have(User::POSTS, Post::TITLE.eq("Rust")).all(db).await.unwrap()), ["Bob", "Cy"]);
    assert_eq!(names(by_id().has(User::PROFILE).all(db).await.unwrap()), ["Ada"]);
    assert_eq!(names(by_id().doesnt_have(User::PROFILE).all(db).await.unwrap()), ["Bob", "Cy"]);
    assert_eq!(Post::query().where_has(Post::USER, User::NAME.eq("Bob")).count(db).await.unwrap(), 1);

    // Inner join on belongs_to, filtered and sorted by either side.
    let rows = Post::query()
        .join(Post::USER)
        .filter_related(User::NAME.eq("Ada"))
        .order_by(Post::TITLE.asc())
        .all(db)
        .await
        .unwrap();
    let titles: Vec<(String, String)> = rows.iter().map(|(p, u)| (p.title.clone(), u.name.clone())).collect();
    assert_eq!(titles, [("Go".into(), "Ada".into()), ("Rust".into(), "Ada".into())]);
    assert!(rows.iter().all(|(p, u)| p.user_id == u.id && p.id != 0), "same-named columns stay apart");
    let (first, author) = Post::query().join(Post::USER).order_by_related(User::NAME.desc()).first(db).await.unwrap().unwrap();
    assert_eq!((first.title.as_str(), author.name.as_str()), ("Zig", "Bob"));
    assert_eq!(Post::query().join(Post::USER).filter(Post::TITLE.ne("Go")).count(db).await.unwrap(), 2);

    // Left join on has_one keeps users without a profile.
    let profiles = by_id().left_join(User::PROFILE).all(db).await.unwrap();
    assert_eq!(profiles.iter().map(|(_, p)| p.as_ref().map(|p| p.website.as_str())).collect::<Vec<_>>(), [Some("ada.dev"), None, None]);

    // has_many joins repeat the parent per child.
    assert_eq!(User::query().join(User::POSTS).count(db).await.unwrap(), 3);
    assert_eq!(User::query().left_join(User::POSTS).count(db).await.unwrap(), 4);
    let page = by_id().join(User::POSTS).order_by_related(Post::TITLE.asc()).limit(2).offset(1).all(db).await.unwrap();
    assert_eq!(page.iter().map(|(_, p)| p.title.as_str()).collect::<Vec<_>>(), ["Rust", "Zig"]);
    assert_eq!(cy.posts().count(db).await.unwrap(), 0);
}

async fn raw_expressions(db: &Db) {
    setup(db).await;
    let mut users = Vec::new();
    for (name, age) in [("Ada", 36), ("bob", 41), ("Cy", 25)] {
        users.push(user(name, age).insert(db).await.unwrap());
    }
    post(&users[0], "a?b").insert(db).await.unwrap();
    post(&users[1], "plain").insert(db).await.unwrap();

    let names = |users: Vec<User>| users.into_iter().map(|u| u.name).collect::<Vec<_>>();
    let by_id = || User::query().order_by(User::ID.asc());
    assert_eq!(names(by_id().filter(Expr::raw("age * 2 > ?", (75,))).all(db).await.unwrap()), ["bob"]);
    assert_eq!(names(by_id().filter(Expr::raw("LOWER(name) IN (?, ?)", ["ada", "bob"])).all(db).await.unwrap()), ["Ada", "bob"]);
    assert_eq!(User::query().filter(Expr::raw("1 = 1", ())).count(db).await.unwrap(), 3);

    // The fragment is parenthesized: its OR must not escape the AND.
    let q = by_id().filter(User::AGE.lt(40)).filter(Expr::raw("name = ? OR name = ?", ("bob", "Cy")));
    assert_eq!(names(q.all(db).await.unwrap()), ["Cy"]);
    let q = by_id().filter(!Expr::raw("age > ? OR age < ?", (40, 30)) & User::AGE.gt(0));
    assert_eq!(names(q.all(db).await.unwrap()), ["Ada"]);

    // Placeholders keep their numbering after typed conditions (PostgreSQL's $n).
    let q = by_id().filter(User::AGE.gt(20)).filter(Expr::raw("age < ?", (40,))).filter(User::NAME.ne("Cy"));
    assert_eq!(names(q.all(db).await.unwrap()), ["Ada"]);

    // `??` is a literal question mark.
    assert_eq!(Post::query().filter(Expr::raw("title = 'a??b'", ())).count(db).await.unwrap(), 1);
    assert_eq!(Post::query().filter(Expr::raw("title <> ? AND title <> 'x??'", ("plain",))).count(db).await.unwrap(), 1);

    // Ordering by an expression, with tie-breakers.
    let q = User::query().order_by(Order::raw("ABS(age - ?)", (40,))).order_by(User::NAME.asc());
    assert_eq!(names(q.all(db).await.unwrap()), ["bob", "Ada", "Cy"]);
    let q = User::query().filter(Expr::raw("age > ?", (20,))).order_by(Order::raw_desc("age % ?", (10,))).limit(2);
    assert_eq!(names(q.all(db).await.unwrap()), ["Ada", "Cy"]);

    // Inside relation subqueries, and in joins with t0 / t1.
    assert_eq!(names(by_id().where_has(User::POSTS, Expr::raw("title LIKE ?", ("a%",))).all(db).await.unwrap()), ["Ada"]);
    let rows = Post::query()
        .join(Post::USER)
        .filter_related(Expr::raw("t1.age > ?", (40,)))
        .order_by(Order::raw("LENGTH(t0.title)", ()))
        .all(db)
        .await
        .unwrap();
    assert_eq!(rows.iter().map(|(p, u)| (p.title.as_str(), u.name.as_str())).collect::<Vec<_>>(), [("plain", "bob")]);
    assert_eq!(User::query().filter(Expr::raw("age < ?", (30,))).update(db, [User::ACTIVE.set(false)]).await.unwrap(), 1);
    assert_eq!(Post::query().filter(Expr::raw("score >= ?", (0.0,))).delete(db).await.unwrap(), 2);

    if db.backend() == Backend::Postgres {
        let meta = json!({ "certs": ["CE", "RoHS"], "moq": 100 });
        Post { meta: Some(meta), ..post(&users[0], "jsonb") }.insert(db).await.unwrap();
        Post { meta: Some(json!({ "certs": ["CE"] })), ..post(&users[1], "jsonb2") }.insert(db).await.unwrap();
        let count = |e: Expr<Post>| Post::query().filter(e).count(db);
        assert_eq!(count(Expr::raw("meta @> ?", (json!({ "certs": ["RoHS"] }),))).await.unwrap(), 1);
        assert_eq!(count(Expr::raw("meta ?? 'moq'", ())).await.unwrap(), 1);
        assert_eq!(count(Expr::raw("meta ?? ?", ("certs",))).await.unwrap(), 2);
        assert_eq!(count(Expr::raw("(meta->>'moq')::int <= ?", (100,))).await.unwrap(), 1);
    }
}

async fn row_locks(db: &Db) {
    setup(db).await;
    let mut users = Vec::new();
    for (name, age) in [("Ada", 36), ("Bob", 41), ("Cy", 25)] {
        users.push(user(name, age).insert(db).await.unwrap());
    }
    post(&users[0], "Rust").insert(db).await.unwrap();
    let names = |users: Vec<User>| users.into_iter().map(|u| u.name).collect::<Vec<_>>();
    let by_id = || User::query().order_by(User::ID.asc());

    // Without contention, every lock kind reads like a plain query.
    let mut tx = db.begin().await.unwrap();
    assert_eq!(names(by_id().for_update().all(&mut tx).await.unwrap()), ["Ada", "Bob", "Cy"]);
    assert_eq!(names(by_id().filter(User::AGE.gt(30)).skip_locked().limit(1).all(&mut tx).await.unwrap()), ["Ada"]);
    assert_eq!(by_id().nowait().first(&mut tx).await.unwrap().unwrap().name, "Ada");
    let page = by_id().for_update().paginate(&mut tx, 2, 2).await.unwrap();
    assert_eq!((names(page.items), page.total), (vec!["Cy".to_string()], 3));
    assert_eq!(by_id().for_update().count(&mut tx).await.unwrap(), 3, "count ignores the lock");
    assert!(by_id().for_update().exists(&mut tx).await.unwrap());
    assert_eq!(by_id().for_update().with(User::POSTS).all(&mut tx).await.unwrap()[0].1.len(), 1);
    // Left joins lock only the main table; its nullable side can't be locked.
    let rows = by_id().for_update().left_join(User::PROFILE).all(&mut tx).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(Post::query().join(Post::USER).skip_locked().all(&mut tx).await.unwrap().len(), 1);
    tx.commit().await.unwrap();

    if db.backend() == Backend::Sqlite {
        return; // One writer at a time: there are no row locks to contend for.
    }
    let mut holder = db.begin().await.unwrap();
    let ada = User::query().filter(User::ID.eq(users[0].id)).for_update().first_or_fail(&mut holder).await.unwrap();
    assert_eq!(ada.name, "Ada");

    // Another transaction skips the locked row, or fails at once with NOWAIT.
    let mut other = db.begin().await.unwrap();
    assert_eq!(names(by_id().skip_locked().all(&mut other).await.unwrap()), ["Bob", "Cy"]);
    let next = by_id().skip_locked().first(&mut other).await.unwrap().unwrap();
    assert_eq!(next.name, "Bob");
    let err = User::query().filter(User::ID.eq(ada.id)).nowait().all(&mut other).await.unwrap_err();
    assert!(matches!(err, Error::LockNotAvailable(_)), "{:?}", err);
    other.rollback().await.unwrap();

    // Plain reads don't take locks, so they aren't blocked.
    assert_eq!(User::find_or_fail(db, ada.id).await.unwrap().name, "Ada");
    holder.commit().await.unwrap();
    let mut tx = db.begin().await.unwrap();
    assert_eq!(User::query().filter(User::ID.eq(ada.id)).nowait().all(&mut tx).await.unwrap().len(), 1, "released on commit");
    tx.rollback().await.unwrap();
}

#[derive(SqlEnum, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Pending,
    InReview,
    #[sql_enum(rename = "o'k")]
    Approved,
}

#[derive(SqlEnum, Clone, Copy, Debug, PartialEq)]
#[sql_enum(rename_all = "SCREAMING_SNAKE_CASE")]
enum Priority {
    Low,
    VeryHigh,
}

#[derive(Model, Clone, Debug, PartialEq)]
struct Ticket {
    id: i64,
    #[model(index)]
    status: Status,
    priority: Priority,
    previous: Option<Status>,
}

/// The `tickets` table seen as plain text, without the `CHECK`.
#[derive(Model)]
#[model(table = "tickets")]
struct RawTicket {
    id: i64,
    status: String,
    priority: String,
    previous: Option<String>,
}

async fn enums(db: &Db) {
    db.create_table::<Ticket>().await.unwrap();
    let ticket = |status, priority, previous| Ticket { id: 0, status, priority, previous };
    let a = ticket(Status::Pending, Priority::Low, None).insert(db).await.unwrap();
    ticket(Status::InReview, Priority::VeryHigh, Some(Status::Pending)).insert(db).await.unwrap();
    ticket(Status::Approved, Priority::Low, Some(Status::InReview)).insert(db).await.unwrap();

    assert_eq!(Ticket::find_or_fail(db, a.id).await.unwrap(), a);
    let count = |e: Expr<Ticket>| Ticket::query().filter(e).count(db);
    assert_eq!(count(Ticket::STATUS.eq(Status::Approved)).await.unwrap(), 1);
    assert_eq!(count(Ticket::STATUS.is_in([Status::Pending, Status::InReview])).await.unwrap(), 2);
    assert_eq!(count(Ticket::PREVIOUS.is_null()).await.unwrap(), 1);
    assert_eq!(count(Ticket::PREVIOUS.eq(Status::Pending)).await.unwrap(), 1);
    assert_eq!(count(Expr::raw("status <> ?", (Status::Pending,))).await.unwrap(), 2);
    let by_priority = Ticket::query().count_by(db, Ticket::PRIORITY).await.unwrap();
    assert_eq!(by_priority, [(Priority::Low, 2), (Priority::VeryHigh, 1)]);

    // Stored as text, as named.
    let raw = RawTicket::query().order_by(RawTicket::ID.asc()).all(db).await.unwrap();
    let stored: Vec<_> = raw.iter().map(|t| (t.status.as_str(), t.priority.as_str(), t.previous.as_deref())).collect();
    assert_eq!(stored, [("pending", "LOW", None), ("in_review", "VERY_HIGH", Some("pending")), ("o'k", "LOW", Some("in_review"))]);

    Ticket::query().filter(Ticket::ID.eq(a.id)).update(db, [Ticket::STATUS.set(Status::Approved), Ticket::PREVIOUS.set(Status::Pending)]).await.unwrap();
    let mut a = Ticket::find_or_fail(db, a.id).await.unwrap();
    assert_eq!((a.status, a.previous), (Status::Approved, Some(Status::Pending)));
    a.status = Status::InReview;
    a.previous = None;
    a.update(db).await.unwrap();
    assert_eq!(Ticket::find_or_fail(db, a.id).await.unwrap(), a);

    // The CHECK constraint refuses other values.
    let bad = RawTicket { id: 0, status: "lost".into(), priority: "LOW".into(), previous: None }.insert(db).await;
    assert!(bad.is_err(), "CHECK let an unknown value in");

    // Reading an unknown value is an error, not a panic.
    db.drop_table::<Ticket>().await.unwrap();
    db.create_table::<RawTicket>().await.unwrap();
    RawTicket { id: 0, status: "archived".into(), priority: "LOW".into(), previous: None }.insert(db).await.unwrap();
    match Ticket::query().first(db).await {
        Err(Error::Decode { column, message }) => {
            assert_eq!(column, "status");
            assert!(message.contains(r#""archived" is not a Status value"#), "{}", message);
        }
        other => panic!("expected a decode error, got {:?}", other),
    }
}

#[derive(Model, Clone, Debug, PartialEq)]
struct Product {
    id: i64,
    #[model(unique)]
    sku: String,
    name: String,
    price: i64,
    #[model(created_at)]
    created_at: DateTime<Utc>,
    #[model(updated_at)]
    updated_at: DateTime<Utc>,
}

/// Unique on (warehouse, sku) together.
#[derive(Model, Clone, Debug, PartialEq)]
struct Stock {
    id: i32,
    #[model(index)]
    warehouse: String,
    #[model(index)]
    sku: String,
    qty: i32,
}

#[derive(Model, Clone, Debug, PartialEq)]
struct Setting {
    #[model(primary_key)]
    key: String,
    value: String,
}

#[derive(Model, Clone, Debug, PartialEq)]
struct Flag {
    #[model(primary_key)]
    name: String,
}

async fn upserts(db: &Db) {
    db.create_table::<Product>().await.unwrap();
    db.create_table::<Stock>().await.unwrap();
    db.create_table::<Setting>().await.unwrap();
    db.create_table::<Flag>().await.unwrap();
    db.execute("CREATE UNIQUE INDEX stock_key ON stocks (warehouse, sku)").await.unwrap();
    let now = Utc::now();
    let product = |sku: &str, name: &str, price| Product {
        id: 0,
        sku: sku.into(),
        name: name.into(),
        price,
        created_at: now,
        updated_at: now,
    };
    let by_sku = || OnConflict::on(Product::SKU);

    // Inserts when new, updates the same row when the SKU exists.
    let lamp = product("L-1", "Lamp", 100).upsert(db, by_sku()).await.unwrap();
    assert!(lamp.id > 0);
    let other = product("C-1", "Chair", 300).upsert(db, by_sku()).await.unwrap();
    assert_ne!(other.id, lamp.id);
    let updated = product("L-1", "Desk lamp", 120).upsert(db, by_sku()).await.unwrap();
    assert_eq!((updated.id, updated.name.as_str(), updated.price), (lamp.id, "Desk lamp", 120));
    assert_eq!(updated.created_at, lamp.created_at, "created_at is kept");
    assert!(updated.updated_at >= lamp.updated_at);
    assert_eq!(Product::find_or_fail(db, lamp.id).await.unwrap(), updated);
    // Unchanged values still give the row back (MySQL then affects no rows).
    assert_eq!(product("L-1", "Desk lamp", 120).upsert(db, by_sku()).await.unwrap().id, lamp.id);
    assert_eq!(Product::query().count(db).await.unwrap(), 2);

    // `update` limits what an upsert overwrites.
    let priced = product("L-1", "Ignored", 99).upsert(db, by_sku().update(Product::PRICE)).await.unwrap();
    assert_eq!((priced.id, priced.name.as_str(), priced.price), (lamp.id, "Desk lamp", 99));

    // insert_or_ignore leaves an existing row alone.
    assert!(product("L-1", "Nope", 1).insert_or_ignore(db, by_sku()).await.unwrap().is_none());
    assert_eq!(Product::find_or_fail(db, lamp.id).await.unwrap().price, 99);
    let fresh = product("T-1", "Table", 500).insert_or_ignore(db, by_sku()).await.unwrap().unwrap();
    assert_eq!((fresh.sku.as_str(), fresh.price), ("T-1", 500));
    assert_eq!(Product::find_or_fail(db, fresh.id).await.unwrap(), fresh);
    assert_eq!(Product::query().count(db).await.unwrap(), 3);

    // A conflict on two columns together.
    let stock = |warehouse: &str, sku: &str, qty| Stock { id: 0, warehouse: warehouse.into(), sku: sku.into(), qty };
    let key = || OnConflict::on(Stock::WAREHOUSE).and(Stock::SKU);
    let a = stock("sz", "L-1", 5).upsert(db, key()).await.unwrap();
    let b = stock("gz", "L-1", 7).upsert(db, key()).await.unwrap();
    assert_ne!(a.id, b.id);
    let a2 = stock("sz", "L-1", 9).upsert(db, key()).await.unwrap();
    assert_eq!((a2.id, a2.qty), (a.id, 9));
    assert_eq!(Stock::query().count(db).await.unwrap(), 2);

    // A primary key that isn't auto-increment, as the conflict.
    let setting = |value: &str| Setting { key: "theme".into(), value: value.into() };
    assert_eq!(setting("dark").upsert(db, OnConflict::on(Setting::KEY)).await.unwrap().value, "dark");
    assert_eq!(setting("light").upsert(db, OnConflict::on(Setting::KEY)).await.unwrap().value, "light");
    assert!(setting("blue").insert_or_ignore(db, OnConflict::on(Setting::KEY)).await.unwrap().is_none());
    assert_eq!(Setting::find_or_fail(db, "theme".to_string()).await.unwrap().value, "light");
    let other = Setting { key: "lang".into(), value: "zh".into() };
    assert_eq!(other.clone().insert_or_ignore(db, OnConflict::on(Setting::KEY)).await.unwrap(), Some(other));

    // Nothing to update besides the key.
    let flag = || Flag { name: "beta".into() };
    assert_eq!(flag().upsert(db, OnConflict::on(Flag::NAME)).await.unwrap(), flag());
    assert_eq!(flag().upsert(db, OnConflict::on(Flag::NAME)).await.unwrap(), flag());
    assert_eq!(Flag::query().count(db).await.unwrap(), 1);

    // Inside a transaction, and rolled back with it.
    let mut tx = db.begin().await.unwrap();
    product("L-1", "In tx", 1).upsert(&mut tx, by_sku()).await.unwrap();
    assert!(product("X-1", "In tx", 1).insert_or_ignore(&mut tx, by_sku()).await.unwrap().is_some());
    assert_eq!(Product::find_or_fail(&mut tx, lamp.id).await.unwrap().name, "In tx");
    tx.rollback().await.unwrap();
    assert_eq!(Product::find_or_fail(db, lamp.id).await.unwrap().name, "Desk lamp");
    assert_eq!(Product::query().count(db).await.unwrap(), 3);

    // A conflict that is ignored doesn't spoil the transaction.
    let mut tx = db.begin().await.unwrap();
    assert!(product("L-1", "Dup", 1).insert_or_ignore(&mut tx, by_sku()).await.unwrap().is_none());
    product("Y-1", "Kept", 1).insert(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(Product::query().filter(Product::SKU.eq("Y-1")).count(db).await.unwrap(), 1);

}

#[derive(Model, Clone, Debug)]
struct Item {
    id: i64,
    name: String,
    fields: serde_json::Value,
    extra: Option<sqlx::types::Json<serde_json::Value>>,
}

async fn json_filters(db: &Db) {
    db.create_table::<Item>().await.unwrap();
    let rows = [
        ("lamp", json!({ "specs": { "color": "red", "watts": 40 }, "price": { "min": 12.5 }, "certs": ["CE", "RoHS"], "moq": 100, "eco": true, "sizes": ["S", "M"] })),
        ("chair", json!({ "specs": { "color": "Red", "watts": null }, "price": { "min": 30 }, "certs": ["CE"], "eco": false, "sizes": ["L"] })),
        ("desk", json!({ "specs": { "color": "blue" }, "price": { "min": "n/a" }, "certs": [], "a,b}": { "q\"uote": "odd" } })),
        ("rug", json!({ "specs": { "color": "red", "watts": 7 }, "price": { "min": 9 }, "moq": null })),
    ];
    for (name, fields) in rows {
        Item { id: 0, name: name.into(), fields, extra: None }.insert(db).await.unwrap();
    }
    Item { id: 0, name: "typed".into(), fields: json!({}), extra: Some(sqlx::types::Json(json!({ "k": "v" }))) }.insert(db).await.unwrap();

    let names = |items: Vec<Item>| items.into_iter().map(|i| i.name).collect::<Vec<_>>();
    let find = |e: Expr<Item>| Item::query().filter(e).order_by(Item::ID.asc()).all(db);
    let f = || Item::FIELDS;

    // Strings: case-sensitive, only JSON strings.
    assert_eq!(names(find(f().key("specs").key("color").text_eq("red")).await.unwrap()), ["lamp", "rug"]);
    assert_eq!(names(find(f().key("specs").key("color").text_ne("red")).await.unwrap()), ["chair", "desk"], "missing isn't 'not red'");
    assert_eq!(names(find(f().key("specs").key("color").text_in(["blue", "Red"])).await.unwrap()), ["chair", "desk"]);
    assert!(find(f().key("specs").key("color").text_in(Vec::<String>::new())).await.unwrap().is_empty());
    assert_eq!(names(find(f().key("specs").key("color").text_like("b%")).await.unwrap()), ["desk"]);
    assert!(find(f().key("moq").text_eq("100")).await.unwrap().is_empty(), "a number isn't text");

    // Numbers: strings and nulls just don't match.
    assert_eq!(names(find(f().key("price").key("min").lte(12.5)).await.unwrap()), ["lamp", "rug"]);
    assert_eq!(names(find(f().key("price").key("min").gt(10)).await.unwrap()), ["lamp", "chair"]);
    assert_eq!(names(find(f().key("price").key("min").num_eq(30)).await.unwrap()), ["chair"]);
    assert_eq!(names(find(f().key("specs").key("watts").gte(0)).await.unwrap()), ["lamp", "rug"]);
    assert_eq!(names(find(f().key("price").key("min").lt(1000)).await.unwrap()), ["lamp", "chair", "rug"], "\"n/a\" doesn't fail the query");

    // Booleans, arrays, existence.
    assert_eq!(names(find(f().key("eco").bool_eq(true)).await.unwrap()), ["lamp"]);
    assert_eq!(names(find(f().key("eco").bool_eq(false)).await.unwrap()), ["chair"]);
    assert_eq!(names(find(f().key("sizes").index(1).text_eq("M")).await.unwrap()), ["lamp"]);
    assert_eq!(names(find(f().key("certs").index(0).exists()).await.unwrap()), ["lamp", "chair"]);
    assert_eq!(names(find(f().key("moq").exists()).await.unwrap()), ["lamp", "rug"], "null still exists");
    assert_eq!(names(find(f().has_key("moq")).await.unwrap()), ["lamp", "rug"]);
    assert_eq!(names(find(!f().has_key("moq") & f().has_key("specs")).await.unwrap()), ["chair", "desk"]);
    assert_eq!(names(find(f().key("specs").key("watts").exists() & !f().key("specs").key("watts").gte(0)).await.unwrap()), ["chair"]);
    // Keys with special characters are bound, not spliced into the SQL. (This
    // also follows an index path of the same shape: a regression test for
    // PostgreSQL statement caching with differently typed parameters.)
    assert_eq!(names(find(f().key("a,b}").key("q\"uote").text_eq("odd")).await.unwrap()), ["desk"]);
    assert_eq!(names(find(Item::EXTRA.key("k").text_eq("v")).await.unwrap()), ["typed"], "Json<T> columns too");

    // Sorting by a path.
    let by_price = Item::query().filter(f().key("price").key("min").gte(0)).order_by(f().key("price").key("min").desc()).all(db).await.unwrap();
    assert_eq!(names(by_price), ["chair", "lamp", "rug"]);
    let by_color = Item::query().filter(f().key("specs").key("color").exists()).order_by(f().key("specs").key("color").asc()).order_by(Item::ID.asc());
    let colors: Vec<String> = by_color.all(db).await.unwrap().into_iter().map(|i| i.fields["specs"]["color"].as_str().unwrap().to_string()).collect();
    // How case sorts is the collation's call: bytewise in SQLite and MySQL
    // (JSON text is binary), lowercase first under PostgreSQL's en_US.
    let mut bytewise = colors.clone();
    bytewise.sort();
    let mut caseless = colors.clone();
    caseless.sort_by_key(|c| c.to_lowercase());
    assert!(colors == bytewise || colors == caseless, "{:?} isn't sorted", colors);
    assert_eq!(colors.len(), 4);

    // A NULL column matches neither a filter nor its negation's opposite.
    assert_eq!(names(find(!Item::EXTRA.has_key("k")).await.unwrap()), ["lamp", "chair", "desk", "rug"]);
    if db.backend() != Backend::Sqlite {
        assert_eq!(names(find(!Item::EXTRA.contains(json!({ "k": "v" }))).await.unwrap()), ["lamp", "chair", "desk", "rug"]);
    }

    // Containment: PostgreSQL and MySQL only.
    let contains = find(f().contains(json!({ "certs": ["RoHS"] }))).await;
    let nested = find(f().contains(json!({ "specs": { "color": "red" } }))).await;
    if db.backend() == Backend::Sqlite {
        let error = contains.unwrap_err().to_string();
        assert!(error.contains("axeon_orm_json_contains_needs_postgres_or_mysql"), "{}", error);
    } else {
        assert_eq!(names(contains.unwrap()), ["lamp"]);
        assert_eq!(names(nested.unwrap()), ["lamp", "rug"]);
        assert_eq!(names(find(f().contains(json!({ "certs": ["CE"] })) & f().key("price").key("min").lt(20)).await.unwrap()), ["lamp"]);
    }

    // With joins and updates too.
    assert_eq!(Item::query().filter(f().key("eco").exists()).update(db, [Item::NAME.set("eco-tagged")]).await.unwrap(), 2);
    assert_eq!(Item::query().filter(Item::NAME.eq("eco-tagged")).count(db).await.unwrap(), 2);
}

async fn bulk_inserts(db: &Db) {
    db.create_table::<Product>().await.unwrap();
    db.create_table::<Setting>().await.unwrap();
    db.create_table::<Flag>().await.unwrap();
    let now = Utc::now();
    let product = |i: usize| Product { id: 0, sku: format!("SKU-{:05}", i), name: format!("Product {}", i), price: i as i64, created_at: now, updated_at: now };

    assert!(Product::insert_many(db, &[]).await.unwrap().is_empty());

    // 2500 rows: several statements, all in one transaction.
    let rows: Vec<Product> = (0..2500).map(product).collect();
    let started = std::time::Instant::now();
    let stored = Product::insert_many(db, &rows).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(stored.len(), 2500);
    assert!(stored.iter().zip(&rows).all(|(s, r)| s.sku == r.sku && s.name == r.name), "same order as the input");
    assert!(stored.windows(2).all(|w| w[0].id < w[1].id), "new IDs, increasing");
    assert!(stored[0].id > 0);
    assert_eq!(Product::query().count(db).await.unwrap(), 2500);
    assert_eq!(Product::find_or_fail(db, stored[1234].id).await.unwrap(), stored[1234]);
    eprintln!("{:?}: 2500 rows in {:?}", db.backend(), elapsed);

    // All or nothing: a duplicate SKU in the third chunk rolls back the rest.
    let mut batch: Vec<Product> = (2500..4000).map(product).collect();
    batch[1300].sku = "SKU-00007".into();
    let failed = Product::insert_many(db, &batch).await;
    assert!(matches!(failed, Err(Error::UniqueViolation(_))), "{:?}", failed.map(|r| r.len()));
    assert_eq!(Product::query().count(db).await.unwrap(), 2500);
    // Inside the caller's transaction, it's the caller's to commit.
    let mut tx = db.begin().await.unwrap();
    Product::insert_many(&mut tx, &[product(5000), product(5001)]).await.unwrap();
    assert_eq!(Product::query().count(&mut tx).await.unwrap(), 2502);
    tx.rollback().await.unwrap();
    assert_eq!(Product::query().count(db).await.unwrap(), 2500);

    // Keys that aren't generated.
    let settings: Vec<Setting> = ["b", "a", "c"].iter().map(|k| Setting { key: k.to_string(), value: format!("v{}", k) }).collect();
    let stored = Setting::insert_many(db, &settings).await.unwrap();
    assert_eq!(stored, settings);
    // Only a primary key: one statement per row, still fine.
    let flags = vec![Flag { name: "x".into() }, Flag { name: "y".into() }];
    assert_eq!(Flag::insert_many(db, &flags).await.unwrap(), flags);

    // Bulk upsert: existing SKUs are updated (keeping their IDs and
    // created_at), new ones inserted, all in input order.
    let existing = Product::query().filter(Product::SKU.is_in(["SKU-00002", "SKU-00001"])).order_by(Product::SKU.desc()).all(db).await.unwrap();
    let later = Utc::now() + chrono::Duration::seconds(5);
    let mut changes = vec![
        Product { name: "renamed 2".into(), price: 999, ..existing[0].clone() },
        Product { sku: "NEW-1".into(), name: "new".into(), ..product(0) },
        Product { name: "renamed 1".into(), ..existing[1].clone() },
    ];
    for change in &mut changes {
        change.created_at = later;
    }
    let upserted = Product::upsert_many(db, &changes, OnConflict::on(Product::SKU)).await.unwrap();
    assert_eq!(upserted.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["renamed 2", "new", "renamed 1"]);
    assert_eq!((upserted[0].id, upserted[0].price), (existing[0].id, 999));
    assert_eq!(upserted[2].id, existing[1].id);
    assert_eq!(upserted[0].created_at, existing[0].created_at, "created_at is kept");
    assert!(upserted[1].id > 0 && upserted[1].id != existing[0].id);
    assert_eq!(Product::query().count(db).await.unwrap(), 2501);
    // Limiting the update.
    let price_only = vec![Product { name: "ignored".into(), price: 1, ..existing[0].clone() }];
    let updated = Product::upsert_many(db, &price_only, OnConflict::on(Product::SKU).update(Product::PRICE)).await.unwrap();
    assert_eq!((updated[0].name.as_str(), updated[0].price), ("renamed 2", 1));
    // Many rows across chunks.
    let refresh: Vec<Product> = (0..2100).map(|i| Product { price: -1, ..product(i) }).collect();
    let refreshed = Product::upsert_many(db, &refresh, OnConflict::on(Product::SKU)).await.unwrap();
    assert_eq!(refreshed.len(), 2100);
    assert!(refreshed.iter().zip(&refresh).all(|(s, r)| s.sku == r.sku && s.price == -1));
    assert_eq!(Product::query().count(db).await.unwrap(), 2501);

    // The same key twice in one call is refused before anything is written.
    let twice = vec![Product { price: 7, ..product(10) }, Product { price: 8, ..product(10) }, product(9000)];
    let refused = Product::upsert_many(db, &twice, OnConflict::on(Product::SKU)).await;
    assert!(matches!(&refused, Err(Error::Config(m)) if m.contains("same conflict key")), "{:?}", refused.map(|r| r.len()));
    assert_eq!(Product::query().filter(Product::PRICE.eq(7)).count(db).await.unwrap(), 0);
    assert_eq!(Product::query().count(db).await.unwrap(), 2501);
}

async fn cursor_pagination(db: &Db) {
    setup(db).await;
    // Repeated ages, so ties are broken by the primary key.
    let ages = [30, 20, 30, 40, 20, 30, 10, 40, 30, 20, 50];
    for (i, age) in ages.into_iter().enumerate() {
        user(&format!("u{:02}", i), age).insert(db).await.unwrap();
    }

    // Walks every page and checks it matches the same order read in one go.
    async fn walk(db: &Db, query: axeon_orm::Query<User>, per_page: u64) -> Vec<i64> {
        let mut seen = Vec::new();
        let mut after: Option<String> = None;
        let mut pages = 0;
        loop {
            let page = query.clone().cursor_paginate(db, after.as_deref(), per_page).await.unwrap();
            assert!(page.items.len() as u64 <= per_page);
            seen.extend(page.items.iter().map(|u| u.id));
            pages += 1;
            assert!(pages < 50, "cursor pagination doesn't advance");
            match page.next_cursor {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        seen
    }
    let ids = |users: Vec<User>| users.into_iter().map(|u| u.id).collect::<Vec<_>>();

    // Default order: primary key ascending.
    let expected = ids(User::query().order_by(User::ID.asc()).all(db).await.unwrap());
    assert_eq!(walk(db, User::query(), 4).await, expected);

    // One direction (a row comparison on PostgreSQL and SQLite).
    let expected = ids(User::query().order_by(User::AGE.desc()).order_by(User::ID.desc()).all(db).await.unwrap());
    for per_page in [1, 3, 11, 20] {
        assert_eq!(walk(db, User::query().order_by(User::AGE.desc()), per_page).await, expected);
    }

    // Mixed directions, a filter and a timestamp key.
    let expected = ids(
        User::query()
            .filter(User::AGE.gte(20))
            .order_by(User::AGE.asc())
            .order_by(User::CREATED_AT.desc())
            .order_by(User::ID.desc())
            .all(db)
            .await
            .unwrap(),
    );
    let mixed = User::query().filter(User::AGE.gte(20)).order_by(User::AGE.asc()).order_by(User::CREATED_AT.desc());
    assert_eq!(walk(db, mixed.clone(), 2).await, expected);

    // The last page has no cursor; an exact fit doesn't leave an empty page.
    let all = User::query().cursor_paginate(db, None, 11).await.unwrap();
    assert_eq!((all.items.len(), all.has_more()), (11, false));

    // Cursors are checked.
    let first = User::query().order_by(User::AGE.desc()).cursor_paginate(db, None, 2).await.unwrap();
    let cursor = first.next_cursor.unwrap();
    let other_order = User::query().order_by(User::AGE.asc()).cursor_paginate(db, Some(&cursor), 2).await;
    assert!(matches!(&other_order, Err(Error::InvalidCursor(m)) if m.contains("sort order")), "{:?}", other_order.map(|p| p.items.len()));
    for bad in ["", "not a cursor", "eyJrIjoxfQ"] {
        let result = User::query().cursor_paginate(db, Some(bad), 2).await;
        assert!(matches!(result, Err(Error::InvalidCursor(_))), "{:?}", bad);
    }
    let nullable = User::query().order_by(User::BIO.asc()).cursor_paginate(db, None, 2).await;
    assert!(matches!(&nullable, Err(Error::Config(m)) if m.contains("nullable")));
    let raw = User::query().order_by(Order::raw("age * 2", ())).cursor_paginate(db, None, 2).await;
    assert!(matches!(raw, Err(Error::Config(_))));

    // A text primary key works too.
    for slug in ["c", "a", "e", "b", "d"] {
        Tag { slug: slug.into(), label: slug.to_uppercase(), uses: 0 }.insert(db).await.unwrap();
    }
    let mut after = None;
    let mut slugs = Vec::new();
    loop {
        let page = Tag::query().cursor_paginate(db, after.as_deref(), 2).await.unwrap();
        slugs.extend(page.items.into_iter().map(|t| t.slug));
        match page.next_cursor {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(slugs, ["a", "b", "c", "d", "e"]);
}

#[derive(Model, Clone, Debug, PartialEq)]
#[model(has_many(Task))]
struct Project {
    id: i64,
    #[model(tenant, index)]
    org_id: i64,
    #[model(unique)]
    code: String,
    name: String,
}

#[derive(Model, Clone, Debug, PartialEq)]
struct Task {
    id: i64,
    #[model(tenant)]
    org_id: i64,
    #[model(belongs_to = Project)]
    project_id: i64,
    title: String,
}

async fn tenant_scopes(db: &Db) {
    setup(db).await;
    db.create_table::<Project>().await.unwrap();
    db.create_table::<Task>().await.unwrap();
    let project = |org: i64, code: &str| Project { id: 0, org_id: org, code: code.into(), name: format!("project {}", code) };
    let task = |org: i64, project: &Project, title: &str| Task { id: 0, org_id: org, project_id: project.id, title: title.into() };
    fn scope_missing<T>(r: &Result<T, Error>) -> bool {
        matches!(r, Err(Error::TenantScope(_)))
    }
    fn wrong_tenant<T>(r: &Result<T, Error>) -> bool {
        matches!(r, Err(Error::WrongTenant(_)))
    }

    // A plain Db refuses tenant-scoped models, but not other models.
    assert!(scope_missing(&Project::query().all(db).await));
    assert!(scope_missing(&Project::find(db, 1).await));
    assert!(scope_missing(&Project::query().count(db).await));
    assert!(scope_missing(&project(1, "X").insert(db).await));
    assert!(scope_missing(&Project::query().delete(db).await));
    assert_eq!(User::query().count(db).await.unwrap(), 0);

    let (a, b, all) = (db.for_tenant(1i64), db.for_tenant(2i64), db.all_tenants());
    assert_eq!(a.tenant(), Some(&axeon_orm::Value::I64(1)));
    let pa = project(1, "A1").insert(&a).await.unwrap();
    let pa2 = project(1, "A2").insert(&a).await.unwrap();
    let pb = project(2, "B1").insert(&b).await.unwrap();
    // Writing another tenant's row is refused.
    assert!(wrong_tenant(&project(2, "B2").insert(&a).await));
    task(1, &pa, "mine").insert(&a).await.unwrap();
    task(2, &pb, "theirs").insert(&b).await.unwrap();
    // A row of org 2 pointing at org 1's project, to check relations.
    task(2, &pa, "foreign").insert(&all).await.unwrap();

    // Reads see only the tenant's rows.
    let codes = |ps: Vec<Project>| ps.into_iter().map(|p| p.code).collect::<Vec<_>>();
    assert_eq!(codes(Project::query().order_by(Project::CODE.asc()).all(&a).await.unwrap()), ["A1", "A2"]);
    assert_eq!(codes(Project::query().all(&b).await.unwrap()), ["B1"]);
    assert_eq!(Project::query().count(&all).await.unwrap(), 3);
    assert_eq!(Project::find(&a, pb.id).await.unwrap(), None);
    assert!(matches!(Project::find_or_fail(&a, pb.id).await, Err(Error::NotFound)));
    assert!(!Project::query().filter(Project::CODE.eq("B1")).exists(&a).await.unwrap());
    let page = Project::query().cursor_paginate(&a, None, 10).await.unwrap();
    assert_eq!(page.items.len(), 2);
    // The query-level opt-out works on any Db.
    assert_eq!(Project::query().all_tenants().count(db).await.unwrap(), 3);
    assert_eq!(Project::query().all_tenants().count(&a).await.unwrap(), 3);

    // Writes through instances can't touch other tenants' rows, even with
    // a forged tenant column.
    assert!(wrong_tenant(&pb.clone().update(&a).await));
    let mut forged = pb.clone();
    forged.org_id = 1;
    forged.name = "hijacked".into();
    assert!(matches!(forged.clone().update(&a).await, Err(Error::NotFound)));
    assert!(matches!(forged.delete(&a).await, Err(Error::NotFound)));
    assert_eq!(Project::find_or_fail(&b, pb.id).await.unwrap().name, "project B1");
    let mut renamed = pa.clone();
    renamed.name = "renamed".into();
    renamed.update(&a).await.unwrap();
    assert_eq!(Project::find_or_fail(&a, pa.id).await.unwrap().name, "renamed");

    // Bulk updates and deletes stay inside the tenant.
    assert_eq!(Project::query().update(&a, [Project::NAME.set("bulk")]).await.unwrap(), 2);
    assert_eq!(Project::find_or_fail(&b, pb.id).await.unwrap().name, "project B1");
    let moved = Project::query().update(&a, [Project::ORG_ID.set(2i64)]).await;
    assert!(wrong_tenant(&moved));
    assert_eq!(Project::query().filter(Project::CODE.like("%2")).delete(&a).await.unwrap(), 1);
    assert_eq!(Project::query().count(&all).await.unwrap(), 2);
    let _ = pa2;

    // Upserts on a global unique key never overwrite another tenant's row.
    let clash = Project { name: "stolen".into(), ..project(1, "B1") };
    assert!(wrong_tenant(&clash.clone().upsert(&a, OnConflict::on(Project::CODE)).await));
    assert!(wrong_tenant(&Project::upsert_many(&a, std::slice::from_ref(&clash), OnConflict::on(Project::CODE)).await));
    assert_eq!(clash.clone().insert_or_ignore(&a, OnConflict::on(Project::CODE)).await.unwrap(), None);
    assert_eq!(Project::find_or_fail(&b, pb.id).await.unwrap().name, "project B1");
    let own = Project { name: "upserted".into(), ..project(1, "A1") };
    assert_eq!(own.upsert(&a, OnConflict::on(Project::CODE)).await.unwrap().name, "upserted");

    // Relation filters and eager loading are scoped on both sides.
    let titles = |ts: &[Task]| ts.iter().map(|t| t.title.clone()).collect::<Vec<_>>();
    let with_foreign = Project::query().where_has(Project::TASKS, Task::TITLE.eq("foreign"));
    assert_eq!(with_foreign.clone().count(&a).await.unwrap(), 0);
    assert_eq!(with_foreign.all_tenants().count(&a).await.unwrap(), 1);
    let loaded = Project::query().with(Project::TASKS).all(&a).await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(titles(&loaded[0].1), ["mine"]);
    let loaded = Project::query().filter(Project::ID.eq(pa.id)).all_tenants().with(Project::TASKS).all(db).await.unwrap();
    let mut seen = titles(&loaded[0].1);
    seen.sort();
    assert_eq!(seen, ["foreign", "mine"]);
    assert_eq!(titles(&pa.tasks().all(&a).await.unwrap()), ["mine"]);

    // Joins filter the joined table too; a left join keeps the row.
    let joined = Task::query().join(Task::PROJECT).all(&b).await.unwrap();
    assert_eq!(joined.iter().map(|(t, _)| t.title.as_str()).collect::<Vec<_>>(), ["theirs"]);
    let left = Task::query().left_join(Task::PROJECT).order_by(Task::TITLE.asc()).all(&b).await.unwrap();
    let left: Vec<_> = left.iter().map(|(t, p)| (t.title.as_str(), p.as_ref().map(|p| p.code.as_str()))).collect();
    assert_eq!(left, [("foreign", None), ("theirs", Some("B1"))]);

    // Transactions keep the scope of the Db they began on.
    let mut tx = b.begin().await.unwrap();
    assert_eq!(codes(Project::query().all(&mut tx).await.unwrap()), ["B1"]);
    assert!(wrong_tenant(&project(1, "T").insert(&mut tx).await));
    tx.rollback().await.unwrap();
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

#[cfg(feature = "mysql")]
async fn mysql(name: &str) -> Option<Db> {
    let Ok(url) = std::env::var("AXEON_ORM_MYSQL_URL") else {
        eprintln!("skipping: AXEON_ORM_MYSQL_URL is not set");
        return None;
    };
    let database = format!("axeon_orm_{}", name);
    let admin = Db::connect(&url).await.unwrap();
    admin.execute(&format!("DROP DATABASE IF EXISTS {0}; CREATE DATABASE {0}", database)).await.unwrap();
    admin.close().await;
    let (base, _) = url.rsplit_once('/').unwrap();
    let db = Db::connect(&format!("{}/{}", base, database)).await.unwrap();
    assert_eq!(db.backend(), Backend::MySql);
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
        #[cfg(feature = "mysql")]
        mod mysql {
            $(#[tokio::test]
            async fn $name() {
                if let Some(db) = super::mysql(stringify!($name)).await {
                    super::$name(&db).await;
                }
            })*
        }
    };
}

backend_tests!(
    crud,
    queries,
    constraints,
    relations,
    types,
    transactions,
    migrations,
    many_to_many,
    aggregates,
    relation_filters_and_joins,
    raw_expressions,
    row_locks,
    enums,
    upserts,
    json_filters,
    bulk_inserts,
    cursor_pagination,
    tenant_scopes,
);

#[test]
fn enum_helpers() {
    assert_eq!(Status::VALUES, ["pending", "in_review", "o'k"]);
    assert_eq!(Priority::VALUES, ["LOW", "VERY_HIGH"]);
    assert_eq!(Status::InReview.as_str(), "in_review");
    assert_eq!(Priority::VeryHigh.to_string(), "VERY_HIGH");
    assert_eq!("o'k".parse::<Status>(), Ok(Status::Approved));
    let err = "Pending".parse::<Status>().unwrap_err();
    assert_eq!((err.value(), err.to_string().as_str()), ("Pending", r#""Pending" is not a Status value"#));

    let pg = schema::create_table::<Ticket>(Backend::Postgres);
    assert!(pg.contains(r#""status" TEXT NOT NULL CHECK ("status" IN ('pending', 'in_review', 'o''k'))"#), "{}", pg);
    assert!(pg.contains(r#""previous" TEXT CHECK ("previous" IN ('pending', 'in_review', 'o''k'))"#), "{}", pg);
    let my = schema::create_table::<Ticket>(Backend::MySql);
    assert!(my.contains("`status` VARCHAR(255) NOT NULL CHECK (`status` IN ('pending', 'in_review', 'o''k'))"), "{}", my);
    assert!(my.contains("`priority` TEXT NOT NULL CHECK (`priority` IN ('LOW', 'VERY_HIGH'))"), "{}", my);
}


#[test]
#[should_panic(expected = "has 2 placeholder(s) but 1 value(s)")]
fn raw_placeholder_count_must_match() {
    let _ = Expr::<User>::raw("age > ? AND age < ?", (1,));
}

#[test]
fn raw_literal_question_marks_are_not_placeholders() {
    let _ = Expr::<User>::raw("meta ?? 'a' AND name = ?", ("x",));
}

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

    let my = schema::create_table::<Post>(Backend::MySql);
    assert!(my.contains("`id` BIGINT AUTO_INCREMENT PRIMARY KEY"), "{}", my);
    assert!(my.contains("`slug` VARCHAR(255) NOT NULL"), "indexed text can't be TEXT: {}", my);
    assert!(my.contains("FOREIGN KEY (`user_id`) REFERENCES `users` (`id`) ON DELETE CASCADE"), "{}", my);
    assert!(my.contains("INDEX `posts_slug_index` (`slug`)"), "{}", my);
    assert!(!my.contains(" REFERENCES `users` (`id`) ON DELETE CASCADE,\n    `title`"), "no inline REFERENCES: {}", my);
    let my_user = schema::create_table::<User>(Backend::MySql);
    assert!(my_user.contains("`created_at` DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)"), "{}", my_user);
    assert!(my_user.contains("`bio` TEXT,"), "{}", my_user);

    let pivot = schema::create_pivot_table(&Post::TAGS, Backend::Postgres);
    assert!(pivot.contains(r#"CREATE TABLE IF NOT EXISTS "post_tag""#), "{}", pivot);
    assert!(pivot.contains(r#""post_id" BIGINT NOT NULL"#) && pivot.contains(r#""tag_id" TEXT NOT NULL"#), "{}", pivot);
    assert!(pivot.contains(r#"PRIMARY KEY ("post_id", "tag_id")"#), "{}", pivot);
    assert!(pivot.contains(r#"FOREIGN KEY ("tag_id") REFERENCES "tag_list" ("slug") ON DELETE CASCADE"#), "{}", pivot);
    assert_eq!((Tag::POSTS.pivot(), Tag::POSTS.foreign_key(), Tag::POSTS.related_key()), ("post_tag", "tag_id", "post_id"));
}

#[tokio::test]
async fn rejects_unknown_urls() {
    assert!(matches!(Db::connect("oracle://localhost/app").await, Err(Error::Config(_))));
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
