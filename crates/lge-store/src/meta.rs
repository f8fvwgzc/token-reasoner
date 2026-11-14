//! The indexed repository, the index runs, and the counts.

use crate::schema::SCHEMA_VERSION;
use crate::values::{col_i, col_s, count, int, rows, st};
use anyhow::{bail, Result};
use lbug::Connection;
use lge_core::{method, repository_id, stable_id, EdgeType, IndexRun};
use std::collections::HashMap;

/// Record the repository. There is one repository per database, so a node for
/// another path is replaced. Services that were indexed before the repository
/// was recorded are hung under it here.
pub(crate) fn set_repository(conn: &Connection, path: &str, head_commit: &str) -> Result<()> {
    let id = repository_id(path);
    rows(
        conn,
        "MATCH (r:Repository) WHERE r.id <> $id DETACH DELETE r;",
        vec![("id", st(&id))],
    )?;
    rows(
        conn,
        "MERGE (r:Repository {id:$id}) SET r.path = $path, r.head_commit = $commit;",
        vec![("id", st(&id)), ("path", st(path)), ("commit", st(head_commit))],
    )?;
    rows(
        conn,
        "MATCH (r:Repository {id:$id}), (s:Service) MERGE (r)-[:CONTAINS]->(s);",
        vec![("id", st(&id))],
    )?;
    Ok(())
}

pub(crate) fn repository(conn: &Connection) -> Result<Option<(String, String)>> {
    let found = rows(
        conn,
        "MATCH (r:Repository) RETURN r.path, r.head_commit LIMIT 1;",
        vec![],
    )?;
    Ok(found.first().map(|r| (col_s(r, 0), col_s(r, 1))))
}

/// Start a run. `seq` grows by one per run, so the last run is well defined
/// even when two runs start within the same second.
pub(crate) fn begin_run(conn: &Connection, now: i64) -> Result<String> {
    let seq = count(conn, "MATCH (r:IndexRun) RETURN max(r.seq);", vec![])? + 1;
    let id = stable_id(&["run", &seq.to_string(), &now.to_string()]);
    rows(
        conn,
        "CREATE (:IndexRun {id:$id, seq:$seq, started_at:$now, finished_at:0, status:'running', files_changed:0, \
         schema_version:$v});",
        vec![
            ("id", st(&id)),
            ("seq", int(seq)),
            ("now", int(now)),
            ("v", int(SCHEMA_VERSION)),
        ],
    )?;
    Ok(id)
}

pub(crate) fn finish_run(conn: &Connection, run_id: &str, now: i64, status: &str, files_changed: i64) -> Result<()> {
    let updated = count(
        conn,
        "MATCH (r:IndexRun {id:$id}) SET r.finished_at = $now, r.status = $status, r.files_changed = $n \
         RETURN count(r);",
        vec![
            ("id", st(run_id)),
            ("now", int(now)),
            ("status", st(status)),
            ("n", int(files_changed)),
        ],
    )?;
    if updated == 0 {
        bail!("unknown index run {run_id}");
    }
    Ok(())
}

/// The run that was started last.
pub(crate) fn last_run(conn: &Connection) -> Result<Option<IndexRun>> {
    let found = rows(
        conn,
        "MATCH (r:IndexRun) \
         RETURN r.id, r.started_at, r.finished_at, r.status, r.files_changed, r.schema_version \
         ORDER BY r.seq DESC LIMIT 1;",
        vec![],
    )?;
    Ok(found.first().map(|r| IndexRun {
        id: col_s(r, 0),
        started_at: col_i(r, 1),
        finished_at: col_i(r, 2),
        status: col_s(r, 3),
        files_changed: col_i(r, 4),
        schema_version: col_i(r, 5),
    }))
}

/// Counts as (label, count). The first seven labels are the ones version 1
/// printed, in the same order; node counts and the remaining edge types follow.
/// Placeholders are not counted as symbols or endpoints.
pub(crate) fn stats(conn: &Connection) -> Result<Vec<(String, i64)>> {
    // (edge type, method) -> count, for every relationship table at once.
    let mut edges: HashMap<(String, String), i64> = HashMap::new();
    for r in rows(conn, "MATCH ()-[r]->() RETURN label(r), r.method, count(*);", vec![])? {
        edges.insert((col_s(&r, 0), col_s(&r, 1)), col_i(&r, 2));
    }
    let edge = |t: EdgeType, m: &str| {
        edges
            .get(&(t.as_str().to_string(), m.to_string()))
            .copied()
            .unwrap_or(0)
    };
    let nodes = |q: &str| count(conn, q, vec![]);
    const METHODS: [&str; 3] = [method::STATIC, method::INFERRED, method::UNRESOLVED];

    let mut out = vec![
        ("files".to_string(), nodes("MATCH (f:File) RETURN count(f);")?),
        (
            "symbols".to_string(),
            nodes("MATCH (s:Symbol) WHERE s.kind <> 'unresolved' RETURN count(s);")?,
        ),
        ("call_sites".to_string(), nodes("MATCH (c:CallSite) RETURN count(c);")?),
        ("calls_static".to_string(), edge(EdgeType::Calls, method::STATIC)),
        ("calls_inferred".to_string(), edge(EdgeType::Calls, method::INFERRED)),
        (
            "calls_unresolved".to_string(),
            edge(EdgeType::Calls, method::UNRESOLVED),
        ),
        (
            "imports".to_string(),
            edges
                .iter()
                .filter(|((t, _), _)| t == EdgeType::Imports.as_str())
                .map(|(_, n)| n)
                .sum(),
        ),
        ("services".to_string(), nodes("MATCH (s:Service) RETURN count(s);")?),
        (
            "endpoints".to_string(),
            nodes("MATCH (e:ApiEndpoint) WHERE e.method <> 'unresolved' RETURN count(e);")?,
        ),
        ("tables".to_string(), nodes("MATCH (t:DbTable) RETURN count(t);")?),
        ("queues".to_string(), nodes("MATCH (q:Queue) RETURN count(q);")?),
        ("fact_sites".to_string(), nodes("MATCH (x:FactSite) RETURN count(x);")?),
    ];
    // CONTAINS is structure, CALLS and IMPORTS are listed above.
    let listed = [EdgeType::Contains, EdgeType::Calls, EdgeType::Imports];
    for t in EdgeType::ALL.into_iter().filter(|t| !listed.contains(t)) {
        for m in METHODS {
            let n = edge(t, m);
            if n > 0 {
                out.push((format!("{}_{m}", t.as_str().to_lowercase()), n));
            }
        }
    }
    Ok(out)
}
