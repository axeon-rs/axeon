# Changelog

## 0.1.0 (2026-10-22)

First release: `Client` on reqwest with egress control against SSRF
(public-only by default, checked on resolved addresses, IP literals and
redirects; host allow-lists), timeouts, retries with backoff and jitter
for safe requests, `Retry-After`, a circuit breaker per service,
request-ID and trace-context propagation, per-client metrics, and
conversion of errors to 502/503/504 in Axeon handlers.
