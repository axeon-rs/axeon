//! Validation, authentication and CSRF protection.

use axeon::auth::{BasicAuth, BasicUser, BearerAuth};
use axeon::validate::{is_email, Validate, ValidationErrors};
use axeon::{App, Json, Method, Request, Response, Server, ServerError};
use base64::Engine;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
struct SignUp {
    email: String,
    password: String,
}

impl Validate for SignUp {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        errors.check(is_email(&self.email), "email", "must be an email address");
        errors.check(self.password.len() >= 12, "password", "must be at least 12 characters");
        errors.into_result()
    }
}

async fn call(app: &App, req: Request) -> Response {
    app.call(req).await
}

#[tokio::test]
async fn validation_failures_list_fields() {
    let mut server = Server::new();
    server.post("/signup", |req| async move {
        let form: SignUp = req.valid_json()?;
        Ok::<_, ServerError>((201, Json(form.email)))
    });
    let app = server.into_app().await.unwrap();

    let ok = Request::new(Method::POST, "/signup")
        .with_json(&serde_json::json!({ "email": "ada@example.com", "password": "a long passphrase" }));
    assert_eq!(call(&app, ok).await.status, 201);

    let bad = Request::new(Method::POST, "/signup").with_json(&serde_json::json!({ "email": "nope", "password": "short" }));
    let res = call(&app, bad).await;
    assert_eq!(res.status, 422);
    let body: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(
        body["error"]["fields"],
        serde_json::json!({
            "email": ["must be an email address"],
            "password": ["must be at least 12 characters"],
        })
    );
    // Malformed JSON is still 400, and a missing field 422 from deserialization.
    let malformed = Request::new(Method::POST, "/signup").with_body("application/json", "{");
    assert_eq!(call(&app, malformed).await.status, 400);
}

#[derive(Clone)]
struct User(String);

#[tokio::test]
async fn bearer_auth_checks_tokens_asynchronously() {
    let mut server = Server::new();
    server.middleware(BearerAuth::new(|token: String| async move {
        tokio::task::yield_now().await; // e.g. a database lookup
        (token == "good").then(|| User("ada".into()))
    }));
    server.get("/me", |req| async move { req.extension::<User>().map(|u| u.0.clone()) });
    let app = server.into_app().await.unwrap();

    let res = call(&app, Request::new(Method::GET, "/me")).await;
    assert_eq!((res.status, res.get_header("www-authenticate")), (401, Some("Bearer")));
    let res = call(&app, Request::new(Method::GET, "/me").with_header("authorization", "Bearer bad")).await;
    assert_eq!(res.status, 401);
    assert!(res.get_header("www-authenticate").unwrap().contains("invalid_token"));
    let res = call(&app, Request::new(Method::GET, "/me").with_header("authorization", "bearer good")).await;
    assert_eq!((res.status, res.body_text().as_ref()), (200, "ada"));
}

#[tokio::test]
async fn basic_auth() {
    let mut server = Server::new();
    server.middleware(BasicAuth::single("admin", "root", "s3cret"));
    server.get("/", |req| async move { req.extension::<BasicUser>().map(|u| u.0.clone()) });
    let app = server.into_app().await.unwrap();
    let basic = |creds: &str| format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(creds));

    let res = call(&app, Request::new(Method::GET, "/")).await;
    assert_eq!(res.status, 401);
    assert_eq!(res.get_header("www-authenticate"), Some("Basic realm=\"admin\", charset=\"UTF-8\""));
    let res = call(&app, Request::new(Method::GET, "/").with_header("authorization", &basic("root:wrong"))).await;
    assert_eq!(res.status, 401);
    let res = call(&app, Request::new(Method::GET, "/").with_header("authorization", &basic("root:s3cret"))).await;
    assert_eq!(res.body_text(), "root");
}

#[cfg(feature = "jwt")]
mod jwt {
    use super::*;
    use axeon::auth::jwt::{now, Jwt};

