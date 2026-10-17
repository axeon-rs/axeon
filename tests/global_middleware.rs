//! Global middleware wraps every request, and errors reach middleware as
//! responses.

mod common;

use axeon::middleware::{from_fn, Cors, RateLimiter, RequestId, SetRequestId};
use axeon::{App, Method, Request, Response, Router, Server, ServerError};
use common::TempDir;

/// Adds `X-Seen: <name>` (appending) to every response it sees.
fn tag(name: &'static str) -> impl axeon::middleware::Middleware {
    from_fn(move |req, next| async move {
        let mut res = next.handle(req).await?;
        let seen = match res.get_header("x-seen") {
            Some(existing) => format!("{},{}", name, existing),
            None => name.to_string(),
        };
        res.header("X-Seen", seen);
        Ok(res)
    })
}

async fn build(server: Server) -> App {
    server.into_app().await.unwrap()
}

#[tokio::test]
async fn global_middleware_wraps_unmatched_requests_and_static_files() {
    let dir = TempDir::new("global-mw");
    std::fs::write(dir.0.join("style.css"), "body {}").unwrap();

    let mut server = Server::new();
    server.middleware(SetRequestId::new());
    server.middleware(tag("global"));
    server.get("/hello", |_req| async { "hi" });
    server.static_dir(dir.0.to_str().unwrap());
    let app = build(server).await;

    for (method, path, status) in [
        (Method::GET, "/hello", 200),
        (Method::GET, "/missing", 404),
        (Method::POST, "/hello", 405),
        (Method::GET, "/style.css", 200),
        (Method::OPTIONS, "/hello", 204),
    ] {
        let res = app.call(Request::new(method, path)).await;
        assert_eq!(res.status, status, "{} {}", method, path);
        assert_eq!(res.get_header("x-seen"), Some("global"), "{} {}", method, path);
        assert!(res.get_header("x-request-id").is_some(), "{} {}", method, path);
    }
}

#[tokio::test]
async fn global_middleware_applies_to_routes_registered_before_it() {
    let mut server = Server::new();
    server.get("/early", |_req| async { "early" });
    server.middleware(tag("global"));
    server.get("/late", |_req| async { "late" });
    let app = build(server).await;
    for path in ["/early", "/late"] {
        assert_eq!(app.call(Request::new(Method::GET, path)).await.get_header("x-seen"), Some("global"));
    }
}

#[tokio::test]
async fn router_middleware_covers_only_its_routes_in_any_order() {
    let mut server = Server::new();
    server.middleware(tag("global"));
    server.get("/outside", |_req| async { "outside" });
    let mut api = Router::new();
    api.get("/before", |_req| async { "before" });
    api.middleware(tag("api"));
    api.get("/after", |_req| async { "after" });
    server.mount("/api", api);
    let app = build(server).await;

    let seen = |res: Response| res.get_header("x-seen").map(str::to_string);
    assert_eq!(seen(app.call(Request::new(Method::GET, "/api/before")).await).as_deref(), Some("global,api"));
    assert_eq!(seen(app.call(Request::new(Method::GET, "/api/after")).await).as_deref(), Some("global,api"));
    assert_eq!(seen(app.call(Request::new(Method::GET, "/outside")).await).as_deref(), Some("global"));
    // A 404 under the router's prefix isn't one of its routes.
    assert_eq!(seen(app.call(Request::new(Method::GET, "/api/nope")).await).as_deref(), Some("global"));
}

#[tokio::test]
async fn errors_and_panics_reach_middleware_as_responses() {
    let mut server = Server::new();
    server.on_error(|err| {
        let mut res = Response::new(err.status_code());
        res.body(format!("custom: {}", err.status_code()));
        res
    });
    server.middleware(tag("outer"));
    server.middleware(from_fn(|req, next| async move {
        // Errors returned by middleware are rendered too.
        if req.path() == "/blocked" {
            return Err(ServerError::TooManyRequests { retry_after: None });
        }
        next.handle(req).await
    }));
    server.get("/forbidden", |_req| async { Err::<&'static str, _>(ServerError::Forbidden("no".into())) });
    server.get("/none", |_req| async { None::<&'static str> });
    server.get("/boom", |_req| async {
        if true {
            panic!("secret detail");
        }
        "unreachable"
    });
    server.get("/blocked", |_req| async { "never" });
    let app = build(server).await;

    for (path, status) in [("/forbidden", 403), ("/none", 404), ("/boom", 500), ("/blocked", 429), ("/missing", 404)] {
        let res = app.call(Request::new(Method::GET, path)).await;
        assert_eq!(res.status, status, "{}", path);
        assert_eq!(res.get_header("x-seen"), Some("outer"), "{}", path);
        assert_eq!(res.body_text(), format!("custom: {}", status), "{}", path);
    }
}

#[tokio::test]
async fn cors_and_request_ids_cover_error_responses() {
    let mut server = Server::new();
    server.middleware(SetRequestId::new());
    server.middleware(Cors::new().allow_origin("https://app.example"));
    server.get("/", |req| async move { req.extension::<RequestId>().map(|id| id.to_string()) });
    let app = build(server).await;

    let res = app.call(Request::new(Method::GET, "/missing").with_header("origin", "https://app.example")).await;
    assert_eq!(res.status, 404);
    assert_eq!(res.get_header("access-control-allow-origin"), Some("https://app.example"));
    assert!(res.get_header("x-request-id").is_some());

    // Preflight for a path with no routes at all is answered by global CORS.
    let res = app
        .call(
            Request::new(Method::OPTIONS, "/anything")
                .with_header("origin", "https://app.example")
                .with_header("access-control-request-method", "POST"),
        )
        .await;
    assert_eq!(res.status, 204);
    assert_eq!(res.get_header("access-control-allow-origin"), Some("https://app.example"));
}

#[tokio::test]
async fn rate_limiter_counts_unmatched_requests() {
    let mut server = Server::new();
    server.middleware(RateLimiter::per_minute(60).burst(2));
    server.get("/", |_req| async { "ok" });
    let (addr, _guard) = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (addr, tokio::spawn(async move { server.serve(listener).await }))
    };
    let mut c = common::Client::connect(addr).await;
    assert_eq!(c.get("/nope").await.status, 404);
    assert_eq!(c.get("/nope").await.status, 404);
    assert_eq!(c.get("/").await.status, 429, "404s count toward the limit");
}
