# Changelog

## Unreleased

### Added

- **Cron schedules:** `Worker::schedule(name, "0 3 * * *", job)`, with a
  `Cron` type (five fields, UTC, names, ranges, steps, `@daily` and the
  like). Each run is enqueued once however many workers declare the
  schedule; runs are skipped while the previous one is still waiting or
  running, and missed runs are made up once.
- **Priorities:** `Job::PRIORITY` and `Enqueue::priority`; among due jobs
  on a queue, higher priorities are taken first.
- `migrations()` returns both schema migrations;
  `SCHEMA_PRIORITY_AND_SCHEDULES` adds the `priority` column, its index and
  the `axeon_jobs_schedules` table. `install` applies it too.

### Changed

- `register` accepts the same job type twice (it still panics for two
  types with one name).

### Deprecated

- `migration()`: use `migrations()`, which includes the 0.2 schema.

## 0.1.0 (2026-10-22)

First release: a `Job` trait, `enqueue` / `enqueue_with` (delays, run
times, unique keys, other queues; with `&db` or inside a transaction), a
`Worker` with per-queue concurrency, `LISTEN`/`NOTIFY` wake-ups, leases,
retries with backoff, failed jobs, graceful shutdown and Prometheus
metrics, and `Jobs` for counts, retries and cancelling. PostgreSQL only.
