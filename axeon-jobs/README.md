# axeon-jobs

Background jobs on PostgreSQL for [Axeon](https://docs.rs/axeon) apps using
[axeon-orm](https://docs.rs/axeon-orm). Jobs live in one table, so they're
saved in the same transaction as the data they belong to, with no extra
infrastructure.

```rust
use axeon_jobs::{Enqueue, Job, JobContext, JobResult, Worker};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Serialize, Deserialize)]
struct SendWebhook { url: String, event_id: i64 }

impl Job for SendWebhook {
    const NAME: &'static str = "send_webhook";
    const QUEUE: &'static str = "webhooks";
    const MAX_ATTEMPTS: u32 = 8;               // default 5
    const TIMEOUT: Duration = Duration::from_secs(30); // default 5 minutes

    async fn run(self, ctx: JobContext) -> JobResult {
        // `?` on any error retries with backoff; JobError::fatal gives up.
        Ok(())
    }
}

// Create the tables (or add `axeon_jobs::migrations()` to your migrations).
axeon_jobs::install(&db).await?;

// Enqueue with `&db`, or `&mut tx` to commit the job with your other writes.
let mut tx = db.begin().await?;
axeon_jobs::enqueue(&mut tx, &SendWebhook { url, event_id }).await?;
tx.commit().await?;

axeon_jobs::enqueue_with(&db, &job, Enqueue::new()
    .delay(Duration::from_secs(60))   // or .run_at(time)
    .unique_key("event:7")            // skipped while one is waiting or running
    .priority(10)).await?;            // higher runs first; default Job::PRIORITY (0)

// The worker process.
Worker::new(db.clone())
    .register::<SendWebhook>()
    .queue("webhooks", 16)            // concurrency; default 4 per queue
    .schedule("rescreen", "0 3 * * *", RescreenAll) // cron, in UTC
    .state(http_client)               // ctx.state::<HttpClient>() in jobs
    .metrics(&metrics)                // Prometheus, on Axeon's /metrics
    .run_until(axeon::shutdown_signal())
    .await?;
```

## How it works

- Workers take due jobs with `FOR UPDATE SKIP LOCKED`, so any number of
  workers share the table and each job runs on one at a time. They wake on
  `LISTEN`/`NOTIFY` when a job is added, sleep until the next delayed job
  is due, and poll every 5 seconds as a safety net.
- A taken job is **leased** for its timeout plus 30 seconds, not held in an
  open transaction, so long jobs (LLM calls, vendor APIs) don't pin a
  database connection. If a worker dies, another one takes its jobs when
  the lease ends.
- Success deletes the job. An error, panic or timeout retries after the
  job's backoff (2s, 4s, 8s... up to an hour, with jitter);
  `JobError::retry_after` sets the delay. Out of attempts, or after
  `JobError::fatal`, the job is marked `failed` and kept.
- Results are recorded only by the run that holds the job, checked by
  attempt number, so a worker whose lease expired can't overwrite a newer
  run.
- Workers only take job names they've registered, so old and new versions
  can run side by side during a deploy.
- On shutdown a worker stops taking jobs and waits (30 seconds by default)
  for running ones; any still running are cancelled and put back without
  using an attempt. Jobs can watch `ctx.shutdown()` to stop early.
- Jobs run **at least once**. Make them safe to repeat, e.g. using
  `ctx.id()` as an idempotency key with vendors.

`Jobs::new(db)` has `counts()` per queue and state, `failed(limit)`,
`retry(id)`, `cancel(id)` and `prune_failed(age)` for operator screens.

With the `metrics` feature (default), `Worker::metrics` records
`jobs_completed_total`, `jobs_failed_total` (`outcome` `retry` or
`failed`), `job_duration_seconds` and `jobs_running`.

## Schedules

`Worker::schedule(name, expression, job)` enqueues `job` on a five-field
cron expression (`minute hour day month weekday`, UTC; names like `MON`
and `JAN`, ranges, steps and lists, plus `@hourly`, `@daily`, `@weekly`,
`@monthly`, `@yearly`).

- Every worker can declare the same schedules. The next run time lives in
  `axeon_jobs_schedules` and is advanced with a compare-and-swap in the
  same transaction as the enqueue, so each run is enqueued exactly once.
- A run is skipped while the previous one is still waiting or running
  (unique key `cron:<name>`), so slow jobs don't pile up.
- Runs missed while no worker was up are made up once, not once per miss.

## Not yet

Batches.

## Testing

```bash
export AXEON_ORM_POSTGRES_URL=postgres://postgres:postgres@localhost:55432/postgres
cargo test -p axeon-jobs
```

## License

MIT
