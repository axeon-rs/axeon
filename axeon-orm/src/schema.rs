//! `CREATE TABLE` statements generated from models.
//!
//! [`Db::create_table`](crate::Db::create_table) runs them directly, which
//! suits tests and prototypes. For production, print the SQL once and save
//! it as a [migration](crate::migrate):
//!
//! ```
//! use axeon_orm::{schema, Backend, Model};
//!
//! #[derive(Model)]
//! struct User {
//!     id: i64,
//!     #[model(unique)]
//!     email: String,
//! }
//!
//! let sql = schema::create_table::<User>(Backend::Postgres);
//! assert!(sql.starts_with(r#"CREATE TABLE IF NOT EXISTS "users""#));
//! ```

use crate::db::Backend;
use crate::model::{primary_key, ColumnDef, Model};
use crate::relation::BelongsToMany;
use crate::sql::ident;
use crate::value::Kind;

/// The SQL type for a column kind. On MySQL, `indexed` text becomes
/// `VARCHAR(255)`, since `TEXT` can't be a key there.
fn sql_type(kind: Kind, backend: Backend, indexed: bool) -> &'static str {
    match (backend, kind) {
        (_, Kind::Bool) => "BOOLEAN",
        (_, Kind::I32) => "INTEGER",
        (Backend::Sqlite, Kind::I64) => "INTEGER",
        (_, Kind::I64) => "BIGINT",
        (Backend::Postgres, Kind::F64) => "DOUBLE PRECISION",
        (Backend::MySql, Kind::F64) => "DOUBLE",
        (_, Kind::F64) => "REAL",
        (Backend::MySql, Kind::Text) if indexed => "VARCHAR(255)",
        (_, Kind::Text) => "TEXT",
        (Backend::Postgres, Kind::Bytes) => "BYTEA",
        (Backend::MySql, Kind::Bytes) => "LONGBLOB",
        (_, Kind::Bytes) => "BLOB",
        #[cfg(feature = "chrono")]
        (Backend::Postgres, Kind::DateTime) => "TIMESTAMPTZ",
        #[cfg(feature = "chrono")]
        (Backend::Postgres, Kind::NaiveDateTime) => "TIMESTAMP",
        #[cfg(feature = "chrono")]
        (Backend::MySql, Kind::DateTime | Kind::NaiveDateTime) => "DATETIME(6)",
        #[cfg(feature = "chrono")]
        (Backend::Postgres | Backend::MySql, Kind::Date) => "DATE",
        #[cfg(feature = "chrono")]
        (_, Kind::DateTime | Kind::NaiveDateTime | Kind::Date) => "TEXT",
        #[cfg(feature = "uuid")]
        (Backend::Postgres, Kind::Uuid) => "UUID",
        #[cfg(feature = "uuid")]
        (Backend::MySql, Kind::Uuid) => "BINARY(16)",
        #[cfg(feature = "uuid")]
        (_, Kind::Uuid) => "BLOB",
        #[cfg(feature = "json")]
        (Backend::Postgres, Kind::Json) => "JSONB",
        #[cfg(feature = "json")]
        (Backend::MySql, Kind::Json) => "JSON",
        #[cfg(feature = "json")]
        (_, Kind::Json) => "TEXT",
        #[cfg(feature = "pgvector")]
        (Backend::Postgres, Kind::Vector) => "vector",
        #[cfg(feature = "pgvector")]
        (_, Kind::Vector) => "TEXT",
    }
}

/// Whether a column is part of a key or index, which MySQL can't build on
/// `TEXT`.
fn is_indexed(column: &ColumnDef) -> bool {
    column.primary_key || column.unique || column.index || column.references.is_some()
}

