//! Against PostgreSQL (when `AXEON_ORM_POSTGRES_URL` is set). Each test
//! uses its own schema.

use axeon_jobs::{Enqueue, Job, JobContext, JobError, JobResult, Jobs, Worker};
use axeon_orm::Db;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

async fn db(name: &str) -> Option<Db> {
    let Ok(url) = std::env::var("AXEON_ORM_POSTGRES_URL") else {
        eprintln!("skipping: AXEON_ORM_POSTGRES_URL is not set");
        return None;
    };
    let schema = format!("axeon_jobs_{}", name);
    let admin = Db::connect(&url).await.unwrap();
    admin.execute(&format!("DROP SCHEMA IF EXISTS {0} CASCADE; CREATE SCHEMA {0}", schema)).await.unwrap();
    admin.close().await;
    let separator = if url.contains('?') { '&' } else { '?' };
    let db = Db::connect(&format!("{}{}options=-c%20search_path%3D{}", url, separator, schema)).await.unwrap();
    axeon_jobs::install(&db).await.unwrap();
    axeon_jobs::install(&db).await.unwrap(); // Safe to run again.
    Some(db)
}

/// What jobs did: (job, attempt) per run.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<(String, u32)>>>);

impl Log {
    fn push(&self, what: impl Into<String>, attempt: u32) {
        self.0.lock().unwrap().push((what.into(), attempt));
    }
    fn runs(&self) -> Vec<(String, u32)> {
        self.0.lock().unwrap().clone()
    }
    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

fn log(ctx: &JobContext) -> &Log {
    ctx.state::<Log>().expect("the Log state")
}

#[derive(Serialize, Deserialize)]
struct Record {
    label: String,
}

impl Job for Record {
    const NAME: &'static str = "record";

    async fn run(self, ctx: JobContext) -> JobResult {
        log(&ctx).push(self.label, ctx.attempt());
        Ok(())
    }
}

/// Fails until attempt `succeed_on` (0: never).
#[derive(Serialize, Deserialize)]
struct Flaky {
    succeed_on: u32,
}

impl Job for Flaky {
    const NAME: &'static str = "flaky";
    const MAX_ATTEMPTS: u32 = 3;

    fn backoff(_attempt: u32) -> Duration {
        Duration::from_millis(10)
    }

    async fn run(self, ctx: JobContext) -> JobResult {
        log(&ctx).push("flaky", ctx.attempt());
        if self.succeed_on != 0 && ctx.attempt() >= self.succeed_on {
            return Ok(());
        }
        Err(JobError::retry(format!("failure {}", ctx.attempt())))
    }
}

#[derive(Serialize, Deserialize)]
enum Trouble {
    Fatal,
    Panic,
    Hang,
    RetryAfter(u64),
    ParseError,
}

#[derive(Serialize, Deserialize)]
struct Troublesome(Trouble);

impl Job for Troublesome {
    const NAME: &'static str = "troublesome";
    const MAX_ATTEMPTS: u32 = 2;
    const TIMEOUT: Duration = Duration::from_millis(200);

    fn backoff(_attempt: u32) -> Duration {
        Duration::from_millis(10)
    }

    async fn run(self, ctx: JobContext) -> JobResult {
        log(&ctx).push("troublesome", ctx.attempt());
        match self.0 {
            Trouble::Fatal => Err(JobError::fatal("bad input")),
            Trouble::Panic => panic!("boom"),
            Trouble::Hang => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            }
            Trouble::RetryAfter(ms) if ctx.attempt() == 1 => Err(JobError::retry_after("rate limited", Duration::from_millis(ms))),
            Trouble::RetryAfter(_) => Ok(()),
            Trouble::ParseError => {
                "nope".parse::<u8>()?;
                Ok(())
            }
        }
    }
}

/// Sleeps, tracking how many run at once.
#[derive(Serialize, Deserialize)]
struct Slow {
    ms: u64,
}

#[derive(Default)]
struct Concurrency {
    now: AtomicUsize,
    max: AtomicUsize,
}

impl Job for Slow {
    const NAME: &'static str = "slow";
    const QUEUE: &'static str = "slow";

    async fn run(self, ctx: JobContext) -> JobResult {
        let gauge = ctx.state::<Arc<Concurrency>>().unwrap();
        let now = gauge.now.fetch_add(1, Ordering::SeqCst) + 1;
        gauge.max.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(self.ms)).await;
        gauge.now.fetch_sub(1, Ordering::SeqCst);
        log(&ctx).push("slow", ctx.attempt());
        Ok(())
    }
}

