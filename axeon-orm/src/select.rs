//! Reports: joins across any number of tables, aggregates, `GROUP BY` and
//! `HAVING`, with rows decoded into your own structs.
//!
//! ```no_run
//! use axeon_orm::select::{Agg, Select};
//! use axeon_orm::{Db, Model};
//! use serde::Deserialize;
//!
//! #[derive(Model)]
//! #[model(has_many(Product))]
//! struct Supplier { id: i64, name: String, active: bool }
//!
//! #[derive(Model)]
//! struct Category { id: i64, name: String }
//!
//! #[derive(Model)]
//! struct Product {
//!     id: i64,
//!     #[model(belongs_to = Supplier)]
//!     supplier_id: i64,
//!     #[model(belongs_to = Category)]
//!     category_id: i64,
//!     price: f64,
//! }
//!
//! #[derive(Deserialize)]
//! struct Row { supplier: String, category: String, products: i64, avg_price: Option<f64> }
//!
//! # async fn run(db: Db) -> axeon_orm::Result<()> {
//! let rows: Vec<Row> = Select::from::<Supplier>()
//!     .join(Supplier::PRODUCTS)        // suppliers → products
//!     .join(Product::CATEGORY)         // products → categories
//!     .column_as(Supplier::NAME, "supplier")
//!     .column_as(Category::NAME, "category")
//!     .aggregate(Agg::count(), "products")
//!     .aggregate(Agg::avg(Product::PRICE), "avg_price")
//!     .filter(Supplier::ACTIVE.eq(true))
//!     .group_by(Supplier::NAME)
//!     .group_by(Category::NAME)
//!     .having(Agg::count().gte(3))
//!     .order_by_alias("products", true)
//!     .limit(50)
//!     .fetch(&db)
//!     .await?;
//! # Ok(()) }
//! ```
//!
//! Each model may appear once (no self-joins); filters, columns and sorts
//! name their model through its typed columns. Tenant scopes apply to every
//! table, as in [`Query`](crate::Query).

use crate::db::{Backend, Executor, Row};
use crate::error::{Error, Result};
use crate::model::Model;
use crate::query::{Column, Expr, Order};
use crate::relation::Join;
use crate::sql::{Cond, Sort, SortTerm, Sql};
use crate::tenant::Scope;
use crate::value::{Kind, SqlType, Value};
use serde::de::DeserializeOwned;
use std::any::TypeId;

/// Table aliases; a report joins at most this many tables.
const ALIASES: [&str; 16] = ["t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7", "t8", "t9", "t10", "t11", "t12", "t13", "t14", "t15"];

/// A column of some model in the report: its model and name.
#[derive(Clone, Copy, Debug)]
struct ColumnRef {
    model: TypeId,
    model_name: &'static str,
    name: &'static str,
    kind: Kind,
}

