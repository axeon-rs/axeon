//! Authentication: reading credentials, bearer-token and basic-auth
//! middleware, and JSON Web Tokens (feature `jwt`).
//!
//! ```rust
//! use axeon::{Server, ServerError};
//! use axeon::auth::BearerAuth;
//!
//! #[derive(Clone)]
//! struct User {
//!     name: String,
//! }
//!
//! let mut app = Server::new();
//! // Look the token up however you like (database, cache, API).
//! app.middleware(BearerAuth::new(|token: String| async move {
//!     (token == "let-me-in").then(|| User { name: "ada".into() })
//! }));
//! app.get("/me", |req| async move { req.extension::<User>().map(|u| u.name.clone()) });
//! ```
//!
//! Failed authentication is answered with `401 Unauthorized` and a
//! `WWW-Authenticate` header.

use crate::error::ServerError;
use crate::message::{Request, Response};
use crate::middleware::{Middleware, MiddlewareResult, Next};
use base64::Engine;
use std::future::Future;
use std::sync::Arc;

/// Compares two byte strings in time that depends only on their lengths,
/// so attackers can't guess secrets character by character from timing.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// A 401 response with a `WWW-Authenticate` challenge.
fn challenge(value: &str, message: &str) -> Response {
    Response::error(ServerError::Unauthorized(message.to_string())).with_header("WWW-Authenticate", value)
}

impl Request {
    /// The token from `Authorization: Bearer <token>`.
    pub fn bearer_token(&self) -> Option<&str> {
        let (scheme, token) = self.get_header("authorization")?.split_once(' ')?;
        let token = token.trim();
        (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
    }

    /// The user name and password from `Authorization: Basic <base64>`.
    pub fn basic_auth(&self) -> Option<(String, String)> {
        let (scheme, encoded) = self.get_header("authorization")?.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("basic") {
            return None;
        }
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
        let decoded = String::from_utf8(decoded).ok()?;
        let (user, password) = decoded.split_once(':')?;
        Some((user.to_string(), password.to_string()))
    }
}

/// Requires `Authorization: Bearer <token>` and checks it with an async
/// function. The identity it returns is stored as a request extension for
/// handlers (`req.extension::<T>()`).
pub struct BearerAuth<F> {
    check: Arc<F>,
}

impl<F> BearerAuth<F> {
    /// Checks each bearer token with `check`; `Some(identity)` accepts it.
    pub fn new<Fut, T>(check: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<T>> + Send + 'static,
        T: Clone + Send + Sync + 'static,
    {
        Self { check: Arc::new(check) }
    }
}

impl<F, Fut, T> Middleware for BearerAuth<F>
where
    F: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Option<T>> + Send + 'static,
    T: Clone + Send + Sync + 'static,
{
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        let Some(token) = req.bearer_token().map(str::to_string) else {
            return Box::pin(async { Ok(challenge("Bearer", "missing bearer token")) });
        };
        let check = Arc::clone(&self.check);
        Box::pin(async move {
            match check(token).await {
                Some(identity) => {
                    req.insert_extension(identity);
                    next.handle(req).await
                }
                None => Ok(challenge("Bearer error=\"invalid_token\"", "invalid token")),
            }
        })
    }
}

/// The user authenticated by [`BasicAuth`], as a request extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BasicUser(pub String);

type BasicCheck = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// HTTP Basic authentication, e.g. for an admin area or internal tools.
/// Use only over HTTPS: the password is sent with every request.
///
/// ```rust
/// use axeon::Server;
/// use axeon::auth::BasicAuth;
///
/// let mut app = Server::new();
/// app.middleware(BasicAuth::single("admin area", "admin", "correct horse battery staple"));
/// ```
pub struct BasicAuth {
    realm: String,
    check: BasicCheck,
}

impl BasicAuth {
    /// Checks credentials with `check(user, password)`. Compare secrets with
    /// [`constant_time_eq`].
    pub fn new<F>(realm: &str, check: F) -> Self
    where
        F: Fn(&str, &str) -> bool + Send + Sync + 'static,
    {
        Self { realm: realm.replace('"', "'"), check: Arc::new(check) }
    }

