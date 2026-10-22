//! Server spans, checked with an in-memory exporter.

use axeon::{Method, Request, Response, Server, ServerError};
use axeon_otel::TraceRequests;
use opentelemetry::trace::{SpanKind, Status, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tracing_subscriber::layer::SubscriberExt;

fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes.iter().find(|kv| kv.key.as_str() == key).map(|kv| kv.value.to_string())
}

#[tokio::test]
async fn requests_get_server_spans_with_context() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder().with_simple_exporter(exporter.clone()).build();
    let subscriber = tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut app = Server::new();
    app.middleware(TraceRequests::new());
    app.get("/users/:id", |req| async move {
        // A span inside the handler, and the trace header outbound calls forward.
        let _work = tracing::info_span!("load user").entered();
        req.get_header("traceparent").unwrap_or_default().to_string()
    });
    app.get("/boom", |_req| async { Err::<Response, _>(ServerError::InternalError("boom".into())) });
    let app = app.into_app().await.unwrap();

    let caller_trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let caller_span = "00f067aa0ba902b7";
    let incoming = format!("00-{}-{}-01", caller_trace, caller_span);
    let response = app
        .call(Request::new(Method::GET, "/users/7").with_header("traceparent", &incoming).with_header("user-agent", "test/1"))
        .await;
    assert_eq!(response.status, 200);
    let forwarded = response.body_text().to_string();
    assert_eq!(app.call(Request::new(Method::GET, "/boom")).await.status, 500);
    assert_eq!(app.call(Request::new(Method::GET, "/nowhere")).await.status, 404);

    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let find = |name: &str| spans.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no span {} in {:?}", name, spans.iter().map(|s| &s.name).collect::<Vec<_>>()));

    let server = find("GET /users/:id");
    assert_eq!(server.span_kind, SpanKind::Server);
    assert_eq!(server.span_context.trace_id().to_string(), caller_trace, "continues the caller's trace");
    assert_eq!(server.parent_span_id.to_string(), caller_span);
    assert_eq!(attribute(server, "http.request.method").as_deref(), Some("GET"));
    assert_eq!(attribute(server, "http.route").as_deref(), Some("/users/:id"));
    assert_eq!(attribute(server, "url.path").as_deref(), Some("/users/7"));
    assert_eq!(attribute(server, "http.response.status_code").as_deref(), Some("200"));
    assert_eq!(attribute(server, "user_agent.original").as_deref(), Some("test/1"));
    assert_eq!(server.status, Status::Unset);

    // The handler's span is a child; the forwarded header names the server span.
    let work = find("load user");
    assert_eq!(work.parent_span_id, server.span_context.span_id());
    assert_eq!(forwarded, format!("00-{}-{}-01", caller_trace, server.span_context.span_id()));

    let boom = find("GET /boom");
    assert!(matches!(boom.status, Status::Error { .. }), "{:?}", boom.status);
    assert_eq!(attribute(boom, "http.response.status_code").as_deref(), Some("500"));
    assert_ne!(boom.span_context.trace_id().to_string(), caller_trace, "a new trace without traceparent");

    let unmatched = find("GET");
    assert_eq!(attribute(unmatched, "http.route"), None);
    assert_eq!(attribute(unmatched, "http.response.status_code").as_deref(), Some("404"));
}
