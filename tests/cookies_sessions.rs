//! Cookies (plain, signed, private) and sessions.
#![cfg(feature = "cookies")]

use axeon::cookies::{Cookie, CookieManager, Key};
use axeon::session::{MemoryStore, Sessions};
use axeon::{App, Method, Request, Response, Server, ServerError};
use std::time::Duration;

fn set_cookies(res: &Response) -> Vec<String> {
    res.headers.get_all("set-cookie").iter().map(|v| v.to_str().unwrap().to_string()).collect()
}

/// The `name=value` part of the Set-Cookie header for `name`.
fn cookie_pair(res: &Response, name: &str) -> Option<String> {
    set_cookies(res)
        .into_iter()
        .find(|c| c.starts_with(&format!("{}=", name)))
        .map(|c| c.split(';').next().unwrap().to_string())
}

async fn get(app: &App, path: &str, cookie: Option<&str>) -> Response {
    let mut req = Request::new(Method::GET, path);
    if let Some(cookie) = cookie {
        req = req.with_header("cookie", cookie);
    }
    app.call(req).await
}

#[tokio::test]
async fn plain_cookies_are_read_set_and_removed_with_safe_defaults() {
    let mut server = Server::new();
    server.middleware(CookieManager::new());
    server.get("/", |req| async move {
        let cookies = req.cookies();
        let theme = cookies.get("theme").map(|c| c.value().to_string());
        cookies.add(Cookie::new("seen", "yes"));
        cookies.remove("old");
        format!("{:?}", theme)
    });
    let app = server.into_app().await.unwrap();

    let res = get(&app, "/", Some("theme=dark; old=1")).await;
    assert_eq!(res.body_text(), "Some(\"dark\")");
    let headers = set_cookies(&res);
    let seen = headers.iter().find(|c| c.starts_with("seen=")).unwrap();
    assert!(seen.contains("HttpOnly") && seen.contains("SameSite=Lax") && seen.contains("Path=/"), "{}", seen);
    let old = headers.iter().find(|c| c.starts_with("old=")).unwrap();
    assert!(old.contains("Max-Age=0"), "{}", old);
}

#[tokio::test]
async fn signed_and_private_cookies_resist_tampering() {
    let mut server = Server::new();
    server.middleware(CookieManager::new().key(Key::generate()));
    server.get("/set", |req| async move {
        req.cookies().signed().add(Cookie::new("user", "ada"));
        req.cookies().private().add(Cookie::new("secret", "s3cr3t"));
        "set"
    });
    server.get("/read", |req| async move {
        let cookies = req.cookies();
        format!(
            "{:?} {:?}",
            cookies.signed().get("user").map(|c| c.value().to_string()),
            cookies.private().get("secret").map(|c| c.value().to_string())
        )
    });
    let app = server.into_app().await.unwrap();

    let res = get(&app, "/set", None).await;
    let user = cookie_pair(&res, "user").unwrap();
    let secret = cookie_pair(&res, "secret").unwrap();
    assert!(!secret.contains("s3cr3t"), "private cookie is readable: {}", secret);

    let both = format!("{}; {}", user, secret);
    assert_eq!(get(&app, "/read", Some(&both)).await.body_text(), "Some(\"ada\") Some(\"s3cr3t\")");

    // Changing the signed value (keeping the signature) or the ciphertext fails verification.
    let tampered_user = user.replace("ada", "eve");
    let tampered_secret = format!("{}x", &secret[..secret.len() - 1]);
    let tampered = format!("{}; {}", tampered_user, tampered_secret);
    assert_eq!(get(&app, "/read", Some(&tampered)).await.body_text(), "None None");
}

