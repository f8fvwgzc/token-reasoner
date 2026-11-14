//! Opening databases that already exist: the current schema, an older one
//! that has to be migrated, and ones this build must not touch.

mod common;

use common::*;
use lbug::{Connection, Database, SystemConfig};
use lge_core::{Direction, EdgeType, HandoffNote, NodeLabel, UsageRecord};
use lge_store::{GraphStore, Store, SCHEMA_VERSION};
use std::path::Path;

/// The schema of version 1, as the first release of `lge-store` created it
/// (`git show 229b4bc:crates/lge-store/src/lib.rs`).
const V1_SCHEMA: &[&str] = &[
    "CREATE NODE TABLE IF NOT EXISTS Meta(key STRING, value STRING, PRIMARY KEY(key));",
    "CREATE NODE TABLE IF NOT EXISTS File(id STRING, path STRING, service STRING, lang STRING, content_hash STRING, indexed_at INT64, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS Symbol(id STRING, name STRING, qualified_name STRING, kind STRING, signature STRING, doc STRING, file_path STRING, service STRING, start_line INT64, end_line INT64, is_test BOOLEAN, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS CallSite(id STRING, caller_id STRING, file_path STRING, name STRING, receiver STRING, line INT64, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS ImportSite(id STRING, file_path STRING, module STRING, name STRING, alias STRING, PRIMARY KEY(id));",
    "CREATE REL TABLE IF NOT EXISTS CONTAINS(FROM File TO Symbol);",
    "CREATE REL TABLE IF NOT EXISTS IMPORTS(FROM File TO File, confidence DOUBLE, method STRING);",
    "CREATE REL TABLE IF NOT EXISTS CALLS(FROM Symbol TO Symbol, confidence DOUBLE, method STRING, evidence_file STRING, line INT64);",
];

/// What version 1 left behind after `lge index`: files, symbols, staged sites,
/// edges, a placeholder, and the full-text index when the extension was there.
const V1_DATA: &[&str] = &[
    "CREATE (:File {id:'f1', path:'work/service.py', service:'work', lang:'python', content_hash:'h1', indexed_at:1});",
    "CREATE (:File {id:'f2', path:'work/repo.py', service:'work', lang:'python', content_hash:'h2', indexed_at:1});",
    "CREATE (:Symbol {id:'s1', name:'create_task', qualified_name:'work.service.create_task', kind:'function', signature:'def create_task()', doc:'Creates a task', file_path:'work/service.py', service:'work', start_line:3, end_line:9, is_test:false});",
    "CREATE (:Symbol {id:'s2', name:'save_task', qualified_name:'work.repo.save_task', kind:'function', signature:'def save_task()', doc:'', file_path:'work/repo.py', service:'work', start_line:2, end_line:5, is_test:false});",
    "CREATE (:Symbol {id:'unresolved:log', name:'log', qualified_name:'log', kind:'unresolved', signature:'', doc:'', file_path:'', service:'', start_line:0, end_line:0, is_test:false});",
    "CREATE (:CallSite {id:'c1', caller_id:'s1', file_path:'work/service.py', name:'save_task', receiver:'', line:5});",
    "CREATE (:ImportSite {id:'i1', file_path:'work/service.py', module:'.repo', name:'save_task', alias:'save_task'});",
    "MATCH (f:File {id:'f1'}), (s:Symbol {id:'s1'}) CREATE (f)-[:CONTAINS]->(s);",
    "MATCH (f:File {id:'f2'}), (s:Symbol {id:'s2'}) CREATE (f)-[:CONTAINS]->(s);",
    "MATCH (a:File {id:'f1'}), (b:File {id:'f2'}) CREATE (a)-[:IMPORTS {confidence:0.9, method:'static'}]->(b);",
    "MATCH (a:Symbol {id:'s1'}), (b:Symbol {id:'s2'}) CREATE (a)-[:CALLS {confidence:0.9, method:'static', evidence_file:'work/service.py', line:5}]->(b);",
    "MATCH (a:Symbol {id:'s1'}), (b:Symbol {id:'unresolved:log'}) CREATE (a)-[:CALLS {confidence:0.0, method:'unresolved', evidence_file:'work/service.py', line:6}]->(b);",
];

