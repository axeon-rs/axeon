# Changelog

## Unreleased

### Added

- **Shared rate limits:** `middleware::rate_limit` adds a `RateLimitStore`
  trait (in-memory by default; Redis in `axeon-redis`), so every instance
  of an app enforces one limit. `RateLimiter::new(Quota)` with
  `Quota::per_minute/per_hour/per_day(..).burst(..)` (GCRA) or
  `Quota::daily(n)` / `fixed_window` (resets at UTC midnight),
  `quota_by` for per-plan limits, `prefix`, `fail_open` and
  `headers(true)` for `RateLimit-Limit`, `-Remaining`, `-Reset` and
  `-Policy`. Existing `RateLimiter::per_minute(..).burst(..)` code behaves
  as before.

- **API keys:** `auth::api_key` (feature `api-keys`, on by default).
  `generate_key("vs_live")` makes `vs_live_<id>_<secret>` keys with a
  256-bit secret, returning the key (show once), its ID and a `KeyHash` to
  store. `ApiKeyAuth::new(prefix, lookup)` reads `Authorization: Bearer`
  or `X-API-Key`, looks the ID up with your async function, compares the
  hash in constant time, caches lookups (60 s by default), attaches an
  `ApiKey` extension (ID and scopes) plus your identity, and with
  `on_used` reports used IDs in batches for a `last_used_at` column.

- **Idempotency keys:** `middleware::idempotency::Idempotency` (feature
  `idempotency`, on by default). A `POST` or `PATCH` with an
  `Idempotency-Key` header that repeats an earlier one gets the stored
  response (`Idempotent-Replayed: true`) instead of running again; the
  same key with a different method, path, query or body gets 422, and a
  repeat while the first still runs gets 409 with `Retry-After`. Keys are
  hashed with the caller's credentials (or `scope_by`), responses below 500
  are kept for 24 hours, and server errors release the key for a retry.
  `IdempotencyStore` trait with `MemoryIdempotencyStore`; Redis and
  PostgreSQL stores in axeon-redis and axeon-orm.

- **HTTP caching for API responses:** `cache::CacheControl` (a
  `Cache-Control` builder), `cache::ETag` (strong, weak, or from a body),
  `Response::with_etag`, `with_last_modified`, `with_cache_control` and
  `not_modified`, and `Request::is_fresh` to answer `If-None-Match` /
  `If-Modified-Since` before doing the work. `cache::ConditionalGet`
  middleware gives successful `GET`/`HEAD` responses a weak ETag and turns
  matching conditional requests into `304 Not Modified`.

- `MatchedRoute`: the route pattern a request matched, for global
  middleware (which runs before routing). `MatchedRoute::watch(&mut req)`
  shares one slot between middleware; `Metrics` uses it too.

### Fixed

- `Server::with_tls` no longer panics with "Could not automatically
  determine the process-level CryptoProvider" when another dependency
  compiles in rustls' ring provider next to aws-lc-rs (as axeon-orm 0.3's
  `tls` feature did). It uses the process default provider, installing
  aws-lc-rs if none is set. All companion crates now use aws-lc-rs.

## 1.1.0 (2026-10-22)

New features for production backends, all additive: no breaking changes, and
nothing to do when upgrading from 1.0. Five companion crates are released
alongside: [axeon-orm 0.3](axeon-orm/CHANGELOG.md) and the new
[axeon-jobs](axeon-jobs/CHANGELOG.md), [axeon-redis](axeon-redis/CHANGELOG.md),
[axeon-openapi](axeon-openapi/CHANGELOG.md) and
[axeon-client](axeon-client/CHANGELOG.md).

### Added

- **Checked uploads:** multipart `Field::limit(bytes)` caps one field
  (413), `Field::accept(types)` checks the real type from the first bytes
  (415; `image/*` works), and `Field::write_to(writer)` / `Field::save(path)`
  stream into any `AsyncWrite` while computing SHA-256, returning an
  `Upload` (size, hash, detected and declared type). A rejected file
  writes nothing. `multipart::sniff` detects PDF, common images, MP4, ZIP,
  gzip and 7z. `Field::bytes` and `text` respect the limit.
