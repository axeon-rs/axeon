//! Claiming and running jobs.

use crate::job::State;
use crate::{Error, Job, JobContext, JobError, JobResult, Result};
use axeon_orm::Db;
use futures::future::BoxFuture;
use futures::FutureExt;
use sqlx::PgPool;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout, timeout_at, Instant};
use tracing::{debug, error, info, warn};

/// Extra lease time beyond a job's timeout, so a slow finish isn't taken
/// for a dead worker.
const LEASE_GRACE: Duration = Duration::from_secs(30);
/// Concurrency of a queue that isn't configured with [`Worker::queue`].
const DEFAULT_CONCURRENCY: usize = 4;

type RunFn = Arc<dyn Fn(serde_json::Value, JobContext) -> BoxFuture<'static, JobResult> + Send + Sync>;

struct Registered {
    run: RunFn,
    backoff: fn(u32) -> Duration,
    queue: &'static str,
}

/// Runs jobs from the `axeon_jobs` table: register the job types it
/// handles, then [`run_until`](Worker::run_until) a shutdown signal.
///
/// ```no_run
/// # use axeon_jobs::{Job, JobContext, JobResult, Worker};
/// # #[derive(serde::Serialize, serde::Deserialize)] struct SendWebhook;
/// # impl Job for SendWebhook { const NAME: &'static str = "w"; const QUEUE: &'static str = "webhooks"; async fn run(self, _: JobContext) -> JobResult { Ok(()) } }
/// # #[derive(serde::Serialize, serde::Deserialize)] struct RunStep;
/// # impl Job for RunStep { const NAME: &'static str = "s"; const QUEUE: &'static str = "onboarding"; async fn run(self, _: JobContext) -> JobResult { Ok(()) } }
/// # async fn run(db: axeon_orm::Db) -> axeon_jobs::Result<()> {
/// Worker::new(db)
///     .register::<SendWebhook>()
///     .register::<RunStep>()
///     .queue("onboarding", 32) // at most 32 at once; other queues default to 4
///     .run_until(async { tokio::signal::ctrl_c().await.unwrap() })
///     .await?;
/// # Ok(()) }
/// ```
///
/// Several workers (in one process or many) can share the table: each job
/// runs on one of them at a time. A worker only takes jobs it has
/// registered, so old and new versions can run side by side during a
/// deploy.
pub struct Worker {
    db: Db,
    jobs: HashMap<&'static str, Registered>,
    concurrency: BTreeMap<String, usize>,
    poll_interval: Duration,
    shutdown_timeout: Duration,
    state: State,
    id: String,
    #[cfg(feature = "metrics")]
    metrics: Option<crate::metrics::WorkerMetrics>,
}

