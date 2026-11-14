//! Pages: markdown views over tagged facts or over the code graph. A page is
//! known by its slug, which is also the name of the file it is exported to.

use crate::schema::PAGE_FTS;
use crate::search::prose_words;
use crate::values::{col_f, col_i, col_s, int, json_list, parse_json_list, rows, st, Prepared, Row};
use anyhow::Result;
use lbug::Connection;
use lge_core::PageRec;
use std::collections::HashMap;

/// Columns of a page, in the order [`page`] expects them.
const PAGE_COLS: &str = "p.id, p.slug, p.title, p.body, p.tags, p.kind, p.updated_at, p.last_memory_seen_at";

/// A row that starts with [`PAGE_COLS`].
fn page(r: &Row) -> PageRec {
    PageRec {
        id: col_s(r, 0),
        slug: col_s(r, 1),
        title: col_s(r, 2),
        body: col_s(r, 3),
        tags: parse_json_list(&col_s(r, 4)),
        kind: col_s(r, 5),
        updated_at: col_i(r, 6),
        last_memory_seen_at: col_i(r, 7),
    }
}

/// Insert or replace a page by slug.
pub(crate) fn put(conn: &Connection, p: &PageRec) -> Result<()> {
    rows(
        conn,
        "MERGE (p:Page {slug:$slug}) SET p.id = $id, p.title = $title, p.body = $body, p.tags = $tags, \
         p.kind = $kind, p.updated_at = $updated_at, p.last_memory_seen_at = $seen;",
        vec![
            ("slug", st(&p.slug)),
            ("id", st(&p.id)),
            ("title", st(&p.title)),
            ("body", st(&p.body)),
            ("tags", st(&json_list(&p.tags))),
            ("kind", st(&p.kind)),
            ("updated_at", int(p.updated_at)),
            ("seen", int(p.last_memory_seen_at)),
        ],
    )?;
    Ok(())
}

pub(crate) fn get(conn: &Connection, slug: &str) -> Result<Option<PageRec>> {
    let q = format!("MATCH (p:Page {{slug:$slug}}) RETURN {PAGE_COLS};");
    Ok(rows(conn, &q, vec![("slug", st(slug))])?.first().map(page))
}

/// Every page, ordered by slug.
pub(crate) fn list(conn: &Connection) -> Result<Vec<PageRec>> {
    let q = format!("MATCH (p:Page) RETURN {PAGE_COLS} ORDER BY p.slug;");
    Ok(rows(conn, &q, vec![])?.iter().map(page).collect())
}

/// Remove a page. A slug that names no page removes nothing.
pub(crate) fn delete(conn: &Connection, slug: &str) -> Result<()> {
    rows(conn, "MATCH (p:Page {slug:$slug}) DELETE p;", vec![("slug", st(slug))])?;
    Ok(())
}

/// Keyword search over title and body, best first. BM25 through the fts
/// extension when it is loaded, otherwise a substring match scored in Rust; a
/// BM25 error or an empty BM25 result also falls back to the substring match.
pub(crate) fn search(conn: &Connection, fts: bool, text: &str, limit: usize) -> Result<Vec<(PageRec, f64)>> {
    let words = prose_words(text);
    if words.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    if fts {
        if let Ok(found) = search_fts(conn, &words, limit) {
            if !found.is_empty() {
                return Ok(found);
            }
        }
    }
    search_substring(conn, &words, limit)
}

fn search_fts(conn: &Connection, words: &[String], limit: usize) -> Result<Vec<(PageRec, f64)>> {
    let q = format!(
        "CALL QUERY_FTS_INDEX('{}', '{}', $q) WITH node AS p, score \
         RETURN {PAGE_COLS}, score ORDER BY score DESC, p.slug LIMIT {limit};",
        PAGE_FTS.table, PAGE_FTS.name
    );
    let found = rows(conn, &q, vec![("q", st(&words.join(" ")))])?;
    Ok(found.iter().map(|r| (page(r), col_f(r, 8))).collect())
}

fn search_substring(conn: &Connection, words: &[String], limit: usize) -> Result<Vec<(PageRec, f64)>> {
    let q = format!("MATCH (p:Page) WHERE lower(p.title) CONTAINS $t OR lower(p.body) CONTAINS $t RETURN {PAGE_COLS};");
    let mut stmt = Prepared::new(conn, &q)?;
    let mut scored: HashMap<String, (PageRec, f64)> = HashMap::new();
    for word in words {
        for r in stmt.rows(vec![("t", st(word))])? {
            let p = page(&r);
            // A word of the title says more about a page than a word of its body.
            let in_title = if p.title.to_lowercase().contains(word.as_str()) {
                3.0
            } else {
                0.0
            };
            let in_body = if p.body.to_lowercase().contains(word.as_str()) {
                1.0
            } else {
                0.0
            };
            scored.entry(p.slug.clone()).or_insert((p, 0.0)).1 += in_title + in_body;
        }
    }
    let mut found: Vec<(PageRec, f64)> = scored.into_values().collect();
    found.sort_by(|(a, x), (b, y)| y.total_cmp(x).then(a.slug.cmp(&b.slug)));
    found.truncate(limit);
    Ok(found)
}
