# Changelog

## 0.3.0 (unreleased)

A rewrite of the server internals on [hyper](https://hyper.rs), with HTTP/2,
an Express-style handler API, realtime features and production tooling.
See [Migrating from 0.2](#migrating-from-02) below.

### Added

- HTTP/2 (ALPN over TLS, or prior knowledge in clear text) alongside HTTP/1.1.
- Handlers can return `&str`, `String`, `Json(..)`, `Html(..)`, `Vec<u8>`,
  `Bytes`, `(status, body)`, `Option<T>` (`None` is 404), `()` (204), or a
  `Result` of any of these, via the public `IntoResponse` trait.
- Closure middleware: `middleware::from_fn(|req, next| async move { .. })`.
- Typed request helpers: `req.param::<T>()`, `req.query_as::<T>()`,
  `req.json::<T>()`, `req.form::<T>()`, `req.state::<T>()`, and typed
  per-request extensions (`req.insert_extension`, `req.extension::<T>()`).
- Streaming responses (`Response::stream`), server-sent events
  (`axeon::sse`, `Response::sse`) and WebSockets (`req.upgrade_websocket`).
- Catch-all routes (`/files/*path`); routes are matched by a radix tree.
- Graceful shutdown: `listen()` handles Ctrl+C/SIGTERM;
  `serve_with_shutdown(listener, signal)`, `shutdown_signal()`,
  `shutdown_timeout`. Handlers can watch `req.shutdown()`; event streams end
  cleanly and WebSockets are drained.
- `Server::serve(listener)` to run inside an existing Tokio runtime.
- Structured logging with `tracing`; `SetRequestId` and `RequestLogger`
  middleware.
- `Server::into_app()` and `App::call` for in-process requests in tests, plus
  `Request::new` with `with_header`, `with_body`, `with_json`.
- Limits and timeouts: `max_body_size`, `max_header_size`, `max_headers`,
  `request_timeout`, `write_timeout`, `keep_alive`, `max_connections`.
- Cargo features `tls`, `compression`, `ws` and `cache` (all on by default).
- `Response::append_header`, `get_header`, `remove_header`, `append_vary`,
  `body_bytes`, `binary`.
- Criterion benchmarks (`cargo bench`) and CI.

### Changed

- Keep-alive connections are reused; previously every request opened a new
  connection.
- `max_connections` applies backpressure instead of spinning a CPU core.
- Requests for an existing path with the wrong method get 405 with `Allow`;
  `OPTIONS` is answered automatically (so CORS preflights work); unknown
  methods get 501 instead of running the GET handler.
- `HEAD` responses carry the real `Content-Length`.
- Conflicting routes (e.g. `/users/:id` and `/users/:name`) fail at startup.
- Panic and IO error details are logged, not sent to clients.
- `Cors` sends `*` for wildcard origins instead of reflecting the request's
  origin, and panics on the unsafe wildcard + credentials combination.
- `RateLimiter` is a per-instance token bucket keyed by the peer address;
  `X-Forwarded-For` is only used with `trust_proxy: true`.
- `listen()` returns `Ok(())` after a graceful shutdown.

### Fixed

- Binary bodies (images, compressed responses) were corrupted by UTF-8
  conversion.
- `CompressionMiddleware` never compressed (it read `Accept-Encoding` from the
  response) and produced invalid `deflate` data.
- `Request::get_header` was case-sensitive, so `"Authorization"` never matched.
- Query strings and path parameters were not percent-decoded.
- JSON bodies with `; charset=utf-8` were rejected.
- `cargo test` hung on doctests that called `listen()`.
- Multipart uploads lost the last two bytes of a file when the closing
  boundary had no trailing CRLF, and truncated bodies panicked.
- `ConnectionPool` ignored `max_size` for checked-out connections and never
  enforced `max_lifetime`.

### Security

- Form bodies could crash the whole process: deeply nested keys
  (`a[x][x]...`) overflowed the stack. Nesting is now limited to 32 levels.
- Form bodies could allocate unbounded memory: `a[1000000000]=x` built a huge
  array. Array indexes may now only overwrite or append.
- Multipart part headers could override the field name.
- Requests with both `Transfer-Encoding` and `Content-Length` are handled per
  RFC 9112 (no request smuggling); header values containing CR/LF are refused.
- Slow or stalled clients are cut off by header, body and write timeouts.

### Removed

- `Middleware::clone_box` (middleware is now shared, not cloned per request).
- The `Response::stream(&mut self, content_type)` setter; use
  `Response::stream(content_type, stream)`.

## Migrating from 0.2

- **Middleware:** delete `fn clone_box`. One instance now serves all
  requests, so any state it holds is shared.
- **Handlers returning only `Ok(..)`:** a handler with no `?` or `Err` needs
  the error type spelled once: `Ok::<_, ServerError>(response)`, or return the
  value directly (`response`, `"text"`, `Json(..)`).
- **`Response.body`** is `Bytes` and **`Response.headers` /
  `Request.headers`** are `http::HeaderMap`. `res.body(..)`,
  `res.header(..)`, `res.get_header(..)` and `req.get_header(..)` work as
  before.
- **`Method::from_string`** is deprecated; use `Method::parse`, which returns
  `None` for unknown methods.
- **`RateLimitConfig`** has a `trust_proxy` field; **`ServerError`** is
  `#[non_exhaustive]`, so add a `_ =>` arm when matching it.
- **Logs** go through `tracing`: install a subscriber
  (`tracing_subscriber::fmt().init()`) to see them.
- **`max_header_size`** cannot go below 8 KiB.
