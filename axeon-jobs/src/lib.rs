//! Background jobs on PostgreSQL, for [Axeon](https://docs.rs/axeon) apps
//! using [axeon-orm](https://docs.rs/axeon-orm).
//!
//! Jobs live in one `axeon_jobs` table, so they are saved with the same
//! transaction as the data they belong to, and no extra infrastructure is
//! needed. Workers take jobs with `FOR UPDATE SKIP LOCKED`, wake up on
//! `LISTEN`/`NOTIFY`, and retry failures with backoff.
//!
//! ```no_run
//! use axeon_jobs::{Enqueue, Job, JobContext, JobResult, Worker};
//! use serde::{Deserialize, Serialize};
//! use std::time::Duration;
//!
//! #[derive(Serialize, Deserialize)]
//! struct SendWebhook {
//!     url: String,
//!     event_id: i64,
//! }
//!
//! impl Job for SendWebhook {
//!     const NAME: &'static str = "send_webhook";
//!     const QUEUE: &'static str = "webhooks";
//!
//!     async fn run(self, ctx: JobContext) -> JobResult {
//!         println!("attempt {}: POST {}", ctx.attempt(), self.url);
//!         Ok(())
//!     }
//! }
//!
//! # async fn run(db: axeon_orm::Db) -> axeon_jobs::Result<()> {
//! // Once: create the table (or add `axeon_jobs::migration()` to your migrations).
//! axeon_jobs::install(&db).await?;
//!
//! // Anywhere, e.g. in a request handler, with `&db` or `&mut tx`:
//! let job = SendWebhook { url: "https://example.com/hook".into(), event_id: 7 };
//! axeon_jobs::enqueue(&db, &job).await?;
//! axeon_jobs::enqueue_with(&db, &job, Enqueue::new().delay(Duration::from_secs(60)).unique_key("event:7")).await?;
//!
//! // In the worker process:
//! Worker::new(db)
//!     .register::<SendWebhook>()
//!     .queue("webhooks", 16)
//!     .run_until(async { tokio::signal::ctrl_c().await.unwrap() })
//!     .await?;
//! # Ok(()) }
//! ```
//!
//! # How jobs run
//!
//! - A worker takes due jobs from each of its queues, up to the queue's
//!   concurrency, and runs each with its [`Job::TIMEOUT`].
//! - Success deletes the job. An error, a panic or a timeout schedules a
//!   retry after [`Job::backoff`]; after [`Job::MAX_ATTEMPTS`] runs, or a
//!   [`JobError::fatal`], the job is marked `failed` and kept for
//!   [inspection and retry](Jobs).
//! - A taken job is leased for its timeout plus 30 seconds. If the worker
//!   dies, another worker takes the job when the lease ends; that counts as
//!   an attempt.
//! - Jobs run **at least once**: after a crash, a job that did its work but
//!   wasn't recorded as done runs again. Make jobs safe to repeat, e.g. with
//!   the job's [ID](JobContext::id) as an idempotency key.
//!
//! # Priorities and schedules
//!
//! Among due jobs on a queue, higher [`Job::PRIORITY`] (or
//! [`Enqueue::priority`]) values are taken first.
//!
//! [`Worker::schedule`] enqueues a job on a [`Cron`] schedule, such as a
//! nightly re-screen at `0 3 * * *` (UTC). However many workers declare a
//! schedule, each run is enqueued once.

#![warn(missing_docs)]

mod admin;
mod cron;
mod enqueue;
mod job;
#[cfg(feature = "metrics")]
mod metrics;
mod worker;

pub use admin::{FailedJob, Jobs, QueueCounts};
pub use cron::{Cron, CronError};
pub use enqueue::{enqueue, enqueue_with, Enqueue, Target};
pub use job::{default_backoff, Job, JobContext, JobError, JobResult};
pub use worker::Worker;

use axeon_orm::migrate::Migration;
use axeon_orm::Db;
use std::fmt;

