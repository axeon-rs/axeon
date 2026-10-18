//! Versioned SQL migrations.
//!
//! Put one file per change in a `migrations` folder, named
//! `<version>_<name>.sql`, where the version is a number that only goes up
//! (a date like `20261017120000` works well):
//!
//! ```text
//! migrations/
//!   20261017120000_create_users.sql
//!   20261018090000_add_posts.sql
//! ```
//!
//! Then run the pending ones at startup:
//!
//! ```ignore
//! # async fn run(db: axeon_orm::Db) -> axeon_orm::Result<()> {
//! // Embedded in the binary at compile time:
//! axeon_orm::embed_migrations!("migrations").run(&db).await?;
//!
//! // Or read from disk at runtime:
//! axeon_orm::migrate::Migrator::from_dir("migrations")?.run(&db).await?;
//! # Ok(()) }
//! ```
//!
//! Applied migrations are recorded in the `_axeon_migrations` table, with a
//! checksum: editing a migration after it ran is an error, so add a new one
//! instead. All pending migrations run in one transaction, so on PostgreSQL
//! and SQLite a failure leaves the database unchanged. Statements that
//! can't run inside a transaction, like `CREATE INDEX CONCURRENTLY`, aren't
//! supported. A lock (an advisory lock on PostgreSQL, `GET_LOCK` on MySQL)
//! stops two instances from migrating at once.
//!
//! **MySQL** commits every schema change (`CREATE`, `ALTER`, `DROP`) on its
//! own, so a failing migration there can leave earlier statements applied.
//! Keep MySQL migrations small: one change per file.
//!
//! The [`axeon-orm` command-line tool](https://docs.rs/axeon-orm-cli)
//! creates, runs and lists migrations: `axeon-orm migrate new|run|status`.
//!
//! [`embed_migrations!`](crate::embed_migrations) reads the folder at
//! compile time. Cargo doesn't notice new files on its own, so add a
//! `build.rs` with `println!("cargo:rerun-if-changed=migrations");`.

use crate::db::{Backend, Conn, Db};
use crate::error::{Error, Result};
use crate::sql::{ident, Sql};
use crate::value::Value;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;

const TABLE: &str = "_axeon_migrations";
/// Arbitrary key for `pg_advisory_xact_lock`.
const LOCK_KEY: i64 = 0x6178_656f_6e6d_6967;

/// One migration: a version, a name and its SQL.
#[derive(Clone, Debug)]
pub struct Migration {
    /// The version; migrations run in increasing order.
    pub version: i64,
    /// A description, from the file name.
    pub name: Cow<'static, str>,
    /// The SQL, which may hold several statements.
    pub sql: Cow<'static, str>,
}

impl Migration {
    /// A migration from its parts.
    pub fn new(version: i64, name: impl Into<Cow<'static, str>>, sql: impl Into<Cow<'static, str>>) -> Self {
        Self { version, name: name.into(), sql: sql.into() }
    }

    /// FNV-1a over the SQL; detects edits to applied migrations.
    fn checksum(&self) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.sql.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{:016x}", hash)
    }
}

/// An ordered set of migrations; see the [module docs](self).
#[derive(Clone, Debug, Default)]
pub struct Migrator {
    migrations: Vec<Migration>,
}

impl Migrator {
    /// Migrations from a list; sorted by version.
    pub fn new(mut migrations: Vec<Migration>) -> Self {
        migrations.sort_by_key(|m| m.version);
        Self { migrations }
    }

    /// Reads `<version>_<name>.sql` files from a folder; other files are
    /// ignored.
    pub fn from_dir(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let entries = std::fs::read_dir(path)
            .map_err(|e| Error::Migration(format!("cannot read {}: {}", path.display(), e)))?;
        let mut migrations = Vec::new();
        for entry in entries {
            let file = entry.map_err(|e| Error::Migration(e.to_string()))?.path();
            if file.extension().and_then(|e| e.to_str()) != Some("sql") {
                continue;
            }
            let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let (version, name) = parse_name(stem)
                .ok_or_else(|| Error::Migration(format!("{}: expected `<version>_<name>.sql`", file.display())))?;
            let sql = std::fs::read_to_string(&file)
                .map_err(|e| Error::Migration(format!("cannot read {}: {}", file.display(), e)))?;
            migrations.push(Migration::new(version, name.to_string(), sql));
        }
        Ok(Self::new(migrations))
    }