/// Build a database directly, the way an older binary would have.
fn create_database(path: &Path, statements: &[&[&str]], version: Option<&str>, with_fts_index: bool) -> bool {
    let db = Database::new(path, SystemConfig::default()).unwrap();
    let conn = Connection::new(&db).unwrap();
    for q in statements.iter().flat_map(|s| s.iter()) {
        conn.query(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    if let Some(v) = version {
        conn.query(&format!("MERGE (m:Meta {{key:'schema_version'}}) SET m.value='{v}';"))
            .unwrap();
    }
    // Version 1 built this index after every `lge index` when fts was loaded.
    let fts = with_fts_index && conn.query("LOAD fts;").is_ok();
    if fts {
        conn.query("CALL CREATE_FTS_INDEX('Symbol', 'sym_fts', ['name', 'qualified_name', 'signature', 'doc']);")
            .unwrap();
    }
    fts
}

fn rows(store: &Store, cypher: &str) -> Vec<String> {
    store
        .raw(cypher)
        .unwrap()
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn stored_version(store: &Store) -> Vec<String> {
    rows(store, "MATCH (m:Meta) WHERE m.key = 'schema_version' RETURN m.value;")
}

/// The store works as a version 2 store: index the fixture, resolve, read back.
fn assert_usable(store: &Store) {
    let fx = fixture();
    store.set_repository("/repo/polyglot", COMMIT).unwrap();
    store.insert_parsed_bulk(&fx.files, COMMIT, NOW).unwrap();
    let resolved = resolved(&fx);
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    assert_eq!(stat(store, "files"), fx.files.len() as i64);
    assert_eq!(stat(store, "symbols"), fx.symbol_count() as i64);
    let from: Vec<String> = resolved.edges.iter().map(|e| e.from.clone()).collect();
    assert_eq!(
        sorted(store.edges(&from, &[], Direction::Out).unwrap()),
        sorted(resolved.edges.clone())
    );
    let create_order = fx.symbol("orders_api.routes.create_order");
    assert_eq!(
        store.symbol_body(&create_order.id).unwrap(),
        Some(create_order.body.clone())
    );
    assert_eq!(store.search("create order", 5).unwrap()[0].name, "create_order");
}

fn assert_empty_graph(store: &Store) {
    assert!(
        store.stats().unwrap().iter().all(|(_, n)| *n == 0),
        "{:?}",
        store.stats().unwrap()
    );
    assert!(store.file_hashes().unwrap().is_empty());
    assert_eq!(store.repository().unwrap(), None);
    assert_eq!(store.last_run().unwrap(), None);
    let input = store.load_resolve_input().unwrap();
    assert!(input.files.is_empty() && input.symbols.is_empty() && input.calls.is_empty());
    assert!(input.imports.is_empty() && input.facts.is_empty());
    for label in NodeLabel::ALL {
        assert!(store.nodes_by_label(label, 10).unwrap().is_empty(), "{label:?}");
    }
}

#[test]
fn a_new_database_gets_the_current_schema() {
    let dir = tempfile::tempdir().unwrap();
    // Parent directories are created.
    let path = dir.path().join("nested").join(".lge").join("graph.lbug");
    let store = Store::open(&path).unwrap();
    assert_eq!(SCHEMA_VERSION, 2);
    assert_eq!(stored_version(&store), vec!["2"]);
    assert_empty_graph(&store);

    let tables = rows(&store, "CALL SHOW_TABLES() RETURN name;");
    for t in EdgeType::ALL {
        assert!(tables.contains(&t.as_str().to_string()), "no relationship table {t}");
    }
    for label in NodeLabel::ALL {
        assert!(tables.contains(&label.as_str().to_string()), "no node table {label:?}");
    }
    for t in [
        "Meta",
        "IndexRun",
        "CallSite",
        "ImportSite",
        "FactSite",
        "UsageRecord",
        "HandoffNote",
    ] {
        assert!(tables.contains(&t.to_string()), "no table {t}");
    }
    assert_usable(&store);
}

#[test]
fn a_version_1_database_is_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let had_fts_index = create_database(&path, &[V1_SCHEMA, V1_DATA], Some("1"), true);
    println!("version 1 database with a full-text index: {had_fts_index}");

    let store = Store::open(&path).unwrap();
    assert_eq!(stored_version(&store), vec!["2"]);
    // The old graph is gone: it is rebuilt by the next index run.
    assert_empty_graph(&store);

    // The tables have the version 2 columns.
    store.raw("MATCH (s:Symbol) RETURN s.body;").unwrap();
    store.raw("MATCH (f:File) RETURN f.indexed_commit;").unwrap();
    store
        .raw("MATCH ()-[r:CALLS]->() RETURN r.start_line, r.end_line, r.commit, r.created_at;")
        .unwrap();
    store
        .raw("MATCH ()-[r:IMPORTS]->() RETURN r.evidence_file, r.start_line, r.commit;")
        .unwrap();
    store
        .raw("MATCH (:Service)-[r:CONTAINS]->(:File) RETURN count(r);")
        .unwrap();
    assert!(
        store.raw("MATCH ()-[r:CALLS]->() RETURN r.line;").is_err(),
        "the version 1 column is gone"
    );

    assert_usable(&store);

    // Opening the migrated database again changes nothing.
    let stats = store.stats().unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.stats().unwrap(), stats);
    assert_eq!(stored_version(&store), vec!["2"]);
}

#[test]
fn migration_keeps_the_usage_log_and_the_handoff_notes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let usage = UsageRecord {
        id: "u1".to_string(),
        task_id: "t1".to_string(),
        task: "fix the orders endpoint".to_string(),
        attempt: 2,
        level: "top".to_string(),
        started_at: 1000,
        input_tokens: 123_456,
        escalated: true,
        files: vec![ROUTES_PY.to_string(), REPO_PY.to_string()],
        services: vec!["orders_api".to_string()],
        ..UsageRecord::default()
    };
    let note = HandoffNote {
        id: "n1".to_string(),
        task_id: "t1".to_string(),
        task: "fix the orders endpoint".to_string(),
        created_at: 1100,
        text: "left off at the repo layer".to_string(),
    };
    {
        // An older database that already has a usage log: the current tables
        // with an older version number.
        let store = Store::open(&path).unwrap();
        assert_usable(&store);
        let run = store.begin_run(NOW).unwrap();
        store.finish_run(&run, NOW + 1, "ok", 11).unwrap();
        store.put_usage(&usage).unwrap();
        store.put_handoff(&note).unwrap();
        store
            .raw("MATCH (m:Meta) WHERE m.key = 'schema_version' SET m.value = '1';")
            .unwrap();
    }

    let store = Store::open(&path).unwrap();
    assert_eq!(stored_version(&store), vec!["2"]);
    assert_empty_graph(&store);
    assert_eq!(store.list_usage(0).unwrap(), vec![usage]);
    assert_eq!(store.latest_handoffs(10).unwrap(), vec![note]);
    assert_usable(&store);
}

