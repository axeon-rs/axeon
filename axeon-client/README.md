# axeon-client

An outbound HTTP client for [Axeon](https://docs.rs/axeon) apps, on
[reqwest](https://docs.rs/reqwest): for calling vendor APIs and fetching
URLs that users provide, safely.

```rust
use axeon_client::{Client, Egress};
use std::time::Duration;

// A vendor: only its hosts.
let kyb = Client::builder("kyb")
    .egress(Egress::hosts(["api.kyb-vendor.com"]))
    .timeout(Duration::from_secs(5))
    .metrics(&metrics)
    .build()?;

let report: Report = kyb.get("https://api.kyb-vendor.com/v1/companies/9131...")
    .bearer_auth(&token)
    .context(&req)                 // passes X-Request-Id and traceparent on
    .send().await?                 // retried when safe
    .error_for_status()?
    .json().await?;

// URLs from users (a website crawler): any public host, nothing internal.
let crawler = Client::builder("crawler").egress(Egress::public()).build()?;
```

## What it adds to reqwest

**Egress control (SSRF).** By default only public addresses are reachable:
private networks, loopback, link-local, cloud metadata (`169.254.169.254`,
Alibaba Cloud's `100.100.100.200`), IPv4-mapped and NAT64 forms of those,
and other special ranges are refused. The check runs on the addresses a
name resolves to (so `evil.example → 10.0.0.1` is refused), on IP-literal
URLs, and on every redirect. `Egress::hosts([...])` limits a client to its
vendor's hosts (`*.example.com` for subdomains); `allow_private()` is for
your own internal services only. Environment proxies are ignored, since a
proxy would resolve names around these checks.

**Timeouts.** 10 s per attempt and 3 s to connect by default.

**Retries, only when safe.** Up to 3 attempts with exponential backoff and
full jitter (200 ms doubling to 5 s). Connection failures are retried for
any request, since nothing was sent. Timeouts and 429 / 502 / 503 / 504
are retried only for idempotent requests: `GET`, `HEAD`, `OPTIONS`,
`PUT`, `DELETE`, anything with an `Idempotency-Key` header, or
`.idempotent(true)`. `Retry-After` (seconds) is honoured unless it's
longer than the maximum backoff, in which case the response comes back.

**Circuit breaking.** Per service (host and port): after 5 failures in a
row (connection errors, timeouts, 5xx), calls fail with
`Error::CircuitOpen` for 30 s, then one probe decides. 4xx responses
aren't failures. A circuit that opens mid-retries returns the real
response.

**Request IDs.** `.context(&req)` copies the incoming request's ID (from
`SetRequestId` or `X-Request-Id`) and `traceparent` / `tracestate`;
`.request_id(id)` for background jobs.

**Metrics.** With `.metrics(&metrics)`: `http_client_requests_total`
(by `outcome`: `2xx`... `5xx`, `error`, `blocked`, `circuit_open`),
`http_client_request_duration_seconds`, `http_client_retries_total` and
`http_client_circuit_opened_total`, labelled by the client's name, so
crawling arbitrary hosts doesn't create unbounded series.

**Errors in handlers.** `?` on an `axeon_client::Error` in an Axeon
handler answers 504 (timeout), 503 (open circuit) or 502, logging the
details instead of sending them.

## License

MIT
