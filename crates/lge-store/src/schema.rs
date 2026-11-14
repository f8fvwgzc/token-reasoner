//! Schema version 2: the tables, the endpoints of every relationship, and the
//! migration from older versions.
//!
//! The graph and staging tables hold nothing that cannot be rebuilt from the
//! source tree, so an older database is migrated by dropping and recreating
//! them; the next index run fills them again. The usage log and the handoff
//! notes cannot be rebuilt and are kept.

use crate::values::{col_s, rows, run, st, vlist};
use anyhow::{bail, Context, Result};
use lbug::Connection;
use lge_core::{EdgeType, NodeLabel};

pub const SCHEMA_VERSION: i64 = 2;

/// The one full-text index, over the Symbol table.
pub(crate) const FTS_INDEX: &str = "sym_fts";
pub(crate) const FTS_COLUMNS: [&str; 4] = ["name", "qualified_name", "signature", "doc"];

/// Columns of every relationship table except CONTAINS.
const PROVENANCE: &str = "confidence DOUBLE, method STRING, evidence_file STRING, start_line INT64, end_line INT64, \
     commit STRING, created_at INT64";

/// Holds `schema_version`. Created first, so the version can be read before
/// anything else is decided.
const META_TABLE: (&str, &str) = ("Meta", "key STRING, value STRING, PRIMARY KEY(key)");

/// Tables a migration keeps: they hold what cannot be rebuilt by indexing.
const KEPT_TABLES: &[(&str, &str)] = &[
    (
        "UsageRecord",
        "id STRING, task_id STRING, task STRING, attempt INT64, level STRING, reason STRING, runner STRING, \
         model STRING, session_id STRING, started_at INT64, finished_at INT64, input_tokens INT64, \
         cached_input_tokens INT64, reasoning_tokens INT64, output_tokens INT64, pack_tokens INT64, \
         outcome STRING, escalated BOOLEAN, files STRING[], services STRING[], PRIMARY KEY(id)",
    ),
    (
        "HandoffNote",
        "id STRING, task_id STRING, task STRING, created_at INT64, text STRING, PRIMARY KEY(id)",
    ),
];

/// Graph, index-run and staging tables: dropped by a migration and rebuilt by
/// the next index run.
///
/// `File` and `Symbol` repeat `service` and `file_path` so lookups do not need
/// a join. The derived tables (`ApiEndpoint`, `DbTable`, `Queue`) carry the
/// service and source location the resolver gave them. `IndexRun.seq` orders
/// runs that start within the same second.
const GRAPH_TABLES: &[(&str, &str)] = &[
    (
        "Repository",
        "id STRING, path STRING, head_commit STRING, PRIMARY KEY(id)",
    ),
    ("Service", "id STRING, name STRING, root STRING, PRIMARY KEY(id)"),
    (
        "File",
        "id STRING, path STRING, service STRING, lang STRING, content_hash STRING, indexed_commit STRING, \
         indexed_at INT64, PRIMARY KEY(id)",
    ),
    (
        "Symbol",
        "id STRING, name STRING, qualified_name STRING, kind STRING, signature STRING, doc STRING, body STRING, \
         file_path STRING, service STRING, start_line INT64, end_line INT64, is_test BOOLEAN, PRIMARY KEY(id)",
    ),
    (
        "ApiEndpoint",
        "id STRING, name STRING, method STRING, route STRING, service STRING, file_path STRING, start_line INT64, \
         end_line INT64, PRIMARY KEY(id)",
    ),
    (
        "DbTable",
        "id STRING, name STRING, service STRING, file_path STRING, start_line INT64, end_line INT64, PRIMARY KEY(id)",
    ),
    (
        "Queue",
        "id STRING, name STRING, kind STRING, service STRING, file_path STRING, start_line INT64, end_line INT64, \
         PRIMARY KEY(id)",
    ),
    (
        "IndexRun",
        "id STRING, seq INT64, started_at INT64, finished_at INT64, status STRING, files_changed INT64, \
         schema_version INT64, PRIMARY KEY(id)",
    ),
    (
        "CallSite",
        "id STRING, caller_id STRING, file_path STRING, name STRING, receiver STRING, line INT64, PRIMARY KEY(id)",
    ),
    (
        "ImportSite",
        "id STRING, file_path STRING, module STRING, name STRING, alias STRING, PRIMARY KEY(id)",
    ),
    (
        "FactSite",
        "id STRING, symbol_id STRING, file_path STRING, kind STRING, key STRING, detail STRING, line INT64, \
         end_line INT64, literal BOOLEAN, PRIMARY KEY(id)",
    ),
];

/// The (from, to) node tables of a relationship table, as in `docs/PLAN.md`
/// section 4. Only CONTAINS has more than one pair.
pub(crate) fn rel_pairs(t: EdgeType) -> &'static [(NodeLabel, NodeLabel)] {
    use NodeLabel::{ApiEndpoint, DbTable, File, Queue, Repository, Service, Symbol};
    match t {
        EdgeType::Contains => &[(Repository, Service), (Service, File), (File, Symbol)],
        EdgeType::Imports => &[(File, File)],
        EdgeType::Calls | EdgeType::Inherits | EdgeType::Implements | EdgeType::References | EdgeType::TestedBy => {
            &[(Symbol, Symbol)]
        }
        EdgeType::ExposesApi | EdgeType::CallsApi => &[(Symbol, ApiEndpoint)],
        EdgeType::ReadsFrom | EdgeType::WritesTo => &[(Symbol, DbTable)],
        EdgeType::Publishes | EdgeType::Consumes => &[(Symbol, Queue)],
        EdgeType::DependsOn => &[(Service, Service)],
    }
}

