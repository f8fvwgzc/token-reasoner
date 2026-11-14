//! Seed search (BM25 through the `fts` extension, or a substring scorer) and
//! symbol lookup by name.

use crate::schema::{drop_fts_index, fts_indexes, FTS_COLUMNS, FTS_INDEX};
use crate::values::{col_b, col_f, col_i, col_s, rows, run, st, Prepared, Row};
use anyhow::Result;
use lbug::Connection;
use lge_core::Hit;
use std::collections::HashMap;

pub(crate) const HIT_COLS: &str =
    "s.id, s.name, s.qualified_name, s.kind, s.file_path, s.service, s.start_line, s.end_line, s.is_test";

const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "that",
    "this",
    "from",
    "into",
    "when",
    "add",
    "fix",
    "update",
    "change",
    "make",
    "new",
    "bug",
    "feature",
    "implement",
    "support",
    "should",
    "need",
    "use",
    "not",
    "are",
    "can",
    "all",
    "any",
];

/// A row that starts with [`HIT_COLS`].
pub(crate) fn hit(row: &Row, score: f64) -> Hit {
    Hit {
        id: col_s(row, 0),
        name: col_s(row, 1),
        qualified_name: col_s(row, 2),
        kind: col_s(row, 3),
        file_path: col_s(row, 4),
        service: col_s(row, 5),
        start_line: col_i(row, 6),
        end_line: col_i(row, 7),
        is_test: col_b(row, 8),
        score,
    }
}

/// Words of a task description that are worth searching for.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split(|c: char| !c.is_alphanumeric()) {
        let w = w.to_lowercase();
        if w.len() >= 3 && !STOPWORDS.contains(&w.as_str()) && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// Make sure the full-text index over Symbol exists and covers the right
/// columns. Needs the extension loaded and must run outside a transaction.
///
/// LadybugDB keeps the index up to date on every insert, update and delete,
/// so "refreshing" it means creating it when it is missing, not rebuilding it.
pub(crate) fn ensure_fts_index(conn: &Connection) -> Result<()> {
    for (table, index, columns) in fts_indexes(conn)? {
        if table == "Symbol" && index == FTS_INDEX {
            if columns == FTS_COLUMNS {
                return Ok(());
            }
            drop_fts_index(conn, &table, &index)?;
        }
    }
    let columns: Vec<String> = FTS_COLUMNS.iter().map(|c| format!("'{c}'")).collect();
    run(
        conn,
        &format!(
            "CALL CREATE_FTS_INDEX('Symbol', '{FTS_INDEX}', [{}]);",
            columns.join(", ")
        ),
    )
}

/// Seed search. BM25 through the fts extension when it is loaded, otherwise a
/// substring match scored in Rust. A BM25 error or an empty BM25 result also
/// falls back to the substring match.
pub(crate) fn search(conn: &Connection, fts: bool, text: &str, limit: usize) -> Result<Vec<Hit>> {
    if fts {
        if let Ok(hits) = search_fts(conn, text, limit) {
            if !hits.is_empty() {
                return Ok(hits);
            }
        }
    }
    search_substring(conn, text, limit)
}

fn search_fts(conn: &Connection, text: &str, limit: usize) -> Result<Vec<Hit>> {
    let q = format!(
        "CALL QUERY_FTS_INDEX('Symbol', '{FTS_INDEX}', $q) WITH node AS s, score \
         WHERE s.kind <> 'module' AND s.kind <> 'unresolved' \
         RETURN {HIT_COLS}, score ORDER BY score DESC, s.qualified_name LIMIT {limit};"
    );
    let found = rows(conn, &q, vec![("q", st(&tokens(text).join(" ")))])?;
    Ok(found.iter().map(|r| hit(r, col_f(r, 9))).collect())
}

fn search_substring(conn: &Connection, text: &str, limit: usize) -> Result<Vec<Hit>> {
    let q = format!(
        "MATCH (s:Symbol) WHERE s.kind <> 'module' AND s.kind <> 'unresolved' \
         AND (lower(s.name) CONTAINS $t OR lower(s.qualified_name) CONTAINS $t OR lower(s.doc) CONTAINS $t \
         OR lower(s.signature) CONTAINS $t) \
         RETURN {HIT_COLS}, s.doc, s.signature;"
    );
    let mut stmt = Prepared::new(conn, &q)?;
    let mut scored: HashMap<String, Hit> = HashMap::new();
    for t in tokens(text) {
        for r in stmt.rows(vec![("t", st(&t))])? {
            let name = col_s(&r, 1).to_lowercase();
            let words: Vec<&str> = name.split('_').collect();
            let mut score = 0.0;
            if name == t {
                score += 5.0;
            } else if words.contains(&t.as_str()) {
                score += 4.0;
            } else if name.contains(&t) {
                score += 3.0;
            } else if col_s(&r, 2).to_lowercase().contains(&t) {
                score += 1.5;
            }
            if col_s(&r, 9).to_lowercase().contains(&t) {
                score += 1.0;
            }
            // The full-text index covers the signature, so the fallback does too.
            if score == 0.0 || col_s(&r, 10).to_lowercase().contains(&t) {
                score += 0.5;
            }
            scored.entry(col_s(&r, 0)).or_insert_with(|| hit(&r, 0.0)).score += score;
        }
    }
    let mut hits: Vec<Hit> = scored.into_values().collect();
    // Prefer production code over tests when scores tie.
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.is_test.cmp(&b.is_test))
            .then(a.qualified_name.cmp(&b.qualified_name))
    });
    hits.truncate(limit);
    Ok(hits)
}

/// Symbols by exact id, qualified name or name. Placeholders are not symbols
/// and are left out.
pub(crate) fn find(conn: &Connection, needle: &str) -> Result<Vec<Hit>> {
    let q = format!(
        "MATCH (s:Symbol) WHERE s.kind <> 'unresolved' AND (s.id = $n OR s.qualified_name = $n OR s.name = $n) \
         RETURN {HIT_COLS};"
    );
    let mut hits: Vec<Hit> = rows(conn, &q, vec![("n", st(needle))])?
        .iter()
        .map(|r| hit(r, 0.0))
        .collect();
    sort_by_name(&mut hits);
    Ok(hits)
}

/// Symbols whose name starts with `prefix`, ignoring case. The shortest names
/// come first: they are the closest to what was typed.
pub(crate) fn find_prefix(conn: &Connection, prefix: &str, limit: usize) -> Result<Vec<Hit>> {
    let q = format!(
        "MATCH (s:Symbol) WHERE s.kind <> 'unresolved' AND lower(s.name) STARTS WITH $p \
         RETURN {HIT_COLS} ORDER BY size(s.name), s.qualified_name, s.id LIMIT {limit};"
    );
    Ok(rows(conn, &q, vec![("p", st(&prefix.to_lowercase()))])?
        .iter()
        .map(|r| hit(r, 0.0))
        .collect())
}

fn sort_by_name(hits: &mut [Hit]) {
    hits.sort_by(|a, b| (&a.qualified_name, &a.file_path, &a.id).cmp(&(&b.qualified_name, &b.file_path, &b.id)));
}
