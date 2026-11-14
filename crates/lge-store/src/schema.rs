//! Schema version 3: the tables, the endpoints of every relationship, the
//! full-text indexes, and the migration from older versions.
//!
//! The graph and staging tables hold nothing that cannot be rebuilt from the
//! source tree, so an older database is migrated by dropping and recreating
//! them; the next index run fills them again. The usage log, the handoff
//! notes, the remembered facts and the pages cannot be rebuilt and are kept.

use crate::values::{col_s, count, rows, run, st, vlist};
use anyhow::{bail, Context, Result};
use lbug::Connection;
use lge_core::{EdgeType, NodeLabel};

pub const SCHEMA_VERSION: i64 = 3;

/// A full-text index: the table, the name of the index and the columns it covers.
pub(crate) struct FtsIndex {
    pub table: &'static str,
    pub name: &'static str,
    pub columns: &'static [&'static str],
}

/// `search_text` holds the words of the identifiers, so `order handler` finds
/// `getOrderHandler`; the raw columns keep whole names findable.
pub(crate) const SYMBOL_FTS: FtsIndex = FtsIndex {
    table: "Symbol",
    name: "sym_fts",
    columns: &["name", "qualified_name", "signature", "doc", "search_text"],
};

/// `signals` holds the names of the code a fact mentions, so the fact is
/// found by them as well as by its own words.
pub(crate) const MEMORY_FTS: FtsIndex = FtsIndex {
    table: "Memory",
    name: "mem_fts",
    columns: &["text", "key", "tags_text", "signals"],
};

pub(crate) const PAGE_FTS: FtsIndex = FtsIndex {
    table: "Page",
    name: "page_fts",
    columns: &["title", "body"],
};

pub(crate) const FTS_INDEXES: [&FtsIndex; 3] = [&SYMBOL_FTS, &MEMORY_FTS, &PAGE_FTS];

/// Relationship from a remembered fact to the code it mentions. It is derived
/// from `Memory.links` and rebuilt from there whenever its targets come back.
pub(crate) const MENTIONS: &str = "MENTIONS";

/// Node tables a fact can mention.
pub(crate) const MENTION_TARGETS: [NodeLabel; 3] = [NodeLabel::Symbol, NodeLabel::File, NodeLabel::Service];

/// Columns of every relationship table except CONTAINS.
const PROVENANCE: &str = "confidence DOUBLE, method STRING, evidence_file STRING, start_line INT64, end_line INT64, \
     commit STRING, created_at INT64";

/// Holds `schema_version` and the vector length of each embedded table.
/// Created first, so the version can be read before anything else is decided.
const META_TABLE: (&str, &str) = ("Meta", "key STRING, value STRING, PRIMARY KEY(key)");

/// Tables a migration keeps: they hold what cannot be rebuilt by indexing.
///
/// `Memory.tags` and `Memory.links` are JSON lists; `tags_text` repeats the
/// tags as words for the full-text index. A page is known by its slug.
const KEPT_TABLES: &[(&str, &str)] = &[
    (
        "UsageRecord",
        "id STRING, task_id STRING, task STRING, attempt INT64, level STRING, reason STRING, runner STRING, \
         model STRING, session_id STRING, started_at INT64, finished_at INT64, input_tokens INT64, \
         cached_input_tokens INT64, reasoning_tokens INT64, output_tokens INT64, pack_tokens INT64, \
         outcome STRING, escalated BOOLEAN, files STRING[], services STRING[], \
         recommended_level STRING DEFAULT '', mode STRING DEFAULT '', PRIMARY KEY(id)",
    ),
    (
        "HandoffNote",
        "id STRING, task_id STRING, task STRING, created_at INT64, text STRING, PRIMARY KEY(id)",
    ),
    (
        "Memory",
        "id STRING, kind STRING, text STRING, key STRING, tags STRING, tags_text STRING, source STRING, \
         created_at INT64, valid_at INT64, invalid_at INT64, expired_at INT64, proof_count INT64, links STRING, \
         signals STRING, embedding FLOAT[], PRIMARY KEY(id)",
    ),
    (
        "Page",
        "slug STRING, id STRING, title STRING, body STRING, tags STRING, kind STRING, updated_at INT64, \
         last_memory_seen_at INT64, PRIMARY KEY(slug)",
    ),
];

/// Columns that kept tables gained after their first release, as
/// (table, column, type and default). A kept table is never recreated, so an
/// older one gets them added; rows written before read the default.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("UsageRecord", "recommended_level", "STRING DEFAULT ''"),
    ("UsageRecord", "mode", "STRING DEFAULT ''"),
];

/// Graph, index-run and staging tables: dropped by a migration and rebuilt by
/// the next index run.
///
/// `File` and `Symbol` repeat `service` and `file_path` so lookups do not need
/// a join. `Symbol.search_text` is what the full-text index reads besides the
/// raw columns; `rank`, `community` and `degree` are written by the ranking
/// step and start at "not computed". The derived tables (`ApiEndpoint`,
/// `DbTable`, `Queue`) carry the service and source location the resolver gave
/// them. `IndexRun.seq` orders runs that start within the same second.
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
         file_path STRING, service STRING, start_line INT64, end_line INT64, is_test BOOLEAN, \
         search_text STRING DEFAULT '', rank DOUBLE DEFAULT 0.0, community INT64 DEFAULT -1, \
         degree INT64 DEFAULT 0, embedding FLOAT[], PRIMARY KEY(id)",
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

