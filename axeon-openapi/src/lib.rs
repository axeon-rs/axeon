//! OpenAPI 3.1 documents for [Axeon](https://docs.rs/axeon) routes, built
//! from your Rust types with [utoipa](https://docs.rs/utoipa).
//!
//! Document a route with [`RouteDocs::doc`], build the document once all
//! routes are registered, and serve it:
//!
//! ```
//! use axeon::{Json, Server};
//! use axeon_openapi::{op, RouteDocs, Spec};
//! use serde::{Deserialize, Serialize};
//! use utoipa::{IntoParams, ToSchema};
//!
//! #[derive(Deserialize, IntoParams)]
//! struct ProductQuery { category: Option<String> }
//!
//! #[derive(Serialize, ToSchema)]
//! struct ProductDto { id: i64, name: String }
//!
//! let mut app = Server::new();
//! app.route("/v1/products")
//!     .doc(op().summary("List products").tag("Catalog")
//!         .query::<ProductQuery>()
//!         .response::<Vec<ProductDto>>(200, "Matching products"))
//!     .get(|_req| async { Json(Vec::<ProductDto>::new()) });
//! app.route("/v1/products/:id")
//!     .doc(op().summary("Get a product").tag("Catalog")
//!         .path_param::<i64>("id", "Product ID")
//!         .response::<ProductDto>(200, "The product")
//!         .error(404, "No such product"))
//!     .get(|_req| async { "..." });
//!
//! let openapi = Spec::new("Venture Studio API", "1.0").build(app.routes()).unwrap();
//! app.mount("/", axeon_openapi::router(&openapi)); // GET /openapi.json
//! ```
//!
//! A route builder's [`doc`](RouteDocs::doc) applies to every handler on
//! it, so document a path's methods with one builder each:
//! `app.route("/posts").doc(list).get(..)` and
//! `app.route("/posts").doc(create).post(..)`.
//!
//! Types come from utoipa's derives (`ToSchema` for bodies and responses,
//! `IntoParams` for query strings), so add `utoipa = "6"` to your
//! dependencies. Plain named types become named schemas (`ProductDto`);
//! generic ones are written inline, and the types they wrap must be added
//! with [`Operation::schema`] or [`Spec::schema`]. [`Spec::build`] checks
//! that every referenced schema exists, so a mistake fails at startup,
//! not in a client generator.

#![warn(missing_docs)]

mod operation;

pub use operation::{op, Operation};
pub use utoipa;

use axeon::{Method, Response, RouteBuilder, RouteInfo, Router};
use operation::{ResponseBody, Typed};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItemBuilder};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::schema::Schema;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme};
use utoipa::openapi::{
    ComponentsBuilder, Content, Deprecated, InfoBuilder, OpenApi, OpenApiBuilder, PathsBuilder, RefOr, Required,
    ResponseBuilder, ServerBuilder, Tag,
};
use utoipa::ToSchema;

/// The name of the schema for Axeon's error responses.
const ERROR_SCHEMA: &str = "ErrorResponse";

/// Adds [`doc`](RouteDocs::doc) to Axeon's route builder.
pub trait RouteDocs {
    /// Documents this builder's handlers.
    fn doc(&mut self, operation: Operation) -> &mut Self;
}

impl RouteDocs for RouteBuilder<'_> {
    fn doc(&mut self, operation: Operation) -> &mut Self {
        self.meta(operation)
    }
}

/// The document's settings: title, servers, security schemes and extra
/// schemas. [`build`](Spec::build) it from the app's routes.
#[derive(Clone)]
pub struct Spec {
    title: String,
    version: String,
    description: Option<String>,
    servers: Vec<(String, Option<String>)>,
    tags: Vec<(String, Option<String>)>,
    schemes: BTreeMap<String, SecurityScheme>,
    required: Vec<(String, Vec<String>)>,
    schemas: Vec<(String, RefOr<Schema>)>,
    include_undocumented: bool,
}

impl fmt::Debug for Spec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Spec").field("title", &self.title).field("version", &self.version).finish_non_exhaustive()
    }
}

