use axeon::{Json, Method, Request, Router, Server};
use axeon_openapi::{op, RouteDocs, Spec};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use utoipa::{IntoParams, ToSchema};

#[derive(Deserialize, IntoParams)]
#[allow(dead_code)]
struct ProductQuery {
    /// Only this category.
    category: Option<String>,
    moq_max: Option<u32>,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct Category {
    name: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct ProductDto {
    id: i64,
    name: String,
    category: Category,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct NewSelection {
    product_id: i64,
    margin: f64,
}

#[derive(Serialize, ToSchema)]
struct Page<T: ToSchema> {
    items: Vec<T>,
    next: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum Status {
    Pending,
    Approved,
}

fn app() -> Server {
    let mut app = Server::new();
    app.route("/v1/products")
        .doc(
            op().summary("List products")
                .description("Filter by category.")
                .tag("Catalog")
                .query::<ProductQuery>()
                .response::<Vec<ProductDto>>(200, "Matching products")
                .error(400, "Bad filters"),
        )
        .get(|_req| async { Json(Vec::<ProductDto>::new()) });
    app.route("/v1/products/:id")
        .doc(
            op().summary("Get a product")
                .tag("Catalog")
                .path_param::<i64>("id", "Product ID")
                .response::<ProductDto>(200, "The product")
                .error(404, "No such product"),
        )
        .get(|_req| async { "x" });
    // One builder per method, each with its own docs.
    app.route("/v1/selections")
        .doc(op().summary("Add a selection").operation_id("addSelection").body::<NewSelection>().response_empty(201, "Added").error(422, "Invalid"))
        .post(|_req| async { "x" });
    app.route("/v1/selections")
        .doc(op().summary("List selections").response::<Page<ProductDto>>(200, "A page").schema::<ProductDto>().deprecated())
        .get(|_req| async { "x" });
    app.route("/v1/status").doc(op().response::<Status>(200, "Status").public()).get(|_req| async { "x" });
    app.route("/files/*path").doc(op().summary("Download")).get(|_req| async { "x" });
    // Mounted routers keep their full paths.
    let mut ops = Router::new();
    ops.route("/cases/:case_id/notes/:note_id").doc(op().tag("Ops").security_scopes("bearer", ["ops:write"])).put(|_req| async { "x" });
    app.mount("/ops", ops);
    // Undocumented and hidden routes.
    app.get("/metrics", |_req| async { "x" });
    app.route("/internal").doc(op().hidden()).get(|_req| async { "x" });
    app
}

fn spec() -> Spec {
    Spec::new("Venture Studio API", "1.0")
        .description("The public API.")
        .server("https://api.example.com", Some("Production"))
        .tag("Catalog", "Products and categories")
        .api_key("api_key", "X-API-Key")
        .bearer_auth("bearer")
        .require("api_key")
}

fn document() -> Value {
    let app = app();
    serde_json::to_value(spec().build(app.routes()).unwrap()).unwrap()
}

#[test]
fn documents_routes_from_types() {
    let doc = document();
    assert_eq!(doc["openapi"], "3.1.0");
    assert_eq!(doc["info"], json!({ "title": "Venture Studio API", "version": "1.0", "description": "The public API." }));
    assert_eq!(doc["servers"], json!([{ "url": "https://api.example.com", "description": "Production" }]));
    assert_eq!(doc["tags"], json!([{ "name": "Catalog", "description": "Products and categories" }]));

    let paths = doc["paths"].as_object().unwrap();
    let mut listed: Vec<&str> = paths.keys().map(String::as_str).collect();
    listed.sort();
    assert_eq!(
        listed,
        ["/files/{path}", "/ops/cases/{case_id}/notes/{note_id}", "/v1/products", "/v1/products/{id}", "/v1/selections", "/v1/status"],
        "undocumented and hidden routes are left out"
    );

    let list = &doc["paths"]["/v1/products"]["get"];
    assert_eq!(list["operationId"], "get_v1_products");
    assert_eq!(list["summary"], "List products");
    assert_eq!(list["tags"], json!(["Catalog"]));
    let params = list["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 2);
    assert_eq!((params[0]["name"].as_str(), params[0]["in"].as_str()), (Some("category"), Some("query")));
    assert_eq!(params[0]["description"], "Only this category.");
    // A list of a named type is an inline array of `$ref`s.
    let ok = &list["responses"]["200"]["content"]["application/json"]["schema"];
    assert_eq!(ok["type"], "array");
    assert_eq!(ok["items"]["type"], "object");
    assert_eq!(list["responses"]["400"]["content"]["application/json"]["schema"]["$ref"], "#/components/schemas/ErrorResponse");
    assert_eq!(list["security"], json!([{ "api_key": [] }]), "the spec's default");

    // Path parameters: typed when documented.
    let get = &doc["paths"]["/v1/products/{id}"]["get"];
    assert_eq!(get["operationId"], "get_v1_products_by_id");
    assert_eq!(get["parameters"][0]["name"], "id");
    assert_eq!(get["parameters"][0]["in"], "path");
    assert_eq!(get["parameters"][0]["required"], true);
    assert_eq!(get["parameters"][0]["schema"]["type"], "integer");
    assert_eq!(get["responses"]["200"]["content"]["application/json"]["schema"]["$ref"], "#/components/schemas/ProductDto");
    // …and required strings when not.
    let notes = &doc["paths"]["/ops/cases/{case_id}/notes/{note_id}"]["put"];
    let names: Vec<&str> = notes["parameters"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["case_id", "note_id"]);
    assert_eq!(notes["parameters"][1]["schema"]["type"], "string");
    assert_eq!(notes["security"], json!([{ "bearer": ["ops:write"] }]));
    assert_eq!(notes["responses"]["200"]["description"], "Success", "a default response");
    assert_eq!(doc["paths"]["/files/{path}"]["get"]["parameters"][0]["name"], "path");

    // Two methods on one path, each documented.
    let selections = &doc["paths"]["/v1/selections"];
    assert_eq!(selections["post"]["operationId"], "addSelection");
    assert_eq!(selections["post"]["requestBody"]["required"], true);
    assert_eq!(selections["post"]["requestBody"]["content"]["application/json"]["schema"]["$ref"], "#/components/schemas/NewSelection");
    assert!(selections["post"]["responses"]["201"].get("content").is_none());
    assert_eq!(selections["get"]["deprecated"], true);
    // A generic response is inline, referring to the type it wraps.
    let page = &selections["get"]["responses"]["200"]["content"]["application/json"]["schema"];
    assert_eq!(page["properties"]["items"]["items"]["$ref"], "#/components/schemas/ProductDto");
    assert!(doc["paths"]["/v1/status"]["get"].get("security").is_none(), "public");

    let schemas = doc["components"]["schemas"].as_object().unwrap();
    let mut names: Vec<&str> = schemas.keys().map(String::as_str).collect();
    names.sort();
    assert_eq!(names, ["Category", "ErrorResponse", "NewSelection", "ProductDto", "Status"]);
    assert_eq!(schemas["ProductDto"]["properties"]["category"]["$ref"], "#/components/schemas/Category");
    assert_eq!(schemas["Status"]["enum"], json!(["pending", "approved"]));
    let error = &schemas["ErrorResponse"]["properties"]["error"];
    assert_eq!(error["required"], json!(["message", "status"]));
    assert!(error["properties"]["fields"].is_object());
    let schemes = &doc["components"]["securitySchemes"];
    assert_eq!(schemes["api_key"], json!({ "type": "apiKey", "in": "header", "name": "X-API-Key" }));
    assert_eq!(schemes["bearer"], json!({ "type": "http", "scheme": "bearer", "bearerFormat": "JWT" }));
}

#[test]
fn undocumented_routes_on_request() {
    let app = app();
    let doc = serde_json::to_value(spec().include_undocumented().build(app.routes()).unwrap()).unwrap();
    let metrics = &doc["paths"]["/metrics"]["get"];
    assert_eq!(metrics["operationId"], "get_metrics");
    assert!(metrics.get("security").is_none(), "undocumented routes don't get the default security");
    assert!(doc["paths"].get("/internal").is_none(), "hidden stays hidden");
}

#[test]
fn mistakes_fail_the_build() {
    let problems = |app: Server, spec: Spec| spec.build(app.routes()).unwrap_err().problems().to_vec();

    // A generic response whose wrapped type isn't registered.
    let mut app = Server::new();
    app.route("/a").doc(op().response::<Page<ProductDto>>(200, "x")).get(|_req| async { "x" });
    let found = problems(app, Spec::new("t", "1"));
    assert_eq!(found.len(), 1);
    assert!(found[0].contains("schema `ProductDto` is referenced but not defined"), "{:?}", found);
    // Fixed on the spec instead of the operation.
    let mut app = Server::new();
    app.route("/a").doc(op().response::<Page<ProductDto>>(200, "x")).get(|_req| async { "x" });
    assert!(Spec::new("t", "1").schema::<ProductDto>().build(app.routes()).is_ok());

    // Two different types with one name.
    mod other {
        #[derive(utoipa::ToSchema)]
        #[allow(dead_code)]
        pub struct Category {
            pub id: i64,
        }
    }
    let mut app = Server::new();
    app.route("/a").doc(op().response::<Category>(200, "x")).get(|_req| async { "x" });
    app.route("/b").doc(op().response::<other::Category>(200, "x")).get(|_req| async { "x" });
    assert!(problems(app, Spec::new("t", "1"))[0].contains("two different schemas are named `Category`"));

    // Unknown security scheme, a path parameter not in the path, and a
    // duplicate operation ID.
    let mut app = Server::new();
    app.route("/a").doc(op().security("oauth")).get(|_req| async { "x" });
    app.route("/b/:id").doc(op().path_param::<i64>("idd", "typo").operation_id("same")).get(|_req| async { "x" });
    app.route("/c").doc(op().operation_id("same")).get(|_req| async { "x" });
    let found = problems(app, Spec::new("t", "1"));
    assert_eq!(found.len(), 3, "{:?}", found);
    assert!(found.iter().any(|p| p.contains("security scheme `oauth`, which isn't declared")));
    assert!(found.iter().any(|p| p.contains("path parameter `idd`, which isn't in the path")));
    assert!(found.iter().any(|p| p.contains("two operations have the ID `same`")));
    let error = Spec::new("t", "1").require("nope");
    let mut app = Server::new();
    app.route("/a").doc(op()).get(|_req| async { "x" });
    assert!(error.build(app.routes()).unwrap_err().to_string().starts_with("invalid OpenAPI document: "));
}

#[tokio::test]
async fn serves_the_document() {
    let mut app = app();
    let openapi = spec().build(app.routes()).unwrap();
    app.mount("/", axeon_openapi::router(&openapi));
    let app = app.into_app().await.unwrap();
    let res = app.call(Request::new(Method::GET, "/openapi.json")).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.get_header("content-type"), Some("application/json"));
    let served: Value = serde_json::from_str(&res.body_text()).unwrap();
    assert_eq!(served, document());
}

#[test]
fn route_metadata_is_listed_by_axeon() {
    #[derive(Clone)]
    struct Owner(&'static str);
    let mut app = Server::new();
    app.route("/b").meta(Owner("team-b")).get(|_req| async { "x" }).post(|_req| async { "x" });
    app.get("/a", |_req| async { "x" });
    let routes = app.routes();
    let listed: Vec<(Method, &str, Option<&str>)> = routes.iter().map(|r| (r.method(), r.path(), r.meta::<Owner>().map(|o| o.0))).collect();
    assert_eq!(listed, [(Method::GET, "/a", None), (Method::GET, "/b", Some("team-b")), (Method::POST, "/b", Some("team-b"))]);
}

/// Writes the test document to `AXEON_OPENAPI_DUMP`, for checking it with
/// outside tools such as openapi-typescript.
#[test]
fn dump_for_outside_tools() {
    if let Ok(path) = std::env::var("AXEON_OPENAPI_DUMP") {
        std::fs::write(path, serde_json::to_string_pretty(&document()).unwrap()).unwrap();
    }
}
