# axeon-orm

An Eloquent-style ORM for Rust. Models are plain structs; you get typed
queries, relations, migrations and pagination for **PostgreSQL** and
**SQLite**, built on [sqlx](https://github.com/launchbadge/sqlx), with
first-class [Axeon](https://github.com/axeon-rs/axeon) integration.

```toml
[dependencies]
axeon-orm = "0.1"
```

> **Status: 0.x.** The API may change between minor versions while it
> settles. Feedback is very welcome.

## Models

```rust
use axeon_orm::{Db, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone, Debug)]
#[model(has_many(Post))]
struct User {
    id: i64,
    name: String,
    #[model(unique)]
    email: String,
    bio: Option<String>,            // nullable
    #[model(created_at)]
    created_at: DateTime<Utc>,      // set on insert
}

#[derive(Model, Clone, Debug)]
struct Post {
    id: i64,
    #[model(belongs_to = User, on_delete = "cascade")]
    user_id: i64,
    title: String,
    published: bool,
}
```

The table is the struct name in snake case, pluralized (`users`, `posts`),
and the primary key is `id` unless you say otherwise. Integer keys are
assigned by the database.

## Queries

```rust
let db = Db::connect("postgres://localhost/blog").await?;

// Insert returns the stored row with its new id.
let ada = User { id: 0, name: "Ada".into(), email: "ada@example.com".into(),
                 bio: None, created_at: Utc::now() }.insert(&db).await?;

let user = User::find(&db, ada.id).await?;            // Option<User>
let user = User::find_or_fail(&db, ada.id).await?;    // Error::NotFound if missing

let recent = Post::query()
    .filter(Post::PUBLISHED.eq(true) & Post::TITLE.like("%rust%"))
    .order_by(Post::ID.desc())
    .limit(10)
    .all(&db)
    .await?;

let page = Post::query().paginate(&db, 2, 20).await?;   // items, total, last_page
let n = Post::query().filter(Post::USER_ID.eq(ada.id)).count(&db).await?;

let mut user = user;
user.bio = Some("Mathematician".into());
user.update(&db).await?;                                  // saves and reloads
Post::query().filter(Post::PUBLISHED.eq(false)).delete(&db).await?;
```

Columns are typed constants, so `User::EMAIL.eq(42)` or filtering users by
a post column doesn't compile.

## Relations

```rust
let posts = ada.posts().filter(Post::PUBLISHED.eq(true)).all(&db).await?;
let author = posts[0].user().first(&db).await?;

// Eager loading: two queries in total, not one per user.
for (user, posts) in User::query().with(User::POSTS).all(&db).await? {
    println!("{}: {} posts", user.name, posts.len());
}
```

`has_many`, `has_one` and `belongs_to` are supported.

## Transactions

```rust
let mut tx = db.begin().await?;
let user = new_user.insert(&mut tx).await?;
new_post(&user).insert(&mut tx).await?;
tx.commit().await?;     // dropping `tx` instead rolls back
```

## Migrations

Put SQL files named `<version>_<name>.sql` in `migrations/`, then:

```rust
axeon_orm::embed_migrations!().run(&db).await?;
```

Pending migrations run in one transaction, are recorded with a checksum,
and on PostgreSQL take a lock so only one instance migrates at a time. For
prototypes and tests, `db.create_table::<User>()` creates tables straight
from the models, and `schema::create_table::<User>(Backend::Postgres)`
prints the SQL to save as a migration.

## With Axeon

```rust
use axeon::{Json, Server, ServerError};
use axeon::health::Health;
use axeon_orm::prelude::*;

let mut app = Server::new();
app.state(db.clone());
app.mount("/", Health::new().database(&db).router());   // /ready pings the database
app.get("/users/:id", |req| async move {
    // Missing rows become 404, unique violations 409.
    let user = User::find_or_fail(req.db()?, req.param::<i64>("id")?).await?;
    Ok::<_, ServerError>(Json(user))
});

// Or one transaction per request, committed on success:
app.middleware(Transactional::new(db));
app.post("/transfer", |req| async move {
    let mut tx = req.tx()?;
    // ... queries with &mut *tx
    Ok::<_, ServerError>("done")
});
```

[`examples/todo-api`](../examples/todo-api) is a complete application.

## Features

| Feature | Default | Enables |
|---|---|---|
| `postgres` | yes | PostgreSQL |
| `sqlite` | yes | SQLite |
| `chrono` | yes | Date and time columns, `created_at` / `updated_at` |
| `json` | yes | JSON columns (`serde_json::Value`, `sqlx::types::Json<T>`) |
| `axeon` | yes | Axeon integration |
| `uuid` | no | `Uuid` columns |

## Not yet

Planned for later versions: MySQL, many-to-many relations, composite
primary keys, aggregate and join queries, and a migration CLI. For anything
the ORM can't express, use sqlx directly via `db.postgres()` /
`db.sqlite()`.

## Testing

SQLite tests always run. For PostgreSQL, start a database and point the
tests at it:

```bash
docker compose -f axeon-orm/docker-compose.yml up -d
AXEON_ORM_POSTGRES_URL=postgres://postgres:postgres@localhost:55432/postgres cargo test -p axeon-orm --all-features
```

## License

MIT
