//! Embeddings of symbols and facts, and the search over them.
//!
//! The store keeps the vectors and compares them; it does not make them. An
//! embedding sits on the row it describes, so a re-indexed symbol (a new row)
//! has none and is offered for embedding again.
//!
//! The search is a full scan with `array_cosine_similarity`, computed inside
//! the database. That function needs both sides to have one fixed length, so
//! the length of a table's vectors is recorded in `Meta` when the first ones
//! are stored, and a vector of another length is refused.

use crate::values::{col_f, col_s, floats, rows, st, Prepared};
use anyhow::{bail, Context, Result};
use lbug::{Connection, Value};
use lge_core::VectorTable;

fn table_name(table: VectorTable) -> &'static str {
    match table {
        VectorTable::Symbol => "Symbol",
        VectorTable::Memory => "Memory",
    }
}

/// The `Meta` row that holds the vector length of a table.
fn length_key(table: VectorTable) -> &'static str {
    match table {
        VectorTable::Symbol => "vector_length_symbol",
        VectorTable::Memory => "vector_length_memory",
    }
}

/// The vector length recorded for a table; `None` before its first embedding.
fn recorded_length(conn: &Connection, table: VectorTable) -> Result<Option<usize>> {
    let found = rows(
        conn,
        "MATCH (m:Meta) WHERE m.key = $key RETURN m.value;",
        vec![("key", st(length_key(table)))],
    )?;
    match found.first() {
        None => Ok(None),
        Some(row) => {
            let text = col_s(row, 0);
            let length = text
                .trim()
                .parse::<usize>()
                .with_context(|| format!("unreadable {} '{text}'", length_key(table)))?;
            Ok(Some(length))
        }
    }
}

/// The one length of the given vectors. Refuses an empty vector, vectors of
/// different lengths and numbers a similarity cannot be computed from.
fn common_length(items: &[(String, Vec<f32>)]) -> Result<Option<usize>> {
    let Some((_, first)) = items.first() else {
        return Ok(None);
    };
    for (id, vector) in items {
        if vector.is_empty() {
            bail!("the embedding of {id} is empty");
        }
        if vector.len() != first.len() {
            bail!(
                "embeddings of one table must have one length, but {id} has {} numbers and others have {}",
                vector.len(),
                first.len()
            );
        }
        if vector.iter().any(|f| !f.is_finite()) {
            bail!("the embedding of {id} holds a number that is not finite");
        }
    }
    Ok(Some(first.len()))
}

/// Store embeddings by row id. Unknown ids are skipped. Must run inside a
/// transaction: the vectors are stored together with the length they fix.
pub(crate) fn set_embeddings(conn: &Connection, table: VectorTable, items: &[(String, Vec<f32>)]) -> Result<()> {
    let Some(length) = common_length(items)? else {
        return Ok(());
    };
    match recorded_length(conn, table)? {
        Some(recorded) if recorded != length => bail!(
            "{} embeddings have {recorded} numbers, but these have {length}; one table holds vectors of one \
             length (one embedding model)",
            table_name(table)
        ),
        Some(_) => {}
        None => {
            rows(
                conn,
                "MERGE (m:Meta {key:$key}) SET m.value = $value;",
                vec![("key", st(length_key(table))), ("value", st(&length.to_string()))],
            )?;
        }
    }
    let q = format!("MATCH (n:{} {{id:$id}}) SET n.embedding = $vector;", table_name(table));
    let mut stmt = Prepared::new(conn, &q)?;
    for (id, vector) in items {
        stmt.run(vec![("id", st(id)), ("vector", floats(vector))])?;
    }
    Ok(())
}

/// Rows that have no embedding yet, at most `limit`, as (id, text to embed).
///
/// A symbol is described by its qualified name, signature and doc line,
/// joined by newlines; modules and placeholders are not embedded. A fact is
/// described by its text, and only current facts are embedded.
pub(crate) fn missing(conn: &Connection, table: VectorTable, limit: usize) -> Result<Vec<(String, String)>> {
    let q = match table {
        VectorTable::Symbol => format!(
            "MATCH (n:Symbol) WHERE n.embedding IS NULL AND n.kind <> 'module' AND n.kind <> 'unresolved' \
             RETURN n.id, n.qualified_name, n.signature, n.doc ORDER BY n.qualified_name, n.id LIMIT {limit};"
        ),
        VectorTable::Memory => format!(
            "MATCH (n:Memory) WHERE n.embedding IS NULL AND n.expired_at = 0 \
             RETURN n.id, n.text ORDER BY n.created_at, n.id LIMIT {limit};"
        ),
    };
    let found = rows(conn, &q, vec![])?;
    Ok(found
        .iter()
        .map(|r| {
            let text = match table {
                VectorTable::Symbol => [col_s(r, 1), col_s(r, 2), col_s(r, 3)].join("\n"),
                VectorTable::Memory => col_s(r, 1),
            };
            (col_s(r, 0), text)
        })
        .collect())
}

/// Rows nearest to `query` by cosine similarity, as (id, similarity), best
/// first, similarity >= `min_similarity`. Empty when the table has no
/// embeddings. Expired facts are not searched: a superseded fact is not an
/// answer.
pub(crate) fn search(
    conn: &Connection,
    table: VectorTable,
    query: &[f32],
    limit: usize,
    min_similarity: f64,
) -> Result<Vec<(String, f64)>> {
    let Some(length) = recorded_length(conn, table)? else {
        return Ok(Vec::new());
    };
    if query.len() != length {
        bail!(
            "{} embeddings have {length} numbers, but the query vector has {}",
            table_name(table),
            query.len()
        );
    }
    if limit == 0 {
        return Ok(Vec::new());
    }
    let searched = match table {
        VectorTable::Symbol => "n.embedding IS NOT NULL",
        VectorTable::Memory => "n.embedding IS NOT NULL AND n.expired_at = 0",
    };
    // The length is a number this module recorded, not text from a caller.
    let q = format!(
        "MATCH (n:{}) WHERE {searched} \
         WITH n.id AS id, array_cosine_similarity(n.embedding, CAST($query AS FLOAT[{length}])) AS similarity \
         WHERE similarity >= $least RETURN id, similarity ORDER BY similarity DESC, id LIMIT {limit};",
        table_name(table)
    );
    let params = vec![("query", floats(query)), ("least", Value::Double(min_similarity))];
    Ok(rows(conn, &q, params)?
        .iter()
        .map(|r| (col_s(r, 0), col_f(r, 1)))
        .collect())
}
