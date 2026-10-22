//! Adding jobs to the queue.

use crate::{Error, Job, Result};
use axeon_orm::{Db, Transaction};
use chrono::{DateTime, Utc};
use std::time::Duration;

/// Where to enqueue: `&db`, or `&mut tx` to add the job atomically with the
/// transaction's other writes.
pub enum Target<'c> {
    #[doc(hidden)]
    Db(&'c Db),
    #[doc(hidden)]
    Tx(&'c mut Transaction),
}

impl<'c> From<&'c Db> for Target<'c> {
    fn from(db: &'c Db) -> Self {
        Target::Db(db)
    }
}

impl<'c> From<&'c mut Transaction> for Target<'c> {
    fn from(tx: &'c mut Transaction) -> Self {
        Target::Tx(tx)
    }
}

/// Options for [`enqueue_with`]: when to run, a unique key, another queue.
#[derive(Clone, Debug, Default)]
pub struct Enqueue {
    delay: Option<Duration>,
    run_at: Option<DateTime<Utc>>,
    unique_key: Option<String>,
    queue: Option<String>,
    max_attempts: Option<u32>,
    priority: Option<i16>,
}

impl Enqueue {
    /// Default options: run as soon as a worker is free.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run no earlier than `delay` from now (by the database's clock).
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self.run_at = None;
        self
    }

    /// Run no earlier than `at`.
    pub fn run_at(mut self, at: DateTime<Utc>) -> Self {
        self.run_at = Some(at);
        self.delay = None;
        self
    }

    /// Don't enqueue if a job with this key is already waiting or running;
    /// [`enqueue_with`] then returns `None`. Once that job finishes or
    /// fails, the key is free again. E.g. `format!("onboarding:{run}:{step}")`.
    pub fn unique_key(mut self, key: impl Into<String>) -> Self {
        self.unique_key = Some(key.into());
        self
    }

    /// Put the job on this queue instead of its [`Job::QUEUE`]. A worker
    /// must [process](crate::Worker::queue) that queue.
    pub fn queue(mut self, queue: impl Into<String>) -> Self {
        self.queue = Some(queue.into());
        self
    }

    /// Overrides [`Job::MAX_ATTEMPTS`] for this job.
    pub fn max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = Some(attempts);
        self
    }

    /// Overrides [`Job::PRIORITY`] for this job: among due jobs on a queue,
    /// higher priorities run first.
    pub fn priority(mut self, priority: i16) -> Self {
        self.priority = Some(priority);
        self
    }
}

/// A row to insert into `axeon_jobs`.
pub(crate) struct NewJob<'a> {
    pub queue: &'a str,
    pub name: &'a str,
    pub payload: &'a serde_json::Value,
    pub max_attempts: i32,
    pub timeout: Duration,
    pub priority: i16,
    pub run_at: Option<DateTime<Utc>>,
    pub delay: Option<Duration>,
    pub unique_key: Option<&'a str>,
}

impl NewJob<'_> {
    /// Inserts the job, skipping a taken unique key, and wakes workers on
    /// commit. The wake-up also goes out for delayed jobs, so sleeping
    /// workers learn about an earlier due time.
    pub(crate) async fn insert<'e>(&self, ex: impl sqlx::PgExecutor<'e>) -> sqlx::Result<Option<i64>> {
        sqlx::query_scalar::<_, i64>(
            "WITH inserted AS ( \
                 INSERT INTO axeon_jobs (queue, name, payload, max_attempts, timeout_secs, run_at, unique_key, priority) \
                 VALUES ($1, $2, $3, $4, $5, COALESCE($6, now() + make_interval(secs => $7)), $8, $9) \
                 ON CONFLICT (unique_key) WHERE unique_key IS NOT NULL AND state <> 'failed' DO NOTHING \
                 RETURNING id, queue \
             ) \
             SELECT id FROM inserted CROSS JOIN LATERAL (SELECT pg_notify('axeon_jobs', inserted.queue)) AS notified",
        )
        .bind(self.queue)
        .bind(self.name)
        .bind(sqlx::types::Json(self.payload))
        .bind(self.max_attempts)
        .bind(self.timeout.as_secs_f64())
        .bind(self.run_at)
        .bind(self.delay.map(|d| d.as_secs_f64()).unwrap_or(0.0))
        .bind(self.unique_key)
        .bind(self.priority)
        .fetch_optional(ex)
        .await
    }
}

/// Adds a job to run as soon as a worker is free; returns its ID.
///
/// ```no_run
/// # use axeon_jobs::{Job, JobContext, JobResult};
/// # #[derive(serde::Serialize, serde::Deserialize)] struct SendWebhook { url: String }
/// # impl Job for SendWebhook { const NAME: &'static str = "w"; async fn run(self, _: JobContext) -> JobResult { Ok(()) } }
/// # async fn run(db: axeon_orm::Db) -> axeon_jobs::Result<()> {
/// // In a transaction: the job exists only if the transaction commits.
/// let mut tx = db.begin().await?;
/// // ... save the event ...
/// axeon_jobs::enqueue(&mut tx, &SendWebhook { url: "https://example.com/hook".into() }).await?;
/// tx.commit().await?;
/// # Ok(()) }
/// ```
pub async fn enqueue<'c, J: Job>(target: impl Into<Target<'c>>, job: &J) -> Result<i64> {
    enqueue_with(target, job, Enqueue::new()).await.map(|id| id.expect("no unique key, so never skipped"))
}

/// Adds a job with [options](Enqueue); returns its ID, or `None` when its
/// [unique key](Enqueue::unique_key) is taken.
///
/// ```no_run
/// # use axeon_jobs::{Enqueue, Job, JobContext, JobResult};
/// # use std::time::Duration;
/// # #[derive(serde::Serialize, serde::Deserialize)] struct RunStep { run: i64, step: u32 }
/// # impl Job for RunStep { const NAME: &'static str = "s"; async fn run(self, _: JobContext) -> JobResult { Ok(()) } }
/// # async fn run(db: axeon_orm::Db) -> axeon_jobs::Result<()> {
/// let step = RunStep { run: 42, step: 3 };
/// let options = Enqueue::new().delay(Duration::from_secs(30)).unique_key("run:42:step:3");
/// if axeon_jobs::enqueue_with(&db, &step, options).await?.is_none() {
///     println!("already queued");
/// }
/// # Ok(()) }
/// ```
pub async fn enqueue_with<'c, J: Job>(target: impl Into<Target<'c>>, job: &J, options: Enqueue) -> Result<Option<i64>> {
    let payload = serde_json::to_value(job).map_err(Error::Payload)?;
    let queue = options.queue.unwrap_or_else(|| J::QUEUE.to_string());
    let row = NewJob {
        queue: &queue,
        name: J::NAME,
        payload: &payload,
        max_attempts: options.max_attempts.unwrap_or(J::MAX_ATTEMPTS).max(1) as i32,
        timeout: J::TIMEOUT,
        priority: options.priority.unwrap_or(J::PRIORITY),
        run_at: options.run_at,
        delay: options.delay,
        unique_key: options.unique_key.as_deref(),
    };
    let id = match target.into() {
        Target::Db(db) => row.insert(crate::pool(db)?).await,
        Target::Tx(tx) => {
            let conn = tx.postgres().ok_or_else(|| Error::Config("axeon-jobs needs PostgreSQL".into()))?;
            row.insert(conn).await
        }
    };
    id.map_err(|e| Error::Database(e.into()))
}
