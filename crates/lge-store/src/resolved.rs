//! Writing the resolver's output: derived nodes and every edge with its provenance.

use crate::schema::{quoted, rel_pairs};
use crate::values::{col_s, int, rows, run, st, Prepared};
use anyhow::{bail, Context, Result};
use lbug::{Connection, Value};
use lge_core::{kind, method, EdgeRec, EdgeType, NodeLabel, NodeRec, Resolved};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// From this many edges on, a bulk load (`COPY FROM`) pays for its files.
/// One statement per edge costs about 45 microseconds; a bulk load writes
/// 150,000 edges in a third of a second.
pub(crate) const BULK_EDGES: usize = 2_000;

/// How the edges are written.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum EdgeLoad {
    /// `COPY FROM` once there are [`BULK_EDGES`] edges, statements below that.
    Bulk,
    /// One statement per edge, whatever their number.
    Statements,
}

/// Replace all derived data with `resolved`. Must run inside a transaction,
/// so a failure leaves the previous graph in place.
///
/// Returns the number of edges that were not stored because an endpoint does
/// not exist (or because they were CONTAINS edges, which the indexer owns).
pub(crate) fn replace(conn: &Connection, resolved: &Resolved, commit: &str, now: i64, load: EdgeLoad) -> Result<usize> {
    check(resolved)?;
    clear(conn)?;
    write_nodes(conn, &resolved.nodes)?;
    let edges = storable(conn, &resolved.edges)?;
    if load == EdgeLoad::Bulk && edges.len() >= BULK_EDGES {
        copy_edges(conn, &edges, commit, now)?;
    } else {
        insert_edges(conn, &edges, commit, now)?;
    }
    Ok(resolved.edges.len() - edges.len())
}

/// Refuse input that would break a rule of the graph before anything is deleted.
fn check(resolved: &Resolved) -> Result<()> {
    for n in &resolved.nodes {
        let derived = match n.label {
            NodeLabel::ApiEndpoint | NodeLabel::DbTable | NodeLabel::Queue => true,
            NodeLabel::Symbol => n.kind == kind::UNRESOLVED,
            NodeLabel::Repository | NodeLabel::Service | NodeLabel::File => false,
        };
        if !derived {
            bail!(
                "{} node {} ({}) is not a derived node; only ApiEndpoint, DbTable, Queue and placeholder \
                 Symbols are written by replace_edges",
                n.label.as_str(),
                n.id,
                n.name
            );
        }
    }
    // Every edge keeps its provenance; an edge without a known method has none.
    for e in &resolved.edges {
        if ![method::STATIC, method::INFERRED, method::UNRESOLVED].contains(&e.method.as_str()) {
            bail!(
                "{} edge {} -> {} has unknown method '{}'",
                e.edge_type,
                e.from,
                e.to,
                e.method
            );
        }
    }
    Ok(())
}

/// Delete every non-CONTAINS edge and every derived node.
fn clear(conn: &Connection) -> Result<()> {
    for t in EdgeType::ALL.into_iter().filter(|t| *t != EdgeType::Contains) {
        run(conn, &format!("MATCH ()-[r:{}]->() DELETE r;", t.as_str()))?;
    }
    for label in [NodeLabel::ApiEndpoint, NodeLabel::DbTable, NodeLabel::Queue] {
        run(conn, &format!("MATCH (n:{}) DELETE n;", label.as_str()))?;
    }
    run(conn, "MATCH (s:Symbol) WHERE s.kind = 'unresolved' DETACH DELETE s;")
}

