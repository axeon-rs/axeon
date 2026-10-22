//! Signed webhooks: HMAC-SHA256 signatures with a timestamp, so receivers
//! can check that a request came from you, wasn't changed, and isn't an
//! old one replayed.
//!
//! The scheme is the widely used `t=<unix seconds>,v1=<hex>` header (as
//! Stripe's), where `v1` is HMAC-SHA256 over `"<t>.<body>"` with a shared
//! secret.
//!
//! Sending:
//!
//! ```
//! use axeon_webhooks::Signer;
//!
//! let signer = Signer::new("whsec_7c1f...");
//! let body = br#"{"type":"product.updated","id":"p_42"}"#;
//! let header = signer.sign_now(body); // send as e.g. `X-VS-Signature: <header>`
//! assert!(header.starts_with("t="));
//! ```
//!
//! Receiving: [`Verifier::verify`], or in an Axeon app the
//! `VerifyWebhook` middleware (feature `axeon`, on by default).
//!
//! # Rotating secrets
//!
//! Give the [`Verifier`] both the old and the new secret, switch senders to
//! the new one, then drop the old one. During the switch a sender may also
//! put two signatures in one header ([`Signer::sign_with`]).

#![warn(missing_docs)]

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// Seconds since the Unix epoch.
fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// HMAC-SHA256 of `"<timestamp>.<body>"`.
fn mac(secret: &[u8], timestamp: i64, body: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

/// Signs webhook bodies with a shared secret.
#[derive(Clone)]
pub struct Signer {
    secret: Vec<u8>,
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Signer { .. }")
    }
}

impl Signer {
    /// A signer with `secret`; use at least 32 random bytes.
    pub fn new(secret: impl AsRef<[u8]>) -> Self {
        Self { secret: secret.as_ref().to_vec() }
    }

    /// The header value for `body` at `timestamp` (Unix seconds):
    /// `t=<timestamp>,v1=<hex>`.
    pub fn sign(&self, body: &[u8], timestamp: i64) -> String {
        format!("t={},v1={}", timestamp, hex(&mac(&self.secret, timestamp, body).finalize().into_bytes()))
    }

    /// The header value for `body`, timestamped now.
    pub fn sign_now(&self, body: &[u8]) -> String {
        self.sign(body, now())
    }

    /// One header with a signature from this signer and each of `others`,
    /// for rotating secrets: receivers accept it with any of them.
    pub fn sign_with(&self, others: &[&Signer], body: &[u8], timestamp: i64) -> String {
        let mut header = self.sign(body, timestamp);
        for other in others {
            header.push_str(",v1=");
            header.push_str(&hex(&mac(&other.secret, timestamp, body).finalize().into_bytes()));
        }
        header
    }
}

/// Why a webhook signature was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VerifyError {
    /// No signature header.
    Missing,
    /// The header isn't `t=<seconds>,v1=<hex>`.
    Malformed,
    /// The timestamp is outside the tolerance: an old request replayed, or
    /// a clock far off.
    Expired,
    /// No signature matches any secret: wrong secret or changed body.
    Mismatch,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            VerifyError::Missing => "missing webhook signature",
            VerifyError::Malformed => "malformed webhook signature",
            VerifyError::Expired => "webhook signature timestamp outside the tolerance",
            VerifyError::Mismatch => "webhook signature doesn't match",
        })
    }
}

impl std::error::Error for VerifyError {}

/// Checks webhook signatures against one or more secrets.
#[derive(Clone)]
pub struct Verifier {
    secrets: Vec<Vec<u8>>,
    tolerance: Duration,
}

impl fmt::Debug for Verifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Verifier").field("secrets", &self.secrets.len()).field("tolerance", &self.tolerance).finish()
    }
}

impl Verifier {
    /// A verifier for `secret`, accepting timestamps up to 5 minutes away
    /// from now (either way, for clock skew).
    pub fn new(secret: impl AsRef<[u8]>) -> Self {
        Self { secrets: vec![secret.as_ref().to_vec()], tolerance: Duration::from_secs(300) }
    }

    /// Also accepts signatures made with `secret` (while rotating).
    pub fn secret(mut self, secret: impl AsRef<[u8]>) -> Self {
        self.secrets.push(secret.as_ref().to_vec());
        self
    }

    /// How far the timestamp may be from now. Default: 5 minutes.
    pub fn tolerance(mut self, tolerance: Duration) -> Self {
        self.tolerance = tolerance;
        self
    }

    /// Checks `header` (the `t=..,v1=..` value) for `body`, now.
    pub fn verify(&self, header: &str, body: &[u8]) -> Result<(), VerifyError> {
        self.verify_at(header, body, now())
    }

    /// Checks `header` for `body` as if the time were `now` (Unix seconds).
    pub fn verify_at(&self, header: &str, body: &[u8], now: i64) -> Result<(), VerifyError> {
        let mut timestamp = None;
        let mut signatures = Vec::new();
        for part in header.split(',') {
            match part.trim().split_once('=') {
                Some(("t", value)) => timestamp = Some(value.parse::<i64>().map_err(|_| VerifyError::Malformed)?),
                Some(("v1", value)) => signatures.push(unhex(value).ok_or(VerifyError::Malformed)?),
                // Other schemes (v0, future versions) are ignored.
                Some(_) => {}
                None => return Err(VerifyError::Malformed),
            }
        }
        let timestamp = timestamp.ok_or(VerifyError::Malformed)?;
        if signatures.is_empty() {
            return Err(VerifyError::Malformed);
        }
        if now.abs_diff(timestamp) > self.tolerance.as_secs() {
            return Err(VerifyError::Expired);
        }
        // One HMAC per secret, however many signatures the header carries: a
        // sender controls how many there are, and hashing the whole body for
        // each one would multiply the cost of an unauthenticated request.
        // `verify_slice` compares in constant time.
        let matched = self.secrets.iter().any(|secret| {
            let expected = mac(secret, timestamp, body);
            signatures.iter().any(|signature| expected.clone().verify_slice(signature).is_ok())
        });
        if matched {
            Ok(())
        } else {
            Err(VerifyError::Mismatch)
        }
    }
}

