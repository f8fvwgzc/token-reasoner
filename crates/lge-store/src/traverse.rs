//! One-hop expansion and the traversals that run inside the database: the
//! reverse call traversal, bounded reach, and the shortest path.

use crate::nodes::{by_ids, distinct, ID_CHUNK};
use crate::schema::rel_pairs;
use crate::search::{hit, HIT_COLS};
use crate::values::{col_f, col_i, col_list, col_s, count, rows, run, st, strings, Prepared};
use anyhow::{bail, Result};
use lbug::Connection;
use lge_core::{kind, method, Direction, EdgeRec, EdgeType, Hit, NodeLabel, NodeRec};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};

/// Deepest traversal [`callers`] and [`reach`] run.
const MAX_REACH: u8 = 6;

/// Longest path [`shortest_path`] looks for: LadybugDB does not plan a
/// recursive match beyond 30 hops.
const MAX_PATH: u8 = 30;

/// The edge types a traversal follows: the given ones, or every type except
/// CONTAINS when none is given.
fn wanted(types: &[EdgeType]) -> BTreeSet<EdgeType> {
    if types.is_empty() {
        EdgeType::ALL.into_iter().filter(|t| *t != EdgeType::Contains).collect()
    } else {
        types.iter().copied().collect()
    }
}

/// The relationship tables of a pattern, `A|B|C`.
fn rel_names(types: &BTreeSet<EdgeType>) -> String {
    let names: Vec<&str> = types.iter().map(|t| t.as_str()).collect();
    names.join("|")
}