    /// Accepts exactly one user name and password.
    pub fn single(realm: &str, user: &str, password: &str) -> Self {
        let (user, password) = (user.to_string(), password.to_string());
        Self::new(realm, move |u, p| {
            // Evaluate both comparisons to keep the timing independent of which failed.
            let user_ok = constant_time_eq(u.as_bytes(), user.as_bytes());
            let password_ok = constant_time_eq(p.as_bytes(), password.as_bytes());
            user_ok & password_ok
        })
    }
}

impl Middleware for BasicAuth {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        match req.basic_auth() {
            Some((user, password)) if (self.check)(&user, &password) => {
                req.insert_extension(BasicUser(user));
                next.handle(req)
            }
            _ => {
                let value = format!("Basic realm=\"{}\", charset=\"UTF-8\"", self.realm);
                Box::pin(async move { Ok(challenge(&value, "authentication required")) })
            }
        }
    }
}

/// JSON Web Tokens (feature `jwt`): issue tokens and verify them in
/// middleware.
///
/// ```rust
/// use axeon::{Json, Server, ServerError};
/// use axeon::auth::jwt::Jwt;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Clone, Serialize, Deserialize)]
/// struct Claims {
///     sub: String,
///     exp: u64, // expiry (seconds since the epoch) is required
/// }
///
/// // Load the secret from configuration; at least 32 random bytes.
/// let jwt = Jwt::hs256(b"an example secret of at least 32 bytes!!");
///
/// let mut app = Server::new();
/// app.state(jwt.clone());
/// app.post("/token", |req| async move {
///     let jwt = req.state::<Jwt>().unwrap();
///     let claims = Claims { sub: "ada".into(), exp: axeon::auth::jwt::now() + 3600 };
///     Ok::<_, ServerError>(Json(jwt.issue(&claims)?))
/// });
///
/// let mut api = axeon::Router::new();
/// api.middleware(jwt.middleware::<Claims>());
/// api.get("/me", |req| async move { req.extension::<Claims>().map(|c| c.sub.clone()) });
/// app.mount("/api", api);
/// ```
#[cfg(feature = "jwt")]
pub mod jwt {
    use super::challenge;
    use crate::error::{ServerError, ServerResult};
    use crate::message::Request;
    use crate::middleware::{Middleware, MiddlewareResult, Next};
    use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use std::marker::PhantomData;
    use std::sync::Arc;
    use std::time::Duration;

    /// A PEM key passed to [`Jwt::rs256`] or [`Jwt::es256`] couldn't be read.
    #[derive(Debug)]
    pub struct KeyError(jsonwebtoken::errors::Error);

