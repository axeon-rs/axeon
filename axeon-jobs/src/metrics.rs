//! Job metrics on an Axeon registry.

use axeon::metrics::{CounterVec, GaugeVec, HistogramVec, Metrics};
use std::time::Duration;

/// Job durations reach well past HTTP ones: up to 15 minutes.
const BUCKETS: [f64; 12] = [0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0];

pub(crate) struct WorkerMetrics {
    completed: CounterVec,
    failed: CounterVec,
    duration: HistogramVec,
    running: GaugeVec,
}

impl WorkerMetrics {
    pub(crate) fn new(metrics: &Metrics) -> Self {
        Self {
            completed: metrics.counter_vec("jobs_completed_total", "Jobs that ran successfully.", &["queue", "job"]),
            failed: metrics.counter_vec(
                "jobs_failed_total",
                "Job runs that failed; outcome is retry or failed (no attempts left).",
                &["queue", "job", "outcome"],
            ),
            duration: metrics.histogram_vec("job_duration_seconds", "How long job runs took.", &["queue", "job"], &BUCKETS),
            running: metrics.gauge_vec("jobs_running", "Jobs running now.", &["queue"]),
        }
    }

    /// Counts a job as running until the guard drops.
    pub(crate) fn start(&self, queue: &str, _job: &str) -> Running {
        let gauge = self.running.with(&[queue]);
        gauge.inc();
        Running(gauge)
    }

    pub(crate) fn finish(&self, queue: &str, job: &str, outcome: &str, elapsed: Duration) {
        self.duration.with(&[queue, job]).observe(elapsed.as_secs_f64());
        match outcome {
            "completed" => self.completed.with(&[queue, job]).inc(),
            outcome => self.failed.with(&[queue, job, outcome]).inc(),
        }
    }
}

pub(crate) struct Running(axeon::metrics::Gauge);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.dec();
    }
}
