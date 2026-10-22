# axeon-otel

OpenTelemetry for [Axeon](https://docs.rs/axeon) apps: a server span per
request, W3C trace context in and out, and export over OTLP/HTTP, so one
request can be followed across the API, workers, LLM and vendor calls.

```rust
use axeon_otel::{Telemetry, TraceRequests};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

let telemetry = Telemetry::builder("vs-api")
    .endpoint("http://otel-collector:4318")   // default: $OTEL_EXPORTER_OTLP_ENDPOINT
    .sample_ratio(0.2)                         // of new traces; callers' decisions are kept
    .attribute("deployment.environment", "production")
    .build()?;
tracing_subscriber::registry()
    .with(telemetry.layer())                   // tracing spans -> OpenTelemetry
    .with(tracing_subscriber::fmt::layer())    // your usual logs
    .init();

app.middleware(TraceRequests::new());          // first, to cover everything else
// ... serve until shutdown, then:
telemetry.shutdown();                          // exports what's left
```

## Spans

- One `SERVER` span per request, named by route pattern
  (`GET /v1/products/:id`), with `http.request.method`, `http.route`,
  `url.path`, `http.response.status_code`, `user_agent.original` and
  `client.address`. 5xx responses mark it as an error.
- A `traceparent` (and `tracestate`) from the caller makes it a child of
  the caller's span.
- `tracing` spans in your code (`#[tracing::instrument]`, `info_span!`)
  nest under it.
- The request's `traceparent` is pointed at the new span, so
  [axeon-client](../axeon-client)'s `.context(&req)` makes outbound calls
  its children with no further setup.

## Export

- OTLP over HTTP with protobuf (port 4318), batched on a background
  thread. `/v1/traces` is added to a base endpoint.
- Parent-based sampling: `sample_ratio` applies to new traces; requests
  whose caller decided keep that decision, so traces stay whole.
- `header(..)` for hosted backends' API keys; `tls` feature for
  `https://` endpoints (rustls with aws-lc-rs, like the other Axeon
  crates).
- `Telemetry::from_provider` takes a tracer provider you built yourself
  (another exporter or processors).

## Testing

```bash
cargo test -p axeon-otel
```

The tests check spans with an in-memory exporter and export to a fake
collector; no external services are needed.

## License

MIT