- **Broadcast across instances:** `broadcast::Hub` publishes messages to
  topics and streams them to subscribers on every instance, with
  `Hub::sse` for server-sent events resuming from `Last-Event-ID`
  (`Request::last_event_id`). Replay comes from the backend's log, a
  `reset` event marks messages that were trimmed, slow subscribers catch
  up from the log, and each instance uses one wake-up connection however
  many clients it serves. `MemoryBackend` for one process; implement
  `broadcast::Backend` for others (Redis and PostgreSQL ones ship in
  axeon-redis and axeon-orm).
- **Route metadata:** `RouteBuilder::meta(value)` attaches a value to a
  route's handlers, and `Server::routes()` / `Router::routes()` list every
  route (`RouteInfo`: method, pattern, metadata), for tools such as
  axeon-openapi.
- **Handler timeouts:** `Server::handler_timeout` (off by default) and
  per-route `RouteBuilder::timeout` / `no_timeout`, kept through
  `mount`. A handler that runs over is cancelled and answered with 503,
  the new `ServerError::HandlerTimeout`; route middleware counts toward
  the limit, global middleware sees the 503, and streamed responses only
  have to start in time.
- **Application metrics:** `Metrics::counter`, `gauge` and `histogram`
  (and `_vec` variants with labels) register metrics on the same registry
  and `/metrics` endpoint as the HTTP metrics. Handles are cheap to clone
  and update with atomics; histograms have `start_timer()`. Names and
  labels are validated, registering a name again returns the same
  metric, and `metrics::DEFAULT_BUCKETS` is public.

### Fixed

- Closing a connection now reads and discards what the client is still
  sending (up to 2 s or 1 MiB, stopping after 500 ms of quiet) after the
  response is written. An early response such as `415` for an upload is no
  longer lost to a connection reset, which showed up on Windows.
- The README metrics example now compiles.

### Changed

- `ServerError::HandlerTimeout` is a new variant of a `#[non_exhaustive]`
  enum, so matches that already have a wildcard arm keep working.
- Updated dependencies: base64 0.23, getrandom 0.4 (feature `cookies`) and,
  for checked uploads, sha2 (feature `multipart`).

## 1.0.0 (2026-10-17)

