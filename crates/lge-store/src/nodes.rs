//! Nodes of any label in the common [`NodeRec`] shape.
//!
//! The mapping from each table to `NodeRec` follows the table in the doc
//! comment on `lge_core::NodeRec`.

use crate::values::{col_b, col_i, col_s, rows, st, strings, Prepared, Row};
use anyhow::Result;
use lbug::Connection;
use lge_core::{NodeLabel, NodeRec};
use std::collections::HashSet;

/// Ids looked up per query. A list parameter of a few thousand strings is
/// cheap; chunking keeps one statement bounded however many ids are asked for.
pub(crate) const ID_CHUNK: usize = 2000;

/// The ids without repeats, in their original order.
pub(crate) fn distinct(ids: &[String]) -> Vec<&str> {
    let mut seen = HashSet::new();
    ids.iter().map(String::as_str).filter(|id| seen.insert(*id)).collect()
}

/// Columns read for a label, in the order [`node`] expects them.
fn columns(label: NodeLabel) -> &'static str {
    match label {
        NodeLabel::Symbol => {
            "n.id, n.name, n.qualified_name, n.kind, n.signature, n.doc, n.file_path, n.service, n.start_line, \
             n.end_line, n.is_test"
        }
        NodeLabel::File => "n.id, n.path, n.lang, n.service",
        NodeLabel::Service => "n.id, n.name, n.root",
        NodeLabel::Repository => "n.id, n.path, n.head_commit",
        NodeLabel::ApiEndpoint => "n.id, n.name, n.route, n.method, n.service, n.file_path, n.start_line, n.end_line",
        NodeLabel::DbTable => "n.id, n.name, n.service, n.file_path, n.start_line, n.end_line",
        NodeLabel::Queue => "n.id, n.name, n.kind, n.service, n.file_path, n.start_line, n.end_line",
    }
}

/// Listing order of a label: by name, with the id as the last word so the
/// order is total.
fn order(label: NodeLabel) -> &'static str {
    match label {
        NodeLabel::Symbol => "n.qualified_name, n.id",
        NodeLabel::File | NodeLabel::Repository => "n.path, n.id",
        NodeLabel::ApiEndpoint => "n.route, n.method, n.id",
        NodeLabel::Service | NodeLabel::DbTable | NodeLabel::Queue => "n.name, n.id",
    }
}

fn node(label: NodeLabel, r: &Row) -> NodeRec {
    let id = col_s(r, 0);
    let name = col_s(r, 1);
    match label {
        NodeLabel::Symbol => NodeRec {
            id,
            label,
            name,
            qualified_name: col_s(r, 2),
            kind: col_s(r, 3),
            signature: col_s(r, 4),
            doc: col_s(r, 5),
            file_path: col_s(r, 6),
            service: col_s(r, 7),
            start_line: col_i(r, 8),
            end_line: col_i(r, 9),
            is_test: col_b(r, 10),
        },
        // name and qualified name are the path, kind is the language
        NodeLabel::File => NodeRec {
            file_path: name.clone(),
            service: col_s(r, 3),
            ..NodeRec::bare(&id, label, &name, &name, &col_s(r, 2))
        },
        // qualified name is the root directory
        NodeLabel::Service => NodeRec {
            service: name.clone(),
            ..NodeRec::bare(&id, label, &name, &col_s(r, 2), "")
        },
        // name and qualified name are the path, kind is the head commit
        NodeLabel::Repository => NodeRec::bare(&id, label, &name, &name, &col_s(r, 2)),
        // qualified name is the normalized route, kind is the HTTP method
        NodeLabel::ApiEndpoint => located(NodeRec::bare(&id, label, &name, &col_s(r, 2), &col_s(r, 3)), r, 4),
        NodeLabel::DbTable => located(NodeRec::bare(&id, label, &name, &name, ""), r, 2),
        // kind is queue, exchange or topic
        NodeLabel::Queue => located(NodeRec::bare(&id, label, &name, &name, &col_s(r, 2)), r, 3),
    }
}

/// Fill service, file and lines of a derived node from four columns starting at `at`.
fn located(n: NodeRec, r: &Row, at: usize) -> NodeRec {
    NodeRec {
        service: col_s(r, at),
        file_path: col_s(r, at + 1),
        start_line: col_i(r, at + 2),
        end_line: col_i(r, at + 3),
        ..n
    }
}

/// Nodes of any label by id, through the primary-key index of each table.
/// Unknown ids are skipped.
pub(crate) fn by_ids(conn: &Connection, ids: &[String]) -> Result<Vec<NodeRec>> {
    let ids = distinct(ids);
    let mut out = Vec::new();
    if ids.is_empty() {
        return Ok(out);
    }
    for label in NodeLabel::ALL {
        let q = format!(
            "UNWIND $ids AS x MATCH (n:{} {{id:x}}) RETURN {};",
            label.as_str(),
            columns(label)
        );
        let mut stmt = Prepared::new(conn, &q)?;
        for chunk in ids.chunks(ID_CHUNK) {
            out.extend(stmt.rows(vec![("ids", strings(chunk))])?.iter().map(|r| node(label, r)));
        }
    }
    Ok(out)
}

/// Up to `limit` nodes of one label, ordered by name. Placeholders (kind
/// `unresolved`) are nodes of their label and are included.
pub(crate) fn by_label(conn: &Connection, label: NodeLabel, limit: usize) -> Result<Vec<NodeRec>> {
    let q = format!(
        "MATCH (n:{}) RETURN {} ORDER BY {} LIMIT {limit};",
        label.as_str(),
        columns(label),
        order(label)
    );
    Ok(rows(conn, &q, vec![])?.iter().map(|r| node(label, r)).collect())
}

/// Symbols defined in a file, ordered by start line; of two symbols that start
/// on the same line the enclosing one comes first.
pub(crate) fn symbols_in_file(conn: &Connection, path: &str) -> Result<Vec<NodeRec>> {
    let q = format!(
        "MATCH (n:Symbol) WHERE n.file_path = $p AND n.kind <> 'unresolved' \
         RETURN {} ORDER BY n.start_line, n.end_line DESC, n.id;",
        columns(NodeLabel::Symbol)
    );
    Ok(rows(conn, &q, vec![("p", st(path))])?
        .iter()
        .map(|r| node(NodeLabel::Symbol, r))
        .collect())
}

/// Stored source text of a symbol; `None` for an unknown id or a placeholder.
pub(crate) fn symbol_body(conn: &Connection, id: &str) -> Result<Option<String>> {
    let found = rows(
        conn,
        "MATCH (s:Symbol {id:$id}) WHERE s.kind <> 'unresolved' RETURN s.body;",
        vec![("id", st(id))],
    )?;
    Ok(found.first().map(|r| col_s(r, 0)))
}