/// Insert statement of a derived node table. The label decides the table; the
/// columns follow the table in the doc comment on `lge_core::NodeRec`.
fn node_insert(label: NodeLabel) -> &'static str {
    match label {
        NodeLabel::ApiEndpoint => {
            "CREATE (:ApiEndpoint {id:$id, name:$name, method:$kind, route:$qn, service:$svc, file_path:$fp, \
             start_line:$sl, end_line:$el});"
        }
        NodeLabel::DbTable => {
            "CREATE (:DbTable {id:$id, name:$name, service:$svc, file_path:$fp, start_line:$sl, end_line:$el});"
        }
        NodeLabel::Queue => {
            "CREATE (:Queue {id:$id, name:$name, kind:$kind, service:$svc, file_path:$fp, start_line:$sl, \
             end_line:$el});"
        }
        // Only placeholders reach this point, see `check`.
        _ => {
            "CREATE (:Symbol {id:$id, name:$name, qualified_name:$qn, kind:$kind, signature:$sig, doc:$doc, \
             body:'', file_path:$fp, service:$svc, start_line:$sl, end_line:$el, is_test:$t, search_text:''});"
        }
    }
}

fn node_params(n: &NodeRec) -> Vec<(&'static str, Value)> {
    let mut params = vec![
        ("id", st(&n.id)),
        ("name", st(&n.name)),
        ("svc", st(&n.service)),
        ("fp", st(&n.file_path)),
        ("sl", int(n.start_line)),
        ("el", int(n.end_line)),
    ];
    match n.label {
        NodeLabel::DbTable => {}
        NodeLabel::Queue => params.push(("kind", st(&n.kind))),
        NodeLabel::ApiEndpoint => params.extend([("kind", st(&n.kind)), ("qn", st(&n.qualified_name))]),
        _ => params.extend([
            ("kind", st(&n.kind)),
            ("qn", st(&n.qualified_name)),
            ("sig", st(&n.signature)),
            ("doc", st(&n.doc)),
            ("t", Value::Bool(n.is_test)),
        ]),
    }
    params
}

fn write_nodes(conn: &Connection, nodes: &[NodeRec]) -> Result<()> {
    let mut stmts: HashMap<NodeLabel, Prepared> = HashMap::new();
    let mut seen: HashSet<(NodeLabel, &str)> = HashSet::new();
    // The same node may be derived from several facts; it is written once.
    for n in nodes.iter().filter(|n| seen.insert((n.label, n.id.as_str()))) {
        let stmt = match stmts.entry(n.label) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(Prepared::new(conn, node_insert(n.label))?),
        };
        stmt.run(node_params(n))?;
    }
    Ok(())
}

/// The one (from, to) table pair of an edge type; `None` for CONTAINS, which
/// is written with the files and never by the resolver.
fn endpoints(t: EdgeType) -> Option<(NodeLabel, NodeLabel)> {
    match rel_pairs(t) {
        [pair] if t != EdgeType::Contains => Some(*pair),
        _ => None,
    }
}

/// The edges whose two endpoints exist. An edge to a node that is not there
/// is left out instead of failing the run: the resolver works from staged
/// sites, and a file may have been deleted since they were staged.
///
/// Must run after the derived nodes are written, so that they count.
fn storable<'a>(conn: &Connection, edges: &'a [EdgeRec]) -> Result<Vec<&'a EdgeRec>> {
    let mut known: HashMap<NodeLabel, HashSet<String>> = HashMap::new();
    let mut out = Vec::with_capacity(edges.len());
    for e in edges {
        let Some((from, to)) = endpoints(e.edge_type) else {
            continue;
        };
        for label in [from, to] {
            if let Entry::Vacant(v) = known.entry(label) {
                let ids = rows(conn, &format!("MATCH (n:{}) RETURN n.id;", label.as_str()), vec![])?;
                v.insert(ids.iter().map(|r| col_s(r, 0)).collect());
            }
        }
        let exists = |label: NodeLabel, id: &str| known.get(&label).is_some_and(|ids| ids.contains(id));
        if exists(from, &e.from) && exists(to, &e.to) {
            out.push(e);
        }
    }
    Ok(out)
}

fn edge_insert(t: EdgeType, from: NodeLabel, to: NodeLabel) -> String {
    format!(
        "MATCH (a:{} {{id:$a}}), (b:{} {{id:$b}}) \
         CREATE (a)-[:{} {{confidence:$c, method:$m, evidence_file:$f, start_line:$sl, end_line:$el, \
         commit:$commit, created_at:$at}}]->(b);",
        from.as_str(),
        to.as_str(),
        t.as_str()
    )
}

