//! Input validation with per-field error messages.
//!
//! Implement [`Validate`] for a request type, then read it with
//! [`Request::valid_json`], [`Request::valid_query`] or
//! [`Request::valid_form`]: invalid input is answered with
//! `422 Unprocessable Entity` and the errors per field:
//!
//! ```json
//! {"error": {"message": "Validation failed", "status": 422,
//!            "fields": {"email": ["must be an email address"]}}}
//! ```
//!
//! ```rust
//! use axeon::{Json, Server, ServerError};
//! use axeon::validate::{is_email, Validate, ValidationErrors};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Deserialize, Serialize)]
//! struct SignUp {
//!     email: String,
//!     password: String,
//! }
//!
//! impl Validate for SignUp {
//!     fn validate(&self) -> Result<(), ValidationErrors> {
//!         let mut errors = ValidationErrors::new();
//!         errors.check(is_email(&self.email), "email", "must be an email address");
//!         errors.check(self.password.len() >= 12, "password", "must be at least 12 characters");
//!         errors.into_result()
//!     }
//! }
//!
//! let mut app = Server::new();
//! app.post("/signup", |req| async move {
//!     let form: SignUp = req.valid_json()?;
//!     Ok::<_, ServerError>((201, Json(form.email)))
//! });
//! ```
//!
//! To use the [`validator`](https://docs.rs/validator) crate's derive
//! instead, implement `Validate` by calling it and converting each of its
//! field errors with [`ValidationErrors::add`].

use crate::error::{ServerError, ServerResult};
use crate::message::Request;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;

/// A type that can check its own values.
pub trait Validate {
    /// Checks the value, returning every problem found.
    fn validate(&self) -> Result<(), ValidationErrors>;
}

/// Error messages per field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ValidationErrors {
    fields: BTreeMap<String, Vec<String>>,
}

impl ValidationErrors {
    /// No errors yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an error for `field`.
    pub fn add(&mut self, field: impl Into<String>, message: impl Into<String>) -> &mut Self {
        self.fields.entry(field.into()).or_default().push(message.into());
        self
    }

    /// Records `message` for `field` unless `ok` is true.
    pub fn check(&mut self, ok: bool, field: &str, message: &str) -> &mut Self {
        if !ok {
            self.add(field, message);
        }
        self
    }

    /// True if there are no errors.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The messages per field.
    pub fn fields(&self) -> &BTreeMap<String, Vec<String>> {
        &self.fields
    }

    /// `Ok(())` if there are no errors, otherwise `Err(self)`.
    pub fn into_result(self) -> Result<(), Self> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(self)
        }
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (field, messages) in &self.fields {
            for message in messages {
                if !first {
                    f.write_str("; ")?;
                }
                write!(f, "{} {}", field, message)?;
                first = false;
            }
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

impl From<ValidationErrors> for ServerError {
    fn from(errors: ValidationErrors) -> Self {
        ServerError::Invalid(errors)
    }
}

/// A basic structural email check: one `@`, a non-empty local part, and a
/// domain with a dot and no spaces. (Only sending a message proves an
/// address works.)
pub fn is_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else { return false };
    !local.is_empty()
        && local.len() <= 64
        && domain.len() <= 255
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// True if the string has between `min` and `max` characters (inclusive).
pub fn length_between(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.chars().count())
}

impl Request {
    /// [`json`](Request::json), then [`Validate::validate`]: 422 with field
    /// errors if invalid.
    pub fn valid_json<T: DeserializeOwned + Validate>(&self) -> ServerResult<T> {
        let value: T = self.json()?;
        value.validate()?;
        Ok(value)
    }

    /// [`query_as`](Request::query_as), then [`Validate::validate`].
    pub fn valid_query<T: DeserializeOwned + Validate>(&self) -> ServerResult<T> {
        let value: T = self.query_as()?;
        value.validate()?;
        Ok(value)
    }

    /// [`form`](Request::form), then [`Validate::validate`].
    pub fn valid_form<T: DeserializeOwned + Validate>(&self) -> ServerResult<T> {
        let value: T = self.form()?;
        value.validate()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_emails_and_lengths() {
        assert!(is_email("ada@example.com"));
        assert!(!is_email("ada@example"));
        assert!(!is_email("@example.com"));
        assert!(!is_email("ada example@x.com"));
        assert!(!is_email("a@b@c.com"));
        assert!(length_between("héllo", 5, 5));
        assert!(!length_between("hi", 3, 10));
    }

    #[test]
    fn collects_errors() {
        let mut errors = ValidationErrors::new();
        errors.check(true, "a", "never").check(false, "b", "bad").add("b", "worse");
        assert_eq!(errors.to_string(), "b bad; b worse");
        assert!(errors.into_result().is_err());
    }
}
