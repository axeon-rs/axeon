//! Export over OTLP/HTTP to a fake collector.

use axeon::{Response, Server};
use axeon_otel::Telemetry;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spans_reach_the_collector_on_shutdown() {
    // The collector: an Axeon app taking OTLP/HTTP protobuf posts.
    // (content type, body, API key header) per export.
    type Export = (String, Vec<u8>, Option<String>);
    let received: Arc<Mutex<Vec<Export>>> = Arc::default();
    let mut collector = Server::new();
    let store = Arc::clone(&received);
    collector.post("/v1/traces", move |req| {
        let store = Arc::clone(&store);
        async move {
            let content_type = req.get_header("content-type").unwrap_or_default().to_string();
            let auth = req.get_header("x-api-key").map(String::from);
            store.lock().unwrap().push((content_type, req.body().to_vec(), auth));
            Response::new(200)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { collector.serve(listener).await });

    let telemetry = Telemetry::builder("export-test")
        .endpoint(format!("http://{}", addr))
        .header("x-api-key", "k")
        .attribute("deployment.environment", "test")
        .build()
        .unwrap();
    {
        let subscriber = tracing_subscriber::registry().with(telemetry.layer());
        let _guard = tracing::subscriber::set_default(subscriber);
        let _span = tracing::info_span!("checkout", order = 42).entered();
    }
    // Shutdown exports synchronously; keep it off the runtime's threads.
    let shutting = telemetry.clone();
    tokio::task::spawn_blocking(move || shutting.shutdown()).await.unwrap();

    let received = received.lock().unwrap();
    assert_eq!(received.len(), 1, "one batch");
    let (content_type, body, auth) = &received[0];
    assert_eq!(content_type, "application/x-protobuf");
    assert_eq!(auth.as_deref(), Some("k"));
    let contains = |needle: &str| body.windows(needle.len()).any(|w| w == needle.as_bytes());
    assert!(contains("checkout") && contains("export-test") && contains("deployment.environment"));
}
