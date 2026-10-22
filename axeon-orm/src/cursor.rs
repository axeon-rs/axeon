//! Keyset (cursor) pagination: the cursor holds the sort-key values of the
//! last row on a page, and the next page is the rows after them.

use crate::db::Backend;
use crate::error::{Error, Result};
use crate::model::Model;
use crate::sql::{Cond, Fragment, Sort, SortTerm};
use crate::value::{Kind, Value};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Serialize;
use serde_json::Value as Json;

/// One page from [`Query::cursor_paginate`](crate::Query::cursor_paginate).
#[derive(Clone, Debug, Serialize)]
pub struct CursorPage<T> {
    /// The rows on this page.
    pub items: Vec<T>,
    /// Pass this as `after` for the next page; `None` on the last page.
    pub next_cursor: Option<String>,
}

impl<T> CursorPage<T> {
    /// Whether there is a page after this one.
    pub fn has_more(&self) -> bool {
        self.next_cursor.is_some()
    }

    /// Converts the rows, e.g. into response DTOs, keeping the cursor.
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> CursorPage<U> {
        CursorPage { items: self.items.into_iter().map(f).collect(), next_cursor: self.next_cursor }
    }
}

/// A sort key column and whether it sorts descending.
pub(crate) type Key = (&'static str, bool);

/// The query's sort keys, which must be plain non-nullable columns, with
/// the primary key appended as a tie-breaker so every row has a distinct
/// position.
pub(crate) fn keys<M: Model>(order: &[Sort]) -> Result<Vec<Key>> {
    let mut keys = Vec::with_capacity(order.len() + 1);
    for sort in order {
        let SortTerm::Column(column) = &sort.term else {
            return Err(Error::Config("cursor pagination needs order_by on plain columns".into()));
        };
        let def = M::COLUMNS.iter().find(|c| c.name == *column);
        if def.is_some_and(|c| c.nullable) {
            return Err(Error::Config(format!(
                "cursor pagination can't order by nullable column `{}`",
                column
            )));
        }
        keys.push((*column, sort.descending));
    }
    let descending = keys.last().is_some_and(|(_, d)| *d);
    for key in M::PRIMARY_KEYS {
        if !keys.iter().any(|(c, _)| c == key) {
            keys.push((key, descending));
        }
    }
    Ok(keys)
}

/// Identifies the sort order a cursor was made for, so a cursor from
/// another order is rejected instead of skipping or repeating rows.
fn fingerprint(keys: &[Key]) -> String {
    keys.iter().map(|(c, d)| format!("{}:{}", c, if *d { 'd' } else { 'a' })).collect::<Vec<_>>().join(",")
}

/// The cursor pointing just after `row`.
pub(crate) fn encode<M: Model>(row: &M, keys: &[Key]) -> Result<String> {
    let values = keys
        .iter()
        .map(|(column, _)| {
            let value = row.get(column).ok_or_else(|| Error::Config(format!("no column `{}`", column)))?;
            to_json(column, value)
        })
        .collect::<Result<Vec<_>>>()?;
    let doc = serde_json::json!({ "k": fingerprint(keys), "v": values });
    Ok(URL_SAFE_NO_PAD.encode(doc.to_string()))
}

/// The sort-key values in `cursor`, checked against `keys`.
pub(crate) fn decode<M: Model>(cursor: &str, keys: &[Key]) -> Result<Vec<Value>> {
    let invalid = || Error::InvalidCursor("malformed cursor".into());
    let bytes = URL_SAFE_NO_PAD.decode(cursor.trim()).map_err(|_| invalid())?;
    let doc: Json = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if doc.get("k").and_then(Json::as_str) != Some(fingerprint(keys).as_str()) {
        return Err(Error::InvalidCursor("cursor was made for a different sort order".into()));
    }
    let values = doc.get("v").and_then(Json::as_array).ok_or_else(invalid)?;
    if values.len() != keys.len() {
        return Err(invalid());
    }
    keys.iter()
        .zip(values)
        .map(|((column, _), json)| {
            let kind = M::COLUMNS.iter().find(|c| c.name == *column).map(|c| c.kind).ok_or_else(invalid)?;
            from_json(kind, json).ok_or_else(invalid)
        })
        .collect()
}

/// The condition selecting rows after the cursor's position.
///
/// With one sort direction, PostgreSQL and SQLite get a row comparison,
/// `(a, b) > ($1, $2)`, which an index on `(a, b)` serves directly. Mixed
/// directions (and MySQL, whose planner handles row comparisons poorly)
/// get `a >= $1 AND (a > $1 OR (a = $1 AND b > $2))`; the leading `>=`
/// lets an index on `a` narrow the scan.
pub(crate) fn after(backend: Backend, keys: &[Key], values: Vec<Value>) -> Cond {
    let op = |descending: bool| if descending { "<" } else { ">" };
    let uniform = keys.iter().all(|(_, d)| *d == keys[0].1);
    if uniform && backend != Backend::MySql && keys.len() > 1 {
        let columns = keys.iter().map(|(c, _)| crate::sql::ident(backend, c)).collect::<Vec<_>>();
        let marks = vec!["?"; keys.len()].join(", ");
        let sql = format!("({}) {} ({})", columns.join(", "), op(keys[0].1), marks);
        return Cond::Raw(Fragment::new(&sql, values));
    }
    let mut branches = Vec::with_capacity(keys.len());
    for i in 0..keys.len() {
        let mut terms: Vec<Cond> = (0..i).map(|j| Cond::Compare(keys[j].0, "=", values[j].clone())).collect();
        terms.push(Cond::Compare(keys[i].0, op(keys[i].1), values[i].clone()));
        branches.push(Cond::And(terms));
    }
    let first = Cond::Compare(keys[0].0, if keys[0].1 { "<=" } else { ">=" }, values[0].clone());
    Cond::And(vec![first, Cond::Or(branches)])
}

fn to_json(column: &str, value: Value) -> Result<Json> {
    Ok(match value {
        Value::Null(_) => {
            return Err(Error::Config(format!("cursor pagination: `{}` is NULL in a row", column)));
        }
        Value::Bool(v) => Json::from(v),
        Value::I32(v) => Json::from(v),
        Value::I64(v) => Json::from(v),
        Value::F64(v) => Json::from(v),
        Value::Text(v) => Json::from(v),
        Value::Bytes(v) => Json::from(URL_SAFE_NO_PAD.encode(v)),
        #[cfg(feature = "chrono")]
        Value::DateTime(v) => Json::from(v.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)),
        #[cfg(feature = "chrono")]
        Value::NaiveDateTime(v) => Json::from(v.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
        #[cfg(feature = "chrono")]
        Value::Date(v) => Json::from(v.to_string()),
        #[cfg(feature = "uuid")]
        Value::Uuid(v) => Json::from(v.to_string()),
        #[allow(unreachable_patterns)]
        _ => return Err(Error::Config(format!("cursor pagination can't order by `{}`", column))),
    })
}

fn from_json(kind: Kind, json: &Json) -> Option<Value> {
    Some(match kind {
        Kind::Bool => Value::Bool(json.as_bool()?),
        Kind::I32 => Value::I32(i32::try_from(json.as_i64()?).ok()?),
        Kind::I64 => Value::I64(json.as_i64()?),
        Kind::F64 => Value::F64(json.as_f64()?),
        Kind::Text => Value::Text(json.as_str()?.to_string()),
        Kind::Bytes => Value::Bytes(URL_SAFE_NO_PAD.decode(json.as_str()?).ok()?),
        #[cfg(feature = "chrono")]
        Kind::DateTime => {
            Value::DateTime(chrono::DateTime::parse_from_rfc3339(json.as_str()?).ok()?.with_timezone(&chrono::Utc))
        }
        #[cfg(feature = "chrono")]
        Kind::NaiveDateTime => {
            Value::NaiveDateTime(chrono::NaiveDateTime::parse_from_str(json.as_str()?, "%Y-%m-%dT%H:%M:%S%.f").ok()?)
        }
        #[cfg(feature = "chrono")]
        Kind::Date => Value::Date(json.as_str()?.parse().ok()?),
        #[cfg(feature = "uuid")]
        Kind::Uuid => Value::Uuid(json.as_str()?.parse().ok()?),
        #[allow(unreachable_patterns)]
        _ => return None,
    })
}
