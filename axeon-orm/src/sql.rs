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
    #[cfg(feature = "json")]
    Json(&'static str, JsonPath, JsonTest),
    /// `column <op> vector <compare> limit`, e.g. `embedding <=> $1 < 0.3`.
    #[cfg(feature = "pgvector")]
    Distance(&'static str, &'static str, Value, &'static str, f64),
}

/// A step into a JSON document: an object key or an array index.
#[cfg(feature = "json")]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PathStep {
    Key(String),
    Index(i32),
}

/// A path into a JSON column, from the document's root.
#[cfg(feature = "json")]
pub(crate) type JsonPath = Vec<PathStep>;

/// What a JSON condition checks.
#[cfg(feature = "json")]
#[derive(Clone, Debug)]
pub(crate) enum JsonTest {
    /// The document contains this JSON (PostgreSQL `@>`).
    Contains(Value),
    /// The top-level object has this key (PostgreSQL `?`).
    HasKey(String),
    /// The path exists (its value may be JSON `null`).
    Exists,
    /// The string at the path compared with `op`.
    Text(&'static str, Value),
    /// The string at the path is (not) one of these.
    TextIn(Vec<Value>, bool),
    /// The number at the path compared with `op`; non-numbers don't match.
    Number(&'static str, f64),
    /// The value at the path is this boolean.
    Bool(bool),
}

/// A SQL/JSON path string for SQLite and MySQL: `$."a"[0]."b"`.
#[cfg(feature = "json")]
fn json_path_string(path: &[PathStep]) -> String {
    let mut out = String::from("$");
    for step in path {
        match step {
            PathStep::Key(key) => {
                out.push_str(".\"");
                for c in key.chars() {
                    if c == '"' || c == '\\' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            }
            PathStep::Index(i) => {
                let _ = write!(out, "[{}]", i);
            }
        }
    }
    out
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
    #[cfg(feature = "json")]
    Json(&'static str, JsonPath),
    /// `column <op> vector`.
    #[cfg(feature = "pgvector")]
    Distance(&'static str, &'static str, Value),
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
                #[cfg(feature = "pgvector")]
                SortTerm::Distance(column, op, target) => {
                    let previous = std::mem::replace(&mut self.qualifier, sort.qualifier);
                    self.column(column).push(" ").push(op).push(" ").arg(target.clone());
                    self.qualifier = previous;
                }
                #[cfg(feature = "json")]
                SortTerm::Json(column, path) => {
                    let previous = std::mem::replace(&mut self.qualifier, sort.qualifier);
                    self.json_value(column, path);
                    self.qualifier = previous;
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
            #[cfg(feature = "pgvector")]
            Cond::Distance(column, op, target, compare, limit) => {
                self.push("(").column(column).push(" ").push(op).push(" ").arg(target.clone());
                self.push(" ").push(compare).push(" ").arg(Value::F64(*limit)).push(")");
            }
            // Never NULL (a missing key would make it unknown), so `!` gives
            // exactly the other rows. PostgreSQL's `@>` and `?` stay bare
            // so a GIN index can serve them; only a NULL column makes them
            // NULL, which `AND column IS NOT NULL` turns into false.
            #[cfg(feature = "json")]
            Cond::Json(column, path, test) => {
                if self.backend == Backend::Postgres && matches!(test, JsonTest::Contains(_) | JsonTest::HasKey(_)) {
                    self.push("(");
                    self.json_cond(column, path, test);
                    self.push(" AND ").column(column).push(" IS NOT NULL)");
                } else {
                    self.push("COALESCE((");
                    self.json_cond(column, path, test);
                    self.push("), FALSE)");
                }
            }
        }
    }

    /// The JSON value at `path` (path keys are bound, never inlined):
    /// jsonb in PostgreSQL, JSON in MySQL, an SQL value in SQLite.
    #[cfg(feature = "json")]
    fn json_value(&mut self, column: &str, path: &[PathStep]) {
        match self.backend {
            Backend::Postgres => {
                self.column(column);
                for step in path {
                    self.push(" -> ");
                    self.path_step(step);
                }
            }
            Backend::MySql => {
                self.push("JSON_EXTRACT(").column(column).push(", ").arg(Value::Text(json_path_string(path))).push(")");
            }
            Backend::Sqlite => {
                self.push("json_extract(").column(column).push(", ").arg(Value::Text(json_path_string(path))).push(")");
            }
        }
    }

    /// A PostgreSQL path step, cast so that `->` picks the right overload,
    /// and so key and index paths never share a cached statement.
    #[cfg(feature = "json")]
    fn path_step(&mut self, step: &PathStep) {
        match step {
            PathStep::Key(key) => self.arg(Value::Text(key.clone())).push("::text"),
            PathStep::Index(i) => self.arg(Value::I32(*i)).push("::int"),
        };
    }

    /// The string at `path` as text; other JSON values give their text in
    /// PostgreSQL and MySQL.
    #[cfg(feature = "json")]
    fn json_text(&mut self, column: &str, path: &[PathStep]) {
        match (self.backend, path.split_last()) {
            (Backend::Postgres, Some((last, init))) => {
                self.push("(");
                self.json_value(column, init);
                self.push(" ->> ");
                self.path_step(last);
                self.push(")");
            }
            (Backend::MySql, _) => {
                self.push("JSON_UNQUOTE(");
                self.json_value(column, path);
                self.push(")");
            }
            _ => self.json_value(column, path),
        }
    }

    /// The JSON type of the value at `path`, lower-case in every database
    /// (`number` / `integer` / `real` / `double` vary), or NULL if missing.
    #[cfg(feature = "json")]
    fn json_type(&mut self, column: &str, path: &[PathStep]) {
        match self.backend {
            Backend::Postgres => {
                self.push("jsonb_typeof(");
                self.json_value(column, path);
                self.push(")");
            }
            Backend::MySql => {
                self.push("LOWER(JSON_TYPE(");
                self.json_value(column, path);
                self.push("))");
            }
            Backend::Sqlite => {
                self.push("json_type(").column(column).push(", ").arg(Value::Text(json_path_string(path))).push(")");
            }
        }
    }

    /// `= <the JSON string type>`: SQLite's `json_type` calls it `text`.
    #[cfg(feature = "json")]
    fn json_string_type(&self) -> &'static str {
        match self.backend {
            Backend::Sqlite => " = 'text' AND ",
            _ => " = 'string' AND ",
        }
    }

    #[cfg(feature = "json")]
    fn json_cond(&mut self, column: &str, path: &[PathStep], test: &JsonTest) {
        match (test, self.backend) {
            (JsonTest::Contains(value), Backend::Postgres) => {
                self.column(column).push(" @> ").arg(value.clone());
            }
            (JsonTest::Contains(value), Backend::MySql) => {
                self.push("JSON_CONTAINS(").column(column).push(", ").arg(value.clone()).push(")");
            }
            (JsonTest::Contains(_), Backend::Sqlite) => {
                // SQLite has no JSON containment: fail with a clear error.
                self.push("axeon_orm_json_contains_needs_postgres_or_mysql()");
            }
            (JsonTest::HasKey(key), Backend::Postgres) => {
                self.column(column).push(" ? ").arg(Value::Text(key.clone()));
            }
            (JsonTest::HasKey(key), _) => {
                let path = [PathStep::Key(key.clone())];
                self.json_type(column, &path);
                self.push(" IS NOT NULL");
            }
            (JsonTest::Exists, _) => {
                self.json_type(column, path);
                self.push(" IS NOT NULL");
            }
            (JsonTest::Text(op, value), _) => {
                self.json_type(column, path);
                self.push(self.json_string_type());
                self.json_text(column, path);
                self.push(" ").push(op).push(" ").arg(value.clone());
            }
            (JsonTest::TextIn(values, negated), _) => {
                self.json_type(column, path);
                self.push(self.json_string_type());
                self.json_text(column, path);
                self.push(if *negated { " NOT IN (" } else { " IN (" });
                for (i, value) in values.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.arg(value.clone());
                }
                self.push(")");
            }
            (JsonTest::Number(op, number), backend) => {
                // Only JSON numbers take part: no cast of a string can fail.
                let numeric = match backend {
                    Backend::Postgres => "('number')",
                    Backend::MySql => "('integer', 'double', 'decimal', 'unsigned integer')",
                    Backend::Sqlite => "('integer', 'real')",
                };
                self.push("CASE WHEN ");
                self.json_type(column, path);
                self.push(" IN ").push(numeric).push(" THEN ");
                match backend {
                    Backend::Postgres => {
                        self.push("(");
                        self.json_value(column, path);
                        self.push(")::numeric");
                    }
                    Backend::MySql => {
                        self.push("CAST(");
                        self.json_value(column, path);
                        self.push(" AS DOUBLE)");
                    }
                    Backend::Sqlite => self.json_value(column, path),
                }
                self.push(" END ").push(op).push(" ").arg(Value::F64(*number));
            }
            (JsonTest::Bool(expected), _) => {
                self.json_type(column, path);
                self.push(match self.backend {
                    Backend::Sqlite => " = ",
                    _ => " = 'boolean' AND ",
                });
                match self.backend {
                    Backend::Sqlite => {
                        self.arg(Value::Text(expected.to_string()));
                    }
                    _ => {
                        self.json_text(column, path);
                        self.push(" = ").arg(Value::Text(expected.to_string()));
                    }
                }
            }
        }
    }

    pub fn finish(self) -> (String, Vec<Value>) {
        (self.text, self.args)
    }
}
