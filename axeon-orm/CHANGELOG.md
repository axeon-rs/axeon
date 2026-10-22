# Changelog

## Unreleased

### Added

- **Keyset pagination:** `Query::cursor_paginate(ex, after, limit)` returns
  a `CursorPage` with `items` and an opaque, URL-safe `next_cursor`. No
  `OFFSET` or `COUNT(*)`, so deep pages cost the same as the first. Orders
  by the query's `order_by` columns (any mix of directions) plus the primary
  key as tie-breaker; cursors from another sort order, or damaged ones, are
  rejected with the new `Error::InvalidCursor` (400 in Axeon handlers).

- **Tenant scopes:** `#[model(tenant)]` marks a model's tenant column.
  Such models only run through `Db::for_tenant(id)` (rows of that tenant;
  writes of other tenants' rows fail with the new `Error::WrongTenant`,
  403 in Axeon) or an explicit opt-out, `Db::all_tenants()` or
  `Query::all_tenants()`; a plain `Db` fails with the new
  `Error::TenantScope`. Covers queries, aggregates, bulk updates and
  deletes, instance writes, upserts (never overwriting another tenant's
  conflicting row), eager loading, relation filters and joins;
  transactions inherit the scope. `server::TenantScope` middleware scopes
  `req.db()` and `Transactional` to the request's tenant.
  `Model::TENANT` defaults to `None`, so hand-written `Model` impls keep
  compiling.

- **Idempotency store:** `idempotency::PgIdempotencyStore` (features
  `axeon` and `postgres`) keeps Axeon `Idempotency-Key` records in an
  `axeon_idempotency` table (`install`, `migration`, `prune`).

### Changed

- The `tls` feature uses rustls' aws-lc-rs provider
  (`sqlx/tls-rustls-aws-lc-rs`) instead of ring, like Axeon and
  axeon-client. Apps combining them no longer compile in two providers,
  which made `Server::with_tls` and Redis TLS panic.
- `serde_json` is always a dependency (cursors are JSON); the `json`
  feature still controls JSON columns.

## 0.3.0 (2026-10-22)

Additive: everything from 0.2 keeps working. The minor version marks the size
of the release and moves `axeon-orm-macros` and `axeon-orm-cli` in step.
Requires Axeon 1.1 with the `axeon` feature (for the broadcast backend).

### Added

- **Full-text search** (PostgreSQL): `search::FullText` over text columns
  or a stored `tsvector`, `matches` (`websearch_to_tsquery` syntax) and
  `rank` (normalized `ts_rank`), `search::create_index` (a GIN index on
  the exact expression queries use) and `search::generated_column`.
  Ranks and vector `similarity()` combine into weighted `Score`s for
  hybrid search, returned with `Query::ranked(&db, score, k)`.
  `Query::to_sql` shows a query's SQL and parameters.
- **pgvector** (feature `pgvector`): `vector::Vector` columns with
  `#[model(dimensions = n)]`, `cosine_distance` / `l2_distance` /
  `inner_product` for ordering (`asc`/`desc`) and filtering (`lt`/`lte`),
  `Query::nearest(&db, distance, k)` returning rows with their distances,
  and `vector::create_index` for HNSW and IVFFlat indexes. Vectors use
  pgvector's binary format; no extra dependency.
- **Bulk inserts:** `Model::insert_many(&db, &rows)` and
  `Model::upsert_many(&db, &rows, OnConflict::on(..))` send multi-row
  `INSERT`s (up to 1000 rows each, under the parameter limit) and return
  the stored rows in input order, on all three databases. Several
  statements on a pool share one transaction. Duplicate conflict keys in
  one `upsert_many` are refused before writing.
- **JSON filters** (feature `json`): on `serde_json::Value` and
  `Json<T>` columns, `contains` (PostgreSQL `@>`, MySQL `JSON_CONTAINS`)
  and `has_key` (`?`), both able to use a GIN index, and paths with
  `key(..)` / `index(..)` for `exists`, `text_eq` / `text_ne` / `text_like`
  / `text_in`, `num_eq` / `gt` / `gte` / `lt` / `lte`, `bool_eq`, and
  `asc` / `desc` sorting. Works on PostgreSQL, MySQL and SQLite
  (`contains` excepted on SQLite). Typed comparisons skip values of other
  JSON types, conditions are never NULL, and path keys are bound.
- **Raw SQL in queries:** `Expr::raw(sql, args)` for conditions and
  `Order::raw` / `Order::raw_desc` for orderings, combined with typed
  filters, relation filters and joins. `?` placeholders are bound to
  values (`RawArgs`: `()`, tuples of up to eight, arrays, `Vec`), numbered
  `$n` on PostgreSQL, and `??` is a literal `?` for PostgreSQL's JSONB
  operators. A placeholder/value count mismatch panics.
- **Row locks:** `for_update`, `skip_locked` and `nowait` on queries and
  joins (`FOR UPDATE [SKIP LOCKED | NOWAIT]`; joins lock only the main
  table). Left out on SQLite, which has no row locks. A `nowait` conflict
  is the new `Error::LockNotAvailable`.
- **LISTEN / NOTIFY** (PostgreSQL): `Db::notify` and `Transaction::notify`
  (sent on commit only), and `Db::listen` returning a `notify::Listener`
  with `listen`, `unlisten`, `recv`, `try_recv` (`None` after a dropped
  connection, so callers can catch up) and `into_stream`. Reconnects by
  itself. Other databases return `Error::Config`.