/// A job no worker in these tests registers.
#[derive(Serialize, Deserialize)]
struct Unregistered;

impl Job for Unregistered {
    const NAME: &'static str = "unregistered";

    async fn run(self, _ctx: JobContext) -> JobResult {
        Ok(())
    }
}

fn worker(db: &Db, log: &Log) -> Worker {
    Worker::new(db.clone())
        .register::<Record>()
        .register::<Flaky>()
        .register::<Troublesome>()
        .state(log.clone())
        .poll_interval(Duration::from_secs(60)) // Rely on notifications.
}

/// Runs a worker until the returned sender fires (or is dropped).
fn spawn(worker: Worker) -> (oneshot::Sender<()>, JoinHandle<axeon_jobs::Result<()>>) {
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = tokio::spawn(worker.run_until(async move {
        let _ = stopped.await;
    }));
    (stop, handle)
}

async fn eventually<F: Future<Output = bool>>(what: &str, mut check: impl FnMut() -> F) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for: {}", what);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn rows(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM axeon_jobs").fetch_one(db.postgres().unwrap()).await.unwrap()
}

async fn job_row(db: &Db, id: i64) -> Option<(String, i32, Option<String>)> {
    sqlx::query_as("SELECT state, attempts, last_error FROM axeon_jobs WHERE id = $1")
        .bind(id)
        .fetch_optional(db.postgres().unwrap())
        .await
        .unwrap()
}

fn record(label: &str) -> Record {
    Record { label: label.into() }
}