/// A column definition; without `inline_keys`, `PRIMARY KEY` and `UNIQUE`
/// are left for table-level constraints (partitioned tables).
fn column_sql_with(column: &ColumnDef, backend: Backend, inline_keys: bool) -> String {
    let name = ident(backend, column.name);
    if column.primary_key && column.auto_increment && !inline_keys {
        let serial = if column.kind == Kind::I32 { "SERIAL" } else { "BIGSERIAL" };
        return format!("{} {} NOT NULL", name, serial);
    }
    if column.primary_key && column.auto_increment {
        return match (backend, column.kind) {
            (Backend::Postgres, Kind::I32) => format!("{} SERIAL PRIMARY KEY", name),
            (Backend::Postgres, _) => format!("{} BIGSERIAL PRIMARY KEY", name),
            (Backend::MySql, Kind::I32) => format!("{} INT AUTO_INCREMENT PRIMARY KEY", name),
            (Backend::MySql, _) => format!("{} BIGINT AUTO_INCREMENT PRIMARY KEY", name),
            // Only exactly `INTEGER PRIMARY KEY` aliases SQLite's rowid.
            (Backend::Sqlite, _) => format!("{} INTEGER PRIMARY KEY AUTOINCREMENT", name),
        };
    }
    let mut sql = format!("{} {}", name, sql_type(column.kind, backend, is_indexed(column)));
    if let (Some(dimensions), Backend::Postgres) = (column.dimensions, backend) {
        sql.push_str(&format!("({})", dimensions));
    }
    if column.primary_key && inline_keys {
        sql.push_str(" PRIMARY KEY");
    } else if !column.nullable || column.primary_key {
        sql.push_str(" NOT NULL");
    }
    if column.unique && inline_keys {
        sql.push_str(" UNIQUE");
    }
    if let Some(values) = column.values {
        let values: Vec<String> = values.iter().map(|v| format!("'{}'", v.replace('\'', "''"))).collect();
        sql.push_str(&format!(" CHECK ({} IN ({}))", name, values.join(", ")));
    }
    let now = match (backend, column.kind) {
        #[cfg(feature = "chrono")]
        (Backend::MySql, Kind::DateTime | Kind::NaiveDateTime) => "CURRENT_TIMESTAMP(6)",
        _ => "CURRENT_TIMESTAMP",
    };
    match (column.default_sql, column.timestamp) {
        (Some(default), _) => sql.push_str(&format!(" DEFAULT {}", default)),
        (None, Some(_)) => sql.push_str(&format!(" DEFAULT {}", now)),
        _ => {}
    }
    // MySQL ignores inline REFERENCES; it gets a table-level FOREIGN KEY.
    if let (Some(reference), false) = (column.references, backend == Backend::MySql) {
        sql.push_str(&format!(" REFERENCES {} ({})", ident(backend, reference.table), ident(backend, reference.column)));
        if let Some(action) = reference.on_delete {
            sql.push_str(&format!(" ON DELETE {}", action.to_ascii_uppercase()));
        }
    }
    sql
}

fn foreign_key(backend: Backend, column: &str, table: &str, target: &str, on_delete: Option<&str>) -> String {
    let mut sql = format!(
        "    FOREIGN KEY ({}) REFERENCES {} ({})",
        ident(backend, column),
        ident(backend, table),
        ident(backend, target)
    );
    if let Some(action) = on_delete {
        sql.push_str(&format!(" ON DELETE {}", action.to_ascii_uppercase()));
    }
    sql
}