/// Write edges with one prepared statement per edge type.
fn insert_edges(conn: &Connection, edges: &[&EdgeRec], commit: &str, now: i64) -> Result<()> {
    let mut stmts: HashMap<EdgeType, Prepared> = HashMap::new();
    for e in edges {
        let Some((from, to)) = endpoints(e.edge_type) else {
            continue;
        };
        let stmt = match stmts.entry(e.edge_type) {
            Entry::Occupied(s) => s.into_mut(),
            Entry::Vacant(s) => s.insert(Prepared::new(conn, &edge_insert(e.edge_type, from, to))?),
        };
        stmt.run(vec![
            ("a", st(&e.from)),
            ("b", st(&e.to)),
            ("c", Value::Double(e.confidence)),
            ("m", st(&e.method)),
            ("f", st(&e.evidence_file)),
            ("sl", int(e.start_line)),
            ("el", int(e.end_line)),
            ("commit", st(commit)),
            ("at", int(now)),
        ])?;
    }
    Ok(())
}

/// Write edges with one `COPY FROM` per edge type. The columns of the file
/// are the two endpoint ids and then the provenance columns in table order.
///
/// A row the CSV reader could not take back exactly (a line break inside a
/// string, a confidence that is not a number) is written with a statement
/// instead, so both ways store the same values.
fn copy_edges(conn: &Connection, edges: &[&EdgeRec], commit: &str, now: i64) -> Result<()> {
    let mut by_type: BTreeMap<EdgeType, String> = BTreeMap::new();
    let mut by_statement: Vec<&EdgeRec> = Vec::new();
    let commit_ok = csv_safe(commit);
    for e in edges {
        let plain = commit_ok
            && e.confidence.is_finite()
            && [&e.from, &e.to, &e.method, &e.evidence_file]
                .iter()
                .all(|s| csv_safe(s));
        if !plain {
            by_statement.push(e);
            continue;
        }
        let csv = by_type.entry(e.edge_type).or_default();
        for text in [&e.from, &e.to] {
            csv_text(csv, text);
            csv.push(',');
        }
        // `{}` prints the shortest text that reads back as the same f64.
        let _ = write!(csv, "{},", e.confidence);
        for text in [&e.method, &e.evidence_file] {
            csv_text(csv, text);
            csv.push(',');
        }
        let _ = write!(csv, "{},{},", e.start_line, e.end_line);
        csv_text(csv, commit);
        let _ = writeln!(csv, ",{now}");
    }

    let scratch = Scratch::new()?;
    for (t, csv) in &by_type {
        let file = scratch.0.join(format!("{}.csv", t.as_str()));
        std::fs::write(&file, csv).with_context(|| format!("cannot write {}", file.display()))?;
        // Every option is spelled out: the reader must not guess the dialect from the data.
        run(
            conn,
            &format!(
                "COPY {} FROM '{}' (HEADER=false, DELIM=',', QUOTE='\"', ESCAPE='\"', AUTO_DETECT=false);",
                t.as_str(),
                quoted(&file.to_string_lossy())
            ),
        )?;
    }
    insert_edges(conn, &by_statement, commit, now)
}

/// True when the CSV reader takes the text back unchanged from a quoted
/// field. Quotes are doubled and anything else may stand as it is, except
/// control characters: the parallel reader does not accept a line break
/// inside a field.
fn csv_safe(text: &str) -> bool {
    !text.chars().any(char::is_control)
}

/// Append a quoted CSV field. It is always quoted: an unquoted empty field
/// would be read as null, not as the empty string.
fn csv_text(csv: &mut String, text: &str) {
    csv.push('"');
    for c in text.chars() {
        if c == '"' {
            csv.push('"');
        }
        csv.push(c);
    }
    csv.push('"');
}

/// A private directory for the files of one bulk load, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "lge-edges-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(name);
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        // Not `recursive`: a directory that already exists is not ours to write into.
        builder
            .create(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