    #[derive(Clone, Serialize, Deserialize)]
    struct Claims {
        sub: String,
        exp: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        iss: Option<String>,
    }

    fn claims(exp: u64, iss: Option<&str>) -> Claims {
        Claims { sub: "ada".into(), exp, iss: iss.map(String::from) }
    }

    #[tokio::test]
    async fn issues_and_verifies_tokens() {
        let jwt = Jwt::hs256(b"an example secret of at least 32 bytes!!").issuer("axeon");
        let mut server = Server::new();
        server.middleware(jwt.middleware::<Claims>());
        server.get("/me", |req| async move { req.extension::<Claims>().map(|c| c.sub.clone()) });
        let app = server.into_app().await.unwrap();
        let with = |token: &str| Request::new(Method::GET, "/me").with_header("authorization", &format!("Bearer {}", token));

        let good = jwt.issue(&claims(now() + 600, Some("axeon"))).unwrap();
        assert_eq!(call(&app, with(&good)).await.body_text(), "ada");

        let expired = jwt.issue(&claims(now() - 3600, Some("axeon"))).unwrap();
        assert_eq!(call(&app, with(&expired)).await.status, 401);
        let wrong_issuer = jwt.issue(&claims(now() + 600, Some("evil"))).unwrap();
        assert_eq!(call(&app, with(&wrong_issuer)).await.status, 401);
        let other_key = Jwt::hs256(b"a different secret of at least 32 bytes!").issue(&claims(now() + 600, Some("axeon"))).unwrap();
        assert_eq!(call(&app, with(&other_key)).await.status, 401);
        let mut tampered = good.clone();
        tampered.insert(good.len() / 2, 'x');
        assert_eq!(call(&app, with(&tampered)).await.status, 401);
        assert_eq!(call(&app, Request::new(Method::GET, "/me")).await.status, 401);
    }
}

#[cfg(feature = "cookies")]
mod csrf {
    use super::*;
    use axeon::csrf::Csrf;

    async fn csrf_app() -> App {
        let mut server = Server::new();
        server.middleware(Csrf::new());
        server.get("/form", |req| async move { req.csrf_token().unwrap_or_default().to_string() });
        server.post("/save", |_req| async { "saved" });
        server.into_app().await.unwrap()
    }

    #[tokio::test]
    async fn tokens_are_issued_and_required() {
        let app = csrf_app().await;
        let res = call(&app, Request::new(Method::GET, "/form")).await;
        let token = res.body_text().to_string();
        assert_eq!(token.len(), 64);
        let cookie = res.get_header("set-cookie").unwrap().to_string();
        assert!(cookie.starts_with(&format!("csrf_token={}", token)) && cookie.contains("SameSite=Strict"), "{}", cookie);
        let cookie_header = format!("csrf_token={}", token);

        // Header or form field with the matching token.
        let by_header = Request::new(Method::POST, "/save").with_header("cookie", &cookie_header).with_header("x-csrf-token", &token);
        assert_eq!(call(&app, by_header).await.status, 200);
        let by_form = Request::new(Method::POST, "/save")
            .with_header("cookie", &cookie_header)
            .with_body("application/x-www-form-urlencoded", format!("name=x&_csrf={}", token));
        assert_eq!(call(&app, by_form).await.status, 200);

        // Missing cookie, missing token, or a different token: 403.
        let no_cookie = Request::new(Method::POST, "/save").with_header("x-csrf-token", &token);
        assert_eq!(call(&app, no_cookie).await.status, 403);
        let no_token = Request::new(Method::POST, "/save").with_header("cookie", &cookie_header);
        assert_eq!(call(&app, no_token).await.status, 403);
        let other = "0".repeat(64);
        let mismatched = Request::new(Method::POST, "/save").with_header("cookie", &cookie_header).with_header("x-csrf-token", &other);
        assert_eq!(call(&app, mismatched).await.status, 403);

        // An existing token is reused, not reissued.
        let again = call(&app, Request::new(Method::GET, "/form").with_header("cookie", &cookie_header)).await;
        assert_eq!(again.body_text(), token);
        assert!(again.get_header("set-cookie").is_none());
    }
}