#[test]
fn tables_without_a_version_are_treated_as_old() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    // Version 1 tables, but the version row is missing.
    create_database(&path, &[&V1_SCHEMA[1..], V1_DATA], None, false);

    let store = Store::open(&path).unwrap();
    assert_eq!(stored_version(&store), vec!["2"]);
    assert_empty_graph(&store);
    assert_usable(&store);
}

#[test]
fn reopening_a_database_keeps_everything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let fx = fixture();
    let resolved = resolved(&fx);
    let usage = UsageRecord {
        id: "u1".to_string(),
        task_id: "t1".to_string(),
        started_at: 5,
        ..UsageRecord::default()
    };
    let all_ids: Vec<String> = resolved
        .edges
        .iter()
        .flat_map(|e| [e.from.clone(), e.to.clone()])
        .collect();

    let (stats, edges, nodes, hashes, run) = {
        let store = Store::open(&path).unwrap();
        assert_usable(&store);
        let run = store.begin_run(NOW).unwrap();
        store.finish_run(&run, NOW + 3, "ok", fx.files.len() as i64).unwrap();
        store.put_usage(&usage).unwrap();
        (
            store.stats().unwrap(),
            store.edges(&all_ids, &EdgeType::ALL, Direction::Out).unwrap(),
            NodeLabel::ALL.map(|l| store.nodes_by_label(l, 1000).unwrap()),
            store.file_hashes().unwrap(),
            store.last_run().unwrap(),
        )
    };

    let store = Store::open(&path).unwrap();
    assert_eq!(stored_version(&store), vec!["2"]);
    assert_eq!(store.stats().unwrap(), stats);
    assert_eq!(store.edges(&all_ids, &EdgeType::ALL, Direction::Out).unwrap(), edges);
    assert_eq!(NodeLabel::ALL.map(|l| store.nodes_by_label(l, 1000).unwrap()), nodes);
    assert_eq!(store.file_hashes().unwrap(), hashes);
    assert_eq!(store.last_run().unwrap(), run);
    assert_eq!(
        store.repository().unwrap(),
        Some(("/repo/polyglot".to_string(), COMMIT.to_string()))
    );
    assert_eq!(store.list_usage(0).unwrap(), vec![usage]);
    let create_order = fx.symbol("orders_api.routes.create_order");
    assert_eq!(
        store.symbol_body(&create_order.id).unwrap(),
        Some(create_order.body.clone())
    );
    assert_eq!(store.search("create order", 5).unwrap()[0].name, "create_order");
    assert_eq!(store.callers(&fx.id("orders_api.repo.save_order"), 2).unwrap().len(), 2);

    // And it can be written again: an incremental run on the reopened database.
    store.delete_file(CONSUMER_TS).unwrap();
    store.insert_parsed(fx.get(CONSUMER_TS), COMMIT, NOW + 100).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 100).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    assert_eq!(store.stats().unwrap(), stats);
    assert_eq!(store.edges(&all_ids, &EdgeType::ALL, Direction::Out).unwrap(), edges);
}

#[test]
fn a_newer_or_unreadable_schema_version_is_refused_untouched() {
    for bad in ["3", "not a number"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.lbug");
        let stats = {
            let store = Store::open(&path).unwrap();
            assert_usable(&store);
            let stats = store.stats().unwrap();
            store
                .raw(&format!(
                    "MATCH (m:Meta) WHERE m.key = 'schema_version' SET m.value = '{bad}';"
                ))
                .unwrap();
            stats
        };

        let err = Store::open(&path)
            .err()
            .unwrap_or_else(|| panic!("version '{bad}' was accepted"))
            .to_string();
        assert!(err.contains("schema"), "{err}");

        // Nothing was dropped: with the version put back, the data is all there.
        {
            let db = Database::new(&path, SystemConfig::default()).unwrap();
            let conn = Connection::new(&db).unwrap();
            conn.query("MATCH (m:Meta) WHERE m.key = 'schema_version' SET m.value = '2';")
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.stats().unwrap(), stats);
    }
}
