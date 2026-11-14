//! One-hop expansion and the reverse call traversal.

use crate::nodes::{distinct, ID_CHUNK};
use crate::search::{hit, HIT_COLS};
use crate::values::{col_f, col_i, col_s, count, rows, st, strings, Prepared};
use anyhow::Result;
use lbug::Connection;
use lge_core::{method, Direction, EdgeRec, EdgeType, Hit};
use std::collections::BTreeSet;

/// One hop: all edges of the given types that start at (`Out`) or end at
/// (`In`) any of `ids`. An empty `types` means every type except CONTAINS.
///
/// Each id is looked up through the primary-key index of its table, so the
/// cost follows the number of ids and their edges, not the size of the graph.
pub(crate) fn edges(conn: &Connection, ids: &[String], types: &[EdgeType], dir: Direction) -> Result<Vec<EdgeRec>> {
    let ids = distinct(ids);
    let wanted: BTreeSet<EdgeType> = if types.is_empty() {
        EdgeType::ALL.into_iter().filter(|t| *t != EdgeType::Contains).collect()
    } else {
        types.iter().copied().collect()
    };
    let mut out = Vec::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let pattern = |rel: &str| match dir {
        Direction::Out => format!("UNWIND $ids AS x MATCH (a {{id:x}})-[r:{rel}]->(b)"),
        Direction::In => format!("UNWIND $ids AS x MATCH (b {{id:x}})<-[r:{rel}]-(a)"),
    };

    let with_provenance: Vec<&str> = wanted
        .iter()
        .filter(|t| **t != EdgeType::Contains)
        .map(|t| t.as_str())
        .collect();
    if !with_provenance.is_empty() {
        let q = format!(
            "{} RETURN label(r), a.id, b.id, r.confidence, r.method, r.evidence_file, r.start_line, r.end_line;",
            pattern(&with_provenance.join("|"))
        );
        let mut stmt = Prepared::new(conn, &q)?;
        for chunk in ids.chunks(ID_CHUNK) {
            for r in stmt.rows(vec![("ids", strings(chunk))])? {
                // The label always parses: the pattern only names known tables.
                let Some(edge_type) = EdgeType::parse(&col_s(&r, 0)) else {
                    continue;
                };
                out.push(EdgeRec {
                    from: col_s(&r, 1),
                    to: col_s(&r, 2),
                    edge_type,
                    confidence: col_f(&r, 3),
                    method: col_s(&r, 4),
                    evidence_file: col_s(&r, 5),
                    start_line: col_i(&r, 6),
                    end_line: col_i(&r, 7),
                });
            }
        }
    }

    if wanted.contains(&EdgeType::Contains) {
        // CONTAINS has no provenance columns: it is structure the indexer saw,
        // not a resolved reference. It is reported as certain, and the place of
        // the contained file or symbol stands in as its evidence.
        let q = format!(
            "{} RETURN a.id, b.id, b.file_path, b.path, b.start_line, b.end_line;",
            pattern(EdgeType::Contains.as_str())
        );
        let mut stmt = Prepared::new(conn, &q)?;
        for chunk in ids.chunks(ID_CHUNK) {
            for r in stmt.rows(vec![("ids", strings(chunk))])? {
                let (symbol_file, file) = (col_s(&r, 2), col_s(&r, 3));
                out.push(EdgeRec {
                    from: col_s(&r, 0),
                    to: col_s(&r, 1),
                    edge_type: EdgeType::Contains,
                    confidence: 1.0,
                    method: method::STATIC.to_string(),
                    evidence_file: if symbol_file.is_empty() { file } else { symbol_file },
                    start_line: col_i(&r, 4),
                    end_line: col_i(&r, 5),
                });
            }
        }
    }

    out.sort_by(|a, b| (a.edge_type, &a.from, &a.to, a.start_line).cmp(&(b.edge_type, &b.from, &b.to, b.start_line)));
    Ok(out)
}

/// Reverse CALLS traversal: every symbol that reaches `id` within `depth` hops
/// (1..=6). `score` is the lowest hop count.
///
/// The match is variable-length with shortest-path semantics, so the database
/// walks breadth-first from `id` and reports each caller once with its lowest
/// hop count, instead of enumerating every walk of every length.
pub(crate) fn callers(conn: &Connection, id: &str, depth: u8) -> Result<Vec<Hit>> {
    let depth = depth.clamp(1, 6);
    let q = format!(
        "MATCH p = (s:Symbol)-[:CALLS* SHORTEST 1..{depth}]->(t:Symbol) \
         WHERE t.id = $id AND s.kind <> 'unresolved' RETURN {HIT_COLS}, length(p);"
    );
    let mut hits: Vec<Hit> = rows(conn, &q, vec![("id", st(id))])?
        .iter()
        .map(|r| hit(r, col_i(r, 9) as f64))
        .filter(|h| h.id != id)
        .collect();
    hits.sort_by(|a, b| {
        a.score
            .total_cmp(&b.score)
            .then(a.qualified_name.cmp(&b.qualified_name))
    });
    Ok(hits)
}

/// Number of unresolved edges of any type leaving a symbol.
pub(crate) fn unresolved_out(conn: &Connection, id: &str) -> Result<i64> {
    // CONTAINS has no `method`; it reads as null there and never matches.
    count(
        conn,
        "MATCH (s:Symbol {id:$id})-[r]->() WHERE r.method = $m RETURN count(r);",
        vec![("id", st(id)), ("m", st(method::UNRESOLVED))],
    )
}
