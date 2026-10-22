//! Query timing: the slow-query log and per-query metrics.

use std::time::Duration;

/// What to do with each statement's duration, from [`DbOptions`](crate::DbOptions).
#[derive(Clone, Debug, Default)]
pub(crate) struct Observer {
    slow: Option<Duration>,
    #[cfg(feature = "axeon")]
    metrics: Option<QueryMetrics>,
}

#[cfg(feature = "axeon")]
#[derive(Clone)]
pub(crate) struct QueryMetrics {
    duration: axeon::metrics::HistogramVec,
    errors: axeon::metrics::CounterVec,
}

#[cfg(feature = "axeon")]
impl std::fmt::Debug for QueryMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QueryMetrics")
    }
}

#[cfg(feature = "axeon")]
const BUCKETS: [f64; 13] = [0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

impl Observer {
    pub(crate) fn slow(mut self, threshold: Option<Duration>) -> Self {
        self.slow = threshold;
        self
    }

    #[cfg(feature = "axeon")]
    pub(crate) fn metrics(mut self, metrics: &axeon::metrics::Metrics) -> Self {
        self.metrics = Some(QueryMetrics {
            duration: metrics.histogram_vec(
                "db_query_duration_seconds",
                "How long database statements took.",
                &["operation", "table"],
                &BUCKETS,
            ),
            errors: metrics.counter_vec("db_query_errors_total", "Database statements that failed.", &["operation", "table"]),
        });
        self
    }

    /// Whether anything is recorded, so callers can skip keeping the SQL.
    pub(crate) fn active(&self) -> bool {
        #[cfg(feature = "axeon")]
        if self.metrics.is_some() {
            return true;
        }
        self.slow.is_some()
    }

    /// Records one statement.
    pub(crate) fn record(&self, sql: &str, elapsed: Duration, ok: bool) {
        let (operation, table) = describe(sql);
        #[cfg(feature = "axeon")]
        if let Some(metrics) = &self.metrics {
            metrics.duration.with(&[operation, table]).observe(elapsed.as_secs_f64());
            if !ok {
                metrics.errors.with(&[operation, table]).inc();
            }
        }
        if self.slow.is_some_and(|threshold| elapsed >= threshold) {
            // The SQL has placeholders, never values, so it is safe to log
            // and doubles as the query's fingerprint.
            tracing::warn!(
                target: "axeon_orm::slow_query",
                elapsed_ms = elapsed.as_millis() as u64,
                operation,
                table,
                fingerprint = %fingerprint(sql),
                ok,
                sql,
                "slow query"
            );
        }
    }
}

/// The statement's kind and main table, for labels: `("select", "users")`.
pub(crate) fn describe(sql: &str) -> (&'static str, &str) {
    let trimmed = sql.trim_start();
    let first = trimmed.split_whitespace().next().unwrap_or_default();
    let operation = ["select", "insert", "update", "delete", "with"]
        .into_iter()
        .find(|op| first.eq_ignore_ascii_case(op))
        .unwrap_or("other");
    let keyword = match operation {
        "insert" => " INTO ",
        "update" => "UPDATE ",
        _ => " FROM ",
    };
    let table = find_ascii_ci(trimmed, keyword)
        .map(|at| &trimmed[at + keyword.len()..])
        .and_then(quoted_ident)
        .unwrap_or("");
    (operation, table)
}

/// The quoted identifier at the start of `text` (`"users"` or `` `users` ``).
fn quoted_ident(text: &str) -> Option<&str> {
    let quote = text.chars().next().filter(|c| *c == '"' || *c == '`')?;
    let rest = &text[1..];
    rest.find(quote).map(|end| &rest[..end])
}

/// The first position of an ASCII `needle` in `haystack`, ignoring case.
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    (0..=h.len().checked_sub(n.len())?).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// A short stable hash of the SQL, for grouping log lines.
fn fingerprint(sql: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in sql.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:016x}", hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_statements() {
        assert_eq!(describe(r#"SELECT "id", "name" FROM "users" WHERE "age" > $1"#), ("select", "users"));
        assert_eq!(describe(r#"INSERT INTO "posts" ("title") VALUES ($1) RETURNING "id""#), ("insert", "posts"));
        assert_eq!(describe(r#"UPDATE "users" SET "name" = $1"#), ("update", "users"));
        assert_eq!(describe("DELETE FROM `tag_list` WHERE `slug` = ?"), ("delete", "tag_list"));
        assert_eq!(describe(r#"SELECT COUNT(*) AS "value" FROM "users""#), ("select", "users"));
        assert_eq!(describe("SELECT 1"), ("select", ""));
        assert_eq!(describe("CREATE TABLE x (id INT)"), ("other", ""));
        assert_eq!(fingerprint("SELECT 1"), fingerprint("SELECT 1"));
        assert_ne!(fingerprint("SELECT 1"), fingerprint("SELECT 2"));
    }
}
