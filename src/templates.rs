//! HTML templates with Jinja syntax (feature `templates`, rendered by
//! [minijinja](https://docs.rs/minijinja)).
//!
//! ```rust,no_run
//! use axeon::{json, Server, ServerError};
//! use axeon::templates::Templates;
//!
//! let mut app = Server::new();
//! // views/hello.html: <h1>Hello, {{ name }}!</h1>
//! app.templates(Templates::from_dir("views"));
//! app.get("/hello/:name", |req| async move {
//!     req.render("hello.html", json!({ "name": req.params()["name"] }))
//! });
//! ```
//!
//! Templates ending in `.html`, `.htm`, `.xml` or `.svg` escape their
//! variables, so user input can't inject markup. Templates are loaded on
//! first use and cached; restart to pick up changes. The context is any
//! `Serialize` value, e.g. `json!({..})` or your own struct.

use crate::error::{ServerError, ServerResult};
use crate::message::{Request, Response};
use crate::Server;
use serde::Serialize;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

/// A template that couldn't be added.
#[derive(Debug, Clone)]
pub struct TemplateError {
    message: String,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TemplateError {}

/// A set of templates. Cheap to clone.
#[derive(Clone)]
pub struct Templates {
    env: Arc<minijinja::Environment<'static>>,
}

impl Default for Templates {
    fn default() -> Self {
        Self::new()
    }
}

impl Templates {
    /// No templates; add them with [`Templates::add`].
    pub fn new() -> Self {
        Self { env: Arc::new(minijinja::Environment::new()) }
    }

    /// Loads templates from files under `dir` by relative name
    /// (`"emails/welcome.html"`). Names can't escape the directory.
    pub fn from_dir(dir: impl AsRef<Path>) -> Self {
        let mut env = minijinja::Environment::new();
        env.set_loader(minijinja::path_loader(dir.as_ref()));
        Self { env: Arc::new(env) }
    }

    /// Adds a template from a string, e.g. one embedded with `include_str!`.
    /// Fails if it has a syntax error.
    pub fn add(mut self, name: &str, source: &str) -> Result<Self, TemplateError> {
        let env = Arc::get_mut(&mut self.env).expect("add templates before cloning");
        env.add_template_owned(name.to_string(), source.to_string())
            .map_err(|e| TemplateError { message: e.to_string() })?;
        Ok(self)
    }

    /// Renders a template to a string. Errors are logged in detail and
    /// returned as a generic 500, so template internals don't leak.
    pub fn render(&self, name: &str, context: impl Serialize) -> ServerResult<String> {
        let result = self.env.get_template(name).and_then(|template| template.render(context));
        result.map_err(|error| {
            tracing::error!(template = name, error = %error, "template rendering failed");
            ServerError::InternalError("failed to render page".to_string())
        })
    }
}

impl Server {
    /// Registers the templates used by [`Request::render`].
    pub fn templates(&mut self, templates: Templates) -> &mut Self {
        self.state(templates)
    }
}

impl Request {
    /// Renders a template (registered with [`Server::templates`]) as a
    /// 200 HTML response.
    pub fn render(&self, name: &str, context: impl Serialize) -> ServerResult<Response> {
        let templates = self
            .state::<Templates>()
            .ok_or_else(|| ServerError::InternalError("no templates registered: use Server::templates".to_string()))?;
        Ok(Response::html(templates.render(name, context)?))
    }
}