/// `CREATE TABLE IF NOT EXISTS` for the model, plus an index for each
/// `#[model(index)]` column.
pub fn create_table<M: Model>(backend: Backend) -> String {
    let table = ident(backend, M::TABLE);
    let composite = M::PRIMARY_KEYS.len() > 1;
    let mut lines: Vec<String> = M::COLUMNS
        .iter()
        .map(|c| format!("    {}", column_sql_with(c, backend, !(composite && c.primary_key))))
        .collect();
    if composite {
        let key: Vec<String> = M::PRIMARY_KEYS.iter().map(|c| ident(backend, c)).collect();
        lines.push(format!("    PRIMARY KEY ({})", key.join(", ")));
    }
    let indexed = M::COLUMNS.iter().filter(|c| c.index && !c.unique && !c.primary_key);
    let index_name = |column: &str| ident(backend, &format!("{}_{}_index", M::TABLE, column));
    let mut after = String::new();
    if backend == Backend::MySql {
        // MySQL has no CREATE INDEX IF NOT EXISTS; declare everything inline.
        for column in M::COLUMNS {
            if let Some(reference) = column.references {
                lines.push(foreign_key(backend, column.name, reference.table, reference.column, reference.on_delete));
            }
        }
        for column in indexed {
            lines.push(format!("    INDEX {} ({})", index_name(column.name), ident(backend, column.name)));
        }
    } else {
        for column in indexed {
            after.push_str(&format!(
                "\nCREATE INDEX IF NOT EXISTS {} ON {} ({});",
                index_name(column.name),
                table,
                ident(backend, column.name)
            ));
        }
    }
    format!("CREATE TABLE IF NOT EXISTS {} (\n{}\n);{}", table, lines.join(",\n"), after)
}

/// `CREATE TABLE IF NOT EXISTS` for a PostgreSQL table partitioned by range
/// of `column`, whose primary key must include it.
#[cfg(all(feature = "postgres", feature = "chrono"))]
pub(crate) fn create_partitioned_table<M: Model>(column: &str) -> crate::Result<String> {
    let backend = Backend::Postgres;
    if let Some(unique) = M::COLUMNS.iter().find(|c| c.unique && c.name != column) {
        return Err(crate::Error::Config(format!(
            "`{}.{}` is unique, but a partitioned table's unique constraints must include `{}`",
            M::TABLE,
            unique.name,
            column
        )));
    }
    let table = ident(backend, M::TABLE);
    let mut lines: Vec<String> = M::COLUMNS.iter().map(|c| format!("    {}", column_sql_with(c, backend, false))).collect();
    let mut key: Vec<String> = M::PRIMARY_KEYS.iter().map(|c| ident(backend, c)).collect();
    if !M::PRIMARY_KEYS.contains(&column) {
        key.push(ident(backend, column));
    }
    lines.push(format!("    PRIMARY KEY ({})", key.join(", ")));
    let mut after = String::new();
    for indexed in M::COLUMNS.iter().filter(|c| c.index && !c.unique && !c.primary_key) {
        after.push_str(&format!(
            "\nCREATE INDEX IF NOT EXISTS {} ON {} ({});",
            ident(backend, &format!("{}_{}_index", M::TABLE, indexed.name)),
            table,
            ident(backend, indexed.name)
        ));
    }
    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {} (\n{}\n) PARTITION BY RANGE ({});{}",
        table,
        lines.join(",\n"),
        ident(backend, column),
        after
    ))
}

/// `CREATE TABLE IF NOT EXISTS` for a many-to-many pivot table: both key
/// columns, a composite primary key, and foreign keys that delete links
/// with either record.
pub fn create_pivot_table<P: Model, C: Model>(relation: &BelongsToMany<P, C>, backend: Backend) -> String {
    let (parent, related) = (primary_key::<P>(), primary_key::<C>());
    let column = |name: &str, kind: Kind| format!("    {} {} NOT NULL", ident(backend, name), sql_type(kind, backend, true));
    let lines = [
        column(relation.foreign_key(), parent.kind),
        column(relation.related_key(), related.kind),
        format!(
            "    PRIMARY KEY ({}, {})",
            ident(backend, relation.foreign_key()),
            ident(backend, relation.related_key())
        ),
        foreign_key(backend, relation.foreign_key(), P::TABLE, parent.name, Some("cascade")),
        foreign_key(backend, relation.related_key(), C::TABLE, related.name, Some("cascade")),
    ];
    format!("CREATE TABLE IF NOT EXISTS {} (\n{}\n);", ident(backend, relation.pivot()), lines.join(",\n"))
}
