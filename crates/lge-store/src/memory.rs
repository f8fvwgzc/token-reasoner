//! Long-term memory: the facts the agent retained, and the code they mention.
//!
//! A fact is superseded, never deleted: `expired_at` is 0 while it is current.
//!
//! What a fact mentions is kept twice. `Memory.links` holds the ids as JSON
//! and is the record; the MENTIONS edges are built from it and are what the
//! graph queries follow. Re-indexing a file deletes its Symbol nodes and
//! creates them again with the same ids, which takes the edges with it, so
//! [`restore_mentions`] puts back whatever edge is missing after every resolve.

use crate::nodes::{distinct, ID_CHUNK};
use crate::schema::{MEMORY_FTS, MENTIONS, MENTION_TARGETS};
use crate::search::prose_words;
use crate::values::{
    col_f, col_i, col_s, count, int, json_list, json_text, pairs, parse_json_list, rows, st, strings, Prepared, Row,
};
use anyhow::Result;
use lbug::{Connection, Value};
use lge_core::{MemoryRec, NodeLabel};
use std::collections::{HashMap, HashSet};

/// Columns of a fact, in the order [`memory`] expects them.
const MEMORY_COLS: &str = "m.id, m.kind, m.text, m.key, m.tags, m.source, m.created_at, m.valid_at, m.invalid_at, \
     m.expired_at, m.proof_count, m.links";

/// Newest first, with the id as the last word so the order is total.
const NEWEST_FIRST: &str = "m.valid_at DESC, m.created_at DESC, m.id";

/// From this many tags on, a tag filter is checked in Rust only: a statement
/// with one condition per tag would grow without bound.
const TAG_CONDITIONS: usize = 32;

/// A row that starts with [`MEMORY_COLS`]. `links` are the ids as stored;
/// [`live_links`] narrows them to the nodes that exist.
fn memory(r: &Row) -> MemoryRec {
    MemoryRec {
        id: col_s(r, 0),
        kind: col_s(r, 1),
        text: col_s(r, 2),
        key: col_s(r, 3),
        tags: parse_json_list(&col_s(r, 4)),
        source: col_s(r, 5),
        created_at: col_i(r, 6),
        valid_at: col_i(r, 7),
        invalid_at: col_i(r, 8),
        expired_at: col_i(r, 9),
        proof_count: col_i(r, 10),
        links: parse_json_list(&col_s(r, 11)),
    }
}

/// The names a mentioned node is known by.
struct Target {
    name: String,
    qualified_name: String,
}

/// Reads id, name and qualified name of the nodes of one label among `$ids`.
fn target_query(label: NodeLabel) -> String {
    let (names, real) = match label {
        // A placeholder stands for a name nobody could resolve and is written
        // anew by every resolve: it is not something a fact is about.
        NodeLabel::Symbol => ("n.name, n.qualified_name", " WHERE n.kind <> 'unresolved'"),
        NodeLabel::File | NodeLabel::Repository => ("n.path, n.path", ""),
        _ => ("n.name, n.name", ""),
    };
    format!(
        "UNWIND $ids AS x MATCH (n:{} {{id:x}}){real} RETURN n.id, {names};",
        label.as_str()
    )
}

/// The nodes among `ids` that a fact can mention and that exist, by id.
fn targets(conn: &Connection, ids: &[&str]) -> Result<HashMap<String, Target>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    for label in MENTION_TARGETS {
        let mut stmt = Prepared::new(conn, &target_query(label))?;
        for chunk in ids.chunks(ID_CHUNK) {
            for r in stmt.rows(vec![("ids", strings(chunk))])? {
                let target = Target {
                    name: col_s(&r, 1),
                    qualified_name: col_s(&r, 2),
                };
                out.insert(col_s(&r, 0), target);
            }
        }
    }
    Ok(out)
}

/// What the full-text index reads about the code a fact mentions: each
/// node's name and qualified name as written, then their words, so the fact
/// is found by `getOrderHandler` and by `order handler` alike.
fn signals<'a>(mentioned: impl Iterator<Item = &'a Target>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for t in mentioned {
        parts.push(t.name.clone());
        if t.qualified_name != t.name {
            parts.push(t.qualified_name.clone());
        }
        parts.push(lge_core::search_text(&t.name, &t.qualified_name, "", "", ""));
    }
    parts.retain(|p| !p.is_empty());
    parts.join(" ")
}

/// Create MENTIONS edges for (fact id, node id) pairs. A pair whose node does
/// not exist creates nothing. Must run inside a transaction.
fn link<A: AsRef<str>, B: AsRef<str>>(conn: &Connection, wanted: &[(A, B)]) -> Result<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    for label in MENTION_TARGETS {
        let q = format!(
            "UNWIND $pairs AS p MATCH (m:Memory {{id:p[1]}}), (n:{} {{id:p[2]}}) CREATE (m)-[:{MENTIONS}]->(n);",
            label.as_str()
        );
        let mut stmt = Prepared::new(conn, &q)?;
        for chunk in wanted.chunks(ID_CHUNK) {
            stmt.run(vec![("pairs", pairs(chunk))])?;
        }
    }
    Ok(())
}

