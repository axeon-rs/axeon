# Changelog

## 0.1.0 (unreleased)

First release: a `Job` trait, `enqueue` / `enqueue_with` (delays, run
times, unique keys, other queues; with `&db` or inside a transaction), a
`Worker` with per-queue concurrency, `LISTEN`/`NOTIFY` wake-ups, leases,
retries with backoff, failed jobs, graceful shutdown and Prometheus
metrics, and `Jobs` for counts, retries and cancelling. PostgreSQL only.