The first stable release: no breaking changes until 2.0. Upgrading from
0.4? Read the 1.0.0-rc.1 notes below and
[Migrating from 0.4](#migrating-from-04). Changes since 1.0.0-rc.1:

### Changed (breaking since rc.1)

So that upgrading their underlying crates never forces a breaking Axeon
release, WebSocket and JWT types are now Axeon's own:

- `ws::WebSocket` is a struct instead of an alias for tokio-tungstenite's
  stream. It still implements `Stream` and `Sink`, and adds `recv()`,
  `send()` (accepting text, bytes or a `Message`) and `close(code, reason)`.
- `ws::Message`, `ws::CloseFrame` and `ws::Error` are Axeon types:
  `Message::Text` holds a `String`, `Binary`, `Ping` and `Pong` hold
  `Bytes`, and `CloseFrame` has a `u16` code and a `String` reason.
- `Jwt::rs256` and `Jwt::es256` return `auth::jwt::KeyError`;
  `auth::jwt::{Algorithm, Header, Error}` are no longer re-exported.

### Security

- Static files: request paths containing `\` or NUL are refused on every
  platform, and `:` on Windows. On Windows `%5C` could otherwise reach
  hidden files such as `.env`, and `:` alternate data streams.
- Updated dependencies with security advisories: rustls 0.23.46
  (RUSTSEC-2026-0285, -0346, -0348, -0349), rustls-webpki (RUSTSEC-2026-0049,
  -0098, -0099, -0104), bytes 1.11.1 (RUSTSEC-2026-0007), ring
  (RUSTSEC-2025-0009) and crossbeam-epoch (RUSTSEC-2026-0204). Replaced the
  unmaintained rustls-pemfile (RUSTSEC-2025-0134).
- `Server::with_tls` fails on an invalid or empty certificate file instead
  of skipping bad entries.

### Added

- `rust-version = "1.88"`, tested in CI, and a documented
  [stability policy](README.md#stability).
- CI runs the test suite on Windows.

## 1.0.0-rc.1 (2026-10-17)

The first release candidate for 1.0: a stable, consistent API (one
breaking cleanup now, then no breaking changes in 1.x), a complete backend
toolkit, and production hardening. Upgrading from 0.4? See
[Migrating from 0.4](#migrating-from-04).

### Added: backend toolkit

- **Validation** (`axeon::validate`): a `Validate` trait, `ValidationErrors`,
  `req.valid_json()`, `valid_query()`, `valid_form()`; invalid input gets 422
  with messages per field (`ServerError::Invalid`).
- **Authentication** (`axeon::auth`): `req.bearer_token()`,
  `req.basic_auth()`, `BearerAuth` (async token check), `BasicAuth`,
  `constant_time_eq`, and JWT issuing and verification with middleware
  (`auth::jwt`, feature `jwt`).
- **Cookies** (`axeon::cookies`, feature `cookies`): `req.cookies()` with
  plain, signed and encrypted cookies through `CookieManager`;
  `res.set_cookie()` and `remove_cookie()`. Safe defaults: `HttpOnly`,
  `SameSite=Lax`, `Path=/`.
- **Sessions** (`axeon::session`): `Sessions` middleware with a
  `SessionStore` trait and `MemoryStore`; `req.session()` with typed values,
  `regenerate()` against session fixation, and `destroy()`.
- **CSRF protection** (`axeon::csrf`): double-submit tokens via header or
  form field; `req.csrf_token()`.
- **Configuration** (`axeon::config`): typed `from_env()` /
  `from_env_prefixed()` reporting every problem at once, and `.env` loading.
- **Health checks** (`axeon::health`): `/health` and `/ready` with
  concurrent, time-limited checks; readiness fails during shutdown.
- **Metrics** (`axeon::metrics`): Prometheus request counts, latency
  histograms and in-flight requests, labelled by route pattern.
- **Templates** (`axeon::templates`, feature `templates`): Jinja templates
  with auto-escaping; `req.render()`.
- **Static files**: directories serve `index.html`; `Server::spa_fallback`
  serves a single-page app for client-side routes.
- `Server::shutdown_delay`: keep serving (and report not-ready) for a while
  after the shutdown signal, so load balancers can drain traffic first.
- `req.matched_path()` (the route pattern), `Response::redirect_with_status`.
- A complete example API (`examples/todo-api`) and a benchmark suite against
  axum, actix-web and Express (`benchmarks/`).

### Performance

- Per-connection data is shared instead of allocated per request, requests
  without a body skip body collection, and path parameters use a compact
  `Params` type: about 10% more throughput than 0.4.

### Hardening

- Property-based tests (thousands of generated inputs) for the form,
  range, query and config parsers and the router; wire-level tests with
  malformed input and slowloris connections.
- All public items are documented; CI adds a RustSec audit and builds the
  example app.

### Changed (breaking)

- `Request` fields are private; use accessors: `method()`, `path()`,
  `params()`, `query()`, `query_param()`, `headers()`, `get_header()`,
  `content_type()`, `body()`, `text()`, `remote_addr()`, `extensions()`.
- `Response` constructors return `Response` instead of `Result`:
  `text`, `html`, `json`, `binary`, `no_content`, `redirect`,
  `permanent_redirect`, `redirect_with_status`. New consuming builders
  `with_status`, `with_header`, `with_body`. `Response::html` sends
  `text/html; charset=utf-8`.
- All `Server` and `Router` builder methods return `&mut Self` and chain.
- `Server::plugins` is renamed `Server::state`.
- Middleware is configured with builder methods: `Cors::new().allow_origin(..)`,
  `RateLimiter::per_minute(n).burst(..).trust_proxy(..)`,
  `SecurityHeaders::new().content_security_policy(..)`,
  `Compression::new().min_size(..)` (was `CompressionMiddleware`).
- `SecurityHeaders` defaults follow current guidance (helmet): adds
  `Referrer-Policy`, `Cross-Origin-Opener-Policy`,
  `Cross-Origin-Resource-Policy`, `X-DNS-Prefetch-Control`,
  `X-Permitted-Cross-Domain-Policies`; sets `X-XSS-Protection: 0`; keeps
  headers the handler set.
- Invalid middleware configuration (e.g. CORS credentials with any origin)
  makes `serve`/`into_app` return an error instead of panicking.
- `ServerError::TooManyRequests` carries `retry_after`; rate-limited
  responses include `Retry-After`.
- `Method` is `#[non_exhaustive]`.
- `req.params()` returns `&Params` (was `&HashMap<String, String>`):
  `params()["id"]` still works; `get` returns `Option<&str>`.
- `ServerError::ParseError` is removed (use `ValidationError`); new
  `ServerError::Status(code, message)` for any other status and
  `ServerError::Invalid(ValidationErrors)` for validation failures.

### Removed

- `Body` and its `Option`-returning parsers (`json`, `x_www_form_urlencoded`,
  `form_data`); use `req.json()`, `req.form()`, `req.form_value()` and
  `req.multipart()`.
- `req.data`, `set_data`, `get_data`, `get_typed_data`, `get_method`; use
  typed extensions.
- `Response::ok`, `created`, `accepted`, `bad_request`, `unauthorized`,
  `forbidden`, `not_found`, `conflict`, `unprocessable_entity`,
  `too_many_requests`, `service_unavailable`, `method_not_allowed`.
- `Response::with_gzip` and `with_brotli` (set `Content-Encoding` without
  compressing, breaking clients), `send` (printed to stdout), `with_cors`,
  `with_security_headers`, `with_language`, `with_api_version`, `vary`;
  `with_cache_control` and `file_download` are now `cache_control` and
  `attachment` (which escapes the file name correctly).
- The `ok_json!`, `created_json!` and `middlewares!` macros.
- `Method::from_string`, `ParseError`, `CorsConfig`, `RateLimitConfig`,
  `SecurityConfig`, `CompressionConfig`.
- The `buffer` module (unused) and the `database` module: use the new
  companion crate [axeon-orm](axeon-orm), or keep any database pool (e.g.
  sqlx's) in app state with `Server::state`.
- The `cache` module and feature: use [moka](https://docs.rs/moka) directly,
  shared through `Server::state`.

### Added

- `axeon::http` re-exports the `http` crate.

- **Route builder**: `app.route(path)` (also on `Router`) configures one path.
  Its `.middleware(..)`, `.layer(..)`, `.body_limit(..)` and `.stream_body()`
  apply to the handlers registered through that builder only, so a `POST` can
  require auth while `GET` on the same path stays public. Route middleware
  runs after global and router middleware.
- **Streaming request bodies**: on `stream_body()` routes the body isn't
  buffered; read it as it arrives with `req.body_stream()`, a `RequestBody`
  that is a `Stream` of chunks, with `bytes()` and `save_to(path)`. It fails
  with 413 past the route's limit and 408 if the client stops sending.
- **Multipart uploads** (new default feature `multipart`, via multer):
  `req.multipart()` reads `multipart/form-data` field by field; files can be
  written to disk as they arrive with `field.save_to(path)`.
- **Per-route body limits** with `.body_limit(bytes)`, overriding
  `max_body_size`.
- **Tower layers as middleware**: `Server::layer`, `Router::layer` and
  `RouteBuilder::layer` accept any tower `Layer`, including all of
  `tower-http` (compression, timeouts, CORS, decompression, tracing...).
  Streamed bodies stay streamed through layers.
- **Apps as tower services**: `App::into_service()` returns an `AppService`
  implementing tower `Service`, for mounting in other frameworks or custom
  hyper servers.
- `axeon::body::{RequestBody, ResponseBody}`: the body types, implementing
  `http_body::Body`.

### Changed

- New dependencies: `tower-service`, `tower-layer` and `http-body` (small,
  trait-only crates), and `multer` behind the `multipart` feature.

## 0.4.1 (2026-10-17)

### Fixed

- Global middleware (`Server::middleware`) now wraps every request: 404 and
  405 responses, static files and automatic `OPTIONS` replies used to bypass
  it, so `RequestLogger` didn't log them, `SetRequestId` and `Cors` didn't add
  their headers, and `RateLimiter` didn't count them.
- Global middleware applies to all routes, including ones registered before
  it; router middleware applies to all of that router's routes.
- Errors and panics now reach middleware as responses (rendered by
  `on_error`), so headers added by middleware appear on error responses too.
- Static files were read entirely into memory before sending, so large files
  could exhaust memory. They are now streamed from disk with a
  `Content-Length`.
- Static files were sent with `Cache-Control: public, max-age=31536000`, so
  browsers kept stale HTML and assets for a year. The default is now
  `no-cache` (revalidate before use), configurable with
  `Server::static_cache_control`.

### Added

- Range requests for static files (`206 Partial Content`, `416`, `If-Range`):
  resumable downloads and video seeking.
- Conditional requests for static files: `If-None-Match` and
  `If-Modified-Since` get `304 Not Modified`.
- `Response::stream_with_length` for streamed bodies of known length.
- `Server::static_cache_control`.

### Changed

- Hidden files (`.env`, `.git/...`) in the static directory are no longer
  served; `/.well-known/` still is.
- `CompressionMiddleware` marks the `ETag` of compressed responses weak and
  drops `Accept-Ranges`.
- Middleware that matched on `Err` from `next.handle(req)` now receives the
  rendered error response instead; read `res.status` for the status code.
- Each middleware layer costs about 0.1 µs per request (was about 55 ns),
  the price of rendering errors into responses between layers. Requests with
  no global middleware are unaffected.

## 0.4.0 (2026-10-17)

A rewrite of the server internals on [hyper](https://hyper.rs), with HTTP/2,
an Express-style handler API, realtime features and production tooling.
See [Migrating from 0.2](#migrating-from-02) below.

Version 0.3.0 was never published; everything since 0.2.0 is in this release.

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

## Migrating from 0.4

| 0.4 | 1.0 |
|---|---|
| `req.params["id"]`, `req.params.get("id")` | `req.params()["id"]`, or typed `req.param::<u64>("id")?` |
| `req.query.get("q")` | `req.query_param("q")`, or typed `req.query_as::<T>()?` |
| `req.path`, `req.method`, `req.headers` | `req.path()`, `req.method()`, `req.headers()` |
| `req.body.as_bytes()` | `req.body()` |
| `req.body.json::<T>()` (returned `Option`) | `req.json::<T>()?` (415/400/422 errors) |
| `req.body.x_www_form_urlencoded::<T>()` | `req.form::<T>()?`, or `req.form_value()?` for nested keys |
| `req.body.form_data::<T>()` | `req.multipart()?` (streaming) |
| `req.set_data(..)` / `req.get_data(..)` | `req.insert_extension(value)` / `req.extension::<T>()` |
| `app.plugins(value)` | `app.state(value)` |
| `Response::ok(&value)` | `Json(value)` or `Response::json(&value)` |
| `Response::created(&value)` | `(201, Json(value))` |
| `Response::not_found(..)` etc. | `Err(ServerError::NotFound)` or `(404, Json(..))` |
| `Response::text(..)` inside `Ok(..)` | `Response::text(..)` (no longer a `Result`) |
| `ok_json!({..})` | `Json(json!({..}))` |
| `res.with_gzip()` | `Compression::new()` middleware |
| `res.file_download(name, type)` | `res.attachment(name)` plus a `Content-Type` |
| `Cors::new(CorsConfig { allow_origins: vec![..], .. })` | `Cors::new().allow_origin(..)` |
| `RateLimiter::new(RateLimitConfig { requests_per_minute: 60, burst_size: 10, .. })` | `RateLimiter::per_minute(60).burst(10)` |
| `SecurityHeaders::new(SecurityConfig::default())` | `SecurityHeaders::new()` |
| `CompressionMiddleware::new(CompressionConfig::default())` | `Compression::new()` |
| `ServerError::TooManyRequests` | `ServerError::TooManyRequests { retry_after: None }` |
| `axeon::database::ConnectionPool` | `axeon_orm::Db`, or your database crate's pool (e.g. `sqlx::PgPool`), in `app.state(..)` |
| `axeon::cache::CacheManager` | `moka::future::Cache` in `app.state(..)` |
| `req.params().get("id")` returning `Option<&String>` | returns `Option<&str>` |
| `ServerError::ParseError(..)` | `ServerError::ValidationError(..)` |

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
- **`Cargo.toml`**: `axeon = "0.4"`. All optional features (`tls`,
  `compression`, `ws`, `cache`) are on by default; with
  `default-features = false`, list the ones you use.
- **`RateLimitConfig`** has a `trust_proxy` field; **`ServerError`** is
  `#[non_exhaustive]`, so add a `_ =>` arm when matching it.
- **Logs** go through `tracing`: install a subscriber
  (`tracing_subscriber::fmt().init()`) to see them.
- **`max_header_size`** cannot go below 8 KiB.
