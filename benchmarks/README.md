# Benchmarks

Axeon compared with [axum](https://github.com/tokio-rs/axum) 0.8,
[actix-web](https://actix.rs) 4 and [Express](https://expressjs.com) 5 on
three small endpoints:

- `GET /plaintext`: `Hello, World!`
- `GET /json`: `{"message":"Hello, World!"}`
- `GET /users/42`: a path parameter parsed as a number, returned as JSON

The servers are in [`rust/src/bin`](rust/src/bin) and
[`express/server.js`](express/server.js). Each is idiomatic for its
framework with default settings. Rust servers are built with
`--release`, `lto = "fat"` and `codegen-units = 1`. Express runs one worker
per core with Node's `cluster` module, so it uses every core like the Rust
servers do.

## Results

Apple M1 (8 cores), macOS. Load: 4 parallel `ab -k -c 32 -n 50000` clients
per endpoint (128 keep-alive connections). Best of three runs, since the
laptop had background load:

| Framework | Plain text | JSON | Path param | p99 latency | Memory after load |
|---|---|---|---|---|---|
| actix-web 4 | 208k req/s | 207k req/s | 190k req/s | 5-9 ms | 10.6 MiB |
| axum 0.8 | 202k req/s | 195k req/s | 197k req/s | 2-3 ms | 8.5 MiB |
| **Axeon 1.0** | **188k req/s** | **176k req/s** | **185k req/s** | **2 ms** | **8.9 MiB** |
| Express 5 (8 workers) | 38k req/s | 39k req/s | 45k req/s | 14-22 ms | 877 MiB |

In short: Axeon serves about **4-5x** the requests of Express using about
**1/100th of the memory**, and is within roughly **6-10%** of the fastest
Rust frameworks while doing more per request by default (panic isolation,
request limits, graceful shutdown and connection tracking).

## Caveats

- Client and server share one machine, so they compete for CPU, and a single
  `ab` process tops out below what these servers can handle. Treat the
  numbers as a comparison, not as absolute capacity.
- These are tiny endpoints that measure framework overhead. Real
  applications usually spend their time in the database or other services,
  where the framework matters much less.
- Results vary between runs by a few percent; run them yourself on your
  hardware before relying on them.

## Running

Needs a Rust toolchain, Node.js with npm, and `ab` (ApacheBench, preinstalled
on macOS; `apache2-utils` on Debian/Ubuntu):

```bash
benchmarks/run.sh            # 50,000 requests per client
benchmarks/run.sh 200000     # longer runs
```

For framework-internal costs without the network (routing, middleware,
JSON), see `cargo bench` in the repository root.