impl Worker {
    /// A worker on `db`, which must be PostgreSQL with the
    /// [schema](crate::migration) installed.
    pub fn new(db: Db) -> Self {
        let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "worker".to_string());
        Self {
            db,
            jobs: HashMap::new(),
            concurrency: BTreeMap::new(),
            poll_interval: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(30),
            state: State::default(),
            id: format!("{}:{}", host, std::process::id()),
            #[cfg(feature = "metrics")]
            metrics: None,
        }
    }

    /// Runs jobs of type `J`, on its [`QUEUE`](Job::QUEUE).
    ///
    /// # Panics
    ///
    /// When another registered job type has the same [`NAME`](Job::NAME).
    pub fn register<J: Job>(mut self) -> Self {
        let run: RunFn = Arc::new(|payload, ctx| {
            Box::pin(async move {
                let job: J = serde_json::from_value(payload)
                    .map_err(|e| JobError::fatal(format!("can't read the {} payload: {}", J::NAME, e)))?;
                job.run(ctx).await
            })
        });
        let previous = self.jobs.insert(J::NAME, Registered { run, backoff: J::backoff, queue: J::QUEUE });
        assert!(previous.is_none(), "two job types are named {:?}", J::NAME);
        self
    }

    /// Processes `queue`, running at most `concurrency` of its jobs at once.
    /// Queues of registered jobs are processed anyway, 4 at a time unless
    /// set here; name other queues used with
    /// [`Enqueue::queue`](crate::Enqueue::queue) here.
    pub fn queue(mut self, queue: impl Into<String>, concurrency: usize) -> Self {
        self.concurrency.insert(queue.into(), concurrency.max(1));
        self
    }

    /// Shares a value with every job, read with
    /// [`JobContext::state`]: clients, configuration, caches.
    pub fn state<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.state.insert(value);
        self
    }

    /// How often to look for jobs when no notification arrives, as a
    /// safety net. Default: 5 seconds.
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval.max(Duration::from_millis(10));
        self
    }

    /// How long [`run_until`](Worker::run_until) waits for running jobs
    /// after the signal. Jobs still running then are cancelled and put
    /// back, without counting the attempt. Default: 30 seconds.
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// How this worker appears in the `locked_by` column. Default:
    /// `$HOSTNAME:pid`.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Records job metrics on an Axeon [`Metrics`](axeon::metrics::Metrics)
    /// registry: `jobs_completed_total`, `jobs_failed_total` (with
    /// `outcome` `retry` or `failed`), `job_duration_seconds` and
    /// `jobs_running`, labelled by queue and job name.
    #[cfg(feature = "metrics")]
    pub fn metrics(mut self, metrics: &axeon::metrics::Metrics) -> Self {
        self.metrics = Some(crate::metrics::WorkerMetrics::new(metrics));
        self
    }

    /// Runs jobs until `signal` completes, then stops taking new ones and
    /// waits up to the [shutdown timeout](Worker::shutdown_timeout) for
    /// running ones.
    ///
    /// Database errors while running are logged and retried with a pause,
    /// so an outage doesn't stop the worker. It fails only when it can't
    /// start: no registered jobs, not PostgreSQL, or no connection.
    pub async fn run_until(self, signal: impl Future<Output = ()>) -> Result<()> {
        if self.jobs.is_empty() {
            return Err(Error::Config("register at least one job type".into()));
        }
        let pool = crate::pool(&self.db)?.clone();
        let mut queues = self.concurrency.clone();
        for job in self.jobs.values() {
            queues.entry(job.queue.to_string()).or_insert(DEFAULT_CONCURRENCY);
        }
        let mut listener = self.db.listen(["axeon_jobs"]).await?;
        let (stop, stopped) = watch::channel(false);
        let mut names: Vec<String> = self.jobs.keys().map(|n| n.to_string()).collect();
        names.sort();
        let wakers: HashMap<String, Arc<Notify>> = queues.keys().map(|q| (q.clone(), Arc::new(Notify::new()))).collect();
        let shared = Arc::new(Shared {
            pool,
            db: self.db,
            jobs: self.jobs,
            names,
            state: self.state,
            id: self.id,
            poll_interval: self.poll_interval,
            shutdown_timeout: self.shutdown_timeout,
            in_flight: Mutex::new(HashMap::new()),
            wakers,
            #[cfg(feature = "metrics")]
            metrics: self.metrics,
        });
        info!(worker = %shared.id, queues = ?queues, "job worker started");

        // Wake queues on notifications; after a dropped connection, wake
        // all of them, since messages may have been missed.
        let wake = Arc::clone(&shared);
        let mut stop_listening = stopped.clone();
        let listening = tokio::spawn(async move {
            loop {
                let stop = async {
                    let _ = stop_listening.wait_for(|s| *s).await;
                };
                tokio::select! {
                    _ = stop => break,
                    message = listener.try_recv() => match message {
                        Ok(Some(message)) => wake.wake(&message.payload),
                        Ok(None) => wake.wake_all(),
                        Err(error) => {
                            warn!(%error, "job notifications failed; polling until they're back");
                            wake.wake_all();
                            sleep(Duration::from_secs(1)).await;
                        }
                    },
                }
            }
        });

        let mut loops = JoinSet::new();
        for (queue, concurrency) in queues {
            loops.spawn(queue_loop(Arc::clone(&shared), Arc::from(queue), concurrency, stopped.clone()));
        }
        signal.await;
        info!(worker = %shared.id, "job worker stopping");
        let _ = stop.send(true);
        while loops.join_next().await.is_some() {}
        let _ = listening.await;
        info!(worker = %shared.id, "job worker stopped");
        Ok(())
    }
}

struct Shared {
    pool: PgPool,
    db: Db,
    jobs: HashMap<&'static str, Registered>,
    names: Vec<String>,
    state: State,
    id: String,
    poll_interval: Duration,
    shutdown_timeout: Duration,
    /// Running jobs: ID → (attempt, queue), to put back on a forced stop.
    in_flight: Mutex<HashMap<i64, (i32, Arc<str>)>>,
    wakers: HashMap<String, Arc<Notify>>,
    #[cfg(feature = "metrics")]
    metrics: Option<crate::metrics::WorkerMetrics>,
}

impl Shared {
    fn wake(&self, queue: &str) {
        if let Some(waker) = self.wakers.get(queue) {
            waker.notify_one();
        }
    }

    fn wake_all(&self) {
        self.wakers.values().for_each(|w| w.notify_one());
    }
}

/// A job a worker has taken.
#[derive(sqlx::FromRow)]
struct Claimed {
    id: i64,
    name: String,
    payload: sqlx::types::Json<serde_json::Value>,
    attempts: i32,
    max_attempts: i32,
    timeout_secs: f64,
}