fn column_ref<M: Model, T: SqlType>(column: Column<M, T>) -> ColumnRef {
    ColumnRef { model: TypeId::of::<M>(), model_name: M::TABLE, name: column.name(), kind: T::KIND }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Func {
    Count,
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
}

/// An aggregate for [`Select::aggregate`] and [`Select::having`].
#[derive(Clone, Debug)]
pub struct Agg {
    func: Func,
    column: Option<ColumnRef>,
}

impl Agg {
    /// `COUNT(*)`: rows in the group.
    pub fn count() -> Agg {
        Agg { func: Func::Count, column: None }
    }

    /// `COUNT(column)`: non-null values in the group (with a left join,
    /// the matched rows).
    pub fn count_of<M: Model, T: SqlType>(column: Column<M, T>) -> Agg {
        Agg { func: Func::Count, column: Some(column_ref(column)) }
    }

    /// `COUNT(DISTINCT column)`.
    pub fn count_distinct<M: Model, T: SqlType>(column: Column<M, T>) -> Agg {
        Agg { func: Func::CountDistinct, column: Some(column_ref(column)) }
    }

    /// `SUM(column)`, as `i64` for integer columns and `f64` for floats;
    /// `NULL` for an empty group.
    pub fn sum<M: Model, T: crate::Numeric>(column: Column<M, T>) -> Agg {
        Agg { func: Func::Sum, column: Some(column_ref(column)) }
    }

    /// `AVG(column)`, as `f64`.
    pub fn avg<M: Model, T: crate::Numeric>(column: Column<M, T>) -> Agg {
        Agg { func: Func::Avg, column: Some(column_ref(column)) }
    }

    /// `MIN(column)`, in the column's type.
    pub fn min<M: Model, T: SqlType>(column: Column<M, T>) -> Agg {
        Agg { func: Func::Min, column: Some(column_ref(column)) }
    }

    /// `MAX(column)`, in the column's type.
    pub fn max<M: Model, T: SqlType>(column: Column<M, T>) -> Agg {
        Agg { func: Func::Max, column: Some(column_ref(column)) }
    }

    /// The type the aggregate is read as.
    fn kind(&self) -> Kind {
        match (self.func, self.column) {
            (Func::Count | Func::CountDistinct, _) => Kind::I64,
            (Func::Avg, _) => Kind::F64,
            (Func::Sum, Some(c)) if c.kind == Kind::F64 => Kind::F64,
            (Func::Sum, _) => Kind::I64,
            (Func::Min | Func::Max, Some(c)) => c.kind,
            (Func::Min | Func::Max, None) => Kind::I64,
        }
    }

    fn compare(self, op: &'static str, value: impl Into<Value>) -> Having {
        Having { agg: self, op, value: value.into() }
    }

    /// `HAVING aggregate = value`.
    pub fn eq(self, value: impl Into<Value>) -> Having {
        self.compare("=", value)
    }

    /// `HAVING aggregate <> value`.
    pub fn ne(self, value: impl Into<Value>) -> Having {
        self.compare("<>", value)
    }

    /// `HAVING aggregate > value`.
    pub fn gt(self, value: impl Into<Value>) -> Having {
        self.compare(">", value)
    }

    /// `HAVING aggregate >= value`.
    pub fn gte(self, value: impl Into<Value>) -> Having {
        self.compare(">=", value)
    }

    /// `HAVING aggregate < value`.
    pub fn lt(self, value: impl Into<Value>) -> Having {
        self.compare("<", value)
    }

    /// `HAVING aggregate <= value`.
    pub fn lte(self, value: impl Into<Value>) -> Having {
        self.compare("<=", value)
    }
}

/// A condition on an aggregate, from [`Agg::gt`] and friends.
#[derive(Clone, Debug)]
pub struct Having {
    agg: Agg,
    op: &'static str,
    value: Value,
}

#[derive(Clone, Debug)]
struct Table {
    model: TypeId,
    table: &'static str,
    tenant: Option<&'static str>,
    /// How it joins: left join?, the table it joins onto, that table's
    /// column, this table's column.
    join: Option<(bool, usize, &'static str, &'static str)>,
}

#[derive(Clone, Debug)]
enum Item {
    Column(ColumnRef),
    Agg(Agg),
}

#[derive(Clone, Debug)]
enum OrderTerm {
    Sort(TypeId, &'static str, Sort),
    Alias(String, bool),
}

/// A report query; see the [module docs](self).
#[derive(Clone, Debug)]
pub struct Select {
    tables: Vec<Table>,
    items: Vec<(Item, String)>,
    filters: Vec<(TypeId, &'static str, Cond)>,
    group: Vec<ColumnRef>,
    having: Vec<Having>,
    order: Vec<OrderTerm>,
    limit: Option<u64>,
    offset: Option<u64>,
    all_tenants: bool,
    /// Mistakes found while building, reported when it runs.
    errors: Vec<String>,
}

impl Select {
    /// A report over model `M`'s table.
    pub fn from<M: Model>() -> Select {
        Select {
            tables: vec![Table { model: TypeId::of::<M>(), table: M::TABLE, tenant: M::TENANT, join: None }],
            items: Vec::new(),
            filters: Vec::new(),
            group: Vec::new(),
            having: Vec::new(),
            order: Vec::new(),
            limit: None,
            offset: None,
            all_tenants: false,
            errors: Vec::new(),
        }
    }

    fn index_of(&mut self, model: TypeId, name: &str) -> Option<usize> {
        let index = self.tables.iter().position(|t| t.model == model);
        if index.is_none() {
            self.errors.push(format!("`{}` isn't in this report; join it first", name));
        }
        index
    }

    fn add_join<P: Model, R: Join<P>>(mut self, relation: R, left: bool) -> Self {
        let (parent_column, column) = relation.on();
        let Some(parent) = self.index_of(TypeId::of::<P>(), P::TABLE) else { return self };
        let target = TypeId::of::<R::Target>();
        if self.tables.iter().any(|t| t.model == target) {
            self.errors.push(format!("`{}` is joined twice; a report has each model once", R::Target::TABLE));
            return self;
        }
        if self.tables.len() == ALIASES.len() {
            self.errors.push(format!("a report joins at most {} tables", ALIASES.len()));
            return self;
        }
        self.tables.push(Table {
            model: target,
            table: R::Target::TABLE,
            tenant: R::Target::TENANT,
            join: Some((left, parent, parent_column, column)),
        });
        self
    }

    /// Joins a relation of a model already in the report (`INNER JOIN`):
    /// rows without a match are left out.
    pub fn join<P: Model, R: Join<P>>(self, relation: R) -> Self {
        self.add_join(relation, false)
    }

    /// Joins a relation, keeping rows without a match (`LEFT JOIN`); the
    /// joined table's columns are then `NULL`, so read them as `Option`.
    pub fn left_join<P: Model, R: Join<P>>(self, relation: R) -> Self {
        self.add_join(relation, true)
    }

    fn add_item(mut self, item: Item, alias: &str) -> Self {
        if self.items.iter().any(|(_, existing)| existing == alias) {
            self.errors.push(format!("two report columns are named `{}`; use column_as", alias));
        }
        self.items.push((item, alias.to_string()));
        self
    }

    /// Selects a column, named after it in the result.
    pub fn column<M: Model, T: SqlType>(self, column: Column<M, T>) -> Self {
        let name = column.name();
        self.column_as(column, name)
    }

    /// Selects a column under another name, e.g. when two tables both
    /// have `name`.
    pub fn column_as<M: Model, T: SqlType>(self, column: Column<M, T>, alias: &str) -> Self {
        self.add_item(Item::Column(column_ref(column)), alias)
    }

    /// Selects an aggregate under `alias`.
    pub fn aggregate(self, agg: Agg, alias: &str) -> Self {
        self.add_item(Item::Agg(agg), alias)
    }

    /// Keeps rows matching a condition on one of the report's models
    /// (`WHERE`, before grouping).
    pub fn filter<M: Model>(mut self, expr: Expr<M>) -> Self {
        self.filters.push((TypeId::of::<M>(), M::TABLE, expr.cond));
        self
    }

    /// Groups by a column; select only grouped columns and aggregates.
    pub fn group_by<M: Model, T: SqlType>(mut self, column: Column<M, T>) -> Self {
        self.group.push(column_ref(column));
        self
    }

    /// Keeps groups matching a condition on an aggregate (`HAVING`).
    pub fn having(mut self, condition: Having) -> Self {
        self.having.push(condition);
        self
    }

    /// Sorts by a column of one of the report's models.
    pub fn order_by<M: Model>(mut self, order: Order<M>) -> Self {
        self.order.push(OrderTerm::Sort(TypeId::of::<M>(), M::TABLE, order.sort));
        self
    }

    /// Sorts by a selected column or aggregate, by its name in the result.
    pub fn order_by_alias(mut self, alias: &str, descending: bool) -> Self {
        self.order.push(OrderTerm::Alias(alias.to_string(), descending));
        self
    }

    /// Returns at most `n` rows.
    pub fn limit(mut self, n: u64) -> Self {
        self.limit = Some(n);
        self
    }

    /// Skips the first `n` rows.
    pub fn offset(mut self, n: u64) -> Self {
        self.offset = Some(n);
        self
    }

    /// Sees every tenant's rows, as [`Query::all_tenants`](crate::Query::all_tenants).
    pub fn all_tenants(mut self) -> Self {
        self.all_tenants = true;
        self
    }

    fn alias_of(&self, model: TypeId, name: &str) -> Result<&'static str> {
        self.tables
            .iter()
            .position(|t| t.model == model)
            .map(|i| ALIASES[i])
            .ok_or_else(|| Error::Config(format!("`{}` isn't in this report; join it first", name)))
    }

    fn write_column(&self, sql: &mut Sql, column: &ColumnRef) -> Result<()> {
        let alias = self.alias_of(column.model, column.model_name)?;
        sql.ident(alias).push(".").ident(column.name);
        Ok(())
    }

    fn write_agg(&self, sql: &mut Sql, agg: &Agg) -> Result<()> {
        let backend = sql.backend();
        let cast = match (agg.func, backend, agg.kind()) {
            (Func::Avg, Backend::Postgres, _) | (Func::Sum, Backend::Postgres, Kind::F64) => Some("DOUBLE PRECISION"),
            (Func::Sum, Backend::Postgres, _) => Some("BIGINT"),
            (Func::Avg, Backend::MySql, _) | (Func::Sum, Backend::MySql, Kind::F64) => Some("DOUBLE"),
            (Func::Sum, Backend::MySql, _) => Some("SIGNED"),
            (Func::Avg, Backend::Sqlite, _) | (Func::Sum, Backend::Sqlite, Kind::F64) => Some("REAL"),
            (Func::Sum, Backend::Sqlite, _) => Some("INTEGER"),
            _ => None,
        };
        if cast.is_some() {
            sql.push("CAST(");
        }
        let name = match agg.func {
            Func::Count | Func::CountDistinct => "COUNT",
            Func::Sum => "SUM",
            Func::Avg => "AVG",
            Func::Min => "MIN",
            Func::Max => "MAX",
        };
        sql.push(name).push("(");
        if agg.func == Func::CountDistinct {
            sql.push("DISTINCT ");
        }
        match &agg.column {
            Some(column) => self.write_column(sql, column)?,
            None => {
                sql.push("*");
            }
        }
        sql.push(")");
        if let Some(cast) = cast {
            sql.push(" AS ").push(cast).push(")");
        }
        Ok(())
    }

    fn build(&self, backend: Backend, scope: &Scope) -> Result<(String, Vec<Value>)> {
        if let Some(error) = self.errors.first() {
            return Err(Error::Config(error.clone()));
        }
        if self.items.is_empty() {
            return Err(Error::Config("a report needs at least one column or aggregate".into()));
        }
        let mut sql = Sql::new(backend);
        sql.push("SELECT ");
        for (i, (item, alias)) in self.items.iter().enumerate() {
            if i > 0 {
                sql.push(", ");
            }
            match item {
                Item::Column(column) => self.write_column(&mut sql, column)?,
                Item::Agg(agg) => self.write_agg(&mut sql, agg)?,
            }
            sql.push(" AS ").ident(alias);
        }
        sql.push(" FROM ").ident(self.tables[0].table).push(" AS ").ident(ALIASES[0]);
        let mut tenant_filters = Vec::new();
        for (i, table) in self.tables.iter().enumerate() {
            let tenant = scope.filter(table.tenant, table.table)?;
            let Some((left, parent, parent_column, column)) = table.join else {
                tenant_filters.extend(tenant.map(|t| (i, t)));
                continue;
            };
            sql.push(if left { " LEFT JOIN " } else { " INNER JOIN " });
            sql.ident(table.table).push(" AS ").ident(ALIASES[i]).push(" ON ");
            sql.ident(ALIASES[parent]).push(".").ident(parent_column).push(" = ");
            sql.ident(ALIASES[i]).push(".").ident(column);
            match tenant {
                // In `ON`, so a left join keeps rows without an allowed match.
                Some(filter) if left => {
                    sql.push(" AND ").cond_as(&filter, Some(ALIASES[i]));
                }
                Some(filter) => tenant_filters.push((i, filter)),
                None => {}
            }
        }
        for (model, name, cond) in &self.filters {
            let alias = self.alias_of(*model, name)?;
            let mut cond = cond.clone();
            scope_subqueries(scope, &mut cond)?;
            sql.where_clause_as(std::slice::from_ref(&cond), Some(alias));
        }
        for (i, filter) in &tenant_filters {
            sql.where_clause_as(std::slice::from_ref(filter), Some(ALIASES[*i]));
        }
        for (i, column) in self.group.iter().enumerate() {
            sql.push(if i == 0 { " GROUP BY " } else { ", " });
            self.write_column(&mut sql, column)?;
        }
        for (i, having) in self.having.iter().enumerate() {
            sql.push(if i == 0 { " HAVING " } else { " AND " });
            self.write_agg(&mut sql, &having.agg)?;
            sql.push(" ").push(having.op).push(" ").arg(having.value.clone());
        }
        for (i, term) in self.order.iter().enumerate() {
            sql.push(if i == 0 { " ORDER BY " } else { ", " });
            let descending = match term {
                OrderTerm::Sort(model, name, sort) => {
                    let alias = self.alias_of(*model, name)?;
                    match &sort.term {
                        SortTerm::Column(column) => {
                            sql.ident(alias).push(".").ident(column);
                        }
                        _ => return Err(Error::Config("reports sort by columns or by aliases".into())),
                    }
                    sort.descending
                }
                OrderTerm::Alias(alias, descending) => {
                    if !self.items.iter().any(|(_, a)| a == alias) {
                        return Err(Error::Config(format!("no report column is named `{}`", alias)));
                    }
                    sql.ident(alias);
                    *descending
                }
            };
            sql.push(if descending { " DESC" } else { " ASC" });
        }
        sql.limit_offset(self.limit, self.offset);
        Ok(sql.finish())
    }

    /// The SQL this report runs on `backend` (without tenant filters,
    /// which are added when it runs), and its parameters.
    pub fn to_sql(&self, backend: Backend) -> Result<(String, Vec<Value>)> {
        self.build(backend, &Scope::All)
    }

    async fn rows<'c>(&self, ex: impl Executor<'c>) -> Result<Vec<Row>> {
        let mut conn = ex.into_conn();
        let scope = if self.all_tenants { Scope::All } else { conn.scope().clone() };
        let (sql, args) = self.build(conn.backend(), &scope)?;
        conn.fetch_all(sql, args).await
    }

    /// Runs the report, each row as a JSON object keyed by column name,
    /// e.g. to return straight from a dashboard endpoint.
    pub async fn fetch_json<'c>(self, ex: impl Executor<'c>) -> Result<Vec<serde_json::Value>> {
        let rows = self.rows(ex).await?;
        rows.iter()
            .map(|row| {
                let mut object = serde_json::Map::with_capacity(self.items.len());
                for (item, alias) in &self.items {
                    let kind = match item {
                        Item::Column(column) => column.kind,
                        Item::Agg(agg) => agg.kind(),
                    };
                    object.insert(alias.clone(), to_json(row.get_value(alias, kind)?));
                }
                Ok(serde_json::Value::Object(object))
            })
            .collect()
    }

    /// Runs the report, decoding each row into `T` by column name: a struct
    /// with `#[derive(Deserialize)]`, with `Option` fields for columns that
    /// can be `NULL` (left joins, `SUM`/`AVG`/`MIN`/`MAX` of empty groups).
    pub async fn fetch<'c, T: DeserializeOwned>(self, ex: impl Executor<'c>) -> Result<Vec<T>> {
        self.fetch_json(ex)
            .await?
            .into_iter()
            .map(|row| serde_json::from_value(row).map_err(|e| Error::decode("report row", e)))
            .collect()
    }
}

