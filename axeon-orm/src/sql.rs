//! SQL text building shared by queries, models and schema generation.

use crate::db::Backend;
use crate::model::Model;
use crate::value::Value;
use std::fmt::Write;

/// Quotes an identifier: `"users"` in PostgreSQL and SQLite, `` `users` ``
/// in MySQL.
pub(crate) fn ident(backend: Backend, name: &str) -> String {
    match backend {
        Backend::MySql => format!("`{}`", name.replace('`', "``")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

/// The model's columns, quoted and comma-separated.
pub(crate) fn column_list<M: Model>(backend: Backend) -> String {
    M::COLUMNS.iter().map(|c| ident(backend, c.name)).collect::<Vec<_>>().join(", ")
}

/// A subquery selecting one column: `SELECT column FROM table WHERE ...`.
/// Null values are left out, so `NOT IN` works as expected.
#[derive(Clone, Debug)]
pub(crate) struct Subquery {
    pub table: &'static str,
    pub column: &'static str,
    pub conds: Vec<Cond>,
}

/// A condition in a `WHERE` clause.
#[derive(Clone, Debug)]
pub(crate) enum Cond {
    Compare(&'static str, &'static str, Value),
    In(&'static str, Vec<Value>, bool),
    InSubquery(&'static str, Box<Subquery>, bool),
    Null(&'static str, bool),
    And(Vec<Cond>),
    Or(Vec<Cond>),
    Not(Box<Cond>),
    Raw(Fragment),
}

/// Raw SQL from [`Expr::raw`](crate::Expr::raw) or
/// [`Order::raw`](crate::Order::raw): the text between `?` placeholders,
/// and the values bound to them.
#[derive(Clone, Debug)]
pub(crate) struct Fragment {
    pieces: Vec<String>,
    args: Vec<Value>,
}

impl Fragment {
    /// Splits `sql` at each `?`; `??` is a literal `?`.
    ///
    /// Panics when the number of placeholders differs from `args`.
    pub fn new(sql: &str, args: Vec<Value>) -> Self {
        let mut pieces = vec![String::new()];
        let mut chars = sql.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '?' if chars.peek() == Some(&'?') => {
                    chars.next();
                    pieces.last_mut().unwrap().push('?');
                }
                '?' => pieces.push(String::new()),
                c => pieces.last_mut().unwrap().push(c),
            }
        }
        assert!(
            pieces.len() - 1 == args.len(),
            "raw SQL {:?} has {} placeholder(s) but {} value(s)",
            sql,
            pieces.len() - 1,
            args.len()
        );
        Self { pieces, args }
    }
}

/// A row lock: `FOR UPDATE`, and what to do about rows another
/// transaction has locked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lock {
    /// Wait for them.
    Wait,
    /// Leave them out (`SKIP LOCKED`).
    SkipLocked,
    /// Fail at once (`NOWAIT`).
    NoWait,
}

/// An `ORDER BY` term.
#[derive(Clone, Debug)]
pub(crate) struct Sort {
    pub term: SortTerm,
    pub descending: bool,
    /// A table alias to qualify a column with, in joins.
    pub qualifier: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub(crate) enum SortTerm {
    Column(&'static str),
    Raw(Fragment),
}

/// SQL text with its parameters, numbered for the target database.
pub(crate) struct Sql {
    backend: Backend,
    text: String,
    args: Vec<Value>,
    /// A table alias to qualify column names with, in joins.
    qualifier: Option<&'static str>,
    /// Whether `WHERE` has been written, so later conditions use `AND`.
    has_where: bool,
}

impl Sql {
    pub fn new(backend: Backend) -> Self {
        Self { backend, text: String::with_capacity(128), args: Vec::new(), qualifier: None, has_where: false }
    }

    pub fn push(&mut self, sql: &str) -> &mut Self {
        self.text.push_str(sql);
        self
    }

    /// Appends a quoted identifier.
    pub fn ident(&mut self, name: &str) -> &mut Self {
        let quoted = ident(self.backend, name);
        self.push(&quoted)
    }

    /// Appends a column name, qualified with the current table alias.
    pub fn column(&mut self, name: &str) -> &mut Self {
        if let Some(alias) = self.qualifier {
            self.ident(alias).push(".");
        }
        self.ident(name)
    }

    /// Appends a parameter placeholder bound to `value`.
    pub fn arg(&mut self, value: Value) -> &mut Self {
        self.args.push(value);
        match self.backend {
            Backend::Postgres => {
                let _ = write!(self.text, "${}", self.args.len());
            }
            _ => self.text.push('?'),
        }
        self
    }

    /// Appends raw SQL, binding its values.
    pub fn fragment(&mut self, fragment: &Fragment) -> &mut Self {
        for (i, piece) in fragment.pieces.iter().enumerate() {
            if i > 0 {
                self.arg(fragment.args[i - 1].clone());
            }
            self.push(piece);
        }
        self
    }

    /// Appends `ORDER BY` and the terms, if there are any.
    pub fn order_by(&mut self, sorts: &[Sort]) -> &mut Self {
        for (i, sort) in sorts.iter().enumerate() {
            self.push(if i == 0 { " ORDER BY " } else { ", " });
            match &sort.term {
                SortTerm::Column(column) => {
                    let previous = std::mem::replace(&mut self.qualifier, sort.qualifier);
                    self.column(column);
                    self.qualifier = previous;
                }
                SortTerm::Raw(fragment) => {
                    self.push("(").fragment(fragment).push(")");
                }
            }
            self.push(if sort.descending { " DESC" } else { " ASC" });
        }
        self
    }

    /// Appends `LIMIT` and `OFFSET`; SQLite and MySQL need a `LIMIT` before
    /// an `OFFSET`.
    pub fn limit_offset(&mut self, limit: Option<u64>, offset: Option<u64>) -> &mut Self {
        match (limit, self.backend) {
            (Some(limit), _) => {
                let _ = write!(self.text, " LIMIT {}", limit);
            }
            (None, Backend::Sqlite) if offset.is_some() => self.text.push_str(" LIMIT -1"),
            (None, Backend::MySql) if offset.is_some() => self.text.push_str(" LIMIT 18446744073709551615"),
            _ => {}
        }
        if let Some(offset) = offset {
            let _ = write!(self.text, " OFFSET {}", offset);
        }
        self
    }

    /// Appends `FOR UPDATE`, locking only the rows of table alias `of` in a
    /// join. SQLite has no row locks (a write transaction locks the whole
    /// database), so nothing is written there.
    pub fn lock(&mut self, lock: Option<Lock>, of: Option<&'static str>) -> &mut Self {
        let Some(lock) = lock else { return self };
        if self.backend == Backend::Sqlite {
            return self;
        }
        self.push(" FOR UPDATE");
        if let Some(alias) = of {
            self.push(" OF ").ident(alias);
        }
        match lock {
            Lock::Wait => self,
            Lock::SkipLocked => self.push(" SKIP LOCKED"),
            Lock::NoWait => self.push(" NOWAIT"),
        }
    }

    /// Appends `WHERE` (or `AND`, after an earlier `where_clause`) and the
    /// conditions, with columns qualified by `qualifier`.
    pub fn where_clause_as(&mut self, conds: &[Cond], qualifier: Option<&'static str>) -> &mut Self {
        let previous = std::mem::replace(&mut self.qualifier, qualifier);
        for cond in conds {
            let keyword = if self.has_where { " AND " } else { " WHERE " };
            self.has_where = true;
            self.push(keyword);
            self.cond(cond);
        }
        self.qualifier = previous;
        self
    }

    pub fn where_clause(&mut self, conds: &[Cond]) -> &mut Self {
        self.where_clause_as(conds, None)
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
                self.column(column).push(test);
            }
            Cond::Compare(column, op, value) => {
                self.column(column).push(" ").push(op).push(" ").arg(value.clone());
            }
            Cond::In(_, values, negated) if values.is_empty() => {
                self.push(if *negated { "1 = 1" } else { "1 = 0" });
            }
            Cond::In(column, values, negated) => {
                self.column(column).push(if *negated { " NOT IN (" } else { " IN (" });
                for (i, value) in values.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.arg(value.clone());
                }
                self.push(")");
            }
            Cond::InSubquery(column, sub, negated) => {
                self.column(column).push(if *negated { " NOT IN (" } else { " IN (" });
                // Inside the subquery, bare names refer to its own table.
                let outer = self.qualifier.take();
                self.push("SELECT ").ident(sub.column).push(" FROM ").ident(sub.table);
                self.push(" WHERE ").ident(sub.column).push(" IS NOT NULL");
                for cond in &sub.conds {
                    self.push(" AND ");
                    self.cond(cond);
                }
                self.qualifier = outer;
                self.push(")");
            }
            Cond::Null(column, negated) => {
                self.column(column).push(if *negated { " IS NOT NULL" } else { " IS NULL" });
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
            // Parenthesized, so `OR` inside the fragment can't escape it.
            Cond::Raw(fragment) => {
                self.push("(").fragment(fragment).push(")");
            }
        }
    }

    pub fn finish(self) -> (String, Vec<Value>) {
        (self.text, self.args)
    }
}