async fn queue_loop(shared: Arc<Shared>, queue: Arc<str>, concurrency: usize, mut stopped: watch::Receiver<bool>) {
    let permits = Arc::new(Semaphore::new(concurrency));
    let waker = Arc::clone(&shared.wakers[&*queue]);
    let mut running = JoinSet::new();
    let mut failing = Duration::ZERO;
    loop {
        if *stopped.borrow() {
            break;
        }
        while running.try_join_next().is_some() {}
        let free = permits.available_permits();
        let mut wait = shared.poll_interval;
        if free > 0 {
            match claim(&shared, &queue, free).await {
                Ok(claimed) => {
                    failing = Duration::ZERO;
                    let full = claimed.len() == free;
                    for job in claimed {
                        let permit = Arc::clone(&permits).try_acquire_owned().expect("a permit per claimed job");
                        running.spawn(execute(Arc::clone(&shared), Arc::clone(&queue), job, permit, stopped.clone()));
                    }
                    if full {
                        continue; // There may be more waiting.
                    }
                    match next_due(&shared, &queue).await {
                        Ok(Some(due)) => wait = wait.min(due.max(Duration::from_millis(50))),
                        Ok(None) => {}
                        Err(error) => warn!(%error, %queue, "couldn't check for scheduled jobs"),
                    }
                }
                Err(error) => {
                    failing = (failing * 2).clamp(Duration::from_secs(1), Duration::from_secs(30));
                    error!(%error, %queue, retry_in = ?failing, "couldn't claim jobs");
                    wait = failing;
                }
            }
        }
        tokio::select! {
            _ = waker.notified() => {}
            _ = sleep(wait) => {}
            _ = stopped.changed() => {}
        }
    }

    // Let running jobs finish, then cancel the rest and put them back.
    let deadline = Instant::now() + shared.shutdown_timeout;
    while !running.is_empty() {
        if timeout_at(deadline, running.join_next()).await.is_err() {
            break;
        }
    }
    if !running.is_empty() {
        warn!(%queue, jobs = running.len(), "shutdown timeout: cancelling running jobs");
        running.abort_all();
        while running.join_next().await.is_some() {}
        let jobs: Vec<(i64, i32)> = shared
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, (_, q))| **q == *queue)
            .map(|(id, (attempt, _))| (*id, *attempt))
            .collect();
        for (id, attempt) in jobs {
            if let Err(error) = release(&shared.pool, id, attempt).await {
                error!(%error, job = id, "couldn't put a cancelled job back; it runs again when its lease expires");
            }
            shared.in_flight.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        }
    }
}

/// Marks jobs whose last attempt's lease ran out as failed, then takes up to
/// `limit` due jobs (including ones whose worker died) for this worker.
async fn claim(shared: &Shared, queue: &str, limit: usize) -> sqlx::Result<Vec<Claimed>> {
    sqlx::query(
        "UPDATE axeon_jobs SET state = 'failed', locked_until = NULL, updated_at = now(), \
             last_error = 'the last attempt never finished: its worker stopped or lost the database' \
         WHERE queue = $1 AND state = 'running' AND locked_until < now() AND attempts >= max_attempts",
    )
    .bind(queue)
    .execute(&shared.pool)
    .await?;
    sqlx::query_as::<_, Claimed>(
        "UPDATE axeon_jobs AS j SET state = 'running', attempts = j.attempts + 1, locked_by = $3, updated_at = now(), \
             locked_until = now() + make_interval(secs => j.timeout_secs + $4) \
         WHERE j.id IN ( \
             SELECT id FROM axeon_jobs \
             WHERE queue = $1 AND name = ANY($2) \
               AND ((state = 'pending' AND run_at <= now()) \
                    OR (state = 'running' AND locked_until < now() AND attempts < max_attempts)) \
             ORDER BY run_at, id \
             LIMIT $5 \
             FOR UPDATE SKIP LOCKED) \
         RETURNING j.id, j.name, j.payload, j.attempts, j.max_attempts, j.timeout_secs",
    )
    .bind(queue)
    .bind(&shared.names)
    .bind(&shared.id)
    .bind(LEASE_GRACE.as_secs_f64())
    .bind(limit as i64)
    .fetch_all(&shared.pool)
    .await
}

/// How long until this queue's next job is due, if any is waiting.
async fn next_due(shared: &Shared, queue: &str) -> sqlx::Result<Option<Duration>> {
    let seconds: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (LEAST( \
             (SELECT min(run_at) FROM axeon_jobs WHERE queue = $1 AND name = ANY($2) AND state = 'pending'), \
             (SELECT min(locked_until) FROM axeon_jobs WHERE queue = $1 AND name = ANY($2) AND state = 'running') \
         ) - now()))::float8",
    )
    .bind(queue)
    .bind(&shared.names)
    .fetch_one(&shared.pool)
    .await?;
    Ok(seconds.map(|s| Duration::from_secs_f64(s.max(0.0))))
}

