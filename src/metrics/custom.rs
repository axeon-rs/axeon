//! Application metrics registered on a [`Metrics`] registry.

use super::{escape, Metrics};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Names the built-in HTTP metrics use.
const RESERVED: [&str; 3] = ["http_requests_total", "http_request_duration_seconds", "http_requests_in_flight"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// An `f64` updated with atomics.
#[derive(Default)]
struct AtomicF64(AtomicU64);

impl AtomicF64 {
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn set(&self, value: f64) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }

    fn add(&self, delta: f64) {
        let mut current = self.0.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(current) + delta).to_bits();
            match self.0.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
}

/// One labelled series: a value, or a histogram's buckets.
struct Cell {
    value: AtomicF64,
    count: AtomicU64,
    buckets: Box<[AtomicU64]>,
}

impl Cell {
    fn new(buckets: usize) -> Self {
        Cell { value: AtomicF64::default(), count: AtomicU64::new(0), buckets: (0..buckets).map(|_| AtomicU64::new(0)).collect() }
    }
}

/// A metric name with its help text, labels and series.
pub(super) struct Family {
    name: String,
    help: String,
    kind: Kind,
    labels: Vec<String>,
    bounds: Vec<f64>,
    series: RwLock<HashMap<Vec<String>, Arc<Cell>>>,
}

impl Family {
    fn cell(self: &Arc<Self>, values: &[&str]) -> Arc<Cell> {
        assert!(
            values.len() == self.labels.len(),
            "metric `{}` has labels {:?} but got {} value(s)",
            self.name,
            self.labels,
            values.len()
        );
        let key: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        let existing = self.series.read().unwrap_or_else(|e| e.into_inner()).get(&key).cloned();
        existing.unwrap_or_else(|| {
            let mut series = self.series.write().unwrap_or_else(|e| e.into_inner());
            Arc::clone(series.entry(key).or_insert_with(|| Arc::new(Cell::new(self.bounds.len()))))
        })
    }

    pub(super) fn render(&self, out: &mut String) {
        let _ = writeln!(out, "# HELP {} {}", self.name, self.help.replace('\\', "\\\\").replace('\n', "\\n"));
        let _ = writeln!(out, "# TYPE {} {}", self.name, self.kind.as_str());
        let series = self.series.read().unwrap_or_else(|e| e.into_inner());
        let mut entries: Vec<_> = series.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (values, cell) in entries {
            let labels: Vec<String> =
                self.labels.iter().zip(values).map(|(name, value)| format!("{}=\"{}\"", name, escape(value))).collect();
            let labels = labels.join(",");
            if self.kind != Kind::Histogram {
                let _ = writeln!(out, "{}{} {}", self.name, braces(&labels), number(cell.value.get()));
                continue;
            }
            let mut cumulative = 0;
            let prefix = if labels.is_empty() { String::new() } else { format!("{},", labels) };
            for (bound, bucket) in self.bounds.iter().zip(cell.buckets.iter()) {
                cumulative += bucket.load(Ordering::Relaxed);
                let _ = writeln!(out, "{}_bucket{{{}le=\"{}\"}} {}", self.name, prefix, number(*bound), cumulative);
            }
            let count = cell.count.load(Ordering::Relaxed);
            let _ = writeln!(out, "{}_bucket{{{}le=\"+Inf\"}} {}", self.name, prefix, count);
            let _ = writeln!(out, "{}_sum{} {}", self.name, braces(&labels), number(cell.value.get()));
            let _ = writeln!(out, "{}_count{} {}", self.name, braces(&labels), count);
        }
    }
}

fn braces(labels: &str) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", labels)
    }
}

/// A sample value as Prometheus writes it.
fn number(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value.is_infinite() {
        if value > 0.0 { "+Inf" } else { "-Inf" }.into()
    } else {
        value.to_string()
    }
}