/// Filters subqueries in a condition (from `has` / `where_has`) by tenant.
fn scope_subqueries(scope: &Scope, cond: &mut Cond) -> Result<()> {
    let scoped = scope.apply_to(std::slice::from_ref(cond))?;
    *cond = scoped.into_iter().next().expect("one condition in, one out");
    Ok(())
}

/// A column value as JSON, as serde expects it for the matching Rust type.
fn to_json(value: Value) -> serde_json::Value {
    use serde_json::Value as Json;
    match value {
        Value::Null(_) => Json::Null,
        Value::Bool(v) => Json::from(v),
        Value::I32(v) => Json::from(v),
        Value::I64(v) => Json::from(v),
        Value::F64(v) => serde_json::Number::from_f64(v).map(Json::Number).unwrap_or(Json::Null),
        Value::Text(v) => Json::from(v),
        Value::Bytes(v) => Json::from(v),
        #[cfg(feature = "chrono")]
        Value::DateTime(v) => Json::from(v.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)),
        #[cfg(feature = "chrono")]
        Value::NaiveDateTime(v) => Json::from(v.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
        #[cfg(feature = "chrono")]
        Value::Date(v) => Json::from(v.to_string()),
        #[cfg(feature = "uuid")]
        Value::Uuid(v) => Json::from(v.to_string()),
        #[cfg(feature = "json")]
        Value::Json(v) => v,
        #[cfg(feature = "pgvector")]
        Value::Vector(v) => Json::from(v.as_slice().to_vec()),
    }
}
