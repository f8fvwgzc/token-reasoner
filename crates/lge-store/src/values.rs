//! Conversions between `lbug::Value` and plain Rust values, and the two small
//! query helpers every module uses.
//!
//! Reading never panics: a missing column or an unexpected type gives the
//! empty value, so a row from an older or damaged database cannot bring the
//! process down.

use anyhow::{Context, Result};
use lbug::{Connection, LogicalType, PreparedStatement, Value};

pub(crate) type Row = Vec<Value>;

pub(crate) fn vs(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null(_) => String::new(),
        o => o.to_string(),
    }
}

pub(crate) fn vi(v: &Value) -> i64 {
    match v {
        Value::Int64(i) => *i,
        Value::Int32(i) => i64::from(*i),
        Value::Int16(i) => i64::from(*i),
        Value::Int8(i) => i64::from(*i),
        Value::UInt32(i) => i64::from(*i),
        Value::UInt16(i) => i64::from(*i),
        Value::UInt8(i) => i64::from(*i),
        Value::UInt64(i) => i64::try_from(*i).unwrap_or(i64::MAX),
        _ => 0,
    }
}

pub(crate) fn vb(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

pub(crate) fn vf(v: &Value) -> f64 {
    match v {
        Value::Double(f) => *f,
        Value::Float(f) => f64::from(*f),
        o => vi(o) as f64,
    }
}

/// A `STRING[]` column as a vector. Anything else reads as empty.
pub(crate) fn vlist(v: &Value) -> Vec<String> {
    match v {
        Value::List(_, items) | Value::Array(_, items) => items.iter().map(vs).collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn col_s(row: &[Value], i: usize) -> String {
    row.get(i).map(vs).unwrap_or_default()
}

pub(crate) fn col_i(row: &[Value], i: usize) -> i64 {
    row.get(i).map(vi).unwrap_or(0)
}

pub(crate) fn col_b(row: &[Value], i: usize) -> bool {
    row.get(i).map(vb).unwrap_or(false)
}

pub(crate) fn col_f(row: &[Value], i: usize) -> f64 {
    row.get(i).map(vf).unwrap_or(0.0)
}

pub(crate) fn col_list(row: &[Value], i: usize) -> Vec<String> {
    row.get(i).map(vlist).unwrap_or_default()
}

pub(crate) fn st(s: &str) -> Value {
    Value::String(s.to_string())
}

pub(crate) fn int(i: i64) -> Value {
    Value::Int64(i)
}

/// A `STRING[]` parameter. The element type is passed explicitly so that an
/// empty list still binds as a list of strings.
pub(crate) fn strings<S: AsRef<str>>(items: &[S]) -> Value {
    Value::List(LogicalType::String, items.iter().map(|s| st(s.as_ref())).collect())
}

/// Run a statement without parameters and drop its result.
pub(crate) fn run(conn: &Connection, q: &str) -> Result<()> {
    conn.query(q).with_context(|| format!("query failed: {q}"))?;
    Ok(())
}

/// Run a statement with parameters and collect its rows.
pub(crate) fn rows(conn: &Connection, q: &str, params: Vec<(&str, Value)>) -> Result<Vec<Row>> {
    Prepared::new(conn, q)?.rows(params)
}

/// First column of the first row as an integer; 0 when there is no row.
pub(crate) fn count(conn: &Connection, q: &str, params: Vec<(&str, Value)>) -> Result<i64> {
    Ok(rows(conn, q, params)?.first().map(|r| col_i(r, 0)).unwrap_or(0))
}

/// A prepared statement bound to its connection, so a loop can run it many
/// times without planning it again.
pub(crate) struct Prepared<'c, 'db> {
    conn: &'c Connection<'db>,
    stmt: PreparedStatement,
    /// Kept for error messages.
    query: String,
}

impl<'c, 'db> Prepared<'c, 'db> {
    pub(crate) fn new(conn: &'c Connection<'db>, query: &str) -> Result<Self> {
        let stmt = conn
            .prepare(query)
            .with_context(|| format!("prepare failed: {query}"))?;
        Ok(Prepared {
            conn,
            stmt,
            query: query.to_string(),
        })
    }

    pub(crate) fn rows(&mut self, params: Vec<(&str, Value)>) -> Result<Vec<Row>> {
        let result = self
            .conn
            .execute(&mut self.stmt, params)
            .with_context(|| format!("query failed: {}", self.query))?;
        Ok(result.collect())
    }

    pub(crate) fn run(&mut self, params: Vec<(&str, Value)>) -> Result<()> {
        self.conn
            .execute(&mut self.stmt, params)
            .with_context(|| format!("query failed: {}", self.query))?;
        Ok(())
    }
}