fn valid_name(name: &str, colons: bool) -> bool {
    let mut chars = name.chars();
    let ok = |c: char| c.is_ascii_alphanumeric() || c == '_' || (colons && c == ':');
    matches!(chars.next(), Some(c) if ok(c) && !c.is_ascii_digit()) && chars.all(ok)
}

impl Metrics {
    fn family(&self, name: &str, help: &str, kind: Kind, labels: &[&str], bounds: &[f64]) -> Arc<Family> {
        assert!(valid_name(name, true), "invalid metric name {:?}", name);
        assert!(!RESERVED.contains(&name), "metric name {:?} is used by the HTTP metrics", name);
        for label in labels {
            assert!(valid_name(label, false) && !label.starts_with("__"), "invalid label name {:?} on `{}`", label, name);
            assert!(!(kind == Kind::Histogram && *label == "le"), "histogram `{}` can't have an `le` label", name);
        }
        let mut sorted = bounds.to_vec();
        sorted.retain(|b| b.is_finite());
        sorted.sort_by(f64::total_cmp);
        sorted.dedup();

        let mut families = self.registry.custom.write().unwrap_or_else(|e| e.into_inner());
        if let Some(family) = families.iter().find(|f| f.name == name).cloned() {
            drop(families); // Not poisoned by the panic below.
            assert!(
                family.kind == kind && family.labels == labels && family.bounds == sorted,
                "metric `{}` is already registered as a {} with labels {:?}",
                name,
                family.kind.as_str(),
                family.labels
            );
            return family;
        }
        let family = Arc::new(Family {
            name: name.to_string(),
            help: help.to_string(),
            kind,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            bounds: sorted,
            series: RwLock::default(),
        });
        families.push(Arc::clone(&family));
        family
    }

    /// Registers a counter: a value that only goes up, such as LLM tokens
    /// used or jobs finished. Registering the same name again returns the
    /// same counter.
    ///
    /// ```
    /// use axeon::metrics::Metrics;
    ///
    /// let metrics = Metrics::new();
    /// let finished = metrics.counter("jobs_finished_total", "Jobs that ran to completion.");
    /// finished.inc();
    /// assert!(metrics.render().contains("jobs_finished_total 1"));
    /// ```
    ///
    /// # Panics
    ///
    /// When the name isn't a valid Prometheus name, belongs to the HTTP
    /// metrics, or is already registered as another kind or with labels.
    pub fn counter(&self, name: &str, help: &str) -> Counter {
        self.counter_vec(name, help, &[]).with(&[])
    }

    /// Registers a counter with labels; pick a series with
    /// [`with`](CounterVec::with).
    ///
    /// ```
    /// use axeon::metrics::Metrics;
    ///
    /// let metrics = Metrics::new();
    /// let tokens = metrics.counter_vec("llm_tokens_total", "LLM tokens used.", &["model", "kind"]);
    /// tokens.with(&["qwen-max", "prompt"]).inc_by(812.0);
    /// ```
    ///
    /// Each distinct set of label values is a series kept forever, so use
    /// values from a small, fixed set (a model name, a vendor), never IDs
    /// or user input.
    ///
    /// # Panics
    ///
    /// Like [`counter`](Metrics::counter), and when a label name is
    /// invalid.
    pub fn counter_vec(&self, name: &str, help: &str, labels: &[&str]) -> CounterVec {
        CounterVec { family: self.family(name, help, Kind::Counter, labels, &[]) }
    }

    /// Registers a gauge: a value that goes up and down, such as queue
    /// depth or open connections.
    ///
    /// # Panics
    ///
    /// Like [`counter`](Metrics::counter).
    pub fn gauge(&self, name: &str, help: &str) -> Gauge {
        self.gauge_vec(name, help, &[]).with(&[])
    }

