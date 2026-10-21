//! Full-text and hybrid search (when `AXEON_ORM_POSTGRES_URL` is set).

#![cfg(feature = "postgres")]

use axeon_orm::search::{self, FullText};
use axeon_orm::{Db, Error, Model};
use std::collections::HashMap;

#[derive(Model, Clone, Debug, PartialEq)]
#[model(table = "articles")]
struct Article {
    id: i64,
    title: String,
    body: Option<String>,
}

async fn db(name: &str) -> Option<Db> {
    let Ok(url) = std::env::var("AXEON_ORM_POSTGRES_URL") else {
        eprintln!("skipping: AXEON_ORM_POSTGRES_URL is not set");
        return None;
    };
    let schema = format!("axeon_search_{}", name);
    let admin = Db::connect(&url).await.unwrap();
    admin.execute(&format!("DROP SCHEMA IF EXISTS {0} CASCADE; CREATE SCHEMA {0}", schema)).await.unwrap();
    admin.close().await;
    let separator = if url.contains('?') { '&' } else { '?' };
    let db = Db::connect(&format!("{}{}options=-c%20search_path%3D{},public", url, separator, schema)).await.unwrap();
    db.create_table::<Article>().await.unwrap();
    let rows = [
        ("LED desk lamps", Some("Warm white LED lamp for desks. A lamp that lasts.")),
        ("Solar garden light", Some("A solar powered lamp for gardens")),
        ("Office chair", Some("Ergonomic chair with lumbar support")),
        ("Lamp shades", None),
        ("Industrial pendant light", Some("Black metal pendant, E27 bulb, \"vintage\" look")),
    ];
    for (title, body) in rows {
        Article { id: 0, title: title.into(), body: body.map(Into::into) }.insert(&db).await.unwrap();
    }
    Some(db)
}

fn titles(articles: &[Article]) -> Vec<&str> {
    articles.iter().map(|a| a.title.as_str()).collect()
}

fn english() -> FullText<Article> {
    FullText::columns([Article::TITLE, Article::BODY]).config("english")
}

#[tokio::test]
async fn matches_words_phrases_and_exclusions() {
    let Some(db) = db("match").await else { return };
    let find = |q: &str| Article::query().filter(english().matches(q)).order_by(Article::ID.asc()).all(&db);

    // Stemming: "lamps" finds "lamp", "lighting" finds "light".
    assert_eq!(titles(&find("lamps").await.unwrap()), ["LED desk lamps", "Solar garden light", "Lamp shades"]);
    assert_eq!(titles(&find("lighting").await.unwrap()), ["Solar garden light", "Industrial pendant light"]);
    // All words must appear; NULL bodies are fine.
    assert_eq!(titles(&find("lamp desk").await.unwrap()), ["LED desk lamps"]);
    // Web-search syntax.
    assert_eq!(titles(&find("lamp -solar").await.unwrap()), ["LED desk lamps", "Lamp shades"]);
    assert_eq!(titles(&find("chair OR pendant").await.unwrap()), ["Office chair", "Industrial pendant light"]);
    assert_eq!(titles(&find("\"warm white\"").await.unwrap()), ["LED desk lamps"]);
    assert!(find("\"white warm\"").await.unwrap().is_empty(), "phrases keep their order");
    // Odd or hostile input is just text.
    for odd in ["", "&&|!(", "'; DROP TABLE articles; --", "\"unclosed", ":*", "-"] {
        find(odd).await.unwrap_or_else(|e| panic!("{:?}: {}", odd, e));
    }
    assert_eq!(Article::query().count(&db).await.unwrap(), 5);
    // `simple` doesn't stem.
    let simple = FullText::columns([Article::TITLE, Article::BODY]);
    assert!(Article::query().filter(simple.matches("lighting")).all(&db).await.unwrap().is_empty());
    assert_eq!(Article::query().filter(simple.matches("lamps")).count(&db).await.unwrap(), 1);
    // Combined with other conditions.
    let not_shades = Article::query().filter(english().matches("lamp") & Article::TITLE.ne("Lamp shades")).count(&db).await.unwrap();
    assert_eq!(not_shades, 2);
    assert_eq!(Article::query().filter(!english().matches("lamp")).count(&db).await.unwrap(), 2);
}

#[tokio::test]
async fn ranks_and_scores() {
    let Some(db) = db("rank").await else { return };
    // The desk lamp mentions "lamp" three times; it ranks first.
    let ordered = Article::query().filter(english().matches("lamp")).order_by(english().rank("lamp").desc()).all(&db).await.unwrap();
    assert_eq!(ordered[0].title, "LED desk lamps");
    let scored = Article::query().filter(english().matches("lamp")).ranked(&db, english().rank("lamp"), 10).await.unwrap();
    assert_eq!(scored.len(), 3);
    assert_eq!(scored[0].0.title, "LED desk lamps");
    assert!(scored.windows(2).all(|w| w[0].1 >= w[1].1), "best first");
    assert!(scored.iter().all(|(_, s)| (0.0..1.0).contains(s) && *s > 0.0), "normalized to 0..1: {:?}", scored);
    assert_eq!(Article::query().ranked(&db, english().rank("lamp"), 2).await.unwrap().len(), 2, "limit");
    // Weighted sums.
    let doubled = Article::query().filter(english().matches("lamp")).ranked(&db, english().rank("lamp") * 2.0, 10).await.unwrap();
    for ((_, single), (_, double)) in scored.iter().zip(&doubled) {
        assert!((double - 2.0 * single).abs() < 1e-9);
    }
    let summed = Article::query().ranked(&db, english().rank("lamp") + english().rank("desk") * 0.5, 1).await.unwrap();
    assert_eq!(summed[0].0.title, "LED desk lamps");
}