    /// The migrations, by version.
    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    fn check_unique(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for migration in &self.migrations {
            if !seen.insert(migration.version) {
                return Err(Error::Migration(format!("two migrations have version {}", migration.version)));
            }
        }
        Ok(())
    }

    async fn applied(conn: &mut Conn<'_>) -> Result<HashMap<i64, Applied>> {
        let backend = conn.backend();
        let applied_at = match backend {
            Backend::Postgres => format!("CAST({} AS TEXT)", ident(backend, "applied_at")),
            Backend::MySql => format!("CAST({} AS CHAR)", ident(backend, "applied_at")),
            _ => ident(backend, "applied_at"),
        };
        let sql = format!(
            "SELECT {}, {}, {}, {} AS {} FROM {} ORDER BY {}",
            ident(backend, "version"),
            ident(backend, "name"),
            ident(backend, "checksum"),
            applied_at,
            ident(backend, "applied_at"),
            ident(backend, TABLE),
            ident(backend, "version"),
        );
        let rows = conn.fetch_all(sql, Vec::new()).await?;
        rows.iter()
            .map(|row| {
                let applied = Applied {
                    name: row.get("name")?,
                    checksum: row.get("checksum")?,
                    applied_at: row.get("applied_at")?,
                };
                Ok((row.get::<i64>("version")?, applied))
            })
            .collect()
    }

    /// Migrations that haven't been applied yet.
    pub async fn pending(&self, db: &Db) -> Result<Vec<&Migration>> {
        let mut conn = Conn::Db(db);
        conn.execute_raw(create_table_sql(db.backend())).await?;
        let applied = Self::applied(&mut conn).await?;
        Ok(self.migrations.iter().filter(|m| !applied.contains_key(&m.version)).collect())
    }

    /// Every known migration with whether it was applied, plus migrations
    /// recorded in the database that are missing here; by version.
    pub async fn status(&self, db: &Db) -> Result<Vec<MigrationStatus>> {
        let mut conn = Conn::Db(db);
        conn.execute_raw(create_table_sql(db.backend())).await?;
        let mut applied = Self::applied(&mut conn).await?;
        let mut status: Vec<MigrationStatus> = self
            .migrations
            .iter()
            .map(|m| {
                let record = applied.remove(&m.version);
                MigrationStatus {
                    version: m.version,
                    name: m.name.to_string(),
                    changed: record.as_ref().is_some_and(|r| r.checksum != m.checksum()),
                    applied_at: record.map(|r| r.applied_at),
                    missing: false,
                }
            })
            .collect();
        status.extend(applied.into_iter().map(|(version, record)| MigrationStatus {
            version,
            name: record.name,
            applied_at: Some(record.applied_at),
            changed: false,
            missing: true,
        }));
        status.sort_by_key(|s| s.version);
        Ok(status)
    }

    /// Applies every pending migration in one transaction; returns the
    /// versions applied.
    pub async fn run(&self, db: &Db) -> Result<Vec<i64>> {
        self.check_unique()?;
        let mut tx = db.begin().await?;
        let backend = tx.backend();
        let mut conn = Conn::Tx(&mut tx);
        match backend {
            Backend::Postgres => conn.execute_raw(format!("SELECT pg_advisory_xact_lock({})", LOCK_KEY)).await?,
            Backend::MySql => conn.execute_raw(format!("SELECT GET_LOCK('{}', 60)", TABLE)).await?,
            _ => {}
        }
        let result = self.apply(&mut conn, backend).await;
        if backend == Backend::MySql {
            let _ = conn.execute_raw(format!("SELECT RELEASE_LOCK('{}')", TABLE)).await;
        }
        let ran = result?;
        tx.commit().await?;
        Ok(ran)
    }

