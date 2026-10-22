//! Time-partitioned tables on PostgreSQL, for append-mostly data that
//! grows fast (audit events, LLM call logs): each day or month is its own
//! partition, so queries on recent data touch only recent partitions and
//! retention drops whole partitions instead of deleting rows.
//!
//! ```no_run
//! use axeon_orm::partition::Partitioning;
//! use axeon_orm::Model;
//! use chrono::{DateTime, Duration, Utc};
//!
//! #[derive(Model)]
//! struct AuditEvent {
//!     id: i64,
//!     #[model(index)]
//!     subject: String,
//!     action: String,
//!     #[model(created_at)]
//!     created_at: DateTime<Utc>,
//! }
//!
//! # async fn run(db: axeon_orm::Db) -> axeon_orm::Result<()> {
//! let audit = Partitioning::monthly(AuditEvent::CREATED_AT);
//! audit.create_table::<AuditEvent>(&db).await?;   // or save create_table_sql() as a migration
//! audit.ensure(&db, 3).await?;                     // this month and the next 3
//!
//! // From a scheduled job (e.g. axeon-jobs, daily):
//! audit.ensure(&db, 3).await?;
//! audit.drop_before(&db, Utc::now() - Duration::days(400)).await?;
//! # Ok(()) }
//! ```
//!
//! Rows are read and written through the model as usual. Inserting a row
//! whose time has no partition fails, so keep partitions created ahead
//! with [`ensure`](Partitioning::ensure).
//!
//! PostgreSQL requires a partitioned table's primary key to include the
//! partition column, so the table's key is `(id, created_at)`; the id still
//! comes from a sequence and stays unique. Other unique columns aren't
//! allowed, and upserts on `id` alone don't work on these tables.

use crate::query::Column;
use crate::value::{Kind, SqlType};
use crate::{Db, Error, Model, Result};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};

/// How long each partition covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Interval {
    /// One partition per UTC day: `<table>_p2026_10_22`.
    Daily,
    /// One partition per UTC month: `<table>_p2026_10`.
    Monthly,
}

/// One partition of a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    /// The partition's table name.
    pub name: String,
    /// Its range: from (inclusive) and to (exclusive), in UTC; `None` for
    /// `MINVALUE`/`MAXVALUE` bounds or a default partition.
    pub from: Option<DateTime<Utc>>,
    /// See [`from`](Partition::from).
    pub to: Option<DateTime<Utc>>,
}

/// A table partitioned by time ranges of one column; see the
/// [module docs](self).
#[derive(Clone, Debug)]
pub struct Partitioning {
    table: &'static str,
    column: &'static str,
    kind: Kind,
    interval: Interval,
}

impl Partitioning {
    /// Monthly partitions on `column` (a `DateTime<Utc>`,
    /// `NaiveDateTime` or `NaiveDate` column).
    pub fn monthly<M: Model, T: SqlType>(column: Column<M, T>) -> Self {
        Self::new(column, Interval::Monthly)
    }

    /// Daily partitions on `column`.
    pub fn daily<M: Model, T: SqlType>(column: Column<M, T>) -> Self {
        Self::new(column, Interval::Daily)
    }

    fn new<M: Model, T: SqlType>(column: Column<M, T>, interval: Interval) -> Self {
        Self { table: M::TABLE, column: column.name(), kind: T::KIND, interval }
    }

    fn check(&self) -> Result<()> {
        match self.kind {
            Kind::DateTime | Kind::NaiveDateTime | Kind::Date => Ok(()),
            _ => Err(Error::Config(format!(
                "`{}.{}` must be a date or time column to partition by",
                self.table, self.column
            ))),
        }
    }

    /// The `CREATE TABLE ... PARTITION BY RANGE` statement (with its
    /// indexes), for a migration. Partitions come from
    /// [`ensure`](Partitioning::ensure).
    pub fn create_table_sql<M: Model>(&self) -> Result<String> {
        self.check()?;
        crate::schema::create_partitioned_table::<M>(self.column)
    }

    /// Creates the partitioned table if it doesn't exist.
    pub async fn create_table<M: Model>(&self, db: &Db) -> Result<()> {
        postgres(db)?;
        db.execute(&self.create_table_sql::<M>()?).await
    }

    /// The start of the period containing `at`.
    fn period_start(&self, at: DateTime<Utc>) -> NaiveDate {
        let date = at.date_naive();
        match self.interval {
            Interval::Daily => date,
            Interval::Monthly => date.with_day(1).expect("day 1 exists"),
        }
    }

    fn next(&self, start: NaiveDate) -> NaiveDate {
        match self.interval {
            Interval::Daily => start + Duration::days(1),
            Interval::Monthly => {
                let (year, month) = if start.month() == 12 { (start.year() + 1, 1) } else { (start.year(), start.month() + 1) };
                NaiveDate::from_ymd_opt(year, month, 1).expect("the first of a month exists")
            }
        }
    }

    fn name(&self, start: NaiveDate) -> String {
        match self.interval {
            Interval::Daily => format!("{}_p{}", self.table, start.format("%Y_%m_%d")),
            Interval::Monthly => format!("{}_p{}", self.table, start.format("%Y_%m")),
        }
    }

    /// A bound as an SQL literal of the column's type, in UTC whatever the
    /// session's time zone.
    fn literal(&self, date: NaiveDate) -> String {
        match self.kind {
            Kind::DateTime => format!("'{} 00:00:00+00'", date),
            Kind::NaiveDateTime => format!("'{} 00:00:00'", date),
            _ => format!("'{}'", date),
        }
    }

