//! Looking at and managing jobs, for operator screens and scripts.

use crate::{pool, Error, Result};
use axeon_orm::Db;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::time::Duration;

/// Operations on the jobs table: counts, failed jobs, retry and cancel.
///
/// ```no_run
/// # async fn run(db: axeon_orm::Db) -> axeon_jobs::Result<()> {
/// let jobs = axeon_jobs::Jobs::new(db);
/// for failed in jobs.failed(50).await? {
///     println!("{} {} after {} attempts: {}", failed.id, failed.name, failed.attempts, failed.last_error.unwrap_or_default());
///     jobs.retry(failed.id).await?;
/// }
/// # Ok(()) }
/// ```
#[derive(Clone, Debug)]
pub struct Jobs {
    db: Db,
}

/// How many jobs a queue has in each state, from [`Jobs::counts`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
#[non_exhaustive]
pub struct QueueCounts {
    /// The queue.
    pub queue: String,
    /// Due now, waiting for a worker.
    pub ready: i64,
    /// Waiting for a later time: delayed, or retrying after a backoff.
    pub scheduled: i64,
    /// Running now.
    pub running: i64,
    /// Out of attempts, or failed for good.
    pub failed: i64,
}

/// A job that won't run again unless [retried](Jobs::retry), from
/// [`Jobs::failed`].
#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
#[non_exhaustive]
pub struct FailedJob {
    /// Its ID.
    pub id: i64,
    /// Its queue.
    pub queue: String,
    /// Its [`Job::NAME`](crate::Job::NAME).
    pub name: String,
    /// Its input.
    pub payload: sqlx::types::Json<serde_json::Value>,
    /// How many times it ran.
    pub attempts: i32,
    /// The last run's error.
    pub last_error: Option<String>,
    /// When it was enqueued.
    pub created_at: DateTime<Utc>,
    /// When it failed.
    pub failed_at: DateTime<Utc>,
}

impl Jobs {
    /// Operations on `db`'s jobs.
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Job counts per queue and state, for dashboards and alerts.
    pub async fn counts(&self) -> Result<Vec<QueueCounts>> {
        sqlx::query_as(
            "SELECT queue, \
                 count(*) FILTER (WHERE state = 'pending' AND run_at <= now()) AS ready, \
                 count(*) FILTER (WHERE state = 'pending' AND run_at > now()) AS scheduled, \
                 count(*) FILTER (WHERE state = 'running') AS running, \
                 count(*) FILTER (WHERE state = 'failed') AS failed \
             FROM axeon_jobs GROUP BY queue ORDER BY queue",
        )
        .fetch_all(pool(&self.db)?)
        .await
        .map_err(|e| Error::Database(e.into()))
    }

    /// The most recently failed jobs, newest first.
    pub async fn failed(&self, limit: u32) -> Result<Vec<FailedJob>> {
        sqlx::query_as(
            "SELECT id, queue, name, payload, attempts, last_error, created_at, updated_at AS failed_at \
             FROM axeon_jobs WHERE state = 'failed' ORDER BY updated_at DESC, id DESC LIMIT $1",
        )
        .bind(i64::from(limit))
        .fetch_all(pool(&self.db)?)
        .await
        .map_err(|e| Error::Database(e.into()))
    }

    /// Runs a failed job again now, with a fresh set of attempts. Returns
    /// whether it was failed. Fails with a unique violation if another job
    /// with its unique key is already waiting or running.
    pub async fn retry(&self, id: i64) -> Result<bool> {
        let retried = sqlx::query(
            "WITH retried AS ( \
                 UPDATE axeon_jobs SET state = 'pending', attempts = 0, run_at = now(), updated_at = now() \
                 WHERE id = $1 AND state = 'failed' RETURNING queue \
             ) \
             SELECT 1 FROM retried CROSS JOIN LATERAL (SELECT pg_notify('axeon_jobs', retried.queue)) AS notified",
        )
        .bind(id)
        .fetch_optional(pool(&self.db)?)
        .await
        .map_err(|e| Error::Database(e.into()))?;
        Ok(retried.is_some())
    }

    /// Deletes a waiting or failed job. Returns whether it was deleted; a
    /// running job isn't.
    pub async fn cancel(&self, id: i64) -> Result<bool> {
        let deleted = sqlx::query("DELETE FROM axeon_jobs WHERE id = $1 AND state <> 'running'")
            .bind(id)
            .execute(pool(&self.db)?)
            .await
            .map_err(|e| Error::Database(e.into()))?;
        Ok(deleted.rows_affected() == 1)
    }

    /// Deletes failed jobs that failed more than `age` ago; returns how
    /// many. Run it from a scheduled job to keep the table small.
    pub async fn prune_failed(&self, age: Duration) -> Result<u64> {
        let deleted = sqlx::query("DELETE FROM axeon_jobs WHERE state = 'failed' AND updated_at < now() - make_interval(secs => $1)")
            .bind(age.as_secs_f64())
            .execute(pool(&self.db)?)
            .await
            .map_err(|e| Error::Database(e.into()))?;
        Ok(deleted.rows_affected())
    }
}
