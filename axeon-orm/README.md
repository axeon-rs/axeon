# axeon-orm

An Eloquent-style ORM for Rust. Models are plain structs; you get typed
queries, relations (including many-to-many), aggregates, joins, migrations
and pagination for **PostgreSQL**, **MySQL** and **SQLite**, built on
[sqlx](https://github.com/launchbadge/sqlx), with first-class
[Axeon](https://github.com/axeon-rs/axeon) integration.

```toml
[dependencies]
axeon-orm = "0.2"
# MySQL is opt-in:
# axeon-orm = { version = "0.2", features = ["mysql"] }
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

`has_many`, `has_one`, `belongs_to` and `belongs_to_many` are supported.

### Many-to-many

```rust
#[derive(Model, Clone)]
#[model(belongs_to_many(Tag))]      // pivot table `post_tag` (post_id, tag_id)
struct Post { id: i64, title: String }

#[derive(Model, Clone)]
#[model(belongs_to_many(Post))]
struct Tag { id: i64, name: String }

db.create_pivot_table(&Post::TAGS).await?;           // or write it in a migration

Post::TAGS.attach(&db, &post, &rust).await?;         // linking twice is fine
Post::TAGS.sync(&db, &post, [rust.id, web.id]).await?;
Post::TAGS.detach(&db, &post, &web).await?;

let tags = post.tags().all(&db).await?;
let posts_with_tags = Post::query().with(Post::TAGS).all(&db).await?;
```

### Filtering by relations

```rust
let writers = User::query().has(User::POSTS).all(&db).await?;
let rustaceans = User::query()
    .where_has(User::POSTS, Post::TITLE.like("%Rust%"))
    .all(&db)
    .await?;
let untagged = Post::query().doesnt_have(Post::TAGS).count(&db).await?;
```

## Aggregates and joins

```rust
let total: Option<i64> = Order::query().sum(&db, Order::AMOUNT).await?;
let average: Option<f64> = Order::query().avg(&db, Order::AMOUNT).await?;
let latest = Order::query().max(&db, Order::CREATED_AT).await?;
let by_status: Vec<(String, u64)> = Order::query().count_by(&db, Order::STATUS).await?;

// Pairs from one query, filtered and sorted by either side.
let rows: Vec<(Post, User)> = Post::query()
    .join(Post::USER)
    .filter_related(User::ACTIVE.eq(true))
    .order_by_related(User::NAME.asc())
    .all(&db)
    .await?;

// Keep users without a profile.
let rows: Vec<(User, Option<Profile>)> = User::query().left_join(User::PROFILE).all(&db).await?;
```

## Raw SQL in queries

For what typed columns can't express (JSONB, full-text search, pgvector,
functions), write a condition or ordering in SQL and keep the rest typed.
Each `?` binds the next value (a tuple, array or `Vec`; `()` for none),
and `??` is a literal `?`, as in PostgreSQL's JSONB `?` operator:

```rust
use axeon_orm::{Expr, Order};

let products = Product::query()
    .filter(Product::CATEGORY.eq("lighting"))
    .filter(Expr::raw("fields @> ?", (json!({ "certs": ["CE"] }),)))
    .filter(Expr::raw("fields ?? 'moq'", ()))
    .order_by(Order::raw("embedding <=> ?::vector", ("[0.12,0.5,0.33]",)))
    .limit(20)
    .all(&db)
    .await?;
```

Values are always bound as parameters, never spliced into the SQL; don't
build the SQL text from user input. Fragments are parenthesized, so an
`OR` inside one can't escape it. Column names inside them aren't quoted or
qualified; in a join, the main table is `t0` and the joined one `t1`.

## Transactions

```rust
let mut tx = db.begin().await?;
let user = new_user.insert(&mut tx).await?;
new_post(&user).insert(&mut tx).await?;
tx.commit().await?;     // dropping `tx` instead rolls back
```

### Row locks

Inside a transaction, `for_update()` locks the rows a query reads until
commit. `skip_locked()` leaves out rows other transactions have locked, so
several workers can claim different jobs from one table, and `nowait()`
fails at once with `Error::LockNotAvailable` instead of waiting:

```rust
let mut tx = db.begin().await?;
if let Some(job) = Job::query()
    .filter(Job::STATUS.eq("pending"))
    .order_by(Job::ID.asc())
    .skip_locked()
    .first(&mut tx)
    .await?
{
    Job::query().filter(Job::ID.eq(job.id)).update(&mut tx, [Job::STATUS.set("running")]).await?;
}
tx.commit().await?;
```

Joins lock only the main table's rows; `count`, `exists` and aggregates
don't lock. SQLite has no row locks (a write transaction locks the whole
database), so these are left out there.

## LISTEN / NOTIFY

On PostgreSQL, send lightweight messages between connections, to wake
workers or fan events out to every server:

```rust
let mut listener = db.listen(["jobs"]).await?;   // holds one pool connection

let mut tx = db.begin().await?;
// ... insert the job ...
tx.notify("jobs", &job.id.to_string()).await?;   // delivered on commit only
tx.commit().await?;

let message = listener.recv().await?;            // or listener.into_stream()
println!("{} on {}", message.payload, message.channel);
```

Messages aren't stored, so a listener only hears what is sent while it's
connected. It reconnects by itself; `try_recv()` returns `None` when the
connection dropped, so you can re-read what you might have missed.

## Migrations

Put SQL files named `<version>_<name>.sql` in `migrations/`, then:

```rust
axeon_orm::embed_migrations!().run(&db).await?;
```

Pending migrations run in one transaction, are recorded with a checksum,
and take a lock so only one instance migrates at a time. (MySQL commits
each schema change on its own, so keep MySQL migrations to one change per
file.) For prototypes and tests, `db.create_table::<User>()` creates
tables straight from the models, and
`schema::create_table::<User>(Backend::Postgres)` prints the SQL to save as
a migration.

The command-line tool creates, applies and lists migrations:

```bash
cargo install axeon-orm-cli
axeon-orm migrate new create_users   # migrations/20261018120000_create_users.sql
axeon-orm migrate run                # uses DATABASE_URL, also read from .env
axeon-orm migrate status
```

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
| `mysql` | no | MySQL 8 (MariaDB 10.5+ should work but isn't tested) |
| `tls` | yes | Encrypted connections; needed by most managed databases and MySQL 8's default login |
| `chrono` | yes | Date and time columns, `created_at` / `updated_at` |
| `json` | yes | JSON columns (`serde_json::Value`, `sqlx::types::Json<T>`) |
| `axeon` | yes | Axeon integration |
| `uuid` | no | `Uuid` columns |

## Not yet

Planned for later versions: composite primary keys, extra columns on pivot
tables, `GROUP BY` beyond `count_by`, joins across more than two tables,
and down migrations. For anything
the ORM can't express, use sqlx directly via `db.postgres()` /
`db.sqlite()`.

## Testing

SQLite tests always run. For PostgreSQL and MySQL, start the databases
and point the tests at them:

```bash
docker compose -f axeon-orm/docker-compose.yml up -d
export AXEON_ORM_POSTGRES_URL=postgres://postgres:postgres@localhost:55432/postgres
export AXEON_ORM_MYSQL_URL=mysql://root:mysql@localhost:53306/axeon
cargo test -p axeon-orm -p axeon-orm-cli --all-features
```

## License

MIT