#[tokio::test]
async fn cookies_work_without_the_manager_for_simple_cases() {
    let mut server = Server::new();
    server.get("/", |req| async move {
        let lang = req.cookies().get("lang").map(|c| c.value().to_string()).unwrap_or_default();
        let mut res = Response::text(lang);
        res.set_cookie(Cookie::new("visited", "1")).remove_cookie("tracking");
        res
    });
    server.get("/signed", |req| async move { req.cookies().signed().get("x").is_some().to_string() });
    let app = server.into_app().await.unwrap();

    let res = get(&app, "/", Some("lang=en")).await;
    assert_eq!(res.body_text(), "en");
    assert!(cookie_pair(&res, "visited").is_some());
    assert!(set_cookies(&res).iter().any(|c| c.starts_with("tracking=") && c.contains("Max-Age=0")));
    // Signed cookies need a key: a programming error, reported as 500.
    assert_eq!(get(&app, "/signed", Some("x=1")).await.status, 500);
}

fn session_app(store_ttl: Duration) -> Server {
    let mut server = Server::new();
    server.middleware(Sessions::new(MemoryStore::new()).ttl(store_ttl).secure(true));
    server.post("/login", |req| async move {
        let session = req.session()?;
        session.regenerate();
        session.insert("user_id", 42u64)?;
        Ok::<_, ServerError>("in")
    });
    server.get("/me", |req| async move {
        let user: Option<u64> = req.session()?.get("user_id");
        Ok::<_, ServerError>(format!("{:?}", user))
    });
    server.post("/logout", |req| async move {
        req.session()?.destroy();
        Ok::<_, ServerError>("out")
    });
    server
}

#[tokio::test]
async fn sessions_log_in_regenerate_and_log_out() {
    let app = session_app(Duration::from_secs(60)).into_app().await.unwrap();

    // Anonymous visitors get no session or cookie.
    let res = get(&app, "/me", None).await;
    assert_eq!(res.body_text(), "None");
    assert!(set_cookies(&res).is_empty());

    let res = app.call(Request::new(Method::POST, "/login")).await;
    let header = set_cookies(&res).into_iter().find(|c| c.starts_with("sid=")).unwrap();
    assert!(header.contains("HttpOnly") && header.contains("Secure") && header.contains("SameSite=Lax"), "{}", header);
    assert!(header.contains("Max-Age=60"), "{}", header);
    let sid = cookie_pair(&res, "sid").unwrap();
    assert_eq!(sid.len(), "sid=".len() + 32);
    assert_eq!(get(&app, "/me", Some(&sid)).await.body_text(), "Some(42)");

    // Logging in again issues a new ID and invalidates the old one.
    let res = app.call(Request::new(Method::POST, "/login").with_header("cookie", &sid)).await;
    let new_sid = cookie_pair(&res, "sid").unwrap();
    assert_ne!(new_sid, sid);
    assert_eq!(get(&app, "/me", Some(&sid)).await.body_text(), "None");
    assert_eq!(get(&app, "/me", Some(&new_sid)).await.body_text(), "Some(42)");

    // Logout deletes the session and the cookie.
    let res = app.call(Request::new(Method::POST, "/logout").with_header("cookie", &new_sid)).await;
    assert!(set_cookies(&res).iter().any(|c| c.starts_with("sid=") && c.contains("Max-Age=0")));
    assert_eq!(get(&app, "/me", Some(&new_sid)).await.body_text(), "None");

    // Forged or malformed IDs start a fresh session.
    assert_eq!(get(&app, "/me", Some("sid=../../etc/passwd")).await.body_text(), "None");
}

#[tokio::test]
async fn sessions_expire() {
    let app = session_app(Duration::from_millis(50)).into_app().await.unwrap();
    let res = app.call(Request::new(Method::POST, "/login")).await;
    let sid = cookie_pair(&res, "sid").unwrap();
    assert_eq!(get(&app, "/me", Some(&sid)).await.body_text(), "Some(42)");
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(get(&app, "/me", Some(&sid)).await.body_text(), "None");
}

#[tokio::test]
async fn session_without_middleware_is_a_clear_error() {
    let mut server = Server::new();
    server.get("/", |req| async move { req.session().map(|_| "ok") });
    let app = server.into_app().await.unwrap();
    let res = get(&app, "/", None).await;
    assert_eq!(res.status, 500);
}
