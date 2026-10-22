//! Reports with `Select`, on SQLite (always), PostgreSQL (when
//! `AXEON_ORM_POSTGRES_URL` is set) and MySQL (when `AXEON_ORM_MYSQL_URL` is).

#![cfg(feature = "sqlite")]

use axeon_orm::select::{Agg, Select};
use axeon_orm::{Db, Error, Model};
use serde::Deserialize;

#[derive(Model, Clone, Debug)]
#[model(has_many(Product))]
struct Supplier {
    id: i64,
    name: String,
    active: bool,
}

#[derive(Model, Clone, Debug)]
struct Category {
    id: i64,
    name: String,
}

#[derive(Model, Clone, Debug)]
#[model(has_many(Review), has_many(Sale))]
struct Product {
    id: i64,
    #[model(belongs_to = Supplier)]
    supplier_id: i64,
    #[model(belongs_to = Category)]
    category_id: i64,
    title: String,
    price: f64,
    stock: i32,
}

#[derive(Model, Clone, Debug)]
struct Review {
    id: i64,
    #[model(belongs_to = Product)]
    product_id: i64,
    rating: i32,
}

#[derive(Model, Clone, Debug)]
struct Sale {
    id: i64,
    #[model(tenant)]
    org_id: i64,
    #[model(belongs_to = Product)]
    product_id: i64,
    quantity: i32,
}

async fn seed(db: &Db) -> (Vec<Supplier>, Vec<Product>) {
    db.create_table::<Supplier>().await.unwrap();
    db.create_table::<Category>().await.unwrap();
    db.create_table::<Product>().await.unwrap();
    db.create_table::<Review>().await.unwrap();
    db.create_table::<Sale>().await.unwrap();
    let mut suppliers = Vec::new();
    for (name, active) in [("Acme", true), ("Bolt", true), ("Dormant", false)] {
        suppliers.push(Supplier { id: 0, name: name.into(), active }.insert(db).await.unwrap());
    }
    let lighting = Category { id: 0, name: "lighting".into() }.insert(db).await.unwrap();
    let furniture = Category { id: 0, name: "furniture".into() }.insert(db).await.unwrap();
    let rows = [
        (0, &lighting, "lamp", 10.0, 5),
        (0, &lighting, "bulb", 2.0, 100),
        (0, &lighting, "strip", 8.0, 0),
        (0, &furniture, "desk", 120.0, 3),
        (1, &lighting, "torch", 6.0, 7),
        (1, &furniture, "chair", 45.0, 9),
        (2, &lighting, "old lamp", 9.0, 1),
    ];
    let mut products = Vec::new();
    for (supplier, category, title, price, stock) in rows {
        let product = Product { id: 0, supplier_id: suppliers[supplier].id, category_id: category.id, title: title.into(), price, stock };
        products.push(product.insert(db).await.unwrap());
    }
    for (product, rating) in [(0, 5), (0, 3), (1, 4), (4, 2)] {
        Review { id: 0, product_id: products[product].id, rating }.insert(db).await.unwrap();
    }
    let all = db.all_tenants();
    for (org, product, quantity) in [(1, 0, 2), (1, 0, 3), (1, 3, 1), (2, 0, 10)] {
        Sale { id: 0, org_id: org, product_id: products[product].id, quantity }.insert(&all).await.unwrap();
    }
    (suppliers, products)
}