#[tokio::test]
async fn stored_columns_and_indexes() {
    let Some(db) = db("stored").await else { return };
    // A generated column, not a model field: kept up to date by PostgreSQL.
    db.execute(&search::generated_column("search", [Article::TITLE, Article::BODY], "english")).await.unwrap();
    let stored = FullText::<Article>::stored("search").config("english");
    db.execute(&search::create_index(&stored, "articles_search")).await.unwrap();
    db.execute(&search::create_index(&english(), "articles_fts")).await.unwrap();

    assert_eq!(Article::query().filter(stored.matches("lamps")).count(&db).await.unwrap(), 3);
    let mut chair = Article::query().filter(Article::TITLE.eq("Office chair")).first(&db).await.unwrap().unwrap();
    chair.body = Some("Comes with a reading lamp".into());
    chair.update(&db).await.unwrap(); // generated columns aren't fields, so updates work
    assert_eq!(Article::query().filter(stored.matches("lamps")).count(&db).await.unwrap(), 4);
    let new = Article { id: 0, title: "Floor lamp".into(), body: None }.insert(&db).await.unwrap();
    assert!(Article::query().filter(stored.matches("lamp")).all(&db).await.unwrap().contains(&new));

    // Both indexes serve the ORM's own queries: the expression in the
    // generated SQL must match the index's exactly.
    let mut tx = db.begin().await.unwrap();
    tx.execute("SET LOCAL enable_seqscan = off").await.unwrap();
    for (fts, index) in [(stored.clone(), "articles_search"), (english(), "articles_fts")] {
        let (sql, args) = Article::query().filter(fts.matches("lamp")).to_sql(axeon_orm::Backend::Postgres);
        assert_eq!(args.len(), 1);
        let plan: Vec<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN {}", sql))).bind("lamp").fetch_all(tx.postgres().unwrap()).await.unwrap();
        assert!(plan.join("\n").contains(index), "{}:\n{}\n{}", index, sql, plan.join("\n"));
    }
    tx.rollback().await.unwrap();
}

#[cfg(feature = "pgvector")]
#[tokio::test]
async fn hybrid_scores_combine_text_and_vectors() {
    use axeon_orm::vector::Vector;

    #[derive(Model, Clone, Debug)]
    struct Item {
        id: i64,
        name: String,
        #[model(dimensions = 2)]
        embedding: Vector,
    }
    let Some(db) = db("hybrid").await else { return };
    let admin_url = std::env::var("AXEON_ORM_POSTGRES_URL").unwrap();
    Db::connect(&admin_url).await.unwrap().execute("CREATE EXTENSION IF NOT EXISTS vector").await.ok();
    db.create_table::<Item>().await.unwrap();
    let rows = [("red led lamp", [1.0, 0.0]), ("red chair", [0.9, 0.1]), ("blue lamp", [0.0, 1.0]), ("green lamp shade", [0.6, 0.6])];
    for (name, e) in rows {
        Item { id: 0, name: name.into(), embedding: Vector::from(e.to_vec()) }.insert(&db).await.unwrap();
    }
    let fts = FullText::columns([Item::NAME]).config("english");
    let v = Vector::from(vec![1.0, 0.05]);
    let q = "lamp";

    let score = fts.rank(q) * 0.6 + Item::EMBEDDING.cosine_distance(&v).similarity() * 0.4;
    let candidates = || Item::query().filter(fts.matches(q) | Item::EMBEDDING.cosine_distance(&v).lt(0.5));
    let hybrid = candidates().ranked(&db, score, 10).await.unwrap();

    // The same score, from its parts fetched separately.
    let ranks: HashMap<i64, f64> = candidates().ranked(&db, fts.rank(q), 10).await.unwrap().into_iter().map(|(i, s)| (i.id, s)).collect();
    let distances: HashMap<i64, f64> =
        candidates().nearest(&db, Item::EMBEDDING.cosine_distance(&v), 10).await.unwrap().into_iter().map(|(i, d)| (i.id, d)).collect();
    assert_eq!(hybrid.len(), 4, "every lamp or near item");
    for (item, score) in &hybrid {
        let want = 0.6 * ranks[&item.id] + 0.4 * (1.0 - distances[&item.id]);
        assert!((score - want).abs() < 1e-6, "{}: {} vs {}", item.name, score, want);
    }
    assert!(hybrid.windows(2).all(|w| w[0].1 >= w[1].1));
    assert_eq!(hybrid[0].0.name, "red led lamp", "matches the words and is closest");
    // Sorting by the score works in ordinary queries too.
    let first = candidates().order_by((fts.rank(q) * 0.6 + Item::EMBEDDING.cosine_distance(&v).similarity() * 0.4).desc()).first(&db).await.unwrap();
    assert_eq!(first.unwrap().name, "red led lamp");
}

#[test]
fn configuration_names_are_checked() {
    let _ = FullText::<Article>::stored("search").config("pg_catalog.english");
    for bad in ["english'; DROP TABLE x; --", "", "a.b.c", "en glish", ".english"] {
        let result = std::panic::catch_unwind(|| FullText::<Article>::stored("search").config(bad));
        assert!(result.is_err(), "{:?} accepted", bad);
    }
}

#[tokio::test]
async fn needs_postgres() {
    let sqlite = Db::connect("sqlite::memory:").await.unwrap();
    sqlite.create_table::<Article>().await.unwrap();
    assert!(matches!(Article::query().ranked(&sqlite, english().rank("x"), 1).await, Err(Error::Config(_))));
    let error = Article::query().filter(english().matches("x")).all(&sqlite).await.unwrap_err().to_string();
    assert!(error.contains("axeon_orm_full_text_search_needs_postgres"), "{}", error);
}