/// Insert or replace a fact by id and rewrite its MENTIONS edges. Must run
/// inside a transaction.
///
/// Links to nodes that do not exist are dropped: the row keeps the ids that
/// were found, in the order given, and an edge is made to each of them.
pub(crate) fn put(conn: &Connection, m: &MemoryRec) -> Result<()> {
    let asked = distinct(&m.links);
    let found = targets(conn, &asked)?;
    let links: Vec<&str> = asked.into_iter().filter(|id| found.contains_key(*id)).collect();
    let id = || ("id", st(&m.id));

    // An embedding belongs to the text it was made from.
    rows(
        conn,
        "MATCH (m:Memory {id:$id}) WHERE m.text <> $text SET m.embedding = NULL;",
        vec![id(), ("text", st(&m.text))],
    )?;
    rows(
        conn,
        "MERGE (m:Memory {id:$id}) SET m.kind = $kind, m.text = $text, m.key = $key, m.tags = $tags, \
         m.tags_text = $tags_text, m.source = $source, m.created_at = $created_at, m.valid_at = $valid_at, \
         m.invalid_at = $invalid_at, m.expired_at = $expired_at, m.proof_count = $proof_count, m.links = $links, \
         m.signals = $signals;",
        vec![
            id(),
            ("kind", st(&m.kind)),
            ("text", st(&m.text)),
            ("key", st(&m.key)),
            ("tags", st(&json_list(&m.tags))),
            ("tags_text", st(&m.tags.join(" "))),
            ("source", st(&m.source)),
            ("created_at", int(m.created_at)),
            ("valid_at", int(m.valid_at)),
            ("invalid_at", int(m.invalid_at)),
            ("expired_at", int(m.expired_at)),
            ("proof_count", int(m.proof_count)),
            ("links", st(&json_list(&links))),
            ("signals", st(&signals(links.iter().filter_map(|l| found.get(*l))))),
        ],
    )?;
    rows(
        conn,
        &format!("MATCH (m:Memory {{id:$id}})-[r:{MENTIONS}]->() DELETE r;"),
        vec![id()],
    )?;
    let wanted: Vec<(&str, &str)> = links.iter().map(|l| (m.id.as_str(), *l)).collect();
    link(conn, &wanted)
}

/// Put back the MENTIONS edges that `Memory.links` asks for and the graph
/// lacks. Must run inside a transaction, at the end of every resolve: by then
/// the files that were re-indexed have their symbols again.
///
/// A link whose node is gone for good stays on the row and is tried again
/// next time; it costs a lookup and creates nothing.
pub(crate) fn restore_mentions(conn: &Connection) -> Result<()> {
    let recorded = rows(
        conn,
        "MATCH (m:Memory) WHERE m.links <> '[]' RETURN m.id, m.links;",
        vec![],
    )?;
    if recorded.is_empty() {
        return Ok(());
    }
    let stored = rows(
        conn,
        &format!("MATCH (m:Memory)-[:{MENTIONS}]->(n) RETURN m.id, n.id;"),
        vec![],
    )?;
    let stored: HashSet<(String, String)> = stored.iter().map(|r| (col_s(r, 0), col_s(r, 1))).collect();
    let mut missing: Vec<(String, String)> = Vec::new();
    for r in &recorded {
        let id = col_s(r, 0);
        let mut seen: HashSet<String> = HashSet::new();
        for node in parse_json_list(&col_s(r, 1)) {
            let pair = (id.clone(), node);
            if !stored.contains(&pair) && seen.insert(pair.1.clone()) {
                missing.push(pair);
            }
        }
    }
    link(conn, &missing)
}

/// Narrow `links` to the nodes that exist now. The row keeps every id the
/// fact was stored with, so that a link comes back when its node does; a
/// reader is only given the ones it can follow.
fn live_links<'a>(conn: &Connection, facts: impl IntoIterator<Item = &'a mut MemoryRec>) -> Result<()> {
    let mut facts: Vec<&mut MemoryRec> = facts.into_iter().collect();
    let existing = {
        let mut seen: HashSet<&str> = HashSet::new();
        let linked: Vec<&str> = facts
            .iter()
            .flat_map(|m| m.links.iter().map(String::as_str))
            .filter(|id| seen.insert(*id))
            .collect();
        targets(conn, &linked)?
    };
    for m in facts.iter_mut() {
        m.links.retain(|id| existing.contains_key(id));
    }
    Ok(())
}