async fn execute(shared: Arc<Shared>, queue: Arc<str>, job: Claimed, _permit: OwnedSemaphorePermit, stopped: watch::Receiver<bool>) {
    let Claimed { id, name, payload, attempts, max_attempts, timeout_secs } = job;
    shared.in_flight.lock().unwrap_or_else(|e| e.into_inner()).insert(id, (attempts, Arc::clone(&queue)));
    let registered = &shared.jobs[name.as_str()];
    let ctx = JobContext {
        id,
        attempt: attempts as u32,
        queue: Arc::clone(&queue),
        db: shared.db.clone(),
        state: shared.state.clone(),
        shutdown: stopped,
    };
    let limit = Duration::from_secs_f64(timeout_secs.max(0.0));
    #[cfg(feature = "metrics")]
    let _running = shared.metrics.as_ref().map(|m| m.start(&queue, &name));
    let started = std::time::Instant::now();
    let run = AssertUnwindSafe((registered.run)(payload.0, ctx)).catch_unwind();
    let result = match timeout(limit, run).await {
        Ok(Ok(result)) => result,
        Ok(Err(panic)) => Err(JobError::retry(format!("panicked: {}", panic_message(&*panic)))),
        Err(_) => Err(JobError::retry(format!("timed out after {:?}", limit))),
    };
    let elapsed = started.elapsed();

    let outcome = match &result {
        Ok(()) => finish(&shared.pool, id, attempts).await.map(|done| (done, "completed")),
        Err(e) if e.is_fatal() || attempts >= max_attempts => {
            fail(&shared.pool, id, attempts, e.message()).await.map(|done| (done, "failed"))
        }
        Err(e) => {
            let delay = e.delay().unwrap_or_else(|| (registered.backoff)(attempts as u32));
            retry(&shared.pool, id, attempts, e.message(), delay).await.map(|done| (done, "retry"))
        }
    };
    match (&outcome, &result) {
        (Ok((true, "completed")), _) => debug!(job = id, %name, %queue, ?elapsed, "job completed"),
        (Ok((true, "failed")), Err(e)) => error!(job = id, %name, %queue, attempt = attempts, error = %e, "job failed"),
        (Ok((true, _)), Err(e)) => warn!(job = id, %name, %queue, attempt = attempts, error = %e, "job will be retried"),
        (Ok((false, _)), _) => warn!(job = id, %name, "job was taken over by another worker after its lease expired"),
        (Err(error), _) => error!(job = id, %name, %error, "couldn't record the job's result; it runs again when its lease expires"),
        _ => {}
    }
    #[cfg(feature = "metrics")]
    if let (Some(metrics), Ok((_, outcome))) = (&shared.metrics, &outcome) {
        metrics.finish(&queue, &name, outcome, elapsed);
    }
    shared.in_flight.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    shared.wake(&queue); // A slot is free.
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic.downcast_ref::<&str>().copied().or_else(|| panic.downcast_ref::<String>().map(String::as_str)).unwrap_or("panic")
}

// Each update checks the attempt number, so a worker whose lease expired
// can't overwrite the result of a later run. They return whether it applied.

async fn finish(pool: &PgPool, id: i64, attempt: i32) -> sqlx::Result<bool> {
    let done = sqlx::query("DELETE FROM axeon_jobs WHERE id = $1 AND attempts = $2 AND state = 'running'")
        .bind(id)
        .bind(attempt)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() == 1)
}

async fn retry(pool: &PgPool, id: i64, attempt: i32, error: &str, delay: Duration) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "UPDATE axeon_jobs SET state = 'pending', run_at = now() + make_interval(secs => $3), last_error = $4, \
             locked_until = NULL, locked_by = NULL, updated_at = now() \
         WHERE id = $1 AND attempts = $2 AND state = 'running'",
    )
    .bind(id)
    .bind(attempt)
    .bind(delay.as_secs_f64())
    .bind(error)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

async fn fail(pool: &PgPool, id: i64, attempt: i32, error: &str) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "UPDATE axeon_jobs SET state = 'failed', last_error = $3, locked_until = NULL, updated_at = now() \
         WHERE id = $1 AND attempts = $2 AND state = 'running'",
    )
    .bind(id)
    .bind(attempt)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Puts a cancelled job back to run now, without counting the attempt.
async fn release(pool: &PgPool, id: i64, attempt: i32) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE axeon_jobs SET state = 'pending', attempts = attempts - 1, run_at = now(), \
             locked_until = NULL, locked_by = NULL, updated_at = now() \
         WHERE id = $1 AND attempts = $2 AND state = 'running'",
    )
    .bind(id)
    .bind(attempt)
    .execute(pool)
    .await?;
    Ok(())
}
