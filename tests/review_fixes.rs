//! Regression tests for problems found in review of the 1.2 changes.

use axeon::host::ResolveHost;
#[cfg(feature = "idempotency")]
use axeon::middleware::idempotency::Idempotency;
use axeon::{Method, Request, Router, Server, ServerError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[cfg(feature = "idempotency")]
fn orders(calls: Arc<AtomicUsize>, idempotency: Idempotency) -> Server {
    let mut app = Server::new();
    app.middleware(idempotency);
    app.post("/orders", move |_req| {
        let calls = calls.clone();
        async move { format!("order #{}", calls.fetch_add(1, Ordering::SeqCst) + 1) }
    });
    app
}

#[cfg(feature = "idempotency")]
fn post(headers: &[(&str, &str)]) -> Request {
    let mut req = Request::new(Method::POST, "/orders").with_header("idempotency-key", "k1").with_body("application/json", "{}");
    for (name, value) in headers {
        req = req.with_header(name, value);
    }
    req
}

#[cfg(feature = "idempotency")]
#[tokio::test]
async fn idempotency_keys_are_not_shared_between_cookie_sessions() {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = orders(calls.clone(), Idempotency::new()).into_app().await.unwrap();
    let alice = app.call(post(&[("cookie", "session=alice")])).await;
    let bob = app.call(post(&[("cookie", "session=bob")])).await;
    assert_eq!(alice.body_text(), "order #1");
    assert_eq!(bob.body_text(), "order #2", "bob must not be given alice's response");
    assert_eq!(bob.get_header("idempotent-replayed"), None);
    // The same session retrying still gets its own response back.
    let again = app.call(post(&[("cookie", "session=alice")])).await;
    assert_eq!(again.body_text(), "order #1");
    assert_eq!(again.get_header("idempotent-replayed"), Some("true"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[cfg(feature = "idempotency")]
#[tokio::test]
async fn idempotency_scope_by_overrides_the_cookie() {
    let calls = Arc::new(AtomicUsize::new(0));
    let by_user = Idempotency::new().scope_by(|req| req.get_header("x-user").map(String::from));
    let app = orders(calls.clone(), by_user).into_app().await.unwrap();
    // The cookie changes between attempts (a rotated session), the user doesn't.
    app.call(post(&[("x-user", "7"), ("cookie", "session=a")])).await;
    let retry = app.call(post(&[("x-user", "7"), ("cookie", "session=b")])).await;
    assert_eq!(retry.get_header("idempotent-replayed"), Some("true"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn subdomains_are_plain_labels() {
    let mut stores = Router::new();
    stores.get("/", |req| async move { req.subdomain().unwrap_or("?").to_string() });
    let mut app = Server::new();
    app.host("*.stores.example.com", stores);
    app.get("/", |_req| async { "main" });
    let app = app.into_app().await.unwrap();
    let get = |host: &str| Request::new(Method::GET, "/").with_header("host", host);
    assert_eq!(app.call(get("acme.stores.example.com")).await.body_text(), "acme");
    for bad in ["x/y.stores.example.com", "a'b.stores.example.com", "u@v.stores.example.com"] {
        assert_eq!(app.call(get(bad)).await.body_text(), "main", "{:?} is not a store", bad);
    }
}

#[tokio::test]
async fn a_flood_of_made_up_hosts_does_not_evict_real_ones() {
    let lookups = Arc::new(AtomicUsize::new(0));
    let l = lookups.clone();
    let mut app = Server::new();
    app.middleware(ResolveHost::new(move |host: String| {
        let l = l.clone();
        async move {
            l.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ServerError>((host == "shop.acme.com").then_some(1u8))
        }
    }));
    app.get("/", |_req| async { "ok" });
    let app = app.into_app().await.unwrap();
    let get = |host: &str| Request::new(Method::GET, "/").with_header("host", host);
    app.call(get("shop.acme.com")).await;
    for i in 0..10_500 {
        app.call(get(&format!("junk{}.example.org", i))).await;
    }
    let before = lookups.load(Ordering::SeqCst);
    assert_eq!(app.call(get("shop.acme.com")).await.status, 200);
    assert_eq!(lookups.load(Ordering::SeqCst), before, "the real host stayed cached");
}