    /// Creates the partition for the current period and the next `ahead`
    /// periods, if missing; returns the names it created. Safe to run often
    /// and from several instances.
    pub async fn ensure(&self, db: &Db, ahead: u32) -> Result<Vec<String>> {
        self.ensure_at(db, Utc::now(), ahead).await
    }

    /// [`ensure`](Partitioning::ensure) as if it were `now`, e.g. to create
    /// partitions for a backfill of older data.
    pub async fn ensure_at(&self, db: &Db, now: DateTime<Utc>, ahead: u32) -> Result<Vec<String>> {
        self.check()?;
        postgres(db)?;
        let existing: Vec<String> = self.partitions(db).await?.into_iter().map(|p| p.name).collect();
        let mut created = Vec::new();
        let mut start = self.period_start(now);
        for _ in 0..=ahead {
            let end = self.next(start);
            let name = self.name(start);
            if !existing.contains(&name) {
                db.execute(&format!(
                    "CREATE TABLE IF NOT EXISTS {} PARTITION OF {} FOR VALUES FROM ({}) TO ({})",
                    crate::sql::ident(crate::Backend::Postgres, &name),
                    crate::sql::ident(crate::Backend::Postgres, self.table),
                    self.literal(start),
                    self.literal(end)
                ))
                .await?;
                created.push(name);
            }
            start = end;
        }
        Ok(created)
    }

    /// The table's partitions, oldest first.
    pub async fn partitions(&self, db: &Db) -> Result<Vec<Partition>> {
        let pool = postgres(db)?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT c.relname::text, pg_get_expr(c.relpartbound, c.oid) \
             FROM pg_inherits i \
             JOIN pg_class c ON c.oid = i.inhrelid \
             JOIN pg_class p ON p.oid = i.inhparent \
             JOIN pg_namespace n ON n.oid = p.relnamespace \
             WHERE p.relname = $1 AND n.nspname = current_schema()",
        )
        .bind(self.table)
        .fetch_all(pool)
        .await?;
        let mut partitions: Vec<Partition> = rows
            .into_iter()
            .map(|(name, bound)| {
                let (from, to) = parse_bound(&bound);
                Partition { name, from, to }
            })
            .collect();
        partitions.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.name.cmp(&b.name)));
        Ok(partitions)
    }

    /// Detaches and drops every partition that ends at or before `cutoff`
    /// (so all its rows are older); returns the names dropped. Partitions
    /// that reach past the cutoff are kept whole.
    pub async fn drop_before(&self, db: &Db, cutoff: DateTime<Utc>) -> Result<Vec<String>> {
        postgres(db)?;
        let mut dropped = Vec::new();
        for partition in self.partitions(db).await? {
            if partition.to.is_some_and(|to| to <= cutoff) {
                let name = crate::sql::ident(crate::Backend::Postgres, &partition.name);
                let parent = crate::sql::ident(crate::Backend::Postgres, self.table);
                db.execute(&format!("ALTER TABLE {} DETACH PARTITION {}; DROP TABLE {};", parent, name, name)).await?;
                dropped.push(partition.name);
            }
        }
        Ok(dropped)
    }
}

fn postgres(db: &Db) -> Result<&sqlx::PgPool> {
    db.postgres().ok_or_else(|| Error::Config("partitioned tables need PostgreSQL".into()))
}

/// The range of `FOR VALUES FROM ('…') TO ('…')`, as PostgreSQL prints it.
fn parse_bound(bound: &str) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let value = |part: &str| -> Option<DateTime<Utc>> {
        let start = part.find('\'')? + 1;
        let end = start + part[start..].find('\'')?;
        parse_time(&part[start..end])
    };
    let Some(rest) = bound.strip_prefix("FOR VALUES FROM ") else { return (None, None) };
    match rest.split_once(" TO ") {
        Some((from, to)) => (value(from), value(to)),
        None => (None, None),
    }
}

/// A bound value: `2026-10-01 00:00:00+00`, `2026-10-01 08:00:00+08`,
/// `2026-10-01 00:00:00` or `2026-10-01`.
fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(time) = DateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%#z") {
        return Some(time.with_timezone(&Utc));
    }
    if let Ok(time) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S") {
        return Some(Utc.from_utc_datetime(&time));
    }
    let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds() {
        let at = |s: &str| Some(DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc));
        assert_eq!(
            parse_bound("FOR VALUES FROM ('2026-10-01 00:00:00+00') TO ('2026-11-01 00:00:00+00')"),
            (at("2026-10-01T00:00:00Z"), at("2026-11-01T00:00:00Z"))
        );
        assert_eq!(
            parse_bound("FOR VALUES FROM ('2026-10-01 08:00:00+08') TO ('2026-10-02 08:00:00+08')"),
            (at("2026-10-01T00:00:00Z"), at("2026-10-02T00:00:00Z"))
        );
        assert_eq!(parse_bound("FOR VALUES FROM ('2026-10-01') TO ('2026-11-01')").1, at("2026-11-01T00:00:00Z"));
        assert_eq!(parse_bound("FOR VALUES FROM (MINVALUE) TO ('2026-01-01 00:00:00+00')").0, None);
        assert_eq!(parse_bound("DEFAULT"), (None, None));
    }
}
