//! Numbers the ranking step computes over the whole symbol graph and stores
//! on the symbols: rank, community and degree.
//!
//! They live on the Symbol row, so they are gone when a file is re-indexed
//! (its symbols are new rows that start at "not computed") and untouched by
//! `replace_edges`, which only rewrites edges and derived nodes.

use crate::nodes::{distinct, ID_CHUNK};
use crate::search::{hit, HIT_COLS};
use crate::values::{col_f, col_i, col_s, int, rows, st, strings, Prepared};
use anyhow::Result;
use lbug::{Connection, LogicalType, Value};
use lge_core::{Hit, SymbolMetrics};
use std::collections::{HashMap, HashSet};

/// One metrics row as a struct, so a whole chunk travels as one list parameter.
fn metrics_row(m: &SymbolMetrics) -> Value {
    Value::Struct(vec![
        ("id".to_string(), st(&m.id)),
        ("rank".to_string(), Value::Double(m.rank)),
        ("community".to_string(), int(m.community)),
        ("degree".to_string(), int(m.degree)),
    ])
}

fn metrics_row_type() -> LogicalType {
    LogicalType::Struct {
        fields: vec![
            ("id".to_string(), LogicalType::String),
            ("rank".to_string(), LogicalType::Double),
            ("community".to_string(), LogicalType::Int64),
            ("degree".to_string(), LogicalType::Int64),
        ],
    }
}

/// Store rank, community and degree on the listed symbols. Must run inside a
/// transaction, so the metrics of one ranking run are stored together.
///
/// Unknown ids and placeholders are skipped. Symbols that are not listed are
/// left as they are; one that never was listed still has rank 0, community -1
/// and degree 0. Of two rows for the same id the later one counts.
pub(crate) fn set_metrics(conn: &Connection, metrics: &[SymbolMetrics]) -> Result<()> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut last: Vec<&SymbolMetrics> = metrics.iter().rev().filter(|m| seen.insert(m.id.as_str())).collect();
    last.reverse();
    if last.is_empty() {
        return Ok(());
    }
    // One statement per chunk: the rows are joined to the symbols through the
    // primary-key index and updated in one pass.
    let mut stmt = Prepared::new(
        conn,
        "UNWIND $rows AS r MATCH (s:Symbol {id:r.id}) WHERE s.kind <> 'unresolved' \
         SET s.rank = r.rank, s.community = r.community, s.degree = r.degree;",
    )?;
    for chunk in last.chunks(ID_CHUNK) {
        let list = Value::List(metrics_row_type(), chunk.iter().map(|m| metrics_row(m)).collect());
        stmt.run(vec![("rows", list)])?;
    }
    Ok(())
}

/// Stored metrics of the given symbols, in the order they were asked for.
/// Unknown ids and placeholders are skipped.
pub(crate) fn metrics(conn: &Connection, ids: &[String]) -> Result<Vec<SymbolMetrics>> {
    let ids = distinct(ids);
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = Prepared::new(
        conn,
        "UNWIND $ids AS x MATCH (s:Symbol {id:x}) WHERE s.kind <> 'unresolved' \
         RETURN s.id, s.rank, s.community, s.degree;",
    )?;
    let mut found: HashMap<String, SymbolMetrics> = HashMap::new();
    for chunk in ids.chunks(ID_CHUNK) {
        for r in stmt.rows(vec![("ids", strings(chunk))])? {
            let m = SymbolMetrics {
                id: col_s(&r, 0),
                rank: col_f(&r, 1),
                community: col_i(&r, 2),
                degree: col_i(&r, 3),
            };
            found.insert(m.id.clone(), m);
        }
    }
    Ok(ids.iter().filter_map(|id| found.remove(*id)).collect())
}

/// Symbols by stored rank, highest first and then by qualified name,
/// optionally within one service or one community. Modules, tests and
/// placeholders are left out. `score` is the rank.
pub(crate) fn top_ranked(
    conn: &Connection,
    service: Option<&str>,
    community: Option<i64>,
    limit: usize,
) -> Result<Vec<Hit>> {
    let mut conditions = vec!["s.kind <> 'module'", "s.kind <> 'unresolved'", "s.is_test = false"];
    let mut params = Vec::new();
    if let Some(service) = service {
        conditions.push("s.service = $service");
        params.push(("service", st(service)));
    }
    if let Some(community) = community {
        conditions.push("s.community = $community");
        params.push(("community", int(community)));
    }
    let q = format!(
        "MATCH (s:Symbol) WHERE {} RETURN {HIT_COLS}, s.rank \
         ORDER BY s.rank DESC, s.qualified_name, s.id LIMIT {limit};",
        conditions.join(" AND ")
    );
    Ok(rows(conn, &q, params)?.iter().map(|r| hit(r, col_f(r, 9))).collect())
}
