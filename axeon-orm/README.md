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

### Enums

Derive `SqlEnum` on a fieldless enum to use it as a column. It's stored as
text (the variant in snake case unless renamed), so it works the same on
every database, and generated tables get a `CHECK` listing the values:

```rust
use axeon_orm::SqlEnum;

#[derive(SqlEnum, Clone, Copy, Debug, PartialEq)]
enum Status {
    Pending,                    // "pending"
    InReview,                   // "in_review"
    #[sql_enum(rename = "ok")]
    Approved,                   // "ok"
}

#[derive(Model)]
struct Case { id: i64, status: Status, previous: Option<Status> }

let open = Case::query()
    .filter(Case::STATUS.is_in([Status::Pending, Status::InReview]))
    .all(&db)
    .await?;
```

`#[sql_enum(rename_all = "SCREAMING_SNAKE_CASE")]` (or `kebab-case`,
`lowercase`, `UPPERCASE`, `camelCase`, `PascalCase`) changes the case. You
also get `Status::VALUES`, `as_str()`, `Display` and `FromStr`. A row
holding a value the enum doesn't know is a decode error, not a panic.
Native PostgreSQL `ENUM` types aren't supported; use `TEXT`.

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
// Keyset pagination for public APIs: no OFFSET or COUNT(*), opaque cursor.
let page = Post::query().order_by(Post::ID.desc()).cursor_paginate(&db, after.as_deref(), 20).await?;
// page.items, page.next_cursor (None on the last page)

// Whole tables in constant memory (reindexing, exports), e.g. in batches:
let mut batches = Product::query().stream(&db).try_chunks(500);   // futures::TryStreamExt
let n = Post::query().filter(Post::USER_ID.eq(ada.id)).count(&db).await?;

let mut user = user;
user.bio = Some("Mathematician".into());
user.update(&db).await?;                                  // saves and reloads
Post::query().filter(Post::PUBLISHED.eq(false)).delete(&db).await?;
```

Columns are typed constants, so `User::EMAIL.eq(42)` or filtering users by
a post column doesn't compile.

### Bulk inserts

Insert many rows with a few statements instead of one per row (about 9×
faster on a local PostgreSQL for 2500 rows, more over a network):

```rust
let stored: Vec<Product> = Product::insert_many(&db, &products).await?;   // with IDs, in input order
let synced = Product::upsert_many(&db, &products, OnConflict::on(Product::SKU)).await?;
```

Rows are sent up to 1000 per statement, within each database's parameter
limit. With `&db`, several statements run in one transaction, so a failure
inserts nothing; with `&mut tx`, they're part of yours. A conflict key may
appear only once per `upsert_many` call; that's checked before anything is
written.

### Upserts

Insert a row, or update the one that already has the same unique columns,
in one statement, and get the stored row back:

```rust
use axeon_orm::OnConflict;

let product = Product { id: 0, sku: "L-1".into(), name: "Lamp".into(), price: 120, .. }
    .upsert(&db, OnConflict::on(Product::SKU))
    .await?;

// Only overwrite some columns; conflict on several columns together.
row.upsert(&db, OnConflict::on(Stock::WAREHOUSE).and(Stock::SKU).update(Stock::QTY)).await?;

// Insert unless it exists: `None` when it was already there.
let inserted: Option<Event> = event.insert_or_ignore(&db, OnConflict::on(Event::EXTERNAL_ID)).await?;
```

By default an upsert overwrites every column except the primary key, the
conflict columns and `created_at`. The conflict columns need a unique
constraint or index. MySQL ignores them and reacts to a conflict on any
unique key of the table.

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

### Composite keys and pivot tables with data

Mark several fields `#[model(primary_key)]` for a composite key. That also
covers pivot rows that carry data, such as a margin on a buyer's
selection: make the pivot table a model.

```rust
#[derive(Model)]
#[model(table = "selections")]
struct Selection {
    #[model(primary_key, belongs_to = Buyer)]
    buyer_id: i64,
    #[model(primary_key, belongs_to = Product)]
    product_id: i64,
    margin: f64,
}

let s = Selection::find_or_fail(&db, (buyer.id, product.id)).await?;   // find by the tuple
let rows: Vec<(Selection, Product)> = Selection::query()
    .filter(Selection::BUYER_ID.eq(buyer.id))
    .join(Selection::PRODUCT)                                           // pivot data + the product
    .all(&db)
    .await?;
```

