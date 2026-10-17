//! In-process request handling: routing, middleware and responses, without
//! network overhead. Run with `cargo bench`.

use axeon::middleware::from_fn;
use axeon::{json, App, Json, Method, Request, Server};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tokio::runtime::Runtime;

fn build(server: Server, rt: &Runtime) -> App {
    rt.block_on(server.into_app()).unwrap()
}

fn routing(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("routing");
    group.throughput(Throughput::Elements(1));
    for routes in [10, 100, 1000] {
        let mut server = Server::new();
        for i in 0..routes {
            server.get(&format!("/r{}/items/:id", i), |_req| async { "ok" });
            server.get(&format!("/static/{}", i), |_req| async { "ok" });
        }
        let app = build(server, &rt);
        let dynamic = format!("/r{}/items/42", routes - 1);
        let fixed = format!("/static/{}", routes - 1);
        group.bench_with_input(BenchmarkId::new("dynamic", routes), &dynamic, |b, path| {
            b.to_async(&rt).iter(|| app.call(Request::new(Method::GET, path)))
        });
        group.bench_with_input(BenchmarkId::new("static", routes), &fixed, |b, path| {
            b.to_async(&rt).iter(|| app.call(Request::new(Method::GET, path)))
        });
    }
    group.finish();
}

fn middleware(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("middleware");
    group.throughput(Throughput::Elements(1));
    for layers in [0, 3, 10] {
        let mut server = Server::new();
        for _ in 0..layers {
            server.middleware(from_fn(|req, next| async move { next.handle(req).await }));
        }
        server.get("/", |_req| async { "ok" });
        let app = build(server, &rt);
        group.bench_with_input(BenchmarkId::from_parameter(layers), &app, |b, app| {
            b.to_async(&rt).iter(|| app.call(Request::new(Method::GET, "/")))
        });
    }
    group.finish();
}

fn responses(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut server = Server::new();
    server.get("/json", |_req| async {
        Json(json!({ "id": 1, "name": "Ada", "tags": ["admin", "staff"], "active": true }))
    });
    server.post("/echo", |req| async move {
        let value: serde_json::Value = req.json()?;
        Ok::<_, axeon::ServerError>(Json(value))
    });
    let app = build(server, &rt);
    let mut group = c.benchmark_group("responses");
    group.throughput(Throughput::Elements(1));
    group.bench_function("json", |b| b.to_async(&rt).iter(|| app.call(Request::new(Method::GET, "/json"))));
    let body = json!({ "id": 1, "name": "Ada", "tags": ["admin", "staff"] });
    group.bench_function("json_roundtrip", |b| {
        b.to_async(&rt).iter(|| app.call(Request::new(Method::POST, "/echo").with_json(&body)))
    });
    group.finish();
}

criterion_group!(benches, routing, middleware, responses);
criterion_main!(benches);
