//! Host-based routing and runtime-resolved custom domains.

use axeon::host::ResolveHost;
use axeon::middleware::from_fn;
use axeon::{Method, Request, Router, Server, ServerError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

mod common;

fn get(host: &str, path: &str) -> Request {
    Request::new(Method::GET, path).with_header("host", host)
}

#[tokio::test]
async fn routes_by_host() {
    let mut stores = Router::new();
    stores.get("/", |req| async move { format!("store {}", req.subdomain().unwrap_or("?")) });
    stores.get("/products/:id", |req| async move {
        Ok::<_, ServerError>(format!("{} product {}", req.subdomain().unwrap(), req.param::<i64>("id")?))
    });
    let mut admin = Router::new();
    admin.get("/", |_req| async { "admin" });

    let mut app = Server::new();
    app.middleware(from_fn(|req, next| async move {
        let mut response = next.handle(req).await?;
        response.header("X-Global", "yes");
        Ok(response)
    }));
    app.host("*.stores.example.com", stores);
    app.host("admin.example.com", admin);
    app.get("/", |_req| async { "main" });
    app.get("/api/health", |_req| async { "ok" });
    let app = app.into_app().await.unwrap();
    let text = |r: axeon::Response| (r.status, r.body_text().to_string());

    assert_eq!(text(app.call(get("acme.stores.example.com", "/")).await), (200, "store acme".into()));
    assert_eq!(text(app.call(get("Acme.Stores.Example.com:8443", "/products/7")).await), (200, "acme product 7".into()));
    assert_eq!(text(app.call(get("admin.example.com.", "/")).await), (200, "admin".into()));
    assert_eq!(text(app.call(get("example.com", "/")).await), (200, "main".into()));
    assert_eq!(text(app.call(Request::new(Method::GET, "/")).await), (200, "main".into()), "no Host: the server's routes");
    // Not a wildcard match: one label only, and a real suffix.
    assert_eq!(text(app.call(get("a.b.stores.example.com", "/")).await), (200, "main".into()));
    assert_eq!(text(app.call(get("stores.example.com", "/")).await), (200, "main".into()));
    // A host's router owns it: no fallthrough to the server's routes.
    assert_eq!(app.call(get("acme.stores.example.com", "/api/health")).await.status, 404);
    assert_eq!(app.call(get("example.com", "/api/health")).await.status, 200);
    // Global middleware covers every host.
    assert_eq!(app.call(get("admin.example.com", "/")).await.get_header("x-global"), Some("yes"));

    // Bad and duplicate patterns fail at startup.
    let mut bad = Server::new();
    bad.host("*.*.example.com", Router::new());
    assert!(bad.into_app().await.is_err());
    let mut twice = Server::new();
    twice.host("a.example.com", Router::new()).host("A.example.com", Router::new());
    assert!(twice.into_app().await.is_err());
}

#[tokio::test]
async fn body_limits_follow_the_host_route() {
    let mut uploads = Router::new();
    uploads.route("/upload").body_limit(8).post(|_req| async { "small" });
    let mut app = Server::new();
    app.host("uploads.example.com", uploads);
    app.post("/upload", |_req| async { "default limit" });
    let addr = common::start(app).await;

    let post = |host: &str| format!("POST /upload HTTP/1.1\r\nHost: {}\r\nContent-Length: 20\r\nConnection: close\r\n\r\n01234567890123456789", host);
    let mut client = common::Client::connect(addr).await;
    client.send(post("uploads.example.com").as_bytes()).await;
    assert_eq!(client.read().await.unwrap().status, 413);
    let mut client = common::Client::connect(addr).await;
    client.send(post("example.com").as_bytes()).await;
    let response = client.read().await.unwrap();
    assert_eq!((response.status, response.text()), (200, "default limit".to_string()));
}

#[derive(Clone, Debug)]
struct Storefront {
    name: String,
}

#[tokio::test]
async fn custom_domains_resolve_at_runtime() {
    let lookups = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&lookups);
    let mut app = Server::new();
    app.middleware(
        ResolveHost::new(move |host: String| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                match host.as_str() {
                    "shop.acme.com" => Ok(Some(Storefront { name: "Acme".into() })),
                    "broken.example" => Err(ServerError::InternalError("database down".into())),
                    _ => Ok(None),
                }
            }
        })
        .skip(["studio.example.com"]),
    );
    app.get("/", |req| async move { req.extension::<Storefront>().map(|s| s.name.clone()).unwrap_or_else(|| "studio".into()) });
    let app = app.into_app().await.unwrap();

    assert_eq!(app.call(get("Shop.Acme.com:443", "/")).await.body_text(), "Acme");
    assert_eq!(app.call(get("shop.acme.com", "/")).await.body_text(), "Acme");
    assert_eq!(app.call(get("unknown.example", "/")).await.status, 404);
    assert_eq!(app.call(get("unknown.example", "/")).await.status, 404);
    assert_eq!(lookups.load(Ordering::SeqCst), 2, "known and unknown hosts are cached");
    assert_eq!(app.call(get("studio.example.com", "/")).await.body_text(), "studio", "own domains skip the lookup");
    assert_eq!(app.call(get("broken.example", "/")).await.status, 500);
    assert_eq!(app.call(get("broken.example", "/")).await.status, 500);
    assert_eq!(lookups.load(Ordering::SeqCst), 4, "errors aren't cached");
    assert_eq!(app.call(Request::new(Method::GET, "/")).await.status, 400, "no Host at all");
}
