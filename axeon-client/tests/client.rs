use axeon::{Request as AxeonRequest, Response, Server, ServerError};
use axeon_client::{Client, Egress, Error};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

async fn start(app: Server) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = app.serve(listener).await;
    });
    addr
}

/// A server whose `/flaky` fails with `status` until hit `failures` times,
/// and that counts every hit.
async fn flaky(failures: usize, status: u16, retry_after: Option<&'static str>) -> (SocketAddr, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let mut app = Server::new();
    let handler = move |_req: AxeonRequest| {
        let counter = Arc::clone(&counter);
        async move {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n < failures {
                let mut res = Response::new(status);
                if let Some(after) = retry_after {
                    res.header("Retry-After", after);
                }
                res
            } else {
                Response::new(200).with_body("ok")
            }
        }
    };
    app.get("/flaky", handler.clone()).post("/flaky", handler);
    (start(app).await, hits)
}

/// A client for local test servers.
fn local() -> axeon_client::ClientBuilder {
    Client::builder("test").egress(Egress::public().allow_private()).backoff(Duration::from_millis(10), Duration::from_millis(50))
}

#[tokio::test]
async fn blocks_internal_destinations_by_default() {
    let (addr, hits) = flaky(0, 200, None).await;
    let client = Client::builder("crawler").build().unwrap();
    let port = addr.port();
    for url in [
        format!("http://127.0.0.1:{}/flaky", port),
        format!("http://localhost:{}/flaky", port), // a name resolving to loopback
        format!("http://[::ffff:127.0.0.1]:{}/flaky", port),
        "http://169.254.169.254/latest/meta-data/".to_string(),
        "http://100.100.100.200/latest/meta-data/".to_string(),
        "http://10.0.0.1/".to_string(),
        "http://[::1]/".to_string(),
    ] {
        match client.get(&url).send().await {
            Err(Error::Blocked(blocked)) => assert!(blocked.to_string().starts_with("request blocked: "), "{}", blocked),
            other => panic!("{} wasn't blocked: {:?}", url, other.map(|r| r.status())),
        }
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0, "the server was never contacted");
}

#[tokio::test]
async fn allow_lists_apply_to_redirects_too() {
    let mut app = Server::new();
    let target = Arc::new(AtomicUsize::new(0));
    let hits = Arc::clone(&target);
    app.get("/redirect", |req: AxeonRequest| async move {
        let to = req.query_param("to").unwrap_or_default().to_string();
        let mut res = Response::new(302);
        res.header("Location", to);
        res
    });
    app.get("/target", move |_req| {
        hits.fetch_add(1, Ordering::SeqCst);
        async { "target" }
    });
    let addr = start(app).await;
    let client = Client::builder("vendor").egress(Egress::hosts(["127.0.0.1"]).allow_private()).build().unwrap();

    // Within the allow-list: followed.
    let ok = client.get(format!("http://127.0.0.1:{0}/redirect?to=http://127.0.0.1:{0}/target", addr.port())).send().await.unwrap();
    assert_eq!(ok.text().await.unwrap(), "target");
    // To another host: refused, even though it's the same server.
    let away = client.get(format!("http://127.0.0.1:{0}/redirect?to=http://localhost:{0}/target", addr.port())).send().await;
    assert!(matches!(away, Err(Error::Blocked(_))), "{:?}", away.map(|r| r.status()));
    assert!(matches!(client.get(format!("http://localhost:{}/target", addr.port())).send().await, Err(Error::Blocked(_))));
    assert_eq!(target.load(Ordering::SeqCst), 1);

    // Too many redirects.
    let looping = Client::builder("loop").egress(Egress::public().allow_private()).max_redirects(2).build().unwrap();
    let url = format!("http://127.0.0.1:{0}/redirect?to=http://127.0.0.1:{0}/redirect", addr.port());
    let result = looping.get(&url).send().await;
    assert!(matches!(&result, Err(Error::Blocked(b)) if b.to_string().contains("more than 2 redirects")), "{:?}", result.map(|r| r.status()));
    // max_redirects(0): the redirect itself comes back.
    let none = Client::builder("none").egress(Egress::public().allow_private()).max_redirects(0).build().unwrap();
    assert_eq!(none.get(&url).send().await.unwrap().status(), 302);
}