    impl std::fmt::Display for KeyError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "invalid key: {}", self.0)
        }
    }

    impl std::error::Error for KeyError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// Seconds since the Unix epoch, for `exp`, `iat` and `nbf` claims.
    pub fn now() -> u64 {
        jsonwebtoken::get_current_timestamp()
    }

    /// Keys and rules for issuing and verifying tokens. Cheap to clone.
    #[derive(Clone)]
    pub struct Jwt {
        inner: Arc<Inner>,
    }

    struct Inner {
        header: Header,
        encoding: Option<EncodingKey>,
        decoding: DecodingKey,
        validation: Validation,
    }

    impl Jwt {
        fn with(algorithm: Algorithm, encoding: Option<EncodingKey>, decoding: DecodingKey) -> Self {
            let mut validation = Validation::new(algorithm);
            validation.leeway = 30;
            Self { inner: Arc::new(Inner { header: Header::new(algorithm), encoding, decoding, validation }) }
        }

        fn inner_mut(&mut self) -> &mut Inner {
            Arc::get_mut(&mut self.inner).expect("configure Jwt before cloning it")
        }

        /// HMAC-SHA256 with a shared secret (use at least 32 random bytes).
        pub fn hs256(secret: &[u8]) -> Self {
            Self::with(Algorithm::HS256, Some(EncodingKey::from_secret(secret)), DecodingKey::from_secret(secret))
        }

        /// RSA-SHA256 from PEM keys. Without a private key the instance can
        /// only verify tokens (e.g. issued by an identity provider).
        pub fn rs256(public_pem: &[u8], private_pem: Option<&[u8]>) -> Result<Self, KeyError> {
            let encoding = private_pem.map(EncodingKey::from_rsa_pem).transpose().map_err(KeyError)?;
            let decoding = DecodingKey::from_rsa_pem(public_pem).map_err(KeyError)?;
            Ok(Self::with(Algorithm::RS256, encoding, decoding))
        }

        /// ECDSA P-256 from PEM keys; see [`Jwt::rs256`].
        pub fn es256(public_pem: &[u8], private_pem: Option<&[u8]>) -> Result<Self, KeyError> {
            let encoding = private_pem.map(EncodingKey::from_ec_pem).transpose().map_err(KeyError)?;
            let decoding = DecodingKey::from_ec_pem(public_pem).map_err(KeyError)?;
            Ok(Self::with(Algorithm::ES256, encoding, decoding))
        }

        /// Only accept tokens with this `iss` claim.
        pub fn issuer(mut self, issuer: &str) -> Self {
            self.inner_mut().validation.set_issuer(&[issuer]);
            self
        }

        /// Only accept tokens with this `aud` claim.
        pub fn audience(mut self, audience: &str) -> Self {
            self.inner_mut().validation.set_audience(&[audience]);
            self
        }

        /// Clock skew tolerated for `exp` and `nbf`. Default: 30 seconds.
        pub fn leeway(mut self, leeway: Duration) -> Self {
            self.inner_mut().validation.leeway = leeway.as_secs();
            self
        }

        /// Signs `claims` into a token. Include an `exp` claim: tokens
        /// without one are rejected when verified.
        pub fn issue<C: Serialize>(&self, claims: &C) -> ServerResult<String> {
            let key = self
                .inner
                .encoding
                .as_ref()
                .ok_or_else(|| ServerError::InternalError("this Jwt has no private key to sign with".to_string()))?;
            jsonwebtoken::encode(&self.inner.header, claims, key)
                .map_err(|e| ServerError::InternalError(format!("failed to sign token: {}", e)))
        }

        /// Verifies a token's signature, expiry and configured issuer and
        /// audience, returning its claims. Fails with 401.
        pub fn verify<C: DeserializeOwned>(&self, token: &str) -> ServerResult<C> {
            jsonwebtoken::decode::<C>(token, &self.inner.decoding, &self.inner.validation)
                .map(|data| data.claims)
                .map_err(|e| ServerError::Unauthorized(format!("invalid token: {}", e)))
        }

        /// Middleware requiring a valid bearer token; its claims are stored
        /// as a request extension of type `C`.
        pub fn middleware<C>(&self) -> JwtAuth<C>
        where
            C: DeserializeOwned + Clone + Send + Sync + 'static,
        {
            JwtAuth { jwt: self.clone(), claims: PhantomData }
        }
    }

    /// Middleware from [`Jwt::middleware`].
    pub struct JwtAuth<C> {
        jwt: Jwt,
        claims: PhantomData<fn() -> C>,
    }

    impl<C> Middleware for JwtAuth<C>
    where
        C: DeserializeOwned + Clone + Send + Sync + 'static,
    {
        fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
            let Some(token) = req.bearer_token() else {
                return Box::pin(async { Ok(challenge("Bearer", "missing bearer token")) });
            };
            match self.jwt.verify::<C>(token) {
                Ok(claims) => {
                    req.insert_extension(claims);
                    next.handle(req)
                }
                Err(_) => Box::pin(async { Ok(challenge("Bearer error=\"invalid_token\"", "invalid token")) }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secrets"));
    }
}
