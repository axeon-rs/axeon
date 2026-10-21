//! The [`Job`] trait, what a job returns, and what it can see while it runs.

use axeon_orm::Db;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Work to run in the background. The struct is the job's input: it is
/// stored as JSON when enqueued and read back when a worker runs it.
///
/// ```
/// use axeon_jobs::{Job, JobContext, JobResult};
/// use serde::{Deserialize, Serialize};
/// use std::time::Duration;
///
/// #[derive(Serialize, Deserialize)]
/// struct SendWebhook {
///     url: String,
///     event_id: i64,
/// }
///
/// impl Job for SendWebhook {
///     const NAME: &'static str = "send_webhook";
///     const QUEUE: &'static str = "webhooks";
///     const MAX_ATTEMPTS: u32 = 8;
///     const TIMEOUT: Duration = Duration::from_secs(30);
///
///     async fn run(self, ctx: JobContext) -> JobResult {
///         // deliver(&self.url, self.event_id).await?;  any error retries the job
///         Ok(())
///     }
/// }
/// ```
pub trait Job: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// The name the job is stored under. Keep it stable: jobs already in
    /// the table are found by it after a deploy.
    const NAME: &'static str;
    /// The queue it runs on, unless [`Enqueue::queue`](crate::Enqueue::queue)
    /// says otherwise. Each queue has its own concurrency in a
    /// [`Worker`](crate::Worker).
    const QUEUE: &'static str = "default";
    /// How many times it may run before it is marked failed. A crash or a
    /// timeout counts as a run.
    const MAX_ATTEMPTS: u32 = 5;
    /// How long one run may take. After that it is cancelled and retried;
    /// if the worker died, another worker takes it over a little later.
    const TIMEOUT: Duration = Duration::from_secs(300);

    /// The wait before the next run after `attempt` (from 1) failed. By
    /// default 2s, 4s, 8s... up to an hour, with some jitter.
    fn backoff(attempt: u32) -> Duration {
        default_backoff(attempt)
    }

    /// Does the work. Return [`JobError`] to retry (`?` on any error does
    /// that) or [`JobError::fatal`] to give up at once.
    fn run(self, ctx: JobContext) -> impl Future<Output = JobResult> + Send;
}

/// What a [`Job`] returns.
pub type JobResult = Result<(), JobError>;

/// Exponential backoff from 2 seconds, doubling up to an hour, ±20% so
/// failures from one outage don't all retry at the same moment.
pub fn default_backoff(attempt: u32) -> Duration {
    let base = 2f64 * 2f64.powi(attempt.saturating_sub(1).min(20) as i32);
    let capped = base.min(3600.0);
    // Cheap jitter without a random number crate.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let jitter = 0.8 + 0.4 * (nanos % 1000) as f64 / 1000.0;
    Duration::from_secs_f64(capped * jitter)
}

/// Why a [`Job`] run failed, and whether to try again.
///
/// Any `std::error::Error` converts into a retryable `JobError`, so `?`
/// works in [`Job::run`].
pub struct JobError {
    message: String,
    fatal: bool,
    retry_after: Option<Duration>,
}

impl JobError {
    /// Retry after the job's [backoff](Job::backoff), if attempts are left.
    pub fn retry(error: impl fmt::Display) -> Self {
        Self { message: error.to_string(), fatal: false, retry_after: None }
    }

    /// Retry after `delay` instead of the backoff, e.g. when a vendor's
    /// rate limit says when to come back.
    pub fn retry_after(error: impl fmt::Display, delay: Duration) -> Self {
        Self { message: error.to_string(), fatal: false, retry_after: Some(delay) }
    }

    /// Don't retry: mark the job failed now, e.g. for input that will never
    /// work.
    pub fn fatal(error: impl fmt::Display) -> Self {
        Self { message: error.to_string(), fatal: true, retry_after: None }
    }

    /// The error message, stored as the job's `last_error`.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether the job won't be retried.
    pub fn is_fatal(&self) -> bool {
        self.fatal
    }

    pub(crate) fn delay(&self) -> Option<Duration> {
        self.retry_after
    }
}

impl<E: std::error::Error + Send + Sync + 'static> From<E> for JobError {
    fn from(error: E) -> Self {
        JobError::retry(error)
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl fmt::Debug for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobError").field("message", &self.message).field("fatal", &self.fatal).finish()
    }
}

/// Values shared with every job, from [`Worker::state`](crate::Worker::state).
#[derive(Clone, Default)]
pub(crate) struct State(Arc<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>);

impl State {
    pub(crate) fn insert<T: Send + Sync + 'static>(&mut self, value: T) {
        Arc::make_mut(&mut self.0).insert(TypeId::of::<T>(), Arc::new(value));
    }

    fn get<T: 'static>(&self) -> Option<&T> {
        self.0.get(&TypeId::of::<T>()).and_then(|v| v.downcast_ref())
    }
}

/// What a running job can see: its ID and attempt, the database, shared
/// state, and whether the worker is shutting down.
#[derive(Clone)]
pub struct JobContext {
    pub(crate) id: i64,
    pub(crate) attempt: u32,
    pub(crate) queue: Arc<str>,
    pub(crate) db: Db,
    pub(crate) state: State,
    pub(crate) shutdown: watch::Receiver<bool>,
}

impl JobContext {
    /// The job's ID in the `axeon_jobs` table.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Which run this is, from 1.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The queue it runs on.
    pub fn queue(&self) -> &str {
        &self.queue
    }

    /// The worker's database.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// A value added with [`Worker::state`](crate::Worker::state), such as
    /// an HTTP client or an LLM client.
    pub fn state<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.state.get()
    }

    /// Whether the worker has been asked to stop. Long jobs can check this
    /// and return early; a job still running when the worker's
    /// shutdown timeout ends is cancelled and run again later.
    pub fn is_shutting_down(&self) -> bool {
        *self.shutdown.borrow()
    }

    /// Completes when the worker is asked to stop.
    pub async fn shutdown(&self) {
        let mut shutdown = self.shutdown.clone();
        let _ = shutdown.wait_for(|stopping| *stopping).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let secs = |n| default_backoff(n).as_secs_f64();
        assert!((1.6..=2.4).contains(&secs(1)), "{}", secs(1));
        assert!((3.2..=4.8).contains(&secs(2)));
        assert!((6.4..=9.6).contains(&secs(3)));
        assert!((2880.0..=4320.0).contains(&secs(30)), "capped at an hour");
        assert!(secs(u32::MAX) <= 4320.0);
    }

    #[test]
    fn errors_convert() {
        fn fails() -> JobResult {
            "x".parse::<i32>()?;
            Ok(())
        }
        let err = fails().unwrap_err();
        assert!(!err.is_fatal());
        assert_eq!(err.message(), "invalid digit found in string");
        assert!(JobError::fatal("bad input").is_fatal());
        assert_eq!(JobError::retry_after("slow down", Duration::from_secs(9)).delay(), Some(Duration::from_secs(9)));
    }
}
