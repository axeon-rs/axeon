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
use crate::model::{ColumnDef, Model};
use crate::sql::ident;
use crate::value::Kind;

/// The SQL type for a column kind.
pub fn sql_type(kind: Kind, backend: Backend) -> &'static str {
    let postgres = backend == Backend::Postgres;
    match kind {
        Kind::Bool => "BOOLEAN",
        Kind::I32 => "INTEGER",
        Kind::I64 if postgres => "BIGINT",
        Kind::I64 => "INTEGER",
        Kind::F64 if postgres => "DOUBLE PRECISION",
        Kind::F64 => "REAL",
        Kind::Text => "TEXT",
        Kind::Bytes if postgres => "BYTEA",
        Kind::Bytes => "BLOB",
        #[cfg(feature = "chrono")]
        Kind::DateTime if postgres => "TIMESTAMPTZ",
        #[cfg(feature = "chrono")]
        Kind::NaiveDateTime if postgres => "TIMESTAMP",
        #[cfg(feature = "chrono")]
        Kind::Date if postgres => "DATE",
        #[cfg(feature = "chrono")]
        Kind::DateTime | Kind::NaiveDateTime | Kind::Date => "TEXT",
        #[cfg(feature = "uuid")]
        Kind::Uuid if postgres => "UUID",
        #[cfg(feature = "uuid")]
        Kind::Uuid => "BLOB",
        #[cfg(feature = "json")]
        Kind::Json if postgres => "JSONB",
        #[cfg(feature = "json")]
        Kind::Json => "TEXT",
    }
}

fn column_sql(column: &ColumnDef, backend: Backend) -> String {
    let name = ident(column.name);
    if column.primary_key && column.auto_increment {
        return match (backend, column.kind) {
            (Backend::Postgres, Kind::I32) => format!("{} SERIAL PRIMARY KEY", name),
            (Backend::Postgres, _) => format!("{} BIGSERIAL PRIMARY KEY", name),
            // Only exactly `INTEGER PRIMARY KEY` aliases SQLite's rowid.
            (Backend::Sqlite, _) => format!("{} INTEGER PRIMARY KEY AUTOINCREMENT", name),
        };
    }
    let mut sql = format!("{} {}", name, sql_type(column.kind, backend));
    if column.primary_key {
        sql.push_str(" PRIMARY KEY");
    } else if !column.nullable {
        sql.push_str(" NOT NULL");
    }
    if column.unique {
        sql.push_str(" UNIQUE");
    }
    match (column.default_sql, column.timestamp) {
        (Some(default), _) => sql.push_str(&format!(" DEFAULT {}", default)),
        (None, Some(_)) => sql.push_str(" DEFAULT CURRENT_TIMESTAMP"),
        _ => {}
    }
    if let Some(reference) = column.references {
        sql.push_str(&format!(" REFERENCES {} ({})", ident(reference.table), ident(reference.column)));
        if let Some(action) = reference.on_delete {
            sql.push_str(&format!(" ON DELETE {}", action.to_ascii_uppercase()));
        }
    }
    sql
}

/// `CREATE TABLE IF NOT EXISTS` for the model, plus `CREATE INDEX IF NOT
/// EXISTS` for each `#[model(index)]` column.
pub fn create_table<M: Model>(backend: Backend) -> String {
    let columns: Vec<String> = M::COLUMNS.iter().map(|c| format!("    {}", column_sql(c, backend))).collect();
    let mut sql = format!("CREATE TABLE IF NOT EXISTS {} (\n{}\n);", ident(M::TABLE), columns.join(",\n"));
    for column in M::COLUMNS.iter().filter(|c| c.index && !c.unique && !c.primary_key) {
        sql.push_str(&format!(
            "\nCREATE INDEX IF NOT EXISTS {} ON {} ({});",
            ident(&format!("{}_{}_index", M::TABLE, column.name)),
            ident(M::TABLE),
            ident(column.name)
        ));
    }
    sql
}