    /// Registers a gauge with labels; see [`counter_vec`](Metrics::counter_vec).
    ///
    /// # Panics
    ///
    /// Like [`counter_vec`](Metrics::counter_vec).
    pub fn gauge_vec(&self, name: &str, help: &str, labels: &[&str]) -> GaugeVec {
        GaugeVec { family: self.family(name, help, Kind::Gauge, labels, &[]) }
    }

    /// Registers a histogram: the distribution of observed values, such as
    /// durations in seconds, in buckets with these upper bounds
    /// ([`DEFAULT_BUCKETS`](super::DEFAULT_BUCKETS) suits request-like durations).
    ///
    /// ```
    /// use axeon::metrics::{Metrics, DEFAULT_BUCKETS};
    ///
    /// let metrics = Metrics::new();
    /// let step = metrics.histogram("workflow_step_seconds", "Workflow step duration.", &DEFAULT_BUCKETS);
    /// let timer = step.start_timer();
    /// // ... run the step ...
    /// timer.observe(); // or let it drop
    /// ```
    ///
    /// # Panics
    ///
    /// Like [`counter`](Metrics::counter), also when registered before
    /// with other buckets.
    pub fn histogram(&self, name: &str, help: &str, buckets: &[f64]) -> Histogram {
        self.histogram_vec(name, help, &[], buckets).with(&[])
    }

    /// Registers a histogram with labels; see
    /// [`counter_vec`](Metrics::counter_vec).
    ///
    /// # Panics
    ///
    /// Like [`histogram`](Metrics::histogram), also for an `le` label,
    /// which Prometheus reserves for buckets.
    pub fn histogram_vec(&self, name: &str, help: &str, labels: &[&str], buckets: &[f64]) -> HistogramVec {
        HistogramVec { family: self.family(name, help, Kind::Histogram, labels, buckets) }
    }
}

/// A counter from [`Metrics::counter`] or [`CounterVec::with`]. Clones
/// update the same value.
#[derive(Clone)]
pub struct Counter {
    cell: Arc<Cell>,
}

impl Counter {
    /// Adds one.
    pub fn inc(&self) {
        self.cell.value.add(1.0);
    }

    /// Adds `amount`. Negative or NaN amounts are ignored, since a counter
    /// never goes down.
    pub fn inc_by(&self, amount: f64) {
        if amount > 0.0 {
            self.cell.value.add(amount);
        }
    }

    /// The current value.
    pub fn get(&self) -> f64 {
        self.cell.value.get()
    }
}

/// A gauge from [`Metrics::gauge`] or [`GaugeVec::with`]. Clones update the
/// same value.
#[derive(Clone)]
pub struct Gauge {
    cell: Arc<Cell>,
}

impl Gauge {
    /// Sets the value.
    pub fn set(&self, value: f64) {
        self.cell.value.set(value);
    }

    /// Adds one.
    pub fn inc(&self) {
        self.add(1.0);
    }

    /// Subtracts one.
    pub fn dec(&self) {
        self.add(-1.0);
    }

    /// Adds `delta`, which may be negative.
    pub fn add(&self, delta: f64) {
        self.cell.value.add(delta);
    }

    /// The current value.
    pub fn get(&self) -> f64 {
        self.cell.value.get()
    }
}

/// A histogram from [`Metrics::histogram`] or [`HistogramVec::with`].
/// Clones update the same buckets.
#[derive(Clone)]
pub struct Histogram {
    cell: Arc<Cell>,
    family: Arc<Family>,
}

