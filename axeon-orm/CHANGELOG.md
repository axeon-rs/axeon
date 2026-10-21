# Changelog

## 0.2.0 (unreleased)

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
- `From` conversions into `Value` for `serde_json::Value`, `Uuid` and the
  chrono types.
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