async fn reports(db: &Db) {
    let _ = seed(db).await;

    // Three tables, grouped, filtered, with HAVING, ordered and limited.
    #[derive(Deserialize, Debug, PartialEq)]
    struct ByCategory {
        supplier: String,
        category: String,
        products: i64,
        stock: i64,
        avg_price: Option<f64>,
        cheapest: f64,
    }
    let rows: Vec<ByCategory> = Select::from::<Supplier>()
        .join(Supplier::PRODUCTS)
        .join(Product::CATEGORY)
        .column_as(Supplier::NAME, "supplier")
        .column_as(Category::NAME, "category")
        .aggregate(Agg::count(), "products")
        .aggregate(Agg::sum(Product::STOCK), "stock")
        .aggregate(Agg::avg(Product::PRICE), "avg_price")
        .aggregate(Agg::min(Product::PRICE), "cheapest")
        .filter(Supplier::ACTIVE.eq(true))
        .group_by(Supplier::NAME)
        .group_by(Category::NAME)
        .having(Agg::count().gte(1))
        .order_by_alias("products", true)
        .order_by(Supplier::NAME.asc())
        .order_by(Category::NAME.asc())
        .fetch(db)
        .await
        .unwrap();
    let summary: Vec<(&str, &str, i64, i64)> = rows.iter().map(|r| (r.supplier.as_str(), r.category.as_str(), r.products, r.stock)).collect();
    assert_eq!(
        summary,
        [("Acme", "lighting", 3, 105), ("Acme", "furniture", 1, 3), ("Bolt", "furniture", 1, 9), ("Bolt", "lighting", 1, 7)]
    );
    assert_eq!(rows[0].avg_price, Some(20.0 / 3.0));
    assert_eq!(rows[0].cheapest, 2.0);

    // HAVING filters groups; LIMIT/OFFSET page them.
    let busy: Vec<serde_json::Value> = Select::from::<Supplier>()
        .join(Supplier::PRODUCTS)
        .column(Supplier::NAME)
        .aggregate(Agg::count(), "products")
        .group_by(Supplier::NAME)
        .having(Agg::count().gt(2))
        .fetch_json(db)
        .await
        .unwrap();
    assert_eq!(busy, [serde_json::json!({ "name": "Acme", "products": 4 })]);
    let page: Vec<serde_json::Value> = Select::from::<Supplier>()
        .column(Supplier::NAME)
        .order_by(Supplier::NAME.asc())
        .limit(1)
        .offset(1)
        .fetch_json(db)
        .await
        .unwrap();
    assert_eq!(page, [serde_json::json!({ "name": "Bolt" })]);

    // Four tables with a left join: products without reviews are kept.
    #[derive(Deserialize, Debug)]
    struct Rated {
        title: String,
        category: String,
        reviews: i64,
        rating: Option<f64>,
        best: Option<i32>,
    }
    let rated: Vec<Rated> = Select::from::<Product>()
        .join(Product::SUPPLIER)
        .join(Product::CATEGORY)
        .left_join(Product::REVIEWS)
        .column(Product::TITLE)
        .column_as(Category::NAME, "category")
        .aggregate(Agg::count_of(Review::ID), "reviews")
        .aggregate(Agg::avg(Review::RATING), "rating")
        .aggregate(Agg::max(Review::RATING), "best")
        .filter(Supplier::NAME.eq("Acme"))
        .filter(Category::NAME.eq("lighting"))
        .group_by(Product::TITLE)
        .group_by(Category::NAME)
        .order_by(Product::TITLE.asc())
        .fetch(db)
        .await
        .unwrap();
    assert!(rated.iter().all(|r| r.category == "lighting"));
    let rated: Vec<(&str, i64, Option<f64>, Option<i32>)> = rated.iter().map(|r| (r.title.as_str(), r.reviews, r.rating, r.best)).collect();
    assert_eq!(rated, [("bulb", 1, Some(4.0), Some(4)), ("lamp", 2, Some(4.0), Some(5)), ("strip", 0, None, None)]);

    // Distinct counts and sums of floats; no grouping means one row.
    let totals = Select::from::<Product>()
        .aggregate(Agg::count_distinct(Product::CATEGORY_ID), "categories")
        .aggregate(Agg::sum(Product::PRICE), "value")
        .aggregate(Agg::max(Product::TITLE), "last_title")
        .fetch_json(db)
        .await
        .unwrap();
    assert_eq!(totals, [serde_json::json!({ "categories": 2, "value": 200.0, "last_title": "torch" })]);

    // Mistakes are reported when the report runs.
    let config = |r: Result<Vec<serde_json::Value>, Error>| matches!(r, Err(Error::Config(_)));
    assert!(config(Select::from::<Supplier>().column(Product::TITLE).fetch_json(db).await), "not joined");
    assert!(config(Select::from::<Product>().join(Product::CATEGORY).join(Product::CATEGORY).column(Product::TITLE).fetch_json(db).await));
    assert!(config(Select::from::<Product>().join(Product::CATEGORY).column(Product::ID).column(Category::ID).fetch_json(db).await), "two `id`s");
    assert!(config(Select::from::<Product>().column(Product::ID).order_by_alias("nope", false).fetch_json(db).await));
    assert!(config(Select::from::<Product>().fetch_json(db).await), "nothing selected");

    // Tenant scopes: a joined tenant table needs a scope, and filters by it
    // (in ON for a left join, so products without sales stay).
    let sold = || {
        Select::from::<Product>()
            .left_join(Product::SALES)
            .column(Product::TITLE)
            .aggregate(Agg::sum(Sale::QUANTITY), "sold")
            .filter(Product::TITLE.is_in(["lamp", "desk", "bulb"]))
            .group_by(Product::TITLE)
            .order_by(Product::TITLE.asc())
    };
    assert!(matches!(sold().fetch_json(db).await, Err(Error::TenantScope(_))));
    let org1 = sold().fetch_json(&db.for_tenant(1i64)).await.unwrap();
    assert_eq!(
        org1,
        [
            serde_json::json!({ "title": "bulb", "sold": null }),
            serde_json::json!({ "title": "desk", "sold": 1 }),
            serde_json::json!({ "title": "lamp", "sold": 5 }),
        ]
    );
    let everyone = sold().all_tenants().fetch_json(db).await.unwrap();
    assert_eq!(everyone[2], serde_json::json!({ "title": "lamp", "sold": 15 }));

    // The SQL, for EXPLAIN.
    let (sql, _) = sold().to_sql(db.backend()).unwrap();
    assert!(sql.contains("LEFT JOIN") && sql.contains("GROUP BY") && sql.contains("SUM("), "{}", sql);
}

#[tokio::test]
async fn sqlite() {
    reports(&Db::connect("sqlite::memory:").await.unwrap()).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres() {
    let Ok(url) = std::env::var("AXEON_ORM_POSTGRES_URL") else {
        eprintln!("skipping: AXEON_ORM_POSTGRES_URL is not set");
        return;
    };
    let admin = Db::connect(&url).await.unwrap();
    admin.execute("DROP SCHEMA IF EXISTS axeon_orm_select CASCADE; CREATE SCHEMA axeon_orm_select").await.unwrap();
    admin.close().await;
    let separator = if url.contains('?') { '&' } else { '?' };
    let db = Db::connect(&format!("{}{}options=-c%20search_path%3Daxeon_orm_select", url, separator)).await.unwrap();
    assert_eq!(db.backend(), axeon_orm::Backend::Postgres);
    reports(&db).await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn mysql() {
    let Ok(url) = std::env::var("AXEON_ORM_MYSQL_URL") else {
        eprintln!("skipping: AXEON_ORM_MYSQL_URL is not set");
        return;
    };
    let admin = Db::connect(&url).await.unwrap();
    admin.execute("DROP DATABASE IF EXISTS axeon_orm_select; CREATE DATABASE axeon_orm_select").await.unwrap();
    admin.close().await;
    let (base, _) = url.rsplit_once('/').unwrap();
    let db = Db::connect(&format!("{}/axeon_orm_select", base)).await.unwrap();
    reports(&db).await;
}