    async fn apply(&self, conn: &mut Conn<'_>, backend: Backend) -> Result<Vec<i64>> {
        conn.execute_raw(create_table_sql(backend)).await?;
        let applied = Self::applied(conn).await?;
        let mut ran = Vec::new();
        for migration in &self.migrations {
            let checksum = migration.checksum();
            match applied.get(&migration.version) {
                Some(previous) if previous.checksum == checksum => continue,
                Some(_) => {
                    return Err(Error::Migration(format!(
                        "migration {} ({}) was changed after it was applied; add a new migration instead",
                        migration.version, migration.name
                    )))
                }
                None => {}
            }
            conn.execute_raw(migration.sql.to_string()).await.map_err(|e| {
                Error::Migration(format!("migration {} ({}) failed: {}", migration.version, migration.name, e))
            })?;
            let mut sql = Sql::new(backend);
            sql.push("INSERT INTO ").ident(TABLE).push(" (").ident("version").push(", ").ident("name");
            sql.push(", ").ident("checksum").push(") VALUES (");
            sql.arg(Value::I64(migration.version)).push(", ").arg(Value::Text(migration.name.to_string()));
            sql.push(", ").arg(Value::Text(checksum)).push(")");
            let (sql, args) = sql.finish();
            conn.execute(sql, args).await?;
            ran.push(migration.version);
        }
        Ok(ran)
    }
}

/// A migration's row in the `_axeon_migrations` table.
struct Applied {
    name: String,
    checksum: String,
    applied_at: String,
}

/// Where one migration stands, from [`Migrator::status`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MigrationStatus {
    /// The version.
    pub version: i64,
    /// The name.
    pub name: String,
    /// When it was applied, as the database prints it; `None` if pending.
    pub applied_at: Option<String>,
    /// It was applied, but its SQL has changed since.
    pub changed: bool,
    /// It was applied, but its file is gone.
    pub missing: bool,
}

fn create_table_sql(backend: Backend) -> String {
    let (name, applied_at, now) = match backend {
        Backend::Postgres => ("TEXT", "TIMESTAMPTZ", "CURRENT_TIMESTAMP"),
        Backend::MySql => ("VARCHAR(255)", "DATETIME(6)", "CURRENT_TIMESTAMP(6)"),
        _ => ("TEXT", "TEXT", "CURRENT_TIMESTAMP"),
    };
    format!(
        "CREATE TABLE IF NOT EXISTS {} ({} BIGINT PRIMARY KEY, {} {} NOT NULL, {} VARCHAR(64) NOT NULL, {} {} NOT NULL DEFAULT {})",
        ident(backend, TABLE),
        ident(backend, "version"),
        ident(backend, "name"),
        name,
        ident(backend, "checksum"),
        ident(backend, "applied_at"),
        applied_at,
        now,
    )
}

/// Splits `20261017_create_users` into its version and name.
fn parse_name(stem: &str) -> Option<(i64, &str)> {
    let (version, name) = stem.split_once('_')?;
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((version.parse().ok()?, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_names() {
        assert_eq!(parse_name("20261017_create_users"), Some((20261017, "create_users")));
        assert_eq!(parse_name("1_x"), Some((1, "x")));
        assert_eq!(parse_name("create_users"), None);
        assert_eq!(parse_name("12a_x"), None);
        assert_eq!(parse_name("123"), None);
    }

    #[test]
    fn checksum_changes_with_sql() {
        let a = Migration::new(1, "a", "CREATE TABLE a (id INT)");
        let b = Migration::new(1, "a", "CREATE TABLE a (id BIGINT)");
        assert_eq!(a.checksum(), a.clone().checksum());
        assert_ne!(a.checksum(), b.checksum());
    }
}