#[tokio::test]
async fn retries_only_when_safe() {
    // GET: retried through two 503s.
    let (addr, hits) = flaky(2, 503, None).await;
    let client = local().build().unwrap();
    let url = format!("http://{}/flaky", addr);
    let res = client.get(&url).send().await.unwrap();
    assert_eq!((res.status().as_u16(), hits.load(Ordering::SeqCst)), (200, 3));

    // POST: not retried, the 503 comes back.
    let (addr, hits) = flaky(2, 503, None).await;
    let res = client.post(format!("http://{}/flaky", addr)).json(&serde_json::json!({ "a": 1 })).send().await.unwrap();
    assert_eq!((res.status().as_u16(), hits.load(Ordering::SeqCst)), (503, 1));
    // …unless it carries an Idempotency-Key, or is marked idempotent.
    let (addr, hits) = flaky(2, 503, None).await;
    let res = client.post(format!("http://{}/flaky", addr)).header("Idempotency-Key", "k1").body("x").send().await.unwrap();
    assert_eq!((res.status().as_u16(), hits.load(Ordering::SeqCst)), (200, 3));
    let (addr, hits) = flaky(1, 502, None).await;
    let res = client.post(format!("http://{}/flaky", addr)).idempotent(true).send().await.unwrap();
    assert_eq!((res.status().as_u16(), hits.load(Ordering::SeqCst)), (200, 2));
    // …and a GET marked not idempotent isn't retried.
    let (addr, hits) = flaky(1, 503, None).await;
    let res = client.get(format!("http://{}/flaky", addr)).idempotent(false).send().await.unwrap();
    assert_eq!((res.status().as_u16(), hits.load(Ordering::SeqCst)), (503, 1));

    // 4xx and 500 aren't retried; attempts are capped.
    let (addr, hits) = flaky(5, 400, None).await;
    assert_eq!(client.get(format!("http://{}/flaky", addr)).send().await.unwrap().status(), 400);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let (addr, hits) = flaky(5, 503, None).await;
    assert_eq!(client.get(format!("http://{}/flaky", addr)).send().await.unwrap().status(), 503);
    assert_eq!(hits.load(Ordering::SeqCst), 3, "3 attempts in all");
    let once = local().attempts(1).build().unwrap();
    let (addr, hits) = flaky(5, 503, None).await;
    once.get(format!("http://{}/flaky", addr)).send().await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn honours_retry_after_within_the_backoff_limit() {
    let client = local().backoff(Duration::from_millis(10), Duration::from_secs(2)).build().unwrap();
    let (addr, hits) = flaky(1, 429, Some("1")).await;
    let started = Instant::now();
    let res = client.get(format!("http://{}/flaky", addr)).send().await.unwrap();
    assert_eq!((res.status().as_u16(), hits.load(Ordering::SeqCst)), (200, 2));
    assert!(started.elapsed() >= Duration::from_secs(1), "{:?}", started.elapsed());
    // Longer than the client would wait: the 429 comes back at once.
    let (addr, hits) = flaky(1, 429, Some("120")).await;
    let started = Instant::now();
    let res = client.get(format!("http://{}/flaky", addr)).send().await.unwrap();
    assert_eq!((res.status().as_u16(), hits.load(Ordering::SeqCst)), (429, 1));
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn timeouts_and_connection_failures() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let mut app = Server::new();
    let slow = move |_req: AxeonRequest| {
        counter.fetch_add(1, Ordering::SeqCst);
        async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            "late"
        }
    };
    app.get("/slow", slow.clone()).post("/slow", slow);
    let addr = start(app).await;
    let client = local().timeout(Duration::from_millis(150)).build().unwrap();

    let result = client.get(format!("http://{}/slow", addr)).send().await;
    assert!(matches!(&result, Err(e) if e.is_timeout()), "{:?}", result.map(|r| r.status()));
    assert_eq!(hits.load(Ordering::SeqCst), 3, "GET timeouts are retried");
    let result = client.post(format!("http://{}/slow", addr)).send().await;
    assert!(result.unwrap_err().is_timeout());
    assert_eq!(hits.load(Ordering::SeqCst), 4, "a POST may have been processed: not retried");

    // Nothing listening: retried, even for POST, since nothing was sent.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let started = Instant::now();
    let result = local().no_circuit_breaker().build().unwrap().post(format!("http://{}/", closed)).send().await;
    assert!(matches!(&result, Err(Error::Request(e)) if e.is_connect()), "{:?}", result.map(|r| r.status()));
    assert!(started.elapsed() >= Duration::from_millis(5), "it waited between attempts");
}

#[tokio::test]
async fn circuit_breaker_fails_fast_then_probes() {
    let (addr, hits) = flaky(3, 500, None).await;
    let client = local().attempts(1).circuit_breaker(2, Duration::from_millis(300)).build().unwrap();
    let url = format!("http://{}/flaky", addr);
    assert_eq!(client.get(&url).send().await.unwrap().status(), 500);
    assert_eq!(client.get(&url).send().await.unwrap().status(), 500);
    // Open: fails without calling the server.
    match client.get(&url).send().await {
        Err(Error::CircuitOpen { host, retry_in }) => {
            assert_eq!(host, addr.to_string(), "per host and port");
            assert!(retry_in <= Duration::from_millis(300));
        }
        other => panic!("expected an open circuit, got {:?}", other.map(|r| r.status())),
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    tokio::time::sleep(Duration::from_millis(350)).await;
    // The probe fails (third 500): open again at once.
    assert_eq!(client.get(&url).send().await.unwrap().status(), 500);
    assert!(matches!(client.get(&url).send().await, Err(Error::CircuitOpen { .. })));
    tokio::time::sleep(Duration::from_millis(350)).await;
    // Now the server is healthy: the probe closes the circuit.
    assert_eq!(client.get(&url).send().await.unwrap().status(), 200);
    assert_eq!(client.get(&url).send().await.unwrap().status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 5);

    // 4xx don't count as failures.
    let (addr, _) = flaky(10, 404, None).await;
    let url = format!("http://{}/flaky", addr);
    for _ in 0..5 {
        assert_eq!(client.get(&url).send().await.unwrap().status(), 404);
    }

    // A circuit opening during a request's retries returns the real
    // response, not `CircuitOpen`.
    let (addr, hits) = flaky(10, 503, None).await;
    let fragile = local().circuit_breaker(1, Duration::from_secs(60)).build().unwrap();
    assert_eq!(fragile.get(format!("http://{}/flaky", addr)).send().await.unwrap().status(), 503);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "no retry into an open circuit");
}

