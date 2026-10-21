//! pgvector support, against PostgreSQL with the extension available (when
//! `AXEON_ORM_POSTGRES_URL` is set).

#![cfg(feature = "pgvector")]

use axeon_orm::vector::{self, Vector, VectorIndex};
use axeon_orm::{schema, Backend, Db, Error, Model};

#[derive(Model, Clone, Debug, PartialEq)]
struct Doc {
    id: i64,
    category: String,
    #[model(dimensions = 3)]
    embedding: Vector,
    alt: Option<Vector>,
}

async fn db(name: &str) -> Option<Db> {
    let Ok(url) = std::env::var("AXEON_ORM_POSTGRES_URL") else {
        eprintln!("skipping: AXEON_ORM_POSTGRES_URL is not set");
        return None;
    };
    let schema = format!("axeon_vector_{}", name);
    let admin = Db::connect(&url).await.unwrap();
    // `CREATE EXTENSION IF NOT EXISTS` races when run concurrently: once per run.
    static EXTENSION: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    EXTENSION.get_or_init(|| async { admin.execute("CREATE EXTENSION IF NOT EXISTS vector").await.unwrap() }).await;
    admin.execute(&format!("DROP SCHEMA IF EXISTS {0} CASCADE; CREATE SCHEMA {0}", schema)).await.unwrap();
    admin.close().await;
    let separator = if url.contains('?') { '&' } else { '?' };
    // `public` stays on the path for the extension's type and operators.
    let db = Db::connect(&format!("{}{}options=-c%20search_path%3D{},public", url, separator, schema)).await.unwrap();
    db.create_table::<Doc>().await.unwrap();
    Some(db)
}

fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| f64::from(*x) * f64::from(*y)).sum();
    let norm = |v: &[f32]| v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    1.0 - dot / (norm(a) * norm(b))
}

fn l2_distance(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2)).sum::<f64>().sqrt()
}

fn doc(category: &str, embedding: [f32; 3]) -> Doc {
    Doc { id: 0, category: category.into(), embedding: Vector::from(embedding.to_vec()), alt: None }
}

