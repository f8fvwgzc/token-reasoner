//! Seed search (BM25 through the `fts` extension, or a substring scorer) and
//! symbol lookup by name.

use crate::schema::SYMBOL_FTS;
use crate::values::{col_b, col_f, col_i, col_s, rows, st, Prepared, Row};
use anyhow::Result;
use lbug::{Connection, Value};
use lge_core::Hit;
use std::collections::{HashMap, HashSet};

pub(crate) const HIT_COLS: &str =
    "s.id, s.name, s.qualified_name, s.kind, s.file_path, s.service, s.start_line, s.end_line, s.is_test";

/// Words that say nothing about what a text is about.
const FUNCTION_WORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "when", "should", "not", "are", "can", "all", "any",
];

/// Words of a task description that name the work, not the code it is done
/// on: `fix the bug in ...`, `add support for ...`.
const TASK_WORDS: &[&str] = &[
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
    "need",
    "use",
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

/// The words of a text in lower case, in order and once each, without the
/// ones of fewer than three bytes and the ones `left_out` names.
fn words(text: &str, left_out: &[&[&str]]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split(|c: char| !c.is_alphanumeric()) {
        let w = w.to_lowercase();
        let dropped = left_out.iter().any(|list| list.contains(&w.as_str()));
        if w.len() >= 3 && !dropped && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// Each word followed by the words `lge_core::split_words` cuts it into, once each.
fn with_parts(words: Vec<String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for word in words {
        let parts = lge_core::split_words(&word);
        for w in std::iter::once(word).chain(parts) {
            if seen.insert(w.clone()) {
                out.push(w);
            }
        }
    }
    out
}

/// Words of a task description that are worth searching for.
pub fn tokens(text: &str) -> Vec<String> {
    words(text, &[FUNCTION_WORDS, TASK_WORDS])
}

/// The words a symbol is searched by: the [`tokens`] of the query, each
/// followed by the words `lge_core::split_words` cuts it into. The index side
/// stores the same split in `Symbol.search_text`, so `order handler` finds
/// `getOrderHandler` by its words and `getOrderHandler` finds it by its name.
pub(crate) fn query_words(text: &str) -> Vec<String> {
    with_parts(tokens(text))
}

/// The words a fact or a page is searched by. Like [`query_words`], but the
/// words that name the work stay in: a fact may well be about a bug or a
/// feature.
pub(crate) fn prose_words(text: &str) -> Vec<String> {
    with_parts(words(text, &[FUNCTION_WORDS]))
}

/// Seed search, in one service when one is given. BM25 through the fts
/// extension when it is loaded, otherwise a substring match scored in Rust.
/// A BM25 error or an empty BM25 result also falls back to the substring match.
pub(crate) fn search_in(
    conn: &Connection,
    fts: bool,
    text: &str,
    service: Option<&str>,
    limit: usize,
) -> Result<Vec<Hit>> {
    let words = query_words(text);
    if words.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    if fts {
        if let Ok(hits) = search_fts(conn, &words, service, limit) {
            if !hits.is_empty() {
                return Ok(hits);
            }
        }
    }
    search_substring(conn, &words, service, limit)
}

/// The condition both backends share: real symbols, of one service when one is asked for.
fn searchable(service: Option<&str>) -> &'static str {
    match service {
        Some(_) => "s.kind <> 'module' AND s.kind <> 'unresolved' AND s.service = $service",
        None => "s.kind <> 'module' AND s.kind <> 'unresolved'",
    }
}

/// Bind `$service` when [`searchable`] refers to it.
fn with_service<'a>(mut params: Vec<(&'a str, Value)>, service: Option<&str>) -> Vec<(&'a str, Value)> {
    if let Some(service) = service {
        params.push(("service", st(service)));
    }
    params
}

fn search_fts(conn: &Connection, words: &[String], service: Option<&str>, limit: usize) -> Result<Vec<Hit>> {
    // The filter runs after the index is asked, so the index is not given a
    // `TOP`: it would cut the list before modules and other services are removed.
    let q = format!(
        "CALL QUERY_FTS_INDEX('{}', '{}', $q) WITH node AS s, score WHERE {} \
         RETURN {HIT_COLS}, score ORDER BY score DESC, s.qualified_name, s.id LIMIT {limit};",
        SYMBOL_FTS.table,
        SYMBOL_FTS.name,
        searchable(service)
    );
    let params = with_service(vec![("q", st(&words.join(" ")))], service);
    Ok(rows(conn, &q, params)?.iter().map(|r| hit(r, col_f(r, 9))).collect())
}

/// What one query word is worth on one symbol. The name counts most, then the
/// qualified name, then the doc line; a match in the signature or only in the
/// search text (a word of a camelCase name, of the path, of a rationale
/// comment) still makes the symbol a hit.
fn substring_score(name: &str, qualified_name: &str, doc: &str, signature: &str, word: &str) -> f64 {
    let mut score = 0.0;
    if name == word {
        score += 5.0;
    } else if name.split('_').any(|part| part == word) {
        score += 4.0;
    } else if name.contains(word) {
        score += 3.0;
    } else if qualified_name.contains(word) {
        score += 1.5;
    }
    if doc.contains(word) {
        score += 1.0;
    }
    // The full-text index covers the signature and the search text, so the fallback does too.
    if score == 0.0 || signature.contains(word) {
        score += 0.5;
    }
    score
}

fn search_substring(conn: &Connection, words: &[String], service: Option<&str>, limit: usize) -> Result<Vec<Hit>> {
    // `search_text` is stored in lower case already.
    let q = format!(
        "MATCH (s:Symbol) WHERE {} \
         AND (lower(s.name) CONTAINS $t OR lower(s.qualified_name) CONTAINS $t OR lower(s.doc) CONTAINS $t \
         OR lower(s.signature) CONTAINS $t OR s.search_text CONTAINS $t) \
         RETURN {HIT_COLS}, s.doc, s.signature;",
        searchable(service)
    );
    let mut stmt = Prepared::new(conn, &q)?;
    let mut scored: HashMap<String, Hit> = HashMap::new();
    for word in words {
        for r in stmt.rows(with_service(vec![("t", st(word))], service))? {
            let score = substring_score(
                &col_s(&r, 1).to_lowercase(),
                &col_s(&r, 2).to_lowercase(),
                &col_s(&r, 9).to_lowercase(),
                &col_s(&r, 10).to_lowercase(),
                word,
            );
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