#[tokio::test]
async fn passes_the_request_id_and_trace_context_on() {
    let mut downstream = Server::new();
    downstream.get("/echo", |req: AxeonRequest| async move {
        format!(
            "{}|{}|{}",
            req.get_header("x-request-id").unwrap_or("-"),
            req.get_header("traceparent").unwrap_or("-"),
            req.get_header("tracestate").unwrap_or("-")
        )
    });
    let downstream = start(downstream).await;

    let client = local().build().unwrap();
    let mut upstream = Server::new();
    upstream.middleware(axeon::middleware::SetRequestId::new());
    upstream.state(client);
    upstream.get("/call", move |req: AxeonRequest| async move {
        let client = req.state::<Client>().unwrap();
        let res = client.get(format!("http://{}/echo", downstream)).context(&req).send().await?;
        Ok::<_, ServerError>(res.text().await.map_err(axeon_client::Error::from)?)
    });
    let app = upstream.into_app().await.unwrap();
    let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let req = AxeonRequest::new(axeon::Method::GET, "/call")
        .with_header("x-request-id", "req-123")
        .with_header("traceparent", traceparent)
        .with_header("tracestate", "vendor=1");
    let res = app.call(req).await;
    assert_eq!(res.body_text(), format!("req-123|{}|vendor=1", traceparent));
    // Without one, SetRequestId's generated ID is passed on.
    let res = app.call(AxeonRequest::new(axeon::Method::GET, "/call")).await;
    let echoed = res.body_text().to_string();
    let id = res.get_header("x-request-id").unwrap();
    assert!(echoed.starts_with(&format!("{}|-|-", id)), "{} vs {}", echoed, id);
}

#[tokio::test]
async fn failed_calls_become_gateway_errors() {
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let client = local().attempts(1).build().unwrap();
    let mut app = Server::new();
    app.state(client);
    app.get("/proxy", move |req: AxeonRequest| async move {
        let client = req.state::<Client>().unwrap();
        client.get(format!("http://{}/", closed)).send().await?;
        Ok::<_, ServerError>("unreachable")
    });
    let app = app.into_app().await.unwrap();
    let res = app.call(AxeonRequest::new(axeon::Method::GET, "/proxy")).await;
    assert_eq!(res.status, 502);
    assert!(res.body_text().contains("upstream service unavailable"));
}

#[tokio::test]
async fn records_metrics_per_client() {
    let metrics = axeon::metrics::Metrics::new();
    let (addr, _) = flaky(1, 503, None).await;
    let client = local().metrics(&metrics).circuit_breaker(2, Duration::from_secs(60)).build().unwrap();
    let crawler = Client::builder("crawler").metrics(&metrics).build().unwrap();
    client.get(format!("http://{}/flaky", addr)).send().await.unwrap(); // a retry, then 200
    let _ = crawler.get("http://127.0.0.1:1/").send().await; // blocked
    let (bad, _) = flaky(10, 500, None).await;
    client.get(format!("http://{}/flaky", bad)).send().await.unwrap(); // 500
    client.get(format!("http://{}/flaky", bad)).send().await.unwrap(); // 500, opens that service's circuit
    let _ = client.get(format!("http://{}/flaky", bad)).send().await; // circuit open
    // Other services on the same host aren't affected.
    assert_eq!(client.get(format!("http://{}/flaky", addr)).send().await.unwrap().status(), 200);
    let text = metrics.render();
    for line in [
        r#"http_client_requests_total{client="test",outcome="2xx"} 2"#,
        r#"http_client_requests_total{client="test",outcome="5xx"} 2"#,
        r#"http_client_requests_total{client="test",outcome="circuit_open"} 1"#,
        r#"http_client_requests_total{client="crawler",outcome="blocked"} 1"#,
        r#"http_client_retries_total{client="test"} 1"#,
        r#"http_client_circuit_opened_total{client="test"} 1"#,
        r#"http_client_request_duration_seconds_count{client="test"} 5"#,
    ] {
        assert!(text.contains(line), "missing {} in\n{}", line, text);
    }
}