impl Spec {
    /// A document for the API `title` at `version` (the API's version, not
    /// OpenAPI's).
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            description: None,
            servers: Vec::new(),
            tags: Vec::new(),
            schemes: BTreeMap::new(),
            required: Vec::new(),
            schemas: Vec::new(),
            include_undocumented: false,
        }
    }

    /// A description of the API; Markdown is allowed.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// A base URL the API is served at, e.g. `https://api.example.com`.
    pub fn server(mut self, url: impl Into<String>, description: Option<&str>) -> Self {
        self.servers.push((url.into(), description.map(str::to_string)));
        self
    }

    /// Describes a tag used by operations; tags are listed in this order.
    pub fn tag(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.tags.push((name.into(), Some(description.into())));
        self
    }

    /// Declares a bearer-token security scheme named `name` (a JWT in the
    /// `Authorization` header), for [`Operation::security`].
    pub fn bearer_auth(mut self, name: impl Into<String>) -> Self {
        let scheme = HttpBuilder::new().scheme(HttpAuthScheme::Bearer).bearer_format("JWT").build();
        self.schemes.insert(name.into(), SecurityScheme::Http(scheme));
        self
    }

    /// Declares an API key sent in the header `header`, named `name`.
    pub fn api_key(mut self, name: impl Into<String>, header: &str) -> Self {
        self.schemes.insert(name.into(), SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new(header))));
        self
    }

    /// Requires the scheme `name` on every documented operation that
    /// doesn't say otherwise ([`Operation::security`] or
    /// [`Operation::public`]).
    pub fn require(mut self, name: impl Into<String>) -> Self {
        self.required.push((name.into(), Vec::new()));
        self
    }

    /// Adds a named schema, e.g. the `T` of a generic response `Page<T>`.
    pub fn schema<T: ToSchema>(mut self) -> Self {
        self.schemas.extend(Typed::of::<T>().components);
        self
    }

    /// Also lists routes without [`doc`](RouteDocs::doc), with just their
    /// method, path and parameters. Off by default, so internal routes
    /// such as `/metrics` stay out of a public document.
    pub fn include_undocumented(mut self) -> Self {
        self.include_undocumented = true;
        self
    }

    /// Builds the document from `routes`, usually `app.routes()` once
    /// every route is registered.
    ///
    /// Fails when the document would be invalid: a referenced schema or
    /// security scheme that doesn't exist, two different schemas with one
    /// name, a documented path parameter that isn't in the path, or two
    /// operations with the same ID.
    pub fn build<'a>(&self, routes: impl IntoIterator<Item = RouteInfo<'a>>) -> Result<OpenApi, SpecError> {
        let mut problems = Vec::new();
        let mut components: BTreeMap<String, RefOr<Schema>> = BTreeMap::new();
        let mut add = |name: String, schema: RefOr<Schema>, problems: &mut Vec<String>| match components.get(&name) {
            Some(existing) if !same(existing, &schema) => {
                problems.push(format!("two different schemas are named `{}`: rename one with #[schema(as = ...)]", name))
            }
            Some(_) => {}
            None => {
                components.insert(name, schema);
            }
        };
        for (name, schema) in &self.schemas {
            add(name.clone(), schema.clone(), &mut problems);
        }

        let mut paths: BTreeMap<String, PathItemBuilder> = BTreeMap::new();
        let mut ids = BTreeSet::new();
        let mut uses_error = false;
        let undocumented = Operation::default();
        for route in routes {
            let operation = match route.meta::<Operation>() {
                Some(operation) if !operation.hidden => operation,
                None if self.include_undocumented => &undocumented,
                _ => continue,
            };
            let Some(method) = http_method(route.method()) else { continue };
            let (path, names) = openapi_path(route.path());
            let id = operation.operation_id.clone().unwrap_or_else(|| default_id(route.method(), route.path()));
            if !ids.insert(id.clone()) {
                problems.push(format!("two operations have the ID `{}`; set one with .operation_id(...)", id));
            }
            let mut builder = OperationBuilder::new()
                .operation_id(Some(id))
                .summary(operation.summary.clone())
                .description(operation.description.clone());
            if !operation.tags.is_empty() {
                builder = builder.tags(Some(operation.tags.clone()));
            }
            if operation.deprecated {
                builder = builder.deprecated(Some(Deprecated::True));
            }
            for name in operation.path_params.keys() {
                if !names.contains(name) {
                    problems.push(format!("{} {} documents path parameter `{}`, which isn't in the path", route.method(), route.path(), name));
                }
            }
            for name in &names {
                let parameter = operation.path_params.get(name).cloned().unwrap_or_else(|| {
                    ParameterBuilder::new()
                        .name(name)
                        .parameter_in(ParameterIn::Path)
                        .required(Required::True)
                        .schema(Some(<String as utoipa::PartialSchema>::schema()))
                        .build()
                });
                builder = builder.parameter(parameter);
            }
            for parameter in &operation.parameters {
                builder = builder.parameter(parameter.clone());
            }
            if let Some((content_type, typed, required)) = &operation.body {
                for (name, schema) in &typed.components {
                    add(name.clone(), schema.clone(), &mut problems);
                }
                let body = RequestBodyBuilder::new()
                    .content(content_type, Content::new(Some(typed.schema.clone())))
                    .required(Some(if *required { Required::True } else { Required::False }))
                    .build();
                builder = builder.request_body(Some(body));
            }
            if operation.responses.is_empty() {
                builder = builder.response("200", ResponseBuilder::new().description("Success").build());
            }
            for (status, response) in &operation.responses {
                let mut doc = ResponseBuilder::new().description(&response.description);
                match &response.body {
                    ResponseBody::None => {}
                    ResponseBody::Json(typed) => {
                        for (name, schema) in &typed.components {
                            add(name.clone(), schema.clone(), &mut problems);
                        }
                        doc = doc.content("application/json", Content::new(Some(typed.schema.clone())));
                    }
                    ResponseBody::Error => {
                        uses_error = true;
                        let reference = RefOr::Ref(utoipa::openapi::Ref::from_schema_name(ERROR_SCHEMA));
                        doc = doc.content("application/json", Content::new(Some(reference)));
                    }
                }
                builder = builder.response(status.to_string(), doc.build());
            }
            let security = if !operation.security.is_empty() {
                operation.security.clone()
            } else if operation.public || route.meta::<Operation>().is_none() {
                Vec::new()
            } else {
                self.required.clone()
            };
            for (scheme, scopes) in &security {
                if !self.schemes.contains_key(scheme) {
                    problems.push(format!(
                        "{} {} requires security scheme `{}`, which isn't declared; add it with .bearer_auth() or .api_key()",
                        route.method(),
                        route.path(),
                        scheme
                    ));
                }
                builder = builder.security(SecurityRequirement::new(scheme, scopes.clone()));
            }
            for (name, schema) in &operation.schemas {
                add(name.clone(), schema.clone(), &mut problems);
            }
            let item = paths.remove(&path).unwrap_or_default().operation(method, builder.build());
            paths.insert(path, item);
        }
        if uses_error {
            add(ERROR_SCHEMA.to_string(), error_schema(), &mut problems);
        }

        let mut info = InfoBuilder::new().title(&self.title).version(&self.version);
        if let Some(description) = &self.description {
            info = info.description(Some(description));
        }
        let mut path_builder = PathsBuilder::new();
        for (path, item) in paths {
            path_builder = path_builder.path(path, item.build());
        }
        let mut component_builder = ComponentsBuilder::new();
        for (name, schema) in &components {
            component_builder = component_builder.schema(name, schema.clone());
        }
        for (name, scheme) in &self.schemes {
            component_builder = component_builder.security_scheme(name, scheme.clone());
        }
        let mut document = OpenApiBuilder::new().info(info.build()).paths(path_builder.build()).components(Some(component_builder.build()));
        if !self.servers.is_empty() {
            let servers = self.servers.iter().map(|(url, description)| ServerBuilder::new().url(url).description(description.clone()).build());
            document = document.servers(Some(servers.collect::<Vec<_>>()));
        }
        if !self.tags.is_empty() {
            let tags = self.tags.iter().map(|(name, description)| {
                let mut tag = Tag::new(name);
                tag.description = description.clone();
                tag
            });
            document = document.tags(Some(tags.collect::<Vec<_>>()));
        }
        let document = document.build();

        // Every `$ref` must point at a schema in the document.
        let json = serde_json::to_value(&document).unwrap_or_default();
        let mut missing = BTreeSet::new();
        find_refs(&json, &mut |name| {
            if !components.contains_key(name) {
                missing.insert(name.to_string());
            }
        });
        for name in missing {
            problems.push(format!(
                "schema `{}` is referenced but not defined; add it with .schema::<{}>() on the operation or the Spec",
                name, name
            ));
        }
        if problems.is_empty() {
            Ok(document)
        } else {
            Err(SpecError { problems })
        }
    }
}