- **Enums:** `#[derive(SqlEnum)]` makes a fieldless enum a column type,
  stored as text (snake case by default; `rename_all` and per-variant
  `rename`). Generated tables get a `CHECK` constraint with the values,
  and unknown values read from the database are an `Error::Decode`. The
  derive also adds `VALUES`, `as_str()`, `Display` and `FromStr` (with
  the new `UnknownVariant` error). `SqlType` gains a `VALUES` constant
  and `ColumnDef` a `values` field.
- **Upserts:** `model.upsert(&db, OnConflict::on(Product::SKU))` inserts
  or updates the row with the same unique columns and returns the stored
  row; `.and(column)` adds conflict columns and `.update(column)` limits
  what is overwritten (by default all but the primary key, the conflict
  columns and `created_at`). `model.insert_or_ignore(..)` returns `None`
  when the row exists. `ON CONFLICT` on PostgreSQL and SQLite, `ON
  DUPLICATE KEY UPDATE` on MySQL (8.0.19+), where an ignored insert
  doesn't fail the transaction.
- **Broadcast backend** (features `axeon` + `postgres`):
  `broadcast::PgBroadcast` for Axeon's `broadcast::Hub`, with messages in
  an `axeon_broadcast` table and `LISTEN`/`NOTIFY` wake-ups;
  `broadcast::publish_in(&mut tx, ..)` sends on commit only. Per-topic IDs
  follow commit order, so readers never skip a message committed late.
  `PgBroadcast::prune(age)` trims the log.
- `Transaction::postgres`, `sqlite` and `mysql`: the underlying sqlx
  connection, for raw queries inside an ORM transaction.
- `From` conversions into `Value` for `serde_json::Value`, `Uuid` and the
  chrono types.

### Changed

- New `pgvector` feature (off by default) for vector columns.
- `Error` gains `LockNotAvailable` and `SqlType` gains a defaulted `VALUES`
  constant; both enums are `#[non_exhaustive]`, so existing code keeps
  compiling.

## 0.2.0 (2026-10-18)

### Added

- **Many-to-many relations:** `#[model(belongs_to_many(Tag))]` gives
  `Post::TAGS` and `post.tags()`, through a pivot table (`post_tag` with
  `post_id` and `tag_id` by default; all configurable). `attach`,
  `attach_key`, `detach`, `detach_all` and `sync` manage links (linking
  twice is not an error), `.with(Post::TAGS)` eager-loads them, and
  `Db::create_pivot_table` / `schema::create_pivot_table` create the table.
- **Aggregates:** `sum`, `avg`, `min`, `max` and `count_by` on queries.
  Results are cast so every database returns the same Rust type (`sum` of
  integers is `i64`, `avg` is `f64`).
- **Filtering by relations:** `has`, `doesnt_have`, `where_has` and
  `where_doesnt_have`, for every relation type.
- **Joins:** `join` and `left_join` over `belongs_to`, `has_one` and
  `has_many`, returning `(Model, Related)` or `(Model, Option<Related>)`
  pairs from one query, with `filter_related` and `order_by_related`.
- **MySQL** (feature `mysql`): MySQL 8, including inserts without
  `RETURNING`, MySQL-specific table definitions, and `GET_LOCK` around
  migrations. Tested against MySQL 8.4.
- **TLS** (feature `tls`, on by default): encrypted connections to
  PostgreSQL and MySQL with rustls. Most managed databases require it, and
  MySQL 8's default login method needs it.
- `Migrator::status`: applied and pending migrations, edited ones and ones
  whose file is gone.
- **`axeon-orm-cli`**, a new crate with the `axeon-orm` command:
  `migrate new`, `migrate run` and `migrate status`.

### Changed

- `Model::from_row` now has a default implementation; the derive implements
  the new `Model::from_row_prefixed` instead. Only hand-written `Model`
  implementations need updating.

## 0.1.0 (2026-10-17)

First release.

- `#[derive(Model)]`: table and column mapping, primary keys (integer keys
  assigned by the database), `unique`, `index`, `default`, `created_at` /
  `updated_at`, `skip`, and typed column constants.
- CRUD: `find`, `find_or_fail`, `all`, `insert` (returns the stored row),
  `update` (saves and reloads), `delete`.
- Query builder: `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `like`, `is_in`,
  `not_in`, `is_null`, combined with `&`, `|`, `!`; ordering, limit and
  offset; `first`, `count`, `exists`, `paginate`, and bulk `update` and
  `delete`.
- Relations: `has_many`, `has_one` and `belongs_to`, with accessors and
  eager loading (`Query::with`, `load`) in one query per relation. Foreign
  keys are checked at compile time.
- Transactions through `Db::begin`; every query takes `&db` or `&mut tx`.
- Migrations: versioned SQL files, embedded with `embed_migrations!` or read
  at runtime, applied in one transaction with checksums and a PostgreSQL
  advisory lock. `create_table` from models for tests and prototypes.
- Errors: `NotFound`, `UniqueViolation`, `ForeignKeyViolation` and more.
- Axeon integration (feature `axeon`): `req.db()`, the `Transactional`
  middleware with `req.tx()`, conversion to `ServerError` (404 / 409 / 500)
  and a database readiness check.
- PostgreSQL and SQLite, tested against both.