fn rel_ddl(t: EdgeType) -> String {
    let pairs: Vec<String> = rel_pairs(t)
        .iter()
        .map(|(from, to)| format!("FROM {} TO {}", from.as_str(), to.as_str()))
        .collect();
    let columns = if t == EdgeType::Contains {
        String::new()
    } else {
        format!(", {PROVENANCE}")
    };
    format!(
        "CREATE REL TABLE IF NOT EXISTS {}({}{});",
        t.as_str(),
        pairs.join(", "),
        columns
    )
}

/// Bring the database to [`SCHEMA_VERSION`]: migrate an older one, create
/// whatever is missing, and record the version.
pub(crate) fn prepare(conn: &Connection, fts_loaded: bool) -> Result<()> {
    create(conn, &[META_TABLE])?;
    match stored_version(conn)? {
        Some(v) if v > SCHEMA_VERSION => bail!(
            "the database has schema version {v}, but this build of lge only knows version {SCHEMA_VERSION}; \
             use a newer lge or a new database path"
        ),
        Some(v) if v < SCHEMA_VERSION => drop_graph(conn, fts_loaded)?,
        Some(_) => {}
        // No version row: a new database, or tables of unknown origin.
        None => {
            if has_graph_tables(conn)? {
                drop_graph(conn, fts_loaded)?;
            }
        }
    }
    create(conn, KEPT_TABLES)?;
    create(conn, GRAPH_TABLES)?;
    for t in EdgeType::ALL {
        run(conn, &rel_ddl(t))?;
    }
    rows(
        conn,
        "MERGE (m:Meta {key:'schema_version'}) SET m.value = $v;",
        vec![("v", st(&SCHEMA_VERSION.to_string()))],
    )?;
    Ok(())
}

fn create(conn: &Connection, tables: &[(&str, &str)]) -> Result<()> {
    for (name, columns) in tables {
        run(conn, &format!("CREATE NODE TABLE IF NOT EXISTS {name}({columns});"))?;
    }
    Ok(())
}

fn stored_version(conn: &Connection) -> Result<Option<i64>> {
    let found = rows(
        conn,
        "MATCH (m:Meta) WHERE m.key = 'schema_version' RETURN m.value;",
        vec![],
    )?;
    match found.first() {
        None => Ok(None),
        Some(row) => {
            let text = col_s(row, 0);
            let v = text
                .trim()
                .parse::<i64>()
                .with_context(|| format!("unreadable schema_version '{text}'"))?;
            Ok(Some(v))
        }
    }
}

fn has_graph_tables(conn: &Connection) -> Result<bool> {
    let existing = rows(conn, "CALL SHOW_TABLES() RETURN name;", vec![])?;
    Ok(existing.iter().any(|r| {
        let name = col_s(r, 0);
        GRAPH_TABLES.iter().any(|(t, _)| *t == name) || EdgeType::parse(&name).is_some()
    }))
}

/// Drop every graph, index-run and staging table. Each step is "if exists",
/// so a migration that was interrupted simply runs again on the next open.
fn drop_graph(conn: &Connection, fts_loaded: bool) -> Result<()> {
    // A table cannot be dropped while a full-text index refers to it, and the
    // index can only be dropped with the extension loaded.
    for (table, index, _) in fts_indexes(conn)? {
        if !fts_loaded {
            bail!(
                "the database was indexed with the fts extension (index {index} on {table}), which is not \
                 loaded; install it with `lge setup`, then open the database again to migrate it"
            );
        }
        drop_fts_index(conn, &table, &index)?;
    }
    for t in EdgeType::ALL {
        run(conn, &format!("DROP TABLE IF EXISTS {};", t.as_str()))?;
    }
    for (name, _) in GRAPH_TABLES {
        run(conn, &format!("DROP TABLE IF EXISTS {name};"))?;
    }
    Ok(())
}

/// (table, index name, indexed columns) of every full-text index.
pub(crate) fn fts_indexes(conn: &Connection) -> Result<Vec<(String, String, Vec<String>)>> {
    let all = rows(
        conn,
        "CALL SHOW_INDEXES() RETURN table_name, index_name, index_type, property_names;",
        vec![],
    )?;
    Ok(all
        .iter()
        .filter(|r| col_s(r, 2).eq_ignore_ascii_case("FTS"))
        .map(|r| (col_s(r, 0), col_s(r, 1), r.get(3).map(vlist).unwrap_or_default()))
        .collect())
}

pub(crate) fn drop_fts_index(conn: &Connection, table: &str, index: &str) -> Result<()> {
    run(
        conn,
        &format!("CALL DROP_FTS_INDEX('{}', '{}');", quoted(table), quoted(index)),
    )
}

/// Text for the inside of a single-quoted Cypher string literal.
pub(crate) fn quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}
