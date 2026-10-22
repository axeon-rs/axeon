//! Authorization: what an authenticated caller may do.

use crate::error::{ServerError, ServerResult};
use crate::message::{Request, Response};
use crate::middleware::{Middleware, MiddlewareResult, Next};
use std::collections::BTreeSet;

/// The scopes and roles of the authenticated caller, as a request
/// extension. Authentication middleware adds it ([`ApiKeyAuth`] does, from
/// the key's scopes); with your own authentication, insert one:
///
/// ```rust
/// use axeon::auth::Grants;
/// use axeon::middleware::from_fn;
///
/// # let mut app = axeon::Server::new();
/// app.middleware(from_fn(|mut req, next| async move {
///     // e.g. from a verified JWT's `scope` claim and the user's roles
///     req.insert_extension(Grants::new().scopes_from("catalog:read selections:write").role("buyer"));
///     next.handle(req).await
/// }));
/// ```
///
/// A granted scope ending in `:*` covers every scope with that prefix
/// (`catalog:*` covers `catalog:read`), and `*` covers all. Roles match
/// exactly.
///
/// [`ApiKeyAuth`]: crate::auth::api_key::ApiKeyAuth
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grants {
    scopes: BTreeSet<String>,
    roles: BTreeSet<String>,
}

impl Grants {
    /// No scopes or roles.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a scope.
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scopes.insert(scope.into());
        self
    }

    /// Adds scopes.
    pub fn scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes.extend(scopes.into_iter().map(Into::into));
        self
    }

    /// Adds the scopes in a space-separated list, like an OAuth `scope`
    /// claim.
    pub fn scopes_from(self, list: &str) -> Self {
        self.scopes(list.split_whitespace())
    }

    /// Adds a role.
    pub fn role(mut self, role: impl Into<String>) -> Self {
        self.roles.insert(role.into());
        self
    }

    /// Adds roles.
    pub fn roles<I, S>(mut self, roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.roles.extend(roles.into_iter().map(Into::into));
        self
    }

    /// Whether `scope` is granted, directly or by a wildcard.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|granted| match granted.strip_suffix('*') {
            Some("") => true,
            Some(prefix) if prefix.ends_with(':') => scope.starts_with(prefix),
            _ => granted == scope,
        })
    }

    /// Whether the caller has `role`.
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.contains(role)
    }

    /// The granted scopes.
    pub fn scope_list(&self) -> impl Iterator<Item = &str> {
        self.scopes.iter().map(String::as_str)
    }

    /// The caller's roles.
    pub fn role_list(&self) -> impl Iterator<Item = &str> {
        self.roles.iter().map(String::as_str)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Scope,
    Role,
}

/// One requirement: any of these scopes or roles.
#[derive(Clone, Debug)]
struct Need {
    kind: Kind,
    any_of: Vec<String>,
}

impl Need {
    fn met_by(&self, grants: &Grants) -> bool {
        self.any_of.iter().any(|name| match self.kind {
            Kind::Scope => grants.has_scope(name),
            Kind::Role => grants.has_role(name),
        })
    }

    /// The refusal, if any: 401 without grants, 403 with the RFC 6750
    /// challenge when lacking.
    fn refusal(&self, req: &Request) -> Option<Response> {
        let Some(grants) = req.extension::<Grants>() else {
            let error = ServerError::Unauthorized("authentication required".into());
            return Some(Response::error(error).with_header("WWW-Authenticate", "Bearer"));
        };
        if self.met_by(grants) {
            return None;
        }
        let wanted = self.any_of.join(" ");
        Some(match self.kind {
            Kind::Scope => Response::error(ServerError::Forbidden(format!("requires scope {}", wanted.replace(' ', " or "))))
                .with_header(
                    "WWW-Authenticate",
                    format!("Bearer error=\"insufficient_scope\", scope=\"{}\"", wanted.replace('"', "")),
                ),
            Kind::Role => Response::error(ServerError::Forbidden(format!("requires role {}", wanted.replace(' ', " or ")))),
        })
    }
}

/// Middleware from [`require_scope`], [`require_any_scope`],
/// [`require_role`] or [`require_any_role`].
#[derive(Clone, Debug)]
pub struct Require {
    need: Need,
}

impl Middleware for Require {
    fn call(&self, req: Request, next: Next) -> MiddlewareResult {
        match self.need.refusal(&req) {
            None => next.handle(req),
            Some(response) => Box::pin(async move { Ok(response) }),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.need.any_of.is_empty() {
            return Err("a scope or role guard needs at least one name".into());
        }
        Ok(())
    }
}

/// Requires the [`Grants`] to include `scope`: `401` without grants (put it
/// after authentication), `403` with an `insufficient_scope` challenge
/// otherwise.
///
/// ```rust
/// use axeon::auth::require_scope;
/// use axeon::Server;
///
/// let mut app = Server::new();
/// app.route("/v1/products").middleware(require_scope("catalog:read")).get(|_req| async { "[]" });
/// ```
pub fn require_scope(scope: impl Into<String>) -> Require {
    require_any_scope([scope])
}

/// Requires at least one of `scopes`.
pub fn require_any_scope<I, S>(scopes: I) -> Require
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    Require { need: Need { kind: Kind::Scope, any_of: scopes.into_iter().map(Into::into).collect() } }
}

/// Requires the caller to have `role`; see [`require_scope`].
pub fn require_role(role: impl Into<String>) -> Require {
    require_any_role([role])
}

/// Requires at least one of `roles`.
pub fn require_any_role<I, S>(roles: I) -> Require
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    Require { need: Need { kind: Kind::Role, any_of: roles.into_iter().map(Into::into).collect() } }
}

impl Request {
    /// The caller's [`Grants`], if authentication attached any.
    pub fn grants(&self) -> Option<&Grants> {
        self.extension::<Grants>()
    }

    /// Fails with `401` without [`Grants`], or `403` when `scope` isn't
    /// granted: for checks that depend on the request, inside a handler.
    pub fn require_scope(&self, scope: &str) -> ServerResult<()> {
        match self.grants() {
            None => Err(ServerError::Unauthorized("authentication required".into())),
            Some(grants) if grants.has_scope(scope) => Ok(()),
            Some(_) => Err(ServerError::Forbidden(format!("requires scope {}", scope))),
        }
    }

    /// Like [`require_scope`](Request::require_scope), for a role.
    pub fn require_role(&self, role: &str) -> ServerResult<()> {
        match self.grants() {
            None => Err(ServerError::Unauthorized("authentication required".into())),
            Some(grants) if grants.has_role(role) => Ok(()),
            Some(_) => Err(ServerError::Forbidden(format!("requires role {}", role))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_and_wildcards() {
        let grants = Grants::new().scopes_from("catalog:* selections:read").role("buyer");
        assert!(grants.has_scope("catalog:read") && grants.has_scope("catalog:write"));
        assert!(grants.has_scope("selections:read"));
        assert!(!grants.has_scope("selections:write"));
        assert!(!grants.has_scope("catalogue:read"), "prefix wildcards stop at the colon");
        assert!(Grants::new().scope("*").has_scope("anything"));
        assert!(!Grants::new().scope("cat*").has_scope("catalog:read"), "only `:*` and `*` are wildcards");
        assert!(grants.has_role("buyer") && !grants.has_role("operator"));
    }
}