/// MENTIONS has no columns: what a fact says about the code is in the fact.
fn mentions_ddl() -> String {
    let pairs: Vec<String> = MENTION_TARGETS
        .iter()
        .map(|to| format!("FROM Memory TO {}", to.as_str()))
        .collect();
    format!("CREATE REL TABLE IF NOT EXISTS {MENTIONS}({});", pairs.join(", "))
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
    add_columns(conn)?;
    create(conn, GRAPH_TABLES)?;
    for t in EdgeType::ALL {
        run(conn, &rel_ddl(t))?;
    }
    run(conn, &mentions_ddl())?;
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

/// Give kept tables of an older database the columns they lack. A column
/// that is already there is left alone, so this runs on every open.
fn add_columns(conn: &Connection) -> Result<()> {
    for (table, column, definition) in ADDED_COLUMNS {
        run(
            conn,
            &format!("ALTER TABLE {table} ADD IF NOT EXISTS {column} {definition};"),
        )?;
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

fn is_graph_table(name: &str) -> bool {
    GRAPH_TABLES.iter().any(|(t, _)| *t == name)
}

fn has_graph_tables(conn: &Connection) -> Result<bool> {
    let existing = rows(conn, "CALL SHOW_TABLES() RETURN name;", vec![])?;
    Ok(existing.iter().any(|r| {
        let name = col_s(r, 0);
        is_graph_table(&name) || EdgeType::parse(&name).is_some()
    }))
}

/// Drop every graph, index-run and staging table. Each step is "if exists",
/// so a migration that was interrupted simply runs again on the next open.
///
/// The facts and the pages stay, with their full-text indexes. MENTIONS goes,
/// because it points into the tables that go; the next `replace_edges`
/// rebuilds it from `Memory.links`.
fn drop_graph(conn: &Connection, fts_loaded: bool) -> Result<()> {
    // A table cannot be dropped while a full-text index refers to it, and the
    // index can only be dropped with the extension loaded.
    for (table, index, _) in fts_indexes(conn)? {
        if !is_graph_table(&table) {
            continue;
        }
        if !fts_loaded {
            bail!(
                "the database was indexed with the fts extension (index {index} on {table}), which is not \
                 loaded; install it with `lge setup`, then open the database again to migrate it"
            );
        }
        drop_fts_index(conn, &table, &index)?;
    }
    run(conn, &format!("DROP TABLE IF EXISTS {MENTIONS};"))?;
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

/// Make sure one full-text index exists and covers the right columns. Needs
/// the extension loaded and must run outside a transaction.
///
/// LadybugDB keeps an index up to date on every insert, update and delete,
/// so "refreshing" it means creating it when it is missing, not rebuilding it.
pub(crate) fn ensure_fts_index(conn: &Connection, wanted: &FtsIndex) -> Result<()> {
    create_or_replace(conn, wanted, &fts_indexes(conn)?)
}

/// Make sure every table that has rows has its full-text index. Same
/// conditions as [`ensure_fts_index`].
///
/// A table without rows gets none yet: there is nothing to find in it, and
/// building an index takes a quarter of a second, which a new database would
/// pay three times before its first use. The first write to a table builds
/// its index.
pub(crate) fn ensure_fts_indexes(conn: &Connection) -> Result<()> {
    let existing = fts_indexes(conn)?;
    for wanted in FTS_INDEXES {
        let indexed = existing
            .iter()
            .any(|(table, index, _)| table == wanted.table && index == wanted.name);
        let filled = || count(conn, &format!("MATCH (n:{}) RETURN count(n);", wanted.table), vec![]);
        if indexed || filled()? > 0 {
            create_or_replace(conn, wanted, &existing)?;
        }
    }
    Ok(())
}

/// Create `wanted` unless `existing` has it; an index of its name over other
/// columns is replaced.
fn create_or_replace(conn: &Connection, wanted: &FtsIndex, existing: &[(String, String, Vec<String>)]) -> Result<()> {
    let found = existing
        .iter()
        .find(|(table, index, _)| table == wanted.table && index == wanted.name);
    if let Some((table, index, columns)) = found {
        if columns == wanted.columns {
            return Ok(());
        }
        drop_fts_index(conn, table, index)?;
    }
    let columns: Vec<String> = wanted.columns.iter().map(|c| format!("'{c}'")).collect();
    run(
        conn,
        &format!(
            "CALL CREATE_FTS_INDEX('{}', '{}', [{}]);",
            wanted.table,
            wanted.name,
            columns.join(", ")
        ),
    )
}

/// Text for the inside of a single-quoted Cypher string literal.
pub(crate) fn quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}
