//! Without an OpenTelemetry layer (in its own process: tracing caches
//! which callsites are enabled process-wide).

use axeon::{Method, Request, Server};
use axeon_otel::TraceRequests;

#[tokio::test]
async fn without_an_opentelemetry_layer_headers_are_untouched() {
    let mut app = Server::new();
    app.middleware(TraceRequests::new());
    app.get("/", |req| async move { req.get_header("traceparent").unwrap_or("none").to_string() });
    let app = app.into_app().await.unwrap();
    assert_eq!(app.call(Request::new(Method::GET, "/")).await.body_text(), "none");
    let incoming = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    assert_eq!(app.call(Request::new(Method::GET, "/").with_header("traceparent", incoming)).await.body_text(), incoming);
}