/// Why [`Spec::build`] couldn't make a valid document.
#[derive(Debug)]
pub struct SpecError {
    problems: Vec<String>,
}

impl SpecError {
    /// Each problem found.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid OpenAPI document: {}", self.problems.join("; "))
    }
}

impl std::error::Error for SpecError {}

/// A router serving the document at `GET /openapi.json`. It's serialized
/// once, here.
pub fn router(document: &OpenApi) -> Router {
    let json = document.to_json().expect("an OpenAPI document serializes");
    let mut router = Router::new();
    router.get("/openapi.json", move |_req| {
        let json = json.clone();
        async move { Response::new(200).with_header("Content-Type", "application/json").with_body(json) }
    });
    router
}

fn same(a: &RefOr<Schema>, b: &RefOr<Schema>) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

fn find_refs(value: &serde_json::Value, found: &mut impl FnMut(&str)) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(target)) = map.get("$ref") {
                if let Some(name) = target.strip_prefix("#/components/schemas/") {
                    found(name);
                }
            }
            map.values().for_each(|v| find_refs(v, found));
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| find_refs(v, found)),
        _ => {}
    }
}

fn http_method(method: Method) -> Option<HttpMethod> {
    Some(match method {
        Method::GET => HttpMethod::Get,
        Method::POST => HttpMethod::Post,
        Method::PUT => HttpMethod::Put,
        Method::PATCH => HttpMethod::Patch,
        Method::DELETE => HttpMethod::Delete,
        Method::HEAD => HttpMethod::Head,
        Method::OPTIONS => HttpMethod::Options,
        _ => return None,
    })
}