/// The `axeon_jobs` table and its indexes. Safe to run again.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS axeon_jobs (
    id BIGSERIAL PRIMARY KEY,
    queue TEXT NOT NULL,
    name TEXT NOT NULL,
    payload JSONB NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'running', 'failed')),
    attempts INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL,
    timeout_secs DOUBLE PRECISION NOT NULL,
    run_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_until TIMESTAMPTZ,
    locked_by TEXT,
    last_error TEXT,
    unique_key TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS axeon_jobs_due ON axeon_jobs (queue, run_at) WHERE state = 'pending';
CREATE INDEX IF NOT EXISTS axeon_jobs_leases ON axeon_jobs (queue, locked_until) WHERE state = 'running';
CREATE INDEX IF NOT EXISTS axeon_jobs_failed ON axeon_jobs (updated_at) WHERE state = 'failed';
CREATE UNIQUE INDEX IF NOT EXISTS axeon_jobs_unique ON axeon_jobs (unique_key)
    WHERE unique_key IS NOT NULL AND state <> 'failed';
"#;

/// Priorities and recurring schedules, added in 0.2. Safe to run again.
pub const SCHEMA_PRIORITY_AND_SCHEDULES: &str = r#"
ALTER TABLE axeon_jobs ADD COLUMN IF NOT EXISTS priority SMALLINT NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS axeon_jobs_ready ON axeon_jobs (queue, priority DESC, run_at, id) WHERE state = 'pending';
CREATE TABLE IF NOT EXISTS axeon_jobs_schedules (
    name TEXT PRIMARY KEY,
    expression TEXT NOT NULL,
    next_run_at TIMESTAMPTZ NOT NULL,
    last_run_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
"#;

/// The schema as migrations, to run with your own:
///
/// ```no_run
/// # async fn run(db: axeon_orm::Db) -> axeon_orm::Result<()> {
/// use axeon_orm::migrate::Migrator;
///
/// let mut migrations = Migrator::from_dir("migrations")?.migrations().to_vec();
/// migrations.extend(axeon_jobs::migrations());
/// Migrator::new(migrations).run(&db).await?;
/// # Ok(()) }
/// ```
///
/// Or copy [`SCHEMA`] and [`SCHEMA_PRIORITY_AND_SCHEDULES`] into migration
/// files of your own.
pub fn migrations() -> Vec<Migration> {
    vec![
        Migration::new(20261021000000, "axeon_jobs", SCHEMA),
        Migration::new(20261022000000, "axeon_jobs_priority_and_schedules", SCHEMA_PRIORITY_AND_SCHEDULES),
    ]
}

/// The first schema migration only, from 0.1. Use [`migrations`], which
/// also adds priorities and schedules.
#[deprecated(since = "0.2.0", note = "use `migrations()`, which includes priorities and schedules")]
pub fn migration() -> Migration {
    Migration::new(20261021000000, "axeon_jobs", SCHEMA)
}

/// Creates the tables if they don't exist, and upgrades them; handy for
/// tests and prototypes.
pub async fn install(db: &Db) -> Result<()> {
    pool(db)?;
    db.execute(SCHEMA).await.map_err(Error::Database)?;
    db.execute(SCHEMA_PRIORITY_AND_SCHEDULES).await.map_err(Error::Database)
}

/// What can go wrong enqueueing, managing or running jobs.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A database error.
    Database(axeon_orm::Error),
    /// The job couldn't be stored as JSON.
    Payload(serde_json::Error),
    /// Bad setup: not PostgreSQL, or no registered jobs.
    Config(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Database(error) => write!(f, "{}", error),
            Error::Payload(error) => write!(f, "can't serialize the job: {}", error),
            Error::Config(message) => write!(f, "{}", message),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Database(error) => Some(error),
            Error::Payload(error) => Some(error),
            Error::Config(_) => None,
        }
    }
}

impl From<axeon_orm::Error> for Error {
    fn from(error: axeon_orm::Error) -> Self {
        Error::Database(error)
    }
}

/// A `Result` with [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn pool(db: &Db) -> Result<&sqlx::PgPool> {
    db.postgres().ok_or_else(|| Error::Config(format!("axeon-jobs needs PostgreSQL, not {:?}", db.backend())))
}