/// Facts by id, in the order they were asked for. Unknown ids are skipped.
pub(crate) fn get(conn: &Connection, ids: &[String]) -> Result<Vec<MemoryRec>> {
    let ids = distinct(ids);
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let q = format!("UNWIND $ids AS x MATCH (m:Memory {{id:x}}) RETURN {MEMORY_COLS};");
    let mut stmt = Prepared::new(conn, &q)?;
    let mut found: HashMap<String, MemoryRec> = HashMap::new();
    for chunk in ids.chunks(ID_CHUNK) {
        for r in stmt.rows(vec![("ids", strings(chunk))])? {
            let m = memory(&r);
            found.insert(m.id.clone(), m);
        }
    }
    let mut out: Vec<MemoryRec> = ids.iter().filter_map(|id| found.remove(*id)).collect();
    live_links(conn, &mut out)?;
    Ok(out)
}

/// The condition that leaves expired facts out, introduced by `prefix`
/// (`WHERE` or `AND`); nothing when they are wanted too.
fn current(prefix: &str, include_expired: bool) -> String {
    if include_expired {
        String::new()
    } else {
        format!(" {prefix} m.expired_at = 0")
    }
}

/// Keyword search over text, key, tags and the names of the mentioned code,
/// best first. BM25 through the fts extension when it is loaded, otherwise a
/// substring match scored in Rust; a BM25 error or an empty BM25 result also
/// falls back to the substring match. Expired facts only when asked for.
pub(crate) fn search(
    conn: &Connection,
    fts: bool,
    text: &str,
    limit: usize,
    include_expired: bool,
) -> Result<Vec<(MemoryRec, f64)>> {
    let words = prose_words(text);
    if words.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    if fts {
        found = search_fts(conn, &words, limit, include_expired).unwrap_or_default();
    }
    if found.is_empty() {
        found = search_substring(conn, &words, limit, include_expired)?;
    }
    live_links(conn, found.iter_mut().map(|(m, _)| m))?;
    Ok(found)
}

fn search_fts(
    conn: &Connection,
    words: &[String],
    limit: usize,
    include_expired: bool,
) -> Result<Vec<(MemoryRec, f64)>> {
    // No `TOP` for the index: it would cut the list before expired facts are removed.
    let q = format!(
        "CALL QUERY_FTS_INDEX('{}', '{}', $q) WITH node AS m, score{} \
         RETURN {MEMORY_COLS}, score ORDER BY score DESC, {NEWEST_FIRST} LIMIT {limit};",
        MEMORY_FTS.table,
        MEMORY_FTS.name,
        current("WHERE", include_expired)
    );
    let found = rows(conn, &q, vec![("q", st(&words.join(" ")))])?;
    Ok(found.iter().map(|r| (memory(r), col_f(r, 12))).collect())
}

/// What one query word is worth on one fact: its own text counts most, then
/// its key, then its tags and the names of the code it mentions.
fn substring_score(text: &str, key: &str, tags: &str, signals: &str, word: &str) -> f64 {
    let worth = [(text, 2.0), (key, 1.5), (tags, 1.0), (signals, 1.0)];
    worth
        .iter()
        .filter(|(column, _)| column.contains(word))
        .map(|(_, points)| points)
        .sum()
}

fn search_substring(
    conn: &Connection,
    words: &[String],
    limit: usize,
    include_expired: bool,
) -> Result<Vec<(MemoryRec, f64)>> {
    let q = format!(
        "MATCH (m:Memory) WHERE (lower(m.text) CONTAINS $t OR lower(m.key) CONTAINS $t \
         OR lower(m.tags_text) CONTAINS $t OR lower(m.signals) CONTAINS $t){} \
         RETURN {MEMORY_COLS}, m.tags_text, m.signals;",
        current("AND", include_expired)
    );
    let mut stmt = Prepared::new(conn, &q)?;
    let mut scored: HashMap<String, (MemoryRec, f64)> = HashMap::new();
    for word in words {
        for r in stmt.rows(vec![("t", st(word))])? {
            let m = memory(&r);
            let score = substring_score(
                &m.text.to_lowercase(),
                &m.key.to_lowercase(),
                &col_s(&r, 12).to_lowercase(),
                &col_s(&r, 13).to_lowercase(),
                word,
            );
            scored.entry(m.id.clone()).or_insert((m, 0.0)).1 += score;
        }
    }
    let mut found: Vec<(MemoryRec, f64)> = scored.into_values().collect();
    found.sort_by(|(a, x), (b, y)| {
        y.total_cmp(x)
            .then((b.valid_at, b.created_at).cmp(&(a.valid_at, a.created_at)))
            .then(a.id.cmp(&b.id))
    });
    found.truncate(limit);
    Ok(found)
}