/// One hop: all edges of the given types that start at (`Out`) or end at
/// (`In`) any of `ids`. An empty `types` means every type except CONTAINS.
///
/// Each id is looked up through the primary-key index of its table, so the
/// cost follows the number of ids and their edges, not the size of the graph.
pub(crate) fn edges(conn: &Connection, ids: &[String], types: &[EdgeType], dir: Direction) -> Result<Vec<EdgeRec>> {
    let ids = distinct(ids);
    let wanted = wanted(types);
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
    let depth = depth.clamp(1, MAX_REACH);
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

/// Nodes of any label reachable from `ids` over `types` in one direction
/// within `depth` hops (1..=6), each with its lowest hop count, nearest first
/// and then by id, at most `limit`. An empty `types` means every type except
/// CONTAINS. The start nodes and placeholders are left out.
///
/// One recursive match per chunk of ids does the walk inside the database:
/// with shortest-path semantics it is a breadth-first search from each start
/// node, and the lowest hop count over all of them is taken there too.
///
/// The form of the query matters to LadybugDB 0.22.3. The start nodes are
/// named by `WHERE s.id IN $ids`, which is pushed into the scan the search
/// starts from; `UNWIND $ids AS x MATCH (s {id:x})...` searches from every
/// node of the graph and joins afterwards. The placeholder filter comes after
/// the aggregation, because a condition on the far end of the pattern makes
/// the planner start the search there.
pub(crate) fn reach(
    conn: &Connection,
    ids: &[String],
    types: &[EdgeType],
    dir: Direction,
    depth: u8,
    limit: usize,
) -> Result<Vec<(NodeRec, u8)>> {
    let ids = distinct(ids);
    if ids.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let depth = depth.clamp(1, MAX_REACH);
    let rel = rel_names(&wanted(types));
    let pattern = match dir {
        Direction::Out => format!("(s)-[:{rel}* SHORTEST 1..{depth}]->(t)"),
        Direction::In => format!("(s)<-[:{rel}* SHORTEST 1..{depth}]-(t)"),
    };
    // Start nodes are only removed below, where all of them are known, so a
    // chunk is asked for enough rows to lose every one of them and still fill
    // the limit. A node among the nearest overall is among the nearest of the
    // chunk that reaches it soonest, so nothing is missed by cutting here.
    let keep = limit.saturating_add(ids.len());
    // `kind` marks a placeholder Symbol, `method` a placeholder ApiEndpoint;
    // the other labels have neither column and read null.
    let q = format!(
        "MATCH p = {pattern} WHERE s.id IN $ids \
         WITH t.id AS id, t.kind AS kind, t.method AS method, min(length(p)) AS hops \
         WHERE (kind IS NULL OR kind <> $placeholder) AND (method IS NULL OR method <> $placeholder) \
         RETURN id, hops ORDER BY hops, id LIMIT {keep};"
    );
    let mut stmt = Prepared::new(conn, &q)?;
    let mut hops: HashMap<String, u8> = HashMap::new();
    for chunk in ids.chunks(ID_CHUNK) {
        for r in stmt.rows(vec![("ids", strings(chunk)), ("placeholder", st(kind::UNRESOLVED))])? {
            let found = u8::try_from(col_i(&r, 1)).unwrap_or(u8::MAX);
            let lowest = hops.entry(col_s(&r, 0)).or_insert(found);
            *lowest = found.min(*lowest);
        }
    }

    let start: HashSet<&str> = ids.iter().copied().collect();
    let mut nearest: Vec<(String, u8)> = hops
        .into_iter()
        .filter(|(id, _)| !start.contains(id.as_str()))
        .collect();
    nearest.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    nearest.truncate(limit);

    let wanted_ids: Vec<String> = nearest.iter().map(|(id, _)| id.clone()).collect();
    let mut nodes: HashMap<String, NodeRec> = by_ids(conn, &wanted_ids)?
        .into_iter()
        .map(|n| (n.id.clone(), n))
        .collect();
    // A node deleted between the two reads is simply not reported.
    Ok(nearest
        .into_iter()
        .filter_map(|(id, hops)| nodes.remove(&id).map(|n| (n, hops)))
        .collect())
}

/// One shortest path from `from` to `to` that follows `types` along their
/// direction, at most `max_depth` hops (1..=30), as its edges in order with
/// their provenance. `None` when there is no such path, and when `from` is
/// `to`: the search never returns to where it started. An empty `types` means
/// every type except CONTAINS.
///
/// The recursive match finds the nodes of the path; the edges are then read
/// by [`edges`], so a path edge is exactly the record a one-hop expansion
/// gives. Where two nodes of the path are joined by several of the wanted
/// edges, the most certain one is reported. Which of several equally short
/// paths is found is the database's choice.
pub(crate) fn shortest_path(
    conn: &Connection,
    from: &str,
    to: &str,
    types: &[EdgeType],
    max_depth: u8,
) -> Result<Option<Vec<EdgeRec>>> {
    let depth = max_depth.clamp(1, MAX_PATH);
    let wanted = wanted(types);
    let q = format!(
        "MATCH p = (a {{id:$from}})-[:{}* SHORTEST 1..{depth}]->(b {{id:$to}}) \
         RETURN properties(nodes(p), 'id') LIMIT 1;",
        rel_names(&wanted)
    );
    // Both reads see one state of the graph: the edges of the path that was
    // found are still there when they are read.
    run(conn, "BEGIN TRANSACTION READ ONLY;")?;
    let found = rows(conn, &q, vec![("from", st(from)), ("to", st(to))]).and_then(|paths| {
        let Some(path) = paths.first().map(|r| col_list(r, 0)) else {
            return Ok(None);
        };
        let types: Vec<EdgeType> = wanted.iter().copied().collect();
        let hops = edges(conn, &path, &types, Direction::Out)?;
        path_edges(&path, hops).map(Some)
    });
    // Nothing was written; either way of ending the transaction releases it.
    let _ = conn.query("COMMIT;");
    found
}

/// The edge of each step of a path through `nodes`, picked from the outgoing
/// edges of those nodes.
fn path_edges(nodes: &[String], candidates: Vec<EdgeRec>) -> Result<Vec<EdgeRec>> {
    let mut out = Vec::with_capacity(nodes.len().saturating_sub(1));
    for step in nodes.windows(2) {
        let [from, to] = step else { continue };
        let best = candidates
            .iter()
            .filter(|e| &e.from == from && &e.to == to)
            .min_by(|a, b| strongest_first(a, b));
        match best {
            Some(e) => out.push(e.clone()),
            None => bail!("the path from {from} to {to} has no stored edge"),
        }
    }
    Ok(out)
}

/// Order of the edges between the same two nodes: the most certain first,
/// then by type and line so that the choice is always the same.
fn strongest_first(a: &EdgeRec, b: &EdgeRec) -> Ordering {
    b.confidence
        .total_cmp(&a.confidence)
        .then((a.edge_type, a.start_line, a.end_line).cmp(&(b.edge_type, b.start_line, b.end_line)))
        .then(a.evidence_file.cmp(&b.evidence_file))
}

/// Every stored edge between two Symbols that is not CONTAINS and not
/// unresolved, as (from, to, type, confidence), in a fixed order. This is the
/// graph the ranking step works on, so placeholders are not part of it.
pub(crate) fn symbol_graph(conn: &Connection) -> Result<Vec<(String, String, EdgeType, f64)>> {
    let between_symbols: BTreeSet<EdgeType> = EdgeType::ALL
        .into_iter()
        .filter(|t| rel_pairs(*t) == [(NodeLabel::Symbol, NodeLabel::Symbol)])
        .collect();
    let q = format!(
        "MATCH (a:Symbol)-[r:{}]->(b:Symbol) \
         WHERE r.method <> $unresolved AND a.kind <> $placeholder AND b.kind <> $placeholder \
         RETURN a.id, b.id, label(r), r.confidence;",
        rel_names(&between_symbols)
    );
    let params = vec![
        ("unresolved", st(method::UNRESOLVED)),
        ("placeholder", st(kind::UNRESOLVED)),
    ];
    let mut out: Vec<(String, String, EdgeType, f64)> = rows(conn, &q, params)?
        .iter()
        .filter_map(|r| Some((col_s(r, 0), col_s(r, 1), EdgeType::parse(&col_s(r, 2))?, col_f(r, 3))))
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)).then(a.3.total_cmp(&b.3)));
    Ok(out)
}
