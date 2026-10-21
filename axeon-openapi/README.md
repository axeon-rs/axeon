# axeon-openapi

OpenAPI 3.1 documents for [Axeon](https://docs.rs/axeon) routes, built from
your Rust types with [utoipa](https://docs.rs/utoipa) 6. One contract for
your frontends, generated TypeScript clients and public API docs.

```rust
use axeon_openapi::{op, RouteDocs, Spec};
use utoipa::{IntoParams, ToSchema};

#[derive(Deserialize, IntoParams)]
struct ProductQuery { category: Option<String>, moq_max: Option<u32> }

#[derive(Serialize, ToSchema)]
struct ProductDto { id: i64, name: String }

app.route("/v1/products")
    .doc(op().summary("List products").tag("Catalog")
        .query::<ProductQuery>()
        .response::<Vec<ProductDto>>(200, "Matching products")
        .error(400, "Bad filters"))
    .get(list_products);

app.route("/v1/products/:id")
    .doc(op().summary("Get a product").tag("Catalog")
        .path_param::<i64>("id", "Product ID")
        .response::<ProductDto>(200, "The product")
        .error(404, "No such product"))
    .get(get_product);

// After registering every route:
let openapi = Spec::new("Venture Studio API", "1.0")
    .server("https://api.example.com", None)
    .api_key("api_key", "X-API-Key")
    .bearer_auth("bearer")
    .require("api_key")                 // default for documented routes
    .build(app.routes())?;              // fails on an invalid document
app.mount("/", axeon_openapi::router(&openapi)); // GET /openapi.json
```

Then generate a typed client, e.g.
`npx openapi-typescript https://api.example.com/openapi.json -o api.d.ts`.

## What you can document

`op()` has `summary`, `description`, `tag`, `operation_id`, `deprecated`,
`hidden`, `query::<T: IntoParams>()`, `path_param::<T>(name, ..)`,
`header::<T>(..)`, `body::<T>()` (JSON) or `body_as::<T>(content_type)`,
`response::<T>(status, ..)`, `response_empty(status, ..)`,
`error(status, ..)` (Axeon's `{"error": {...}}` body), `security(name)`,
`security_scopes(name, scopes)`, `public()` and `schema::<T>()`.

- `doc` applies to every handler of a route builder, so give each method
  its own builder: `app.route("/posts").doc(list).get(..)` and
  `app.route("/posts").doc(create).post(..)`.
- Path parameters are documented as required strings unless you type them
  with `path_param`. Operation IDs default to e.g. `get_v1_products_by_id`.
- Only routes with `doc` are listed; `Spec::include_undocumented()` lists
  the rest too (`hidden()` still hides).
- Plain named types (`ProductDto`) become named schemas; generic ones
  (`Page<ProductDto>`) are inline, and the type they wrap must be added
  with `.schema::<ProductDto>()` on the operation or the `Spec`.

`build` checks the document and fails with every problem it finds: a
referenced schema or security scheme that doesn't exist, two different
types with one schema name, a documented path parameter missing from the
path, or a duplicate operation ID.

Add `utoipa = "6"` to your dependencies for the derives.

## License

MIT