/// Current facts that mention any of the given nodes, each with the number
/// of those nodes it mentions: most shared first, then newest.
pub(crate) fn mentioning(conn: &Connection, node_ids: &[String], limit: usize) -> Result<Vec<(MemoryRec, i64)>> {
    let ids = distinct(node_ids);
    if ids.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let q = format!(
        "UNWIND $ids AS x MATCH (m:Memory)-[:{MENTIONS}]->(n {{id:x}}) WHERE m.expired_at = 0 \
         RETURN m.id, m.valid_at, m.created_at, count(DISTINCT n.id);"
    );
    let mut stmt = Prepared::new(conn, &q)?;
    // fact id -> (valid_at, created_at, shared). The chunks hold different
    // nodes, so their counts add up.
    let mut shared: HashMap<String, (i64, i64, i64)> = HashMap::new();
    for chunk in ids.chunks(ID_CHUNK) {
        for r in stmt.rows(vec![("ids", strings(chunk))])? {
            let entry = shared.entry(col_s(&r, 0)).or_insert((col_i(&r, 1), col_i(&r, 2), 0));
            entry.2 += col_i(&r, 3);
        }
    }
    let mut order: Vec<(String, (i64, i64, i64))> = shared.into_iter().collect();
    order.sort_by(|(a, x), (b, y)| (y.2, y.0, y.1).cmp(&(x.2, x.0, x.1)).then(a.cmp(b)));
    order.truncate(limit);

    let wanted: Vec<String> = order.iter().map(|(id, _)| id.clone()).collect();
    let counts: HashMap<String, i64> = order.into_iter().map(|(id, c)| (id, c.2)).collect();
    Ok(get(conn, &wanted)?
        .into_iter()
        .filter_map(|m| counts.get(&m.id).map(|n| (m, *n)))
        .collect())
}

/// Current facts, newest `valid_at` first, optionally of one kind or carrying
/// any of the given tags.
pub(crate) fn recent(conn: &Connection, kind: Option<&str>, tags: &[String], limit: usize) -> Result<Vec<MemoryRec>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let tags = distinct(tags);
    let mut conditions = vec!["m.expired_at = 0".to_string()];
    let mut params: Vec<(&str, Value)> = Vec::new();
    if let Some(kind) = kind {
        conditions.push("m.kind = $kind".to_string());
        params.push(("kind", st(kind)));
    }
    // The column holds the tags as JSON, so a fact that carries a tag
    // contains that tag as JSON text. The reverse does not hold for every
    // text, so the rows are checked again below.
    let names: Vec<String> = (0..tags.len()).map(|i| format!("tag{i}")).collect();
    if !tags.is_empty() && tags.len() <= TAG_CONDITIONS {
        let any: Vec<String> = names.iter().map(|n| format!("m.tags CONTAINS ${n}")).collect();
        conditions.push(format!("({})", any.join(" OR ")));
        for (name, tag) in names.iter().zip(&tags) {
            params.push((name.as_str(), st(&json_text(tag))));
        }
    }
    // Without tags the database cuts the list; with them the check below decides.
    let cut = if tags.is_empty() {
        format!(" LIMIT {limit}")
    } else {
        String::new()
    };
    let q = format!(
        "MATCH (m:Memory) WHERE {} RETURN {MEMORY_COLS} ORDER BY {NEWEST_FIRST}{cut};",
        conditions.join(" AND ")
    );
    let carries = |m: &MemoryRec| tags.is_empty() || m.tags.iter().any(|t| tags.contains(&t.as_str()));
    let mut out: Vec<MemoryRec> = rows(conn, &q, params)?
        .iter()
        .map(memory)
        .filter(carries)
        .take(limit)
        .collect();
    live_links(conn, &mut out)?;
    Ok(out)
}

/// Current facts with exactly this key, newest first.
pub(crate) fn by_key(conn: &Connection, key: &str) -> Result<Vec<MemoryRec>> {
    let q = format!(
        "MATCH (m:Memory) WHERE m.key = $key AND m.expired_at = 0 RETURN {MEMORY_COLS} ORDER BY {NEWEST_FIRST};"
    );
    let mut out: Vec<MemoryRec> = rows(conn, &q, vec![("key", st(key))])?.iter().map(memory).collect();
    live_links(conn, &mut out)?;
    Ok(out)
}

/// Number of stored facts: (current, expired).
pub(crate) fn counts(conn: &Connection) -> Result<(i64, i64)> {
    let current = count(conn, "MATCH (m:Memory) WHERE m.expired_at = 0 RETURN count(m);", vec![])?;
    let expired = count(
        conn,
        "MATCH (m:Memory) WHERE m.expired_at <> 0 RETURN count(m);",
        vec![],
    )?;
    Ok((current, expired))
}
