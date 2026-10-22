# Changelog

## Unreleased

First release: `TraceRequests`, middleware opening a `SERVER` span per
request with the OpenTelemetry HTTP attributes (named by route pattern,
continuing the caller's `traceparent`, 5xx as errors, and pointing the
request's `traceparent` at the new span so axeon-client calls become its
children); and `Telemetry`, which sets up OTLP/HTTP export (batched,
parent-based ratio sampling, resource attributes, headers), installs the
W3C propagator and provides the `tracing` layer. Feature `tls` for
`https://` collectors (rustls with aws-lc-rs).
