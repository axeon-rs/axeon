//! SQL text building shared by queries, models and schema generation.

use crate::db::Backend;
use crate::model::Model;
use crate::value::Value;
use std::fmt::Write;

/// Quotes an identifier: `users` becomes `"users"`. Works in PostgreSQL
/// and SQLite.
pub(crate) fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The model's columns, quoted and comma-separated.
pub(crate) fn column_list<M: Model>() -> String {
    M::COLUMNS.iter().map(|c| ident(c.name)).collect::<Vec<_>>().join(", ")
}

/// A condition in a `WHERE` clause.
#[derive(Clone, Debug)]
pub(crate) enum Cond {
    Compare(&'static str, &'static str, Value),
    In(&'static str, Vec<Value>, bool),
    Null(&'static str, bool),
    And(Vec<Cond>),
    Or(Vec<Cond>),
    Not(Box<Cond>),
}

/// SQL text with its parameters, numbered for the target database.
pub(crate) struct Sql {
    backend: Backend,
    text: String,
    args: Vec<Value>,
}

impl Sql {
    pub fn new(backend: Backend) -> Self {
        Self { backend, text: String::with_capacity(128), args: Vec::new() }
    }

    pub fn push(&mut self, sql: &str) -> &mut Self {
        self.text.push_str(sql);
        self
    }

    /// Appends a parameter placeholder bound to `value`.
    pub fn arg(&mut self, value: Value) -> &mut Self {
        self.args.push(value);
        match self.backend {
            Backend::Postgres => {
                let _ = write!(self.text, "${}", self.args.len());
            }
            Backend::Sqlite => self.text.push('?'),
        }
        self
    }

    pub fn where_clause(&mut self, conds: &[Cond]) -> &mut Self {
        for (i, cond) in conds.iter().enumerate() {
            self.push(if i == 0 { " WHERE " } else { " AND " });
            self.cond(cond);
        }
        self
    }

    fn cond(&mut self, cond: &Cond) {
        match cond {
            Cond::Compare(column, op, value) if value.is_null() => {
                // `= NULL` is never true in SQL; mean what the caller meant.
                let test = match *op {
                    "=" => " IS NULL",
                    "<>" => " IS NOT NULL",
                    _ => " = NULL",
                };
                self.push(&ident(column)).push(test);
            }
            Cond::Compare(column, op, value) => {
                self.push(&ident(column)).push(" ").push(op).push(" ").arg(value.clone());
            }
            Cond::In(_, values, negated) if values.is_empty() => {
                self.push(if *negated { "1 = 1" } else { "1 = 0" });
            }
            Cond::In(column, values, negated) => {
                self.push(&ident(column)).push(if *negated { " NOT IN (" } else { " IN (" });
                for (i, value) in values.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.arg(value.clone());
                }
                self.push(")");
            }
            Cond::Null(column, negated) => {
                self.push(&ident(column)).push(if *negated { " IS NOT NULL" } else { " IS NULL" });
            }
            Cond::And(conds) | Cond::Or(conds) if conds.is_empty() => {
                self.push(if matches!(cond, Cond::And(_)) { "1 = 1" } else { "1 = 0" });
            }
            Cond::And(conds) | Cond::Or(conds) => {
                let joiner = if matches!(cond, Cond::And(_)) { " AND " } else { " OR " };
                self.push("(");
                for (i, cond) in conds.iter().enumerate() {
                    if i > 0 {
                        self.push(joiner);
                    }
                    self.cond(cond);
                }
                self.push(")");
            }
            Cond::Not(cond) => {
                self.push("NOT (");
                self.cond(cond);
                self.push(")");
            }
        }
    }

    pub fn finish(self) -> (String, Vec<Value>) {
        (self.text, self.args)
    }
}
