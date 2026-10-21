//! Health checks, metrics and the matched route.

mod common;

use axeon::health::Health;
use axeon::metrics::{Metrics, DEFAULT_BUCKETS};
use axeon::{Method, Request, Server, ServerError};
use std::time::Duration;

#[tokio::test]
async fn liveness_and_readiness() {
    let mut server = Server::new();
    server.mount(
        "/",
        Health::new()
            .timeout(Duration::from_millis(100))
            .check("database", || async { Ok::<_, std::io::Error>(()) })
            .check("cache", || async { Err::<(), _>("connection refused") })
            .check("slow", || async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok::<_, std::io::Error>(())
            })
            .router(),
    );
    let app = server.into_app().await.unwrap();

    let live = app.call(Request::new(Method::GET, "/health")).await;
    assert_eq!((live.status, live.body_text().as_ref()), (200, r#"{"status":"ok"}"#));

    let started = std::time::Instant::now();
    let ready = app.call(Request::new(Method::GET, "/ready")).await;
    assert!(started.elapsed() < Duration::from_secs(1), "checks didn't run concurrently with a timeout");
    assert_eq!(ready.status, 503);
    let body: serde_json::Value = serde_json::from_slice(&ready.body).unwrap();
    assert_eq!(body["status"], "unavailable");
    assert_eq!(body["checks"]["database"]["status"], "up");
    assert_eq!(body["checks"]["cache"]["error"], "connection refused");
    assert!(body["checks"]["slow"]["error"].as_str().unwrap().contains("timed out"));
}

#[tokio::test]
async fn readiness_fails_during_the_shutdown_delay() {
    let mut server = Server::new();
    server.shutdown_delay(Duration::from_millis(300));
    server.mount("/", Health::new().router());
    server.get("/", |_req| async { "still serving" });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move { server.serve_with_shutdown(listener, async { let _ = rx.await; }).await });

    assert_eq!(common::Client::connect(addr).await.get("/ready").await.status, 200);
    tx.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // During the delay new connections are still accepted and served, but
    // readiness reports shutting down so load balancers move traffic away.
    let mut c = common::Client::connect(addr).await;
    let ready = c.get("/ready").await;
    assert_eq!(ready.status, 503);
    assert!(ready.text().contains("shutting_down"));
    assert_eq!(c.get("/").await.text(), "still serving");

    // After the delay the server stops.
    tokio::time::timeout(Duration::from_secs(2), server).await.expect("server did not stop").unwrap().unwrap();
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn metrics_count_requests_by_route_pattern() {
    let metrics = Metrics::new();
    let mut server = Server::new();
    server.middleware(metrics.clone());
    server.get("/users/:id", |req| async move { req.matched_path().unwrap_or("").to_string() });
    server.get("/fail", |_req| async { Err::<&'static str, _>(ServerError::InternalError("x".into())) });
    server.mount("/", metrics.router());
    let app = server.into_app().await.unwrap();

    let res = app.call(Request::new(Method::GET, "/users/1")).await;
    assert_eq!(res.body_text(), "/users/:id");
    app.call(Request::new(Method::GET, "/users/2")).await;
    app.call(Request::new(Method::GET, "/fail")).await;
    app.call(Request::new(Method::GET, "/nope")).await;

    let res = app.call(Request::new(Method::GET, "/metrics")).await;
    assert!(res.get_header("content-type").unwrap().starts_with("text/plain; version=0.0.4"));
    let text = res.body_text().to_string();
    assert!(text.contains(r#"http_requests_total{method="GET",route="/users/:id",status="200"} 2"#), "{}", text);
    assert!(text.contains(r#"http_requests_total{method="GET",route="/fail",status="500"} 1"#), "{}", text);
    assert!(text.contains(r#"http_requests_total{method="GET",route="unmatched",status="404"} 1"#), "{}", text);
    assert!(text.contains(r#"http_request_duration_seconds_bucket{method="GET",route="/users/:id",status="200",le="+Inf"} 2"#), "{}", text);
    assert!(text.contains(r#"http_request_duration_seconds_count{method="GET",route="/users/:id",status="200"} 2"#), "{}", text);
    // The /metrics request itself is still in flight while rendering.
    assert!(text.contains("http_requests_in_flight 1"), "{}", text);
    // Buckets are cumulative and never decrease.
    let buckets: Vec<u64> = text
        .lines()
        .filter(|l| l.starts_with(r#"http_request_duration_seconds_bucket{method="GET",route="/users/:id""#))
        .map(|l| l.rsplit(' ').next().unwrap().parse().unwrap())
        .collect();
    assert!(buckets.windows(2).all(|w| w[0] <= w[1]), "{:?}", buckets);
}

#[tokio::test]
async fn application_metrics_share_the_endpoint() {
    let metrics = Metrics::new();
    let jobs = metrics.counter("jobs_finished_total", "Jobs that ran to completion.");
    let tokens = metrics.counter_vec("llm_tokens_total", "LLM tokens used.", &["model", "kind"]);
    let depth = metrics.gauge_vec("queue_depth", "Jobs waiting.", &["queue"]);
    let cost = metrics.gauge("llm_budget_left", "Budget left today, in CNY.");
    let vendor = metrics.histogram_vec("vendor_seconds", "Vendor call duration.", &["vendor"], &[0.1, 1.0, 0.5]);

    let mut server = Server::new();
    server.middleware(metrics.clone());
    let handler_jobs = jobs.clone();
    server.post("/jobs", move |_req| {
        handler_jobs.inc();
        async { "done" }
    });
    server.mount("/", metrics.router());
    let app = server.into_app().await.unwrap();
    app.call(Request::new(Method::POST, "/jobs")).await;
    app.call(Request::new(Method::POST, "/jobs")).await;

    tokens.with(&["qwen-max", "prompt"]).inc_by(812.0);
    tokens.with(&["qwen-max", "completion"]).inc_by(40.5);
    tokens.with(&["qwen-max", "prompt"]).inc_by(-5.0); // ignored: counters only go up
    tokens.with(&["qwen-max", "prompt"]).inc_by(f64::NAN); // ignored
    depth.with(&["onboarding"]).set(17.0);
    depth.with(&["onboarding"]).dec();
    depth.with(&["catalog"]).add(-2.0);
    cost.set(1250.75);
    let kyb = vendor.with(&["kyb"]);
    kyb.observe(0.05);
    kyb.observe(0.7);
    kyb.observe(3.0);
    kyb.observe(f64::NAN); // ignored
    assert_eq!((kyb.count(), kyb.sum()), (3, 3.75));
    assert_eq!(jobs.get(), 2.0);

    let text = app.call(Request::new(Method::GET, "/metrics")).await.body_text().to_string();
    let expected = [
        "# HELP jobs_finished_total Jobs that ran to completion.",
        "# TYPE jobs_finished_total counter",
        "jobs_finished_total 2",
        "# TYPE llm_tokens_total counter",
        r#"llm_tokens_total{model="qwen-max",kind="completion"} 40.5"#,
        r#"llm_tokens_total{model="qwen-max",kind="prompt"} 812"#,
        "# TYPE queue_depth gauge",
        r#"queue_depth{queue="catalog"} -2"#,
        r#"queue_depth{queue="onboarding"} 16"#,
        "llm_budget_left 1250.75",
        "# TYPE vendor_seconds histogram",
        // Buckets sorted, cumulative, then +Inf, sum and count.
        r#"vendor_seconds_bucket{vendor="kyb",le="0.1"} 1"#,
        r#"vendor_seconds_bucket{vendor="kyb",le="0.5"} 1"#,
        r#"vendor_seconds_bucket{vendor="kyb",le="1"} 2"#,
        r#"vendor_seconds_bucket{vendor="kyb",le="+Inf"} 3"#,
        r#"vendor_seconds_sum{vendor="kyb"} 3.75"#,
        r#"vendor_seconds_count{vendor="kyb"} 3"#,
    ];
    for line in expected {
        assert!(text.lines().any(|l| l == line), "missing {:?} in\n{}", line, text);
    }
    // HTTP metrics come first and are unchanged.
    assert!(text.starts_with("# HELP http_requests_total"), "{}", text);
    assert!(text.contains(r#"http_requests_total{method="POST",route="/jobs",status="200"} 2"#), "{}", text);
    // Series of a family are grouped right under its TYPE line.
    let at = |needle: &str| text.find(needle).unwrap();
    assert!(at("# TYPE queue_depth") < at("queue_depth{queue=\"catalog\"}") && at("queue_depth{queue=\"onboarding\"}") < at("# HELP llm_budget_left"));
    // Every sample line has the `name{labels} value` shape.
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let (series, value) = line.rsplit_once(' ').unwrap();
        assert!(value.parse::<f64>().is_ok() || value == "+Inf", "{}", line);
        assert!(series.chars().next().unwrap().is_ascii_alphabetic(), "{}", line);
    }
}

#[test]
fn metric_registration_rules() {
    let metrics = Metrics::new();
    // Registering again gives the same metric.
    metrics.counter("hits_total", "Hits.").inc();
    metrics.counter("hits_total", "Hits.").inc();
    assert_eq!(metrics.counter("hits_total", "Hits.").get(), 2.0);
    let a = metrics.histogram("step_seconds", "Steps.", &DEFAULT_BUCKETS);
    let b = metrics.histogram("step_seconds", "Steps.", &DEFAULT_BUCKETS);
    a.observe(1.0);
    assert_eq!(b.count(), 1);

    // Label values are escaped; help text keeps one line.
    let odd = metrics.counter_vec("odd_total", "Line one\nline two", &["path"]);
    odd.with(&["a\"b\\c\nd"]).inc();
    let text = metrics.render();
    assert!(text.contains(r#"odd_total{path="a\"b\\c\nd"} 1"#), "{}", text);
    assert!(text.contains("# HELP odd_total Line one\\nline two\n"), "{}", text);

    let panics = |f: Box<dyn FnOnce() + std::panic::UnwindSafe>| std::panic::catch_unwind(f).is_err();
    let m = metrics.clone();
    assert!(panics(Box::new(move || drop(m.gauge("hits_total", "Hits.")))), "another kind");
    let m = metrics.clone();
    assert!(panics(Box::new(move || drop(m.counter_vec("hits_total", "Hits.", &["x"])))), "other labels");
    let m = metrics.clone();
    assert!(panics(Box::new(move || drop(m.histogram("step_seconds", "Steps.", &[1.0])))), "other buckets");
    let m = metrics.clone();
    assert!(panics(Box::new(move || drop(m.counter("http_requests_total", "Mine.")))), "reserved");
    for bad in ["", "9lives", "has-dash", "has space"] {
        let m = metrics.clone();
        assert!(panics(Box::new(move || drop(m.counter(bad, "Bad.")))), "name {:?}", bad);
    }
    for bad in ["__reserved", "a:b", "1x"] {
        let m = metrics.clone();
        assert!(panics(Box::new(move || drop(m.counter_vec("bad_labels_total", "Bad.", &[bad])))), "label {:?}", bad);
    }
    let m = metrics.clone();
    assert!(panics(Box::new(move || drop(m.histogram_vec("h_seconds", "H.", &["le"], &DEFAULT_BUCKETS)))), "le");
    let m = metrics.clone();
    assert!(panics(Box::new(move || drop(m.counter_vec("two_total", "Two.", &["a", "b"]).with(&["only one"])))), "label count");
}

#[test]
fn timers_record_once() {
    let metrics = Metrics::new();
    let step = metrics.histogram("timed_seconds", "Timed.", &DEFAULT_BUCKETS);
    {
        let _timer = step.start_timer();
        std::thread::sleep(Duration::from_millis(20));
    } // dropped: recorded
    let seconds = step.start_timer().observe();
    assert!(seconds < 0.02);
    step.start_timer().discard();
    assert_eq!(step.count(), 2);
    assert!(step.sum() >= 0.02, "{}", step.sum());
}

#[test]
fn updates_from_many_threads_add_up() {
    let metrics = Metrics::new();
    let counter = metrics.counter_vec("parallel_total", "Parallel.", &["worker"]);
    let gauge = metrics.gauge("parallel_level", "Level.");
    let histogram = metrics.histogram("parallel_seconds", "Seconds.", &[0.5]);
    std::thread::scope(|s| {
        for i in 0..8 {
            let (counter, gauge, histogram) = (counter.clone(), gauge.clone(), histogram.clone());
            s.spawn(move || {
                // Half the threads share a series, created concurrently.
                let mine = counter.with(&[if i % 2 == 0 { "even" } else { "odd" }]);
                for _ in 0..10_000 {
                    mine.inc();
                    gauge.add(0.5);
                    histogram.observe(0.25);
                }
            });
        }
    });
    assert_eq!(counter.with(&["even"]).get() + counter.with(&["odd"]).get(), 80_000.0);
    assert_eq!(gauge.get(), 40_000.0);
    assert_eq!((histogram.count(), histogram.sum()), (80_000, 20_000.0));
    assert!(metrics.render().contains(r#"parallel_seconds_bucket{le="0.5"} 80000"#));
}
