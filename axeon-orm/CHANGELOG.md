# Changelog

## 0.1.0 (unreleased)

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
