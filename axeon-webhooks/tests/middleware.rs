//! VerifyWebhook in an Axeon app.

#![cfg(feature = "axeon")]

use axeon::{Method, Request, Server};
use axeon_webhooks::{Signer, Verifier, VerifyWebhook};
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

#[tokio::test]
async fn only_signed_requests_reach_the_handler() {
    let mut app = Server::new();
    app.route("/hooks")
        .middleware(VerifyWebhook::new("X-VS-Signature", Verifier::new("whsec_new").secret("whsec_old")))
        .post(|req| async move { format!("got {}", req.text().unwrap_or_default()) });
    app.post("/open", |_req| async { "no check here" });
    let app = app.into_app().await.unwrap();
    let body = r#"{"type":"product.updated"}"#;
    let post = |signature: Option<String>, body: &str| {
        let mut req = Request::new(Method::POST, "/hooks").with_body("application/json", body.to_string());
        if let Some(signature) = signature {
            req = req.with_header("x-vs-signature", &signature);
        }
        app.call(req)
    };

    let signed = post(Some(Signer::new("whsec_new").sign_now(body.as_bytes())), body).await;
    assert_eq!((signed.status, signed.body_text().to_string()), (200, format!("got {}", body)));
    // The old secret still works while rotating.
    assert_eq!(post(Some(Signer::new("whsec_old").sign_now(body.as_bytes())), body).await.status, 200);

    let missing = post(None, body).await;
    assert_eq!(missing.status, 401);
    assert!(missing.body_text().contains("missing webhook signature"));
    let tampered = post(Some(Signer::new("whsec_new").sign_now(body.as_bytes())), r#"{"type":"order.paid"}"#).await;
    assert!(tampered.status == 401 && tampered.body_text().contains("doesn't match"));
    let replayed = post(Some(Signer::new("whsec_new").sign(body.as_bytes(), now() - 3600)), body).await;
    assert!(replayed.status == 401 && replayed.body_text().contains("tolerance"));
    assert_eq!(post(Some(Signer::new("whsec_wrong").sign_now(body.as_bytes())), body).await.status, 401);
    assert_eq!(post(Some("t=1,v1=nothex".into()), body).await.status, 401);

    assert_eq!(app.call(Request::new(Method::POST, "/open")).await.status, 200);
}

#[tokio::test]
async fn round_trip_over_http() {
    // A receiver on a real socket, and a sender signing the exact bytes it sends.
    let mut receiver = Server::new();
    receiver.route("/hooks").middleware(VerifyWebhook::new("X-Signature", Verifier::new("s3cret"))).post(|_req| async { "ok" });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { receiver.serve(listener).await });

    let body = r#"{"event":"selection.created","id":7}"#;
    let signature = Signer::new("s3cret").sign_now(body.as_bytes());
    let request = format!(
        "POST /hooks HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nX-Signature: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        signature,
        body.len(),
        body
    );
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{}", response);
}