impl Histogram {
    /// Records one value. NaN is ignored.
    pub fn observe(&self, value: f64) {
        if value.is_nan() {
            return;
        }
        self.cell.count.fetch_add(1, Ordering::Relaxed);
        self.cell.value.add(value);
        if let Some(bucket) = self.family.bounds.iter().position(|bound| value <= *bound) {
            self.cell.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Starts timing; the elapsed seconds are recorded when the
    /// [`Timer`] is observed or dropped.
    pub fn start_timer(&self) -> Timer {
        Timer { histogram: Some(self.clone()), started: Instant::now() }
    }

    /// How many values were recorded.
    pub fn count(&self) -> u64 {
        self.cell.count.load(Ordering::Relaxed)
    }

    /// The sum of the recorded values.
    pub fn sum(&self) -> f64 {
        self.cell.value.get()
    }
}

/// Times something for a [`Histogram`], from
/// [`Histogram::start_timer`]. Records the elapsed seconds once, when
/// [`observe`](Timer::observe)d or dropped.
#[must_use = "a timer records when it's dropped, so dropping it at once records ~0s"]
pub struct Timer {
    histogram: Option<Histogram>,
    started: Instant,
}

impl Timer {
    /// Records the elapsed time now; returns it in seconds.
    pub fn observe(mut self) -> f64 {
        self.record()
    }

    /// Drops the timer without recording, e.g. when the work failed and
    /// shouldn't count.
    pub fn discard(mut self) {
        self.histogram = None;
    }

    fn record(&mut self) -> f64 {
        let seconds = self.started.elapsed().as_secs_f64();
        if let Some(histogram) = self.histogram.take() {
            histogram.observe(seconds);
        }
        seconds
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.record();
    }
}

/// Counters with labels, from [`Metrics::counter_vec`].
#[derive(Clone)]
pub struct CounterVec {
    family: Arc<Family>,
}

impl CounterVec {
    /// The counter for these label values, in label order. Keep the result
    /// to skip the lookup on hot paths.
    ///
    /// # Panics
    ///
    /// When the number of values differs from the number of labels.
    pub fn with(&self, values: &[&str]) -> Counter {
        Counter { cell: self.family.cell(values) }
    }
}

/// Gauges with labels, from [`Metrics::gauge_vec`].
#[derive(Clone)]
pub struct GaugeVec {
    family: Arc<Family>,
}

impl GaugeVec {
    /// The gauge for these label values; see [`CounterVec::with`].
    ///
    /// # Panics
    ///
    /// When the number of values differs from the number of labels.
    pub fn with(&self, values: &[&str]) -> Gauge {
        Gauge { cell: self.family.cell(values) }
    }
}

/// Histograms with labels, from [`Metrics::histogram_vec`].
#[derive(Clone)]
pub struct HistogramVec {
    family: Arc<Family>,
}

impl HistogramVec {
    /// The histogram for these label values; see [`CounterVec::with`].
    ///
    /// # Panics
    ///
    /// When the number of values differs from the number of labels.
    pub fn with(&self, values: &[&str]) -> Histogram {
        Histogram { cell: self.family.cell(values), family: Arc::clone(&self.family) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked() {
        assert!(valid_name("llm_tokens_total", true) && valid_name("ns:sub_total", true) && valid_name("_x", true));
        assert!(!valid_name("", true) && !valid_name("9lives", true) && !valid_name("a-b", true) && !valid_name("a b", true));
        assert!(!valid_name("ns:x", false), "no colons in label names");
    }

    #[test]
    fn numbers_render_like_prometheus() {
        assert_eq!(number(1.0), "1");
        assert_eq!(number(0.25), "0.25");
        assert_eq!(number(f64::INFINITY), "+Inf");
        assert_eq!(number(f64::NEG_INFINITY), "-Inf");
        assert_eq!(number(f64::NAN), "NaN");
    }

    #[test]
    fn label_lookup_matches_owned_keys() {
        let metrics = Metrics::new();
        let vec = metrics.counter_vec("lookups_total", "x", &["a", "b"]);
        vec.with(&["1", "2"]).inc();
        vec.with(&["1", "2"]).inc();
        vec.with(&["12", ""]).inc();
        let family = &vec.family;
        assert_eq!(family.series.read().unwrap().len(), 2, "same values share a series");
        assert_eq!(vec.with(&["1", "2"]).get(), 2.0);
    }
}