Insert, update, delete, upserts (`OnConflict::on(..).and(..)`), bulk
inserts and cursor pagination use every key column. A composite-key model
can `belongs_to` others, but can't have `has_many`, `has_one` or
`belongs_to_many` relations, and can't be the target of a `belongs_to`
(both are compile errors).

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

### Reports: more tables, GROUP BY and HAVING

`select::Select` joins any number of tables through their relations and
decodes each row into your own `Deserialize` struct (or JSON):

```rust
use axeon_orm::select::{Agg, Select};

#[derive(Deserialize)]
struct Row { supplier: String, category: String, products: i64, avg_price: Option<f64> }

let rows: Vec<Row> = Select::from::<Supplier>()
    .join(Supplier::PRODUCTS)                 // from any table already in the report
    .join(Product::CATEGORY)
    .left_join(Product::REVIEWS)              // keep products without reviews
    .column_as(Supplier::NAME, "supplier")
    .column_as(Category::NAME, "category")
    .aggregate(Agg::count_distinct(Product::ID), "products")
    .aggregate(Agg::avg(Product::PRICE), "avg_price")
    .filter(Supplier::ACTIVE.eq(true))        // WHERE, on any joined model
    .group_by(Supplier::NAME)
    .group_by(Category::NAME)
    .having(Agg::count().gte(3))              // HAVING
    .order_by_alias("products", true)
    .limit(50)
    .fetch(&db)                               // or .fetch_json(&db) for a dashboard
    .await?;
```