/// `/users/:id/*rest` → `/users/{id}/{rest}`, and the parameter names.
fn openapi_path(path: &str) -> (String, Vec<String>) {
    let mut names = Vec::new();
    let converted: Vec<String> = path
        .split('/')
        .map(|segment| match segment.strip_prefix(':').or_else(|| segment.strip_prefix('*')) {
            Some(name) if !name.is_empty() => {
                names.push(name.to_string());
                format!("{{{}}}", name)
            }
            _ => segment.to_string(),
        })
        .collect();
    (converted.join("/"), names)
}

/// `GET /v1/products/:id` → `get_v1_products_by_id`.
fn default_id(method: Method, path: &str) -> String {
    let mut id = method.as_str().to_ascii_lowercase();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        id.push('_');
        if let Some(name) = segment.strip_prefix(':').or_else(|| segment.strip_prefix('*')) {
            id.push_str("by_");
            id.push_str(name);
        } else {
            id.extend(segment.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }));
        }
    }
    id
}

/// Axeon's error body: `{"error": {"message", "status", "fields"?}}`.
fn error_schema() -> RefOr<Schema> {
    let json = serde_json::json!({
        "type": "object",
        "required": ["error"],
        "properties": {
            "error": {
                "type": "object",
                "required": ["message", "status"],
                "properties": {
                    "message": { "type": "string" },
                    "status": { "type": "integer", "format": "int32" },
                    "fields": {
                        "type": "object",
                        "description": "Validation errors per field (422 only).",
                        "additionalProperties": { "type": "array", "items": { "type": "string" } }
                    }
                }
            }
        }
    });
    RefOr::T(serde_json::from_value(json).expect("a valid schema"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_ids() {
        assert_eq!(openapi_path("/v1/products/:id"), ("/v1/products/{id}".to_string(), vec!["id".to_string()]));
        assert_eq!(openapi_path("/files/*rest"), ("/files/{rest}".to_string(), vec!["rest".to_string()]));
        assert_eq!(openapi_path("/"), ("/".to_string(), vec![]));
        assert_eq!(default_id(Method::GET, "/v1/products/:id"), "get_v1_products_by_id");
        assert_eq!(default_id(Method::POST, "/v1/sign-ups"), "post_v1_sign_ups");
        assert_eq!(default_id(Method::GET, "/"), "get");
    }
}
