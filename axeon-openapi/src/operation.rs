//! Documentation for one route: [`Operation`], from [`op`].

use std::collections::BTreeMap;
use utoipa::openapi::path::{Parameter, ParameterBuilder, ParameterIn};
use utoipa::openapi::schema::Schema;
use utoipa::openapi::{RefOr, Required};
use utoipa::{IntoParams, PartialSchema, ToSchema};

/// A schema to use in the document, and the named schemas it needs.
#[derive(Clone)]
pub(crate) struct Typed {
    pub(crate) schema: RefOr<Schema>,
    pub(crate) components: Vec<(String, RefOr<Schema>)>,
}

impl Typed {
    /// A plain named type, such as a struct deriving `ToSchema`, becomes a
    /// named component referred to by `$ref`, so generated clients get its
    /// name. Generic instantiations (`Page<Product>`), collections and
    /// primitives are written inline.
    pub(crate) fn of<T: ToSchema>() -> Self {
        let mut components = Vec::new();
        T::schemas(&mut components);
        let schema = T::schema();
        let named = !std::any::type_name::<T>().contains('<') && is_object_like(&schema);
        if named {
            let name = T::name().into_owned();
            components.push((name.clone(), schema));
            let reference = utoipa::openapi::Ref::from_schema_name(name);
            Typed { schema: RefOr::Ref(reference), components }
        } else {
            Typed { schema, components }
        }
    }
}

/// Objects, enums and unions get a name; strings, numbers and arrays don't.
fn is_object_like(schema: &RefOr<Schema>) -> bool {
    let RefOr::T(schema) = schema else { return false };
    let json = serde_json::to_value(schema).unwrap_or_default();
    json.get("type").and_then(|t| t.as_str()) == Some("object")
        || json.get("enum").is_some()
        || json.get("oneOf").is_some()
        || json.get("allOf").is_some()
        || json.get("anyOf").is_some()
}

#[derive(Clone)]
pub(crate) enum ResponseBody {
    None,
    Json(Box<Typed>),
    Error,
}

#[derive(Clone)]
pub(crate) struct ResponseDoc {
    pub(crate) description: String,
    pub(crate) body: ResponseBody,
}

/// The documentation of a route, attached with
/// [`RouteDocs::doc`](crate::RouteDocs::doc). Start one with [`op`].
///
/// ```
/// use axeon_openapi::op;
/// use serde::{Deserialize, Serialize};
/// use utoipa::{IntoParams, ToSchema};
///
/// #[derive(Deserialize, IntoParams)]
/// struct ProductQuery { category: Option<String>, moq_max: Option<u32> }
///
/// #[derive(Serialize, ToSchema)]
/// struct ProductDto { id: i64, name: String }
///
/// let list = op()
///     .summary("List products")
///     .tag("Catalog")
///     .query::<ProductQuery>()
///     .response::<Vec<ProductDto>>(200, "Matching products")
///     .error(400, "Invalid filters")
///     .security("api_key");
/// ```
#[derive(Clone, Default)]
pub struct Operation {
    pub(crate) summary: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) operation_id: Option<String>,
    pub(crate) deprecated: bool,
    pub(crate) hidden: bool,
    pub(crate) parameters: Vec<Parameter>,
    /// Explicitly documented path parameters, by name.
    pub(crate) path_params: BTreeMap<String, Parameter>,
    pub(crate) body: Option<(String, Typed, bool)>,
    pub(crate) responses: BTreeMap<u16, ResponseDoc>,
    pub(crate) security: Vec<(String, Vec<String>)>,
    pub(crate) public: bool,
    pub(crate) schemas: Vec<(String, RefOr<Schema>)>,
}