#[cfg(feature = "axeon")]
mod middleware {
    use super::{Verifier, VerifyError};
    use axeon::middleware::{Middleware, MiddlewareResult, Next};
    use axeon::{Request, ServerError};

    /// Middleware that rejects requests whose signature header doesn't
    /// verify, with `401` (and why, in the message), before the handler
    /// runs. Handlers can then trust [`req.body()`](Request::body).
    ///
    /// Not for [`stream_body`](axeon::RouteBuilder::stream_body) routes: the
    /// body must be buffered to be checked, and such requests get `500`.
    ///
    /// ```
    /// use axeon::Server;
    /// use axeon_webhooks::{Verifier, VerifyWebhook};
    ///
    /// let mut app = Server::new();
    /// app.route("/webhooks/replication")
    ///     .middleware(VerifyWebhook::new("X-VS-Signature", Verifier::new("whsec_7c1f...")))
    ///     .post(|req| async move { format!("{} bytes, verified", req.body().len()) });
    /// ```
    #[derive(Clone, Debug)]
    pub struct VerifyWebhook {
        header: String,
        verifier: Verifier,
    }

    impl VerifyWebhook {
        /// Checks the signature in header `header` with `verifier`.
        pub fn new(header: &str, verifier: Verifier) -> Self {
            Self { header: header.to_ascii_lowercase(), verifier }
        }
    }

    impl Middleware for VerifyWebhook {
        fn call(&self, req: Request, next: Next) -> MiddlewareResult {
            if req.body().is_empty() && req.get_header("content-length").is_some_and(|l| l.trim() != "0") {
                let error = ServerError::InternalError("webhook routes can't stream their body".into());
                return Box::pin(async move { Err(error) });
            }
            let result = match req.get_header(&self.header) {
                None => Err(VerifyError::Missing),
                Some(header) => self.verifier.verify(header, req.body()),
            };
            match result {
                Ok(()) => next.handle(req),
                Err(error) => Box::pin(async move { Err(ServerError::Unauthorized(error.to_string())) }),
            }
        }
    }
}

#[cfg(feature = "axeon")]
pub use middleware::VerifyWebhook;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let signer = Signer::new("secret");
        let body = br#"{"id":1}"#;
        let header = signer.sign(body, 1_700_000_000);
        // Known answer, from
        // `printf '1700000000.{"id":1}' | openssl dgst -sha256 -hmac secret`.
        assert_eq!(header, "t=1700000000,v1=3dd1b9aef568d75f6790a84bd2e5dfa1f44409eef3cbdbd3f10b837376100c11");
        let verifier = Verifier::new("secret");
        assert_eq!(verifier.verify_at(&header, body, 1_700_000_100), Ok(()));
        assert_eq!(verifier.verify_at(&header, br#"{"id":2}"#, 1_700_000_100), Err(VerifyError::Mismatch));
        assert_eq!(Verifier::new("other").verify_at(&header, body, 1_700_000_100), Err(VerifyError::Mismatch));
        assert_eq!(verifier.verify_at(&header, body, 1_700_000_301), Err(VerifyError::Expired));
        assert_eq!(verifier.verify_at(&header, body, 1_699_999_699), Err(VerifyError::Expired), "future timestamps too");
        for bad in ["", "garbage", "t=abc,v1=00", "t=1700000000", "t=1700000000,v1=zz", "v1=00"] {
            assert_eq!(verifier.verify_at(bad, body, 1_700_000_000), Err(VerifyError::Malformed), "{:?}", bad);
        }
    }

    #[test]
    fn many_signatures_cost_one_hash() {
        // Not a timing test: just that a header full of signatures is judged right.
        let verifier = Verifier::new("secret");
        let body = b"payload";
        let good = Signer::new("secret").sign(body, 10);
        let junk = format!(",v1={}", "ab".repeat(32)).repeat(100);
        assert!(verifier.verify_at(&format!("{}{}", good, junk), body, 10).is_ok());
        assert_eq!(verifier.verify_at(&format!("t=10{}", junk), body, 10), Err(VerifyError::Mismatch));
    }

    #[test]
    fn rotation() {
        let (old, new) = (Signer::new("old"), Signer::new("new"));
        let body = b"payload";
        // A verifier knowing both accepts either.
        let both = Verifier::new("new").secret("old");
        assert!(both.verify_at(&old.sign(body, 10), body, 10).is_ok());
        assert!(both.verify_at(&new.sign(body, 10), body, 10).is_ok());
        // A header signed with both is accepted by a verifier knowing either.
        let header = new.sign_with(&[&old], body, 10);
        assert!(Verifier::new("old").verify_at(&header, body, 10).is_ok());
        assert!(Verifier::new("new").verify_at(&header, body, 10).is_ok());
        // Unknown schemes are ignored.
        assert!(Verifier::new("new").verify_at(&format!("{},v0=abcd", new.sign(body, 10)), body, 10).is_ok());
        assert!(!format!("{:?}", old).contains("old"), "Debug hides the secret");
    }
}