#[test]
fn schema_has_dimensions_and_indexes() {
    let sql = schema::create_table::<Doc>(Backend::Postgres);
    assert!(sql.contains(r#""embedding" vector(3) NOT NULL"#), "{}", sql);
    assert!(sql.contains(r#""alt" vector,"#) || sql.contains(r#""alt" vector"#), "{}", sql);
    assert_eq!(
        vector::create_index::<Doc>("embedding", VectorIndex::hnsw_cosine()),
        r#"CREATE INDEX IF NOT EXISTS "docs_embedding_hnsw" ON "docs" USING hnsw ("embedding" vector_cosine_ops)"#
    );
    assert!(vector::create_index::<Doc>("embedding", VectorIndex::ivfflat_cosine()).ends_with(r#"USING ivfflat ("embedding" vector_cosine_ops)"#));
    assert!(vector::create_index::<Doc>("embedding", VectorIndex::hnsw_inner_product()).ends_with("vector_ip_ops)"));
}

#[tokio::test]
async fn stores_and_reads_vectors() {
    let Some(db) = db("roundtrip").await else { return };
    let stored = Doc { alt: Some(Vector::from(vec![0.5, -1.25])), ..doc("a", [1.0, 2.5, -3.0]) }.insert(&db).await.unwrap();
    assert_eq!(stored.embedding.as_slice(), &[1.0, 2.5, -3.0]);
    assert_eq!(stored.alt.as_ref().unwrap().as_slice(), &[0.5, -1.25], "a column without dimensions takes any size");
    assert_eq!(Doc::find_or_fail(&db, stored.id).await.unwrap(), stored);

    let mut changed = stored.clone();
    changed.embedding = Vector::from(vec![0.0, 0.0, 1.0]);
    changed.alt = None;
    changed.update(&db).await.unwrap();
    assert_eq!(Doc::find_or_fail(&db, stored.id).await.unwrap(), changed);
    Doc::query().filter(Doc::ID.eq(stored.id)).update(&db, [Doc::EMBEDDING.set(Vector::from(vec![3.0, 2.0, 1.0]))]).await.unwrap();
    assert_eq!(Doc::find_or_fail(&db, stored.id).await.unwrap().embedding.as_slice(), &[3.0, 2.0, 1.0]);

    // Bulk inserts bind vectors too.
    let many = Doc::insert_many(&db, &[doc("b", [1.0, 0.0, 0.0]), doc("b", [0.0, 1.0, 0.0])]).await.unwrap();
    assert_eq!(many[1].embedding.as_slice(), &[0.0, 1.0, 0.0]);

    // The column's dimensions are enforced.
    assert!(matches!(Doc { embedding: Vector::from(vec![1.0, 2.0]), ..doc("c", [0.0; 3]) }.insert(&db).await, Err(Error::Database(_))));
    // NaN isn't a valid vector value.
    assert!(Doc { embedding: Vector::from(vec![f32::NAN, 0.0, 0.0]), ..doc("c", [0.0; 3]) }.insert(&db).await.is_err());

    // A realistic embedding size.
    #[derive(Model, Debug)]
    struct Big {
        id: i64,
        #[model(dimensions = 1024)]
        embedding: Vector,
    }
    db.create_table::<Big>().await.unwrap();
    let values: Vec<f32> = (0..1024).map(|i| (i as f32).sin()).collect();
    let big = Big { id: 0, embedding: Vector::from(values.clone()) }.insert(&db).await.unwrap();
    assert_eq!(Big::find_or_fail(&db, big.id).await.unwrap().embedding.into_vec(), values);
}

#[tokio::test]
async fn nearest_neighbours_with_distances() {
    let Some(db) = db("nearest").await else { return };
    let points = [
        ("lighting", [1.0, 0.0, 0.0]),
        ("lighting", [0.9, 0.1, 0.0]),
        ("lighting", [0.0, 1.0, 0.0]),
        ("furniture", [0.95, 0.05, 0.0]),
        ("lighting", [-1.0, 0.0, 0.0]),
        ("lighting", [0.5, 0.5, 0.7]),
    ];
    for (category, embedding) in points {
        doc(category, embedding).insert(&db).await.unwrap();
    }
    let query = Vector::from(vec![1.0, 0.05, 0.0]);

    // Cosine: nearest first, distances matching an independent computation.
    let hits = Doc::query().filter(Doc::CATEGORY.eq("lighting")).nearest(&db, Doc::EMBEDDING.cosine_distance(&query), 3).await.unwrap();
    assert_eq!(hits.len(), 3);
    let mut expected: Vec<(Vec<f32>, f64)> = points
        .iter()
        .filter(|(c, _)| *c == "lighting")
        .map(|(_, e)| (e.to_vec(), cosine_distance(e, query.as_slice())))
        .collect();
    expected.sort_by(|a, b| a.1.total_cmp(&b.1));
    for ((doc, distance), (embedding, want)) in hits.iter().zip(&expected) {
        assert_eq!(doc.embedding.as_slice(), &embedding[..]);
        assert!((distance - want).abs() < 1e-5, "{} vs {}", distance, want);
    }
    assert!(hits.iter().all(|(d, _)| d.category == "lighting"), "filters apply");

    // Euclidean and inner product.
    let l2 = Doc::query().nearest(&db, Doc::EMBEDDING.l2_distance(vec![0.0, 1.0, 0.0]), 1).await.unwrap();
    assert_eq!(l2[0].0.embedding.as_slice(), &[0.0, 1.0, 0.0]);
    assert!(l2[0].1.abs() < 1e-6);
    let far = Doc::query().nearest(&db, Doc::EMBEDDING.l2_distance(vec![0.0, 0.0, 0.0]), 10).await.unwrap();
    assert!(far.windows(2).all(|w| w[0].1 <= w[1].1), "ascending distances");
    let farthest = points.iter().map(|(_, e)| l2_distance(e, &[0.0; 3])).fold(0.0, f64::max);
    assert!((far.last().unwrap().1 - farthest).abs() < 1e-5, "{} vs {}", far.last().unwrap().1, farthest);
    let ip = Doc::query().nearest(&db, Doc::EMBEDDING.inner_product(vec![1.0, 0.0, 0.0]), 1).await.unwrap();
    assert_eq!(ip[0].0.embedding.as_slice(), &[1.0, 0.0, 0.0]);
    assert!((ip[0].1 - -1.0).abs() < 1e-6, "the negative inner product: {}", ip[0].1);

    // As a filter and an ordering in ordinary queries.
    let close = Doc::query()
        .filter(Doc::EMBEDDING.cosine_distance(&query).lt(0.05))
        .order_by(Doc::EMBEDDING.cosine_distance(&query).asc())
        .all(&db)
        .await
        .unwrap();
    let mut want: Vec<([f32; 3], f64)> =
        points.iter().map(|(_, e)| (*e, cosine_distance(e, query.as_slice()))).filter(|(_, d)| *d < 0.05).collect();
    want.sort_by(|a, b| a.1.total_cmp(&b.1));
    let got: Vec<&[f32]> = close.iter().map(|d| d.embedding.as_slice()).collect();
    assert_eq!(got, want.iter().map(|(e, _)| &e[..]).collect::<Vec<_>>());
    assert_eq!(close.len(), 3);
    let farthest = Doc::query().order_by(Doc::EMBEDDING.cosine_distance(&query).desc()).first(&db).await.unwrap().unwrap();
    assert_eq!(farthest.embedding.as_slice(), &[-1.0, 0.0, 0.0]);
    let within = points.iter().filter(|(_, e)| l2_distance(e, &[0.0; 3]) <= 0.99).count();
    assert!(within > 0 && within < points.len(), "the filter keeps some rows and drops others");
    assert_eq!(Doc::query().filter(Doc::EMBEDDING.l2_distance(vec![0.0; 3]).lte(0.99)).count(&db).await.unwrap(), within as u64);
    assert!(Doc::query().nearest(&db, Doc::EMBEDDING.cosine_distance(&query), 0).await.unwrap().is_empty());
}

#[tokio::test]
async fn hnsw_index_serves_nearest_queries() {
    let Some(db) = db("index").await else { return };
    let rows: Vec<Doc> = (0..500).map(|i| doc("x", [(i as f32).sin(), (i as f32).cos(), (i % 7) as f32 / 7.0])).collect();
    Doc::insert_many(&db, &rows).await.unwrap();
    db.execute(&vector::create_index::<Doc>("embedding", VectorIndex::hnsw_cosine())).await.unwrap();
    db.execute("ANALYZE docs").await.unwrap();

    // The same SQL shape `nearest` sends.
    let mut tx = db.begin().await.unwrap();
    tx.execute("SET LOCAL enable_seqscan = off").await.unwrap();
    let plan: Vec<String> = sqlx::query_scalar(
        r#"EXPLAIN SELECT "id", "embedding" <=> $1 AS "__axeon_distance" FROM "docs" ORDER BY "__axeon_distance" LIMIT 5"#,
    )
    .bind(Vector::from(vec![0.1, 0.9, 0.2]))
    .fetch_all(tx.postgres().unwrap())
    .await
    .unwrap();
    let plan = plan.join("\n");
    assert!(plan.contains("docs_embedding_hnsw"), "{}", plan);
    tx.rollback().await.unwrap();

    let hits = Doc::query().nearest(&db, Doc::EMBEDDING.cosine_distance(vec![0.1, 0.9, 0.2]), 5).await.unwrap();
    assert_eq!(hits.len(), 5);
    assert!(hits.windows(2).all(|w| w[0].1 <= w[1].1));
}

#[tokio::test]
async fn needs_postgres() {
    let sqlite = Db::connect("sqlite::memory:").await.unwrap();
    let result = Doc::query().nearest(&sqlite, Doc::EMBEDDING.cosine_distance(vec![1.0, 0.0, 0.0]), 3).await;
    assert!(matches!(result, Err(Error::Config(m)) if m.contains("pgvector")));
}