#[tokio::test]
async fn runs_jobs_once_and_deletes_them() {
    let Some(db) = db("runs").await else { return };
    let log = Log::default();
    for label in ["a", "b", "c"] {
        axeon_jobs::enqueue(&db, &record(label)).await.unwrap();
    }
    assert_eq!(rows(&db).await, 3);
    let (stop, handle) = spawn(worker(&db, &log));
    eventually("3 runs", || async { log.len() == 3 }).await;
    eventually("rows deleted", || async { rows(&db).await == 0 }).await;
    let mut runs = log.runs();
    runs.sort();
    assert_eq!(runs, [("a".into(), 1), ("b".into(), 1), ("c".into(), 1)]);

    // A worker that is already waiting wakes on the notification, well
    // before its 60s poll.
    let started = Instant::now();
    axeon_jobs::enqueue(&db, &record("late")).await.unwrap();
    eventually("the late job", || async { log.len() == 4 }).await;
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());

    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn enqueued_in_a_transaction_only_on_commit() {
    let Some(db) = db("tx").await else { return };
    let log = Log::default();
    let (stop, handle) = spawn(worker(&db, &log));

    let mut tx = db.begin().await.unwrap();
    axeon_jobs::enqueue(&mut tx, &record("rolled back")).await.unwrap();
    tx.rollback().await.unwrap();

    let mut tx = db.begin().await.unwrap();
    axeon_jobs::enqueue(&mut tx, &record("committed")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(log.len(), 0, "not visible before commit");
    tx.commit().await.unwrap();
    eventually("the committed job", || async { log.len() == 1 }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(log.runs(), [("committed".into(), 1)]);

    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn retries_then_fails_and_can_be_retried() {
    let Some(db) = db("retries").await else { return };
    let log = Log::default();
    let (stop, handle) = spawn(worker(&db, &log));

    let recovers = axeon_jobs::enqueue(&db, &Flaky { succeed_on: 2 }).await.unwrap();
    let never = axeon_jobs::enqueue(&db, &Flaky { succeed_on: 0 }).await.unwrap();
    eventually("recovered and deleted", || async { job_row(&db, recovers).await.is_none() }).await;
    eventually("out of attempts", || async { job_row(&db, never).await.is_some_and(|r| r.0 == "failed") }).await;
    let (_, attempts, error) = job_row(&db, never).await.unwrap();
    assert_eq!((attempts, error.as_deref()), (3, Some("failure 3")));
    assert_eq!(log.len(), 2 + 3);

    let jobs = Jobs::new(db.clone());
    let failed = jobs.failed(10).await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!((failed[0].id, failed[0].name.as_str(), failed[0].attempts), (never, "flaky", 3));
    assert_eq!(failed[0].payload.0, serde_json::json!({ "succeed_on": 0 }));
    let counts = jobs.counts().await.unwrap();
    assert_eq!((counts[0].queue.as_str(), counts[0].failed, counts[0].ready), ("default", 1, 0));

    // Retrying gives it a fresh set of attempts, and wakes the worker.
    assert!(jobs.retry(never).await.unwrap());
    assert!(!jobs.retry(never).await.unwrap(), "no longer failed");
    eventually("failed again after 3 more", || async { job_row(&db, never).await.is_some_and(|r| r.0 == "failed") && log.len() == 8 }).await;
    assert!(jobs.cancel(never).await.unwrap());
    assert_eq!(rows(&db).await, 0);
    assert_eq!(jobs.prune_failed(Duration::ZERO).await.unwrap(), 0);

    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn fatal_errors_panics_timeouts_and_bad_payloads() {
    let Some(db) = db("trouble").await else { return };
    let log = Log::default();
    let (stop, handle) = spawn(worker(&db, &log));
    let state = |id| {
        let db = db.clone();
        async move { job_row(&db, id).await }
    };

    let fatal = axeon_jobs::enqueue(&db, &Troublesome(Trouble::Fatal)).await.unwrap();
    let panics = axeon_jobs::enqueue(&db, &Troublesome(Trouble::Panic)).await.unwrap();
    let hangs = axeon_jobs::enqueue(&db, &Troublesome(Trouble::Hang)).await.unwrap();
    let parse = axeon_jobs::enqueue(&db, &Troublesome(Trouble::ParseError)).await.unwrap();
    for id in [fatal, panics, hangs, parse] {
        eventually("job failed", || async { state(id).await.is_some_and(|r| r.0 == "failed") }).await;
    }
    assert_eq!(state(fatal).await.unwrap(), ("failed".into(), 1, Some("bad input".into())), "fatal: no retry");
    assert_eq!(state(panics).await.unwrap(), ("failed".into(), 2, Some("panicked: boom".into())));
    assert_eq!(state(hangs).await.unwrap(), ("failed".into(), 2, Some("timed out after 200ms".into())));
    assert_eq!(state(parse).await.unwrap(), ("failed".into(), 2, Some("invalid digit found in string".into())));

    // retry_after overrides the backoff.
    let started = Instant::now();
    let delayed = axeon_jobs::enqueue(&db, &Troublesome(Trouble::RetryAfter(500))).await.unwrap();
    eventually("retried and done", || async { state(delayed).await.is_none() }).await;
    assert!(started.elapsed() >= Duration::from_millis(500), "{:?}", started.elapsed());

    // A payload that no longer fits the type fails at once.
    let bad: i64 = sqlx::query_scalar(
        "INSERT INTO axeon_jobs (queue, name, payload, max_attempts, timeout_secs) VALUES ('default', 'record', '{\"nope\": 1}', 5, 60) RETURNING id",
    )
    .fetch_one(db.postgres().unwrap())
    .await
    .unwrap();
    sqlx::query("SELECT pg_notify('axeon_jobs', 'default')").execute(db.postgres().unwrap()).await.unwrap();
    eventually("bad payload failed", || async { state(bad).await.is_some_and(|r| r.0 == "failed") }).await;
    let (_, attempts, error) = state(bad).await.unwrap();
    assert_eq!(attempts, 1);
    assert!(error.unwrap().starts_with("can't read the record payload: missing field `label`"));

    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn unique_keys_and_delays() {
    let Some(db) = db("unique").await else { return };
    let log = Log::default();
    let keyed = || Enqueue::new().unique_key("run:42:step:3");

    let first = axeon_jobs::enqueue_with(&db, &record("first"), keyed()).await.unwrap();
    assert!(first.is_some());
    assert_eq!(axeon_jobs::enqueue_with(&db, &record("dup"), keyed()).await.unwrap(), None);
    // Other keys, and no key, aren't affected.
    assert!(axeon_jobs::enqueue_with(&db, &record("other"), Enqueue::new().unique_key("other")).await.unwrap().is_some());
    axeon_jobs::enqueue(&db, &record("plain")).await.unwrap();

    let (stop, handle) = spawn(worker(&db, &log));
    eventually("3 runs", || async { log.len() == 3 }).await;
    // Done, so the key is free again.
    assert!(axeon_jobs::enqueue_with(&db, &record("again"), keyed()).await.unwrap().is_some());
    eventually("4 runs", || async { log.len() == 4 }).await;

    // A failed job doesn't hold its key.
    let failed = axeon_jobs::enqueue_with(&db, &Troublesome(Trouble::Fatal), Enqueue::new().unique_key("t")).await.unwrap().unwrap();
    eventually("failed", || async { job_row(&db, failed).await.is_some_and(|r| r.0 == "failed") }).await;
    assert!(axeon_jobs::enqueue_with(&db, &record("t again"), Enqueue::new().unique_key("t")).await.unwrap().is_some());
    eventually("6 runs", || async { log.len() == 6 }).await;

    // Delays: the sleeping worker wakes for the due time, not its 60s poll.
    let started = Instant::now();
    axeon_jobs::enqueue_with(&db, &record("delayed"), Enqueue::new().delay(Duration::from_millis(400))).await.unwrap();
    axeon_jobs::enqueue_with(&db, &record("at"), Enqueue::new().run_at(chrono::Utc::now() + chrono::Duration::milliseconds(600))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(log.len(), 6, "not yet due");
    eventually("delayed jobs", || async { log.len() == 8 }).await;
    assert!(started.elapsed() >= Duration::from_millis(600));
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    let labels: Vec<String> = log.runs().into_iter().map(|r| r.0).collect();
    assert_eq!(&labels[6..], ["delayed", "at"]);

    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn concurrency_limits_and_many_workers() {
    let Some(db) = db("concurrency").await else { return };
    let log = Log::default();
    let gauge = Arc::new(Concurrency::default());
    for _ in 0..12 {
        axeon_jobs::enqueue(&db, &Slow { ms: 60 }).await.unwrap();
    }
    let slow_worker = || Worker::new(db.clone()).register::<Slow>().queue("slow", 2).state(log.clone()).state(Arc::clone(&gauge));
    let (stop, handle) = spawn(slow_worker());
    eventually("12 slow jobs", || async { log.len() == 12 }).await;
    assert_eq!(gauge.max.load(Ordering::SeqCst), 2, "at most 2 at once");
    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();

    // Three workers share 300 jobs: each runs exactly once.
    let log = Log::default();
    let mut tx = db.begin().await.unwrap();
    for i in 0..300 {
        axeon_jobs::enqueue(&mut tx, &record(&i.to_string())).await.unwrap();
    }
    tx.commit().await.unwrap();
    let workers: Vec<_> = (0..3).map(|i| spawn(worker(&db, &log).queue("default", 8).id(format!("w{}", i)))).collect();
    eventually("300 runs", || async { log.len() >= 300 }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (label, attempt) in log.runs() {
        assert_eq!(attempt, 1);
        *seen.entry(label).or_default() += 1;
    }
    assert_eq!(seen.len(), 300);
    assert!(seen.values().all(|n| *n == 1), "a job ran twice");
    for (stop, handle) in workers {
        stop.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }
    assert_eq!(rows(&db).await, 0);
}

#[tokio::test]
async fn jobs_of_dead_workers_are_taken_over() {
    let Some(db) = db("leases").await else { return };
    let log = Log::default();
    let pool = db.postgres().unwrap();
    // As left by a worker that died: running, with an expired lease.
    let insert = |attempts: i32, label: &str| {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO axeon_jobs (queue, name, payload, state, attempts, max_attempts, timeout_secs, locked_until, locked_by) \
             VALUES ('default', 'record', $1, 'running', $2, 3, 60, now() - interval '1 second', 'dead') RETURNING id",
        )
        .bind(sqlx::types::Json(serde_json::json!({ "label": label })))
        .bind(attempts)
        .fetch_one(pool)
    };
    let orphan = insert(1, "orphan").await.unwrap();
    let last = insert(3, "last").await.unwrap();
    // Still leased by a live worker: left alone.
    let leased: i64 = sqlx::query_scalar(
        "INSERT INTO axeon_jobs (queue, name, payload, state, attempts, max_attempts, timeout_secs, locked_until) \
         VALUES ('default', 'record', '{\"label\": \"leased\"}', 'running', 1, 3, 60, now() + interval '1 hour') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();

    let (stop, handle) = spawn(worker(&db, &log));
    eventually("orphan rerun", || async { job_row(&db, orphan).await.is_none() }).await;
    assert_eq!(log.runs(), [("orphan".into(), 2)], "counts as the second attempt");
    eventually("last attempt failed", || async { job_row(&db, last).await.is_some_and(|r| r.0 == "failed") }).await;
    assert!(job_row(&db, last).await.unwrap().2.unwrap().contains("never finished"));
    assert_eq!(job_row(&db, leased).await.unwrap().0, "running");
    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_waits_then_puts_jobs_back() {
    let Some(db) = db("shutdown").await else { return };
    let log = Log::default();
    let gauge = Arc::new(Concurrency::default());
    let slow_worker = |timeout| {
        Worker::new(db.clone()).register::<Slow>().state(log.clone()).state(Arc::clone(&gauge)).shutdown_timeout(timeout)
    };

    // Finishes within the shutdown timeout.
    axeon_jobs::enqueue(&db, &Slow { ms: 300 }).await.unwrap();
    let (stop, handle) = spawn(slow_worker(Duration::from_secs(5)));
    eventually("started", || async { gauge.now.load(Ordering::SeqCst) == 1 }).await;
    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
    assert_eq!(log.len(), 1, "waited for the running job");
    assert_eq!(rows(&db).await, 0);

    // Too slow: cancelled and put back without using an attempt.
    let id = axeon_jobs::enqueue(&db, &Slow { ms: 20_000 }).await.unwrap();
    let (stop, handle) = spawn(slow_worker(Duration::from_millis(100)));
    eventually("started", || async { gauge.now.load(Ordering::SeqCst) == 1 }).await;
    let started = Instant::now();
    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    let (state, attempts, _) = job_row(&db, id).await.unwrap();
    assert_eq!((state.as_str(), attempts), ("pending", 0));
}

#[tokio::test]
async fn workers_take_only_jobs_they_know() {
    let Some(db) = db("known").await else { return };
    let log = Log::default();
    let unknown = axeon_jobs::enqueue(&db, &Unregistered).await.unwrap();
    axeon_jobs::enqueue(&db, &record("known")).await.unwrap();
    // Another queue, named on enqueue, runs once the worker processes it.
    axeon_jobs::enqueue_with(&db, &record("elsewhere"), Enqueue::new().queue("other")).await.unwrap();
    let (stop, handle) = spawn(worker(&db, &log));
    eventually("known job", || async { log.len() == 1 }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(job_row(&db, unknown).await.unwrap(), ("pending".into(), 0, None), "left for a worker that knows it");
    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();

    let (stop, handle) = spawn(worker(&db, &log).queue("other", 1));
    eventually("other queue", || async { log.len() == 2 }).await;
    assert_eq!(log.runs()[1].0, "elsewhere");
    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn metrics_count_runs() {
    let Some(db) = db("metrics").await else { return };
    let log = Log::default();
    let metrics = axeon::metrics::Metrics::new();
    axeon_jobs::enqueue(&db, &record("x")).await.unwrap();
    axeon_jobs::enqueue(&db, &Flaky { succeed_on: 2 }).await.unwrap();
    axeon_jobs::enqueue(&db, &Troublesome(Trouble::Fatal)).await.unwrap();
    let (stop, handle) = spawn(worker(&db, &log).metrics(&metrics));
    eventually("4 runs", || async { log.len() == 4 }).await;
    eventually("recorded", || async { metrics.render().contains(r#"jobs_failed_total{queue="default",job="troublesome",outcome="failed"} 1"#) }).await;
    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
    let text = metrics.render();
    for line in [
        r#"jobs_completed_total{queue="default",job="record"} 1"#,
        r#"jobs_completed_total{queue="default",job="flaky"} 1"#,
        r#"jobs_failed_total{queue="default",job="flaky",outcome="retry"} 1"#,
        r#"job_duration_seconds_count{queue="default",job="flaky"} 2"#,
        r#"jobs_running{queue="default"} 0"#,
    ] {
        assert!(text.contains(line), "missing {} in\n{}", line, text);
    }
}

#[tokio::test]
async fn needs_postgres_and_jobs() {
    let sqlite = Db::connect("sqlite::memory:").await.unwrap();
    assert!(matches!(axeon_jobs::install(&sqlite).await, Err(axeon_jobs::Error::Config(_))));
    assert!(matches!(axeon_jobs::enqueue(&sqlite, &record("x")).await, Err(axeon_jobs::Error::Config(_))));
    let none = Worker::new(sqlite.clone()).run_until(async {}).await;
    assert!(matches!(none, Err(axeon_jobs::Error::Config(m)) if m.contains("register")));
    let not_pg = Worker::new(sqlite).register::<Record>().run_until(async {}).await;
    assert!(matches!(not_pg, Err(axeon_jobs::Error::Config(m)) if m.contains("PostgreSQL")));
}

#[tokio::test]
async fn higher_priorities_run_first() {
    let Some(db) = db("priorities").await else { return };
    let log = Log::default();
    // Enqueued oldest first, with priorities mixed in.
    for (label, priority) in [("low", -5), ("normal-1", 0), ("urgent", 10), ("normal-2", 0), ("high", 5)] {
        axeon_jobs::enqueue_with(&db, &record(label), Enqueue::new().priority(priority)).await.unwrap();
    }
    let (stop, handle) = spawn(worker(&db, &log).queue("default", 1));
    eventually("5 runs", || async { log.len() == 5 }).await;
    let order: Vec<String> = log.runs().into_iter().map(|(label, _)| label).collect();
    assert_eq!(order, ["urgent", "high", "normal-1", "normal-2", "low"]);
    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn schedules_enqueue_each_run_once() {
    let Some(db) = db("schedules").await else { return };
    let pool = db.postgres().unwrap();
    let log = Log::default();
    // A schedule that came due a while ago, as after a weekend with no
    // workers running: it is made up once.
    sqlx::query(
        "INSERT INTO axeon_jobs_schedules (name, expression, next_run_at) VALUES ('nightly', '0 3 * * *', now() - interval '2 days')",
    )
    .execute(pool)
    .await
    .unwrap();
    // Three workers declare the same schedules.
    let mut workers = Vec::new();
    for i in 0..3 {
        let worker = worker(&db, &log)
            .id(format!("w{}", i))
            .schedule("nightly", "0 3 * * *", record("nightly"))
            .schedule("hourly", "@hourly", record("hourly"));
        workers.push(spawn(worker));
    }
    eventually("the nightly run", || async { log.len() == 1 }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(log.runs(), [("nightly".to_string(), 1)]);

    // Next runs are in the future, at the next match of each expression.
    type Time = chrono::DateTime<chrono::Utc>;
    let rows: Vec<(String, String, Time, Option<Time>)> =
        sqlx::query_as("SELECT name, expression, next_run_at, last_run_at FROM axeon_jobs_schedules ORDER BY name")
            .fetch_all(pool)
            .await
            .unwrap();
    let now = chrono::Utc::now();
    let (hourly, nightly) = (&rows[0], &rows[1]);
    assert_eq!((hourly.0.as_str(), nightly.0.as_str()), ("hourly", "nightly"));
    assert_eq!(nightly.2, "0 3 * * *".parse::<axeon_jobs::Cron>().unwrap().next_after(now).unwrap());
    assert_eq!(hourly.2, "@hourly".parse::<axeon_jobs::Cron>().unwrap().next_after(now).unwrap());
    assert!(nightly.3.is_some() && hourly.3.is_none());

    // A changed expression replaces the next run time.
    for (stop, handle) in workers {
        stop.send(()).unwrap();
        handle.await.unwrap().unwrap();
    }
    let (stop, handle) = spawn(worker(&db, &log).schedule("nightly", "30 4 * * *", record("nightly")));
    eventually("the new expression", || async {
        let expression: String = sqlx::query_scalar("SELECT expression FROM axeon_jobs_schedules WHERE name = 'nightly'")
            .fetch_one(pool)
            .await
            .unwrap();
        expression == "30 4 * * *"
    })
    .await;
    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
    assert_eq!(log.len(), 1);
}

#[tokio::test]
async fn bad_schedules_stop_the_worker() {
    let Some(db) = db("bad_schedules").await else { return };
    let log = Log::default();
    for (worker, error) in [
        (worker(&db, &log).schedule("x", "61 * * * *", record("x")), "minute"),
        (worker(&db, &log).schedule("x", "0 0 31 2 *", record("x")), "never matches"),
        (worker(&db, &log).schedule("x", "@daily", record("x")).schedule("x", "@hourly", record("y")), "two schedules"),
    ] {
        let result = worker.run_until(async {}).await;
        assert!(matches!(&result, Err(axeon_jobs::Error::Config(m)) if m.contains(error)), "{:?}", result);
    }
    // A scheduled type is registered once, even if also registered by hand.
    let worker = Worker::new(db.clone()).schedule("x", "@daily", record("x")).register::<Record>().state(log);
    worker.run_until(async {}).await.unwrap();
}