#[cfg(feature = "api-keys")]
#[tokio::test]
async fn api_keys_authenticate_cache_and_report_use() {
    use axeon::auth::api_key::{generate_key, ApiKey, ApiKeyAuth, ApiKeyRecord, KeyHash};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Clone, Debug, PartialEq)]
    struct Org(i64);

    let live = generate_key("vs_live");
    let other = generate_key("vs_live");
    // The "database": id -> (hash, scopes, org); `other` is not stored.
    type Row = (KeyHash, Vec<String>, i64);
    let table: Arc<HashMap<String, Row>> =
        Arc::new([(live.id.clone(), (live.hash, vec!["catalog:read".to_string()], 7))].into_iter().collect());
    let lookups = Arc::new(AtomicUsize::new(0));
    let used = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));

    let mut app = Server::new();
    let (t, l, u) = (Arc::clone(&table), Arc::clone(&lookups), Arc::clone(&used));
    app.middleware(
        ApiKeyAuth::new("vs_live", move |id: String| {
            let (table, lookups) = (Arc::clone(&t), Arc::clone(&l));
            async move {
                lookups.fetch_add(1, Ordering::SeqCst);
                if id == "aaaaaaaaaaaaaaaa" {
                    return Err(ServerError::InternalError("database down".into()));
                }
                Ok(table.get(&id).map(|(hash, scopes, org)| ApiKeyRecord {
                    hash: *hash,
                    scopes: scopes.clone(),
                    identity: Org(*org),
                }))
            }
        })
        .on_used(Duration::ZERO, move |ids| {
            let used = Arc::clone(&u);
            async move { used.lock().unwrap().push(ids) }
        }),
    );
    app.get("/me", |req| async move {
        let key = req.extension::<ApiKey>().unwrap();
        let org = req.extension::<Org>().unwrap();
        format!("{} {} {}", key.id, key.has_scope("catalog:read"), org.0)
    });
    let app = app.into_app().await.unwrap();
    let get = |header: &'static str, value: String| app.call(Request::new(Method::GET, "/me").with_header(header, &value));

    let ok = get("authorization", format!("Bearer {}", live.key)).await;
    assert_eq!((ok.status, ok.body_text().to_string()), (200, format!("{} true 7", live.id)));
    assert_eq!(get("x-api-key", live.key.clone()).await.status, 200);
    // Cached: two requests, one lookup.
    assert_eq!(lookups.load(Ordering::SeqCst), 1);

    let missing = app.call(Request::new(Method::GET, "/me")).await;
    assert_eq!(missing.status, 401);
    assert_eq!(missing.get_header("www-authenticate"), Some("Bearer"));
    for bad in [
        format!("{}x", live.key),                       // wrong secret, known ID
        other.key.clone(),                              // unknown ID
        live.key.replacen("vs_live", "vs_test", 1),     // other prefix
        "vs_live_nonsense".to_string(),                 // malformed
    ] {
        let response = get("x-api-key", bad.clone()).await;
        assert_eq!(response.status, 401, "{}", bad);
        assert!(response.get_header("www-authenticate").unwrap().contains("invalid_token"));
    }
    // Lookup failures are server errors, not 401s.
    assert_eq!(get("x-api-key", "vs_live_aaaaaaaaaaaaaaaa_secret".to_string()).await.status, 500);

    // Used IDs are reported (on a spawned task).
    tokio::time::sleep(Duration::from_millis(50)).await;
    let reported: Vec<String> = used.lock().unwrap().concat();
    assert!(!reported.is_empty() && reported.iter().all(|id| *id == live.id), "{:?}", reported);
}
