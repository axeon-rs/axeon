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
//! instead. All pending migrations run in one transaction, so a failure
//! leaves the database unchanged; on PostgreSQL an advisory lock stops two
//! instances from migrating at once. Statements that can't run inside a
//! transaction, like `CREATE INDEX CONCURRENTLY`, aren't supported.
//!
//! [`embed_migrations!`](crate::embed_migrations) reads the folder at
//! compile time. Cargo doesn't notice new files on its own, so add a
//! `build.rs` with `println!("cargo:rerun-if-changed=migrations");`.

use crate::db::{Backend, Conn, Db};
use crate::error::{Error, Result};
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

    async fn applied(conn: &mut Conn<'_>) -> Result<HashMap<i64, String>> {
        let rows = conn.fetch_all(format!("SELECT \"version\", \"checksum\" FROM \"{}\"", TABLE), Vec::new()).await?;
        rows.iter().map(|row| Ok((row.get::<i64>("version")?, row.get::<String>("checksum")?))).collect()
    }

    /// Migrations that haven't been applied yet.
    pub async fn pending(&self, db: &Db) -> Result<Vec<&Migration>> {
        let mut conn = Conn::Db(db);
        conn.execute_raw(create_table_sql(db.backend())).await?;
        let applied = Self::applied(&mut conn).await?;
        Ok(self.migrations.iter().filter(|m| !applied.contains_key(&m.version)).collect())
    }

    /// Applies every pending migration in one transaction; returns the
    /// versions applied.
    pub async fn run(&self, db: &Db) -> Result<Vec<i64>> {
        self.check_unique()?;
        let mut tx = db.begin().await?;
        let backend = tx.backend();
        let mut conn = Conn::Tx(&mut tx);
        if backend == Backend::Postgres {
            conn.execute_raw(format!("SELECT pg_advisory_xact_lock({})", LOCK_KEY)).await?;
        }
        conn.execute_raw(create_table_sql(backend)).await?;
        let applied = Self::applied(&mut conn).await?;

        let mut ran = Vec::new();
        for migration in &self.migrations {
            let checksum = migration.checksum();
            match applied.get(&migration.version) {
                Some(previous) if *previous == checksum => continue,
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
            let insert = match backend {
                Backend::Postgres => format!("INSERT INTO \"{}\" (\"version\", \"name\", \"checksum\") VALUES ($1, $2, $3)", TABLE),
                Backend::Sqlite => format!("INSERT INTO \"{}\" (\"version\", \"name\", \"checksum\") VALUES (?, ?, ?)", TABLE),
            };
            let args = vec![Value::I64(migration.version), Value::Text(migration.name.to_string()), Value::Text(checksum)];
            conn.execute(insert, args).await?;
            ran.push(migration.version);
        }
        tx.commit().await?;
        Ok(ran)
    }
}

fn create_table_sql(backend: Backend) -> String {
    let applied_at = match backend {
        Backend::Postgres => "TIMESTAMPTZ",
        Backend::Sqlite => "TEXT",
    };
    format!(
        "CREATE TABLE IF NOT EXISTS \"{}\" (\"version\" BIGINT PRIMARY KEY, \"name\" TEXT NOT NULL, \
         \"checksum\" TEXT NOT NULL, \"applied_at\" {} NOT NULL DEFAULT CURRENT_TIMESTAMP)",
        TABLE, applied_at
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