- Aggregates: `count`, `count_of`, `count_distinct`, `sum` (`i64` or
  `f64`), `avg` (`f64`), `min` and `max` (the column's type); read
  nullable results (left joins, empty groups) as `Option`.
- Each model appears once (no self-joins); mistakes such as a column of a
  model that isn't joined fail with `Error::Config` when the report runs.
- Tenant scopes apply to every table (in `ON` for left joins), and
  `to_sql` shows the SQL for `EXPLAIN`.

## Full-text and hybrid search (PostgreSQL)

```rust
use axeon_orm::search::{self, FullText};

let fts = FullText::columns([Product::NAME, Product::DESCRIPTION]).config("english");
db.execute(&search::create_index(&fts, "products_fts")).await?;   // GIN, matching the queries

// Web-search syntax: words, "quoted phrases", OR, -excluded.
let found = Product::query()
    .filter(fts.matches("led lamp -solar"))
    .order_by(fts.rank("led lamp -solar").desc())
    .all(&db)
    .await?;

// Hybrid: text rank and vector similarity in one score, best first.
let q = "led lamp";
let score = fts.rank(q) * 0.6 + Product::EMBEDDING.cosine_distance(&v).similarity() * 0.4;
let top: Vec<(Product, f64)> = Product::query()
    .filter(fts.matches(q) | Product::EMBEDDING.cosine_distance(&v).lt(0.5))
    .ranked(&db, score, 20)
    .await?;
```

Ranks are normalized to 0..1 so they mix with similarities. Search a
stored, generated `tsvector` column instead with `FullText::stored("search")`
and `search::generated_column(...)`. `Query::to_sql` shows the SQL, e.g. to
`EXPLAIN` it. PostgreSQL has no Chinese word splitting of its own (`simple`
treats a run of Chinese characters as one word): install zhparser or
pg_jieba and pass its configuration to `.config(...)`.

## Vector search (pgvector)

With the `pgvector` feature and the extension in PostgreSQL
(`CREATE EXTENSION vector`):

```rust
use axeon_orm::vector::{self, Vector, VectorIndex};

#[derive(Model)]
struct Product {
    id: i64,
    category: String,
    #[model(dimensions = 1024)]
    embedding: Vector,
}

db.execute(&vector::create_index::<Product>("embedding", VectorIndex::hnsw_cosine())).await?;

// The 10 nearest in a category, with their distances.
let hits: Vec<(Product, f64)> = Product::query()
    .filter(Product::CATEGORY.eq("lighting"))
    .nearest(&db, Product::EMBEDDING.cosine_distance(&query), 10)
    .await?;

// Distances also sort and filter ordinary queries.
Product::query().filter(Product::EMBEDDING.cosine_distance(&query).lt(0.3))
    .order_by(Product::EMBEDDING.cosine_distance(&query).asc());
```

`cosine_distance` (`<=>`), `l2_distance` (`<->`) and `inner_product`
(`<#>`, negated so smaller is closer). Vectors travel in pgvector's binary
format. HNSW (or IVFFlat) indexes make nearest-neighbour queries
approximate and fast; filters on other columns apply to the index's
candidates, so with very selective filters raise `hnsw.ef_search`.

## JSON filters

Filter and sort by values inside JSON columns (`serde_json::Value` or
`Json<T>`; `JSONB` in PostgreSQL), on every database:

```rust
let found = Product::query()
    .filter(Product::FIELDS.contains(json!({ "certs": ["CE"] })))   // @> (GIN index)
    .filter(Product::FIELDS.has_key("moq"))                         // ?  (GIN index)
    .filter(Product::FIELDS.key("specs").key("color").text_in(["red", "blue"]))
    .filter(Product::FIELDS.key("price").key("min").lte(25))
    .filter(Product::FIELDS.key("sizes").index(0).exists())
    .order_by(Product::FIELDS.key("price").key("min").asc())
    .all(&db)
    .await?;
```

Paths also have `text_eq`, `text_ne`, `text_like`, `num_eq`, `gt`, `gte`,
`lt` and `bool_eq`. Text comparisons only match JSON strings and number
comparisons only JSON numbers, so `"n/a"` in a price field doesn't match
rather than failing the query. Conditions are never NULL, so `!` gives
exactly the other rows. Keys are bound as parameters. `contains` needs
PostgreSQL or MySQL; SQLite has no JSON containment.

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

## Optimistic locking

Mark an integer column with `#[model(version)]` to stop concurrent edits
from overwriting each other silently:

```rust
#[derive(Model)]
struct ProductVersion {
    id: i64,
    title: String,
    #[model(version)]
    version: i64,
}

let mut mine = ProductVersion::find_or_fail(&db, id).await?;
mine.title = "New title".into();
match mine.update(&db).await {                  // UPDATE ... WHERE id = $1 AND version = $2
    Ok(()) => {}                                // mine.version is now one higher
    Err(Error::StaleRecord(_)) => { /* someone saved first: reload, merge, retry */ }
    Err(other) => return Err(other),
}
```

- `update` and `delete` on an instance apply only while the stored version
  equals the loaded one; otherwise they fail with `Error::StaleRecord`
  (409 in Axeon handlers). A row that no longer exists is still `NotFound`.
- `update` increments the version and reloads it into the struct.
  `Query::update` and upserts increment it too, so copies loaded before
  them are stale (`Query::update` leaves it alone if you assign it).

## Tenant scopes

Mark the column that says which tenant (organization, account, workspace)
a row belongs to. Such a model only runs through a `Db` that names the
tenant, or one that opts out explicitly, so a forgotten `org_id` filter is
an error instead of a data leak.

```rust
#[derive(Model)]
struct Selection {
    id: i64,
    #[model(tenant, index)]
    org_id: i64,
    product_id: i64,
}

let org = db.for_tenant(org_id);                       // cheap; shares the pool
let mine = Selection::query().all(&org).await?;        // WHERE org_id = $1
Selection::query().all(&db).await;                     // Err(Error::TenantScope)

let everyone = db.all_tenants();                       // jobs, operator tools
let n = Selection::query().count(&everyone).await?;
let n = Selection::query().all_tenants().count(&org).await?; // per query
```

- Every read, aggregate, update and delete gets `org_id = <tenant>`, as do
  `find`, eager loading (`with`), relation accessors, `has` / `where_has`
  subqueries and both sides of joins (a left join keeps rows without an
  allowed match).
- Writes check the row: inserting, updating or bulk-inserting a row of
  another tenant fails with `Error::WrongTenant` (403 in Axeon), as does
  `update` setting the tenant column to another value. `update` and
  `delete` on an instance also require the *stored* row to be the
  tenant's, so a forged `org_id` gets `NotFound`.
- Upserts never overwrite another tenant's row that conflicts on a global
  unique key: `upsert` and `upsert_many` fail with `WrongTenant`, and
  `insert_or_ignore` returns `None`.
- Transactions keep the scope of the `Db` they began on.
- Not covered: raw SQL (`db.execute`, `db.postgres()`, `Expr::raw` still
  gets the filter added beside it), pivot tables, and models without a
  tenant column.

## Read replicas

```rust
let db = Db::connect(&primary_url).await?.with_replicas(vec![
    Db::connect(&replica_1_url).await?,
    Db::connect(&replica_2_url).await?,
]);

// Reads that tolerate replication lag go to a replica, in turn:
let products = Product::query().filter(Product::CATEGORY.eq("lighting")).all(&db.replica()).await?;
// Writes, and reads right after them, stay on `db`.
```

- `db.replica()` is a handle like any `Db` (queries, `find`, streams,
  reports, cursors), with the same tenant scope and metrics; without
  replicas it reads the primary, so the same code runs everywhere.
- Writes through it (and through transactions begun on it) fail with
  `Error::Config` before reaching the replica.
- Routing is explicit because only the app knows which reads can be a
  little stale.

## Query observability

```rust
let db = Db::connect_with(&url, DbOptions::new()
    .metrics(&metrics)                              // on Axeon's /metrics
    .slow_query(Duration::from_millis(200))         // warn! with the SQL, never its values
    .statement_timeout(Duration::from_secs(5)))     // the database cancels longer statements
    .await?;

// A tighter limit for one transaction (PostgreSQL):
let mut tx = db.begin().await?;
tx.statement_timeout(Duration::from_millis(300)).await?;
```

- `db_query_duration_seconds` (histogram) and `db_query_errors_total`,
  labelled by `operation` (`select`, `insert`, `update`, `delete`) and
  `table`, cover every statement except streamed ones.
- Slow statements are logged at `warn` with target
  `axeon_orm::slow_query`: duration, operation, table, a fingerprint for
  grouping, and the SQL with placeholders.
- A cancelled statement fails with `Error::StatementTimeout` (503 in Axeon
  handlers). PostgreSQL applies the timeout to every statement; MySQL's
  `max_execution_time` covers only `SELECT`s; SQLite has none.

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

## Partitioned tables (PostgreSQL)

For fast-growing, append-mostly tables (audit events, LLM call logs), one
partition per month or day keeps recent queries fast and makes retention
a `DROP` instead of a huge `DELETE`:

```rust
use axeon_orm::partition::Partitioning;

let audit = Partitioning::monthly(AuditEvent::CREATED_AT);     // or ::daily
audit.create_table::<AuditEvent>(&db).await?;                   // or save create_table_sql() as a migration

// Daily, from a scheduled job:
audit.ensure(&db, 3).await?;                                    // this month and the next 3
audit.drop_before(&db, Utc::now() - Duration::days(400)).await?;
```

- The model is used as usual. Rows whose time has no partition can't be
  inserted, so keep partitions created ahead with `ensure`.
- PostgreSQL requires the primary key to include the partition column,
  so the table's key is `(id, created_at)`; ids still come from one
  sequence. Other unique columns are refused, and upserts on `id` alone
  don't work on these tables.
- Partition bounds are UTC whatever the session time zone; `partitions()`
  lists them, and `#[model(index)]` indexes reach every partition.

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

// Idempotency-Key records in PostgreSQL (prune them from a scheduled job):
axeon_orm::idempotency::install(&db).await?;
app.middleware(Idempotency::new().store(PgIdempotencyStore::new(db.clone())));

// Scope req.db() and transactions to the caller's tenant (after auth):
app.middleware(TenantScope::new(|req| req.extension::<Member>().map(|m| m.org_id)));

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
| `testing` | no | Test helpers (`axeon_orm::testing`); enable it in `[dev-dependencies]` |

## Not yet

Planned for later versions: relations from composite-key models,
self-joins in reports, and down migrations. For anything
the ORM can't express, use sqlx directly via `db.postgres()` /
`db.sqlite()`.

## Testing your app

With the `testing` feature (in `[dev-dependencies]`), `axeon_orm::testing`
gives each test its own database, and builds test records:

```rust
use axeon_orm::testing::{rollback, Factory, TestDb};

impl Factory for User {
    fn definition(n: u64) -> Self {
        User { id: 0, name: format!("User {n}"), email: format!("user{n}@example.com"), ..Default::default() }
    }
}

#[tokio::test]
async fn deactivates_users() -> axeon_orm::Result<()> {
    let db = TestDb::new().await?;                 // own schema on $AXEON_TEST_DATABASE_URL, else SQLite
    Migrator::from_dir("migrations")?.run(&db).await?;

    let ada = User::factory().with(|u| u.name = "Ada".into()).create(&*db).await?;
    let others = User::factory().count(20).create_many(&*db).await?;   // one bulk insert

    // Or keep a test's writes from sticking at all:
    rollback(&db, async |tx| { /* ... queries with &mut *tx ... */ Ok(()) }).await?;

    db.cleanup().await                             // drops the schema
}
```

- `TestDb` makes a PostgreSQL schema or MySQL database per test (or an
  in-memory SQLite database), so tests run in parallel against a real
  server without seeing each other's rows. A test that panics before
  `cleanup` still has its schema dropped when the `TestDb` is dropped.
- Factory sequence numbers count up per model across the test process,
  so unique columns built from them don't collide.

## Testing axeon-orm

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