impl std::fmt::Debug for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Operation")
            .field("summary", &self.summary)
            .field("tags", &self.tags)
            .field("operation_id", &self.operation_id)
            .field("responses", &self.responses.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Starts documenting a route.
pub fn op() -> Operation {
    Operation::default()
}

impl Operation {
    /// A one-line summary.
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// A longer description; Markdown is allowed.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Groups the route under a tag; call again for more.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// The operation's ID, which client generators use as the function
    /// name. Default: from the method and path, e.g. `get_v1_products_by_id`.
    pub fn operation_id(mut self, id: impl Into<String>) -> Self {
        self.operation_id = Some(id.into());
        self
    }

    /// Marks the route deprecated.
    pub fn deprecated(mut self) -> Self {
        self.deprecated = true;
        self
    }

    /// Leaves the route out of the document, even with
    /// [`Spec::include_undocumented`](crate::Spec::include_undocumented).
    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }

    /// Query parameters from a type deriving `IntoParams`, usually the one
    /// the handler reads with `req.query()`.
    pub fn query<T: IntoParams>(mut self) -> Self {
        self.parameters.extend(T::into_params(|| Some(ParameterIn::Query)));
        self
    }

    /// Documents a path parameter, such as `id` in `/products/:id`. Path
    /// parameters are documented as required strings without this.
    pub fn path_param<T: PartialSchema>(mut self, name: &str, description: impl Into<String>) -> Self {
        let parameter = ParameterBuilder::new()
            .name(name)
            .parameter_in(ParameterIn::Path)
            .required(Required::True)
            .description(Some(description.into()))
            .schema(Some(T::schema()))
            .build();
        self.path_params.insert(name.to_string(), parameter);
        self
    }

    /// A request header.
    pub fn header<T: PartialSchema>(mut self, name: &str, description: impl Into<String>, required: bool) -> Self {
        let parameter = ParameterBuilder::new()
            .name(name)
            .parameter_in(ParameterIn::Header)
            .required(if required { Required::True } else { Required::False })
            .description(Some(description.into()))
            .schema(Some(T::schema()))
            .build();
        self.parameters.push(parameter);
        self
    }

    /// A required JSON request body of type `T`.
    pub fn body<T: ToSchema>(self) -> Self {
        self.body_as::<T>("application/json")
    }

    /// A required request body of type `T` with another content type, e.g.
    /// `multipart/form-data` for an upload form.
    pub fn body_as<T: ToSchema>(mut self, content_type: &str) -> Self {
        self.body = Some((content_type.to_string(), Typed::of::<T>(), true));
        self
    }

    /// A JSON response of type `T`.
    pub fn response<T: ToSchema>(mut self, status: u16, description: impl Into<String>) -> Self {
        self.responses.insert(status, ResponseDoc { description: description.into(), body: ResponseBody::Json(Box::new(Typed::of::<T>())) });
        self
    }

    /// A response without a body, such as 204.
    pub fn response_empty(mut self, status: u16, description: impl Into<String>) -> Self {
        self.responses.insert(status, ResponseDoc { description: description.into(), body: ResponseBody::None });
        self
    }

    /// An error response in Axeon's error format:
    /// `{"error": {"message": ..., "status": ..., "fields": {...}}}`.
    pub fn error(mut self, status: u16, description: impl Into<String>) -> Self {
        self.responses.insert(status, ResponseDoc { description: description.into(), body: ResponseBody::Error });
        self
    }

    /// Requires the security scheme `name`, declared on the
    /// [`Spec`](crate::Spec) (e.g. with
    /// [`bearer_auth`](crate::Spec::bearer_auth)). Several calls mean any
    /// one of them is enough.
    pub fn security(self, name: &str) -> Self {
        self.security_scopes(name, std::iter::empty::<&str>())
    }

    /// Requires the security scheme `name` with these scopes.
    pub fn security_scopes<S: Into<String>>(mut self, name: &str, scopes: impl IntoIterator<Item = S>) -> Self {
        self.security.push((name.to_string(), scopes.into_iter().map(Into::into).collect()));
        self
    }

    /// Needs no authentication, even when the spec has a
    /// [default](crate::Spec::require).
    pub fn public(mut self) -> Self {
        self.public = true;
        self
    }

    /// Adds a named schema the document needs but can't find on its own:
    /// the `T` in a generic response like `Page<T>`. See also
    /// [`Spec::schema`](crate::Spec::schema).
    pub fn schema<T: ToSchema>(mut self) -> Self {
        let typed = Typed::of::<T>();
        self.schemas.extend(typed.components);
        self
    }
}
