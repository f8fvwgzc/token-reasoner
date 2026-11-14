//! Opening databases that already exist: the current schema, an older one
//! that has to be migrated, and ones this build must not touch.

mod common;

use common::*;
use lbug::{Connection, Database, SystemConfig};
use lge_core::{
    Direction, EdgeType, HandoffNote, MemoryRec, NodeLabel, PageRec, SymbolMetrics, UsageRecord, VectorTable,
};
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

/// The schema of version 2 (`crates/lge-store/src/schema.rs` at commit
/// 6076dc7): the node tables as that file spells them, and its relationship
/// tables, which all but CONTAINS share one list of provenance columns.
const V2_NODE_TABLES: &[&str] = &[
    "CREATE NODE TABLE IF NOT EXISTS Meta(key STRING, value STRING, PRIMARY KEY(key));",
    "CREATE NODE TABLE IF NOT EXISTS UsageRecord(id STRING, task_id STRING, task STRING, attempt INT64, level STRING, reason STRING, runner STRING, model STRING, session_id STRING, started_at INT64, finished_at INT64, input_tokens INT64, cached_input_tokens INT64, reasoning_tokens INT64, output_tokens INT64, pack_tokens INT64, outcome STRING, escalated BOOLEAN, files STRING[], services STRING[], PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS HandoffNote(id STRING, task_id STRING, task STRING, created_at INT64, text STRING, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS Repository(id STRING, path STRING, head_commit STRING, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS Service(id STRING, name STRING, root STRING, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS File(id STRING, path STRING, service STRING, lang STRING, content_hash STRING, indexed_commit STRING, indexed_at INT64, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS Symbol(id STRING, name STRING, qualified_name STRING, kind STRING, signature STRING, doc STRING, body STRING, file_path STRING, service STRING, start_line INT64, end_line INT64, is_test BOOLEAN, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS ApiEndpoint(id STRING, name STRING, method STRING, route STRING, service STRING, file_path STRING, start_line INT64, end_line INT64, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS DbTable(id STRING, name STRING, service STRING, file_path STRING, start_line INT64, end_line INT64, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS Queue(id STRING, name STRING, kind STRING, service STRING, file_path STRING, start_line INT64, end_line INT64, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS IndexRun(id STRING, seq INT64, started_at INT64, finished_at INT64, status STRING, files_changed INT64, schema_version INT64, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS CallSite(id STRING, caller_id STRING, file_path STRING, name STRING, receiver STRING, line INT64, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS ImportSite(id STRING, file_path STRING, module STRING, name STRING, alias STRING, PRIMARY KEY(id));",
    "CREATE NODE TABLE IF NOT EXISTS FactSite(id STRING, symbol_id STRING, file_path STRING, kind STRING, key STRING, detail STRING, line INT64, end_line INT64, literal BOOLEAN, PRIMARY KEY(id));",
];

const V2_PROVENANCE: &str =
    "confidence DOUBLE, method STRING, evidence_file STRING, start_line INT64, end_line INT64, commit STRING, created_at INT64";

/// (relationship table, its FROM ... TO ... pair) of version 2.
const V2_REL_TABLES: &[(&str, &str)] = &[
    ("IMPORTS", "FROM File TO File"),
    ("CALLS", "FROM Symbol TO Symbol"),
    ("INHERITS", "FROM Symbol TO Symbol"),
    ("IMPLEMENTS", "FROM Symbol TO Symbol"),
    ("REFERENCES", "FROM Symbol TO Symbol"),
    ("EXPOSES_API", "FROM Symbol TO ApiEndpoint"),
    ("CALLS_API", "FROM Symbol TO ApiEndpoint"),
    ("READS_FROM", "FROM Symbol TO DbTable"),
    ("WRITES_TO", "FROM Symbol TO DbTable"),
    ("PUBLISHES", "FROM Symbol TO Queue"),
    ("CONSUMES", "FROM Symbol TO Queue"),
    ("TESTED_BY", "FROM Symbol TO Symbol"),
    ("DEPENDS_ON", "FROM Service TO Service"),
];

fn v2_schema() -> Vec<String> {
    let mut statements: Vec<String> = V2_NODE_TABLES.iter().map(|s| s.to_string()).collect();
    statements.push(
        "CREATE REL TABLE IF NOT EXISTS CONTAINS(FROM Repository TO Service, FROM Service TO File, FROM File TO Symbol);"
            .to_string(),
    );
    for (name, pair) in V2_REL_TABLES {
        statements.push(format!(
            "CREATE REL TABLE IF NOT EXISTS {name}({pair}, {V2_PROVENANCE});"
        ));
    }
    statements
}

/// What version 2 left behind after `lge index` and one session: a graph with
/// provenance on every edge, staged sites, a run, a usage record and a note.
const V2_DATA: &[&str] = &[
    "CREATE (:Repository {id:'r1', path:'/repo/work', head_commit:'abc123'});",
    "CREATE (:Service {id:'sv1', name:'work', root:'work'}), (:Service {id:'sv2', name:'mail', root:'mail'});",
    "CREATE (:File {id:'f1', path:'work/service.py', service:'work', lang:'python', content_hash:'h1', indexed_commit:'abc123', indexed_at:1});",
    "CREATE (:File {id:'f2', path:'work/repo.py', service:'work', lang:'python', content_hash:'h2', indexed_commit:'abc123', indexed_at:1});",
    "CREATE (:Symbol {id:'s1', name:'create_task', qualified_name:'work.service.create_task', kind:'function', signature:'def create_task()', doc:'Creates a task', body:'def create_task(): ...', file_path:'work/service.py', service:'work', start_line:3, end_line:9, is_test:false});",
    "CREATE (:Symbol {id:'s2', name:'save_task', qualified_name:'work.repo.save_task', kind:'function', signature:'def save_task()', doc:'', body:'def save_task(): ...', file_path:'work/repo.py', service:'work', start_line:2, end_line:5, is_test:false});",
    "CREATE (:Symbol {id:'unresolved:log', name:'log', qualified_name:'log', kind:'unresolved', signature:'', doc:'', body:'', file_path:'', service:'', start_line:0, end_line:0, is_test:false});",
    "CREATE (:ApiEndpoint {id:'e1', name:'POST /tasks', method:'POST', route:'/tasks', service:'work', file_path:'work/service.py', start_line:2, end_line:3});",
    "CREATE (:DbTable {id:'t1', name:'tasks', service:'work', file_path:'work/repo.py', start_line:3, end_line:4});",
    "CREATE (:Queue {id:'q1', name:'task.created', kind:'queue', service:'work', file_path:'work/service.py', start_line:7, end_line:8});",
    "CREATE (:IndexRun {id:'run1', seq:1, started_at:10, finished_at:12, status:'ok', files_changed:2, schema_version:2});",
    "CREATE (:CallSite {id:'c1', caller_id:'s1', file_path:'work/service.py', name:'save_task', receiver:'', line:5});",
    "CREATE (:ImportSite {id:'i1', file_path:'work/service.py', module:'.repo', name:'save_task', alias:'save_task'});",
    "CREATE (:FactSite {id:'x1', symbol_id:'s1', file_path:'work/service.py', kind:'api_expose', key:'/tasks', detail:'POST', line:2, end_line:3, literal:true});",
    "MATCH (a:Repository {id:'r1'}), (b:Service) CREATE (a)-[:CONTAINS]->(b);",
    "MATCH (a:Service {id:'sv1'}), (b:File) CREATE (a)-[:CONTAINS]->(b);",
    "MATCH (f:File {id:'f1'}), (s:Symbol {id:'s1'}) CREATE (f)-[:CONTAINS]->(s);",
    "MATCH (f:File {id:'f2'}), (s:Symbol {id:'s2'}) CREATE (f)-[:CONTAINS]->(s);",
    "MATCH (a:File {id:'f1'}), (b:File {id:'f2'}) CREATE (a)-[:IMPORTS {confidence:0.9, method:'static', evidence_file:'work/service.py', start_line:1, end_line:1, commit:'abc123', created_at:11}]->(b);",
    "MATCH (a:Symbol {id:'s1'}), (b:Symbol {id:'s2'}) CREATE (a)-[:CALLS {confidence:0.9, method:'static', evidence_file:'work/service.py', start_line:5, end_line:5, commit:'abc123', created_at:11}]->(b);",
    "MATCH (a:Symbol {id:'s1'}), (b:Symbol {id:'unresolved:log'}) CREATE (a)-[:CALLS {confidence:0.0, method:'unresolved', evidence_file:'work/service.py', start_line:6, end_line:6, commit:'abc123', created_at:11}]->(b);",
    "MATCH (a:Symbol {id:'s1'}), (b:ApiEndpoint {id:'e1'}) CREATE (a)-[:EXPOSES_API {confidence:0.9, method:'static', evidence_file:'work/service.py', start_line:2, end_line:3, commit:'abc123', created_at:11}]->(b);",
    "MATCH (a:Symbol {id:'s2'}), (b:DbTable {id:'t1'}) CREATE (a)-[:WRITES_TO {confidence:0.5, method:'inferred', evidence_file:'work/repo.py', start_line:3, end_line:4, commit:'abc123', created_at:11}]->(b);",
    "MATCH (a:Symbol {id:'s1'}), (b:Queue {id:'q1'}) CREATE (a)-[:PUBLISHES {confidence:0.9, method:'static', evidence_file:'work/service.py', start_line:7, end_line:8, commit:'abc123', created_at:11}]->(b);",
    "MATCH (a:Service {id:'sv2'}), (b:Service {id:'sv1'}) CREATE (a)-[:DEPENDS_ON {confidence:0.6, method:'inferred', evidence_file:'mail/client.py', start_line:4, end_line:5, commit:'abc123', created_at:11}]->(b);",
    "CREATE (:UsageRecord {id:'u1', task_id:'t1', task:'fix the tasks endpoint', attempt:2, level:'top', reason:'more than one service', runner:'codex', model:'gpt-5-codex', session_id:'sess-1', started_at:1000, finished_at:1090, input_tokens:123456, cached_input_tokens:80000, reasoning_tokens:9000, output_tokens:4321, pack_tokens:5900, outcome:'success', escalated:true, files:['work/service.py', 'work/repo, with comma.py'], services:['work']});",
    "CREATE (:HandoffNote {id:'n1', task_id:'t1', task:'fix the tasks endpoint', created_at:1100, text:'left off at the repo layer'});",
];

/// The usage record and the note of [`V2_DATA`], as the current version reads them.
fn v2_usage_and_note() -> (UsageRecord, HandoffNote) {
    let usage = UsageRecord {
        id: "u1".to_string(),
        task_id: "t1".to_string(),
        task: "fix the tasks endpoint".to_string(),
        attempt: 2,
        level: "top".to_string(),
        reason: "more than one service".to_string(),
        runner: "codex".to_string(),
        model: "gpt-5-codex".to_string(),
        session_id: "sess-1".to_string(),
        started_at: 1000,
        finished_at: 1090,
        input_tokens: 123_456,
        cached_input_tokens: 80_000,
        reasoning_tokens: 9_000,
        output_tokens: 4_321,
        pack_tokens: 5_900,
        outcome: "success".to_string(),
        escalated: true,
        files: vec!["work/service.py".to_string(), "work/repo, with comma.py".to_string()],
        services: vec!["work".to_string()],
        // Version 2 did not record these: they read back empty.
        recommended_level: String::new(),
        mode: String::new(),
    };
    let note = HandoffNote {
        id: "n1".to_string(),
        task_id: "t1".to_string(),
        task: "fix the tasks endpoint".to_string(),
        created_at: 1100,
        text: "left off at the repo layer".to_string(),
    };
    (usage, note)
}

/// Build a version 2 database with [`V2_DATA`]; true when it has a full-text index.
fn create_v2_database(path: &Path) -> bool {
    let schema = v2_schema();
    let schema: Vec<&str> = schema.iter().map(String::as_str).collect();
    create_database(path, &[&schema, V2_DATA], Some("2"), true)
}

/// The full-text indexes of the database, as `table|name|columns`.
fn fts_indexes(store: &Store) -> Vec<String> {
    store
        .raw("CALL SHOW_INDEXES() RETURN table_name, index_name, property_names, index_type;")
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_suffix("|FTS"))
        .map(str::to_string)
        .collect()
}

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

/// The store works as a current store: index the fixture, resolve, read back.
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
    assert_eq!(SCHEMA_VERSION, 3);
    assert_eq!(stored_version(&store), vec!["3"]);
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
        "Memory",
        "Page",
        "MENTIONS",
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
    assert_eq!(stored_version(&store), vec!["3"]);
    // The old graph is gone: it is rebuilt by the next index run.
    assert_empty_graph(&store);

    // The tables have the columns of the current version.
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
    assert_eq!(stored_version(&store), vec!["3"]);
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
    assert_eq!(stored_version(&store), vec!["3"]);
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
    assert_eq!(stored_version(&store), vec!["3"]);
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
    assert_eq!(stored_version(&store), vec!["3"]);
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
    for bad in ["4", "not a number"] {
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
            conn.query("MATCH (m:Meta) WHERE m.key = 'schema_version' SET m.value = '3';")
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.stats().unwrap(), stats);
    }
}

#[test]
fn a_version_2_database_is_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let had_fts_index = create_v2_database(&path);
    println!("version 2 database with a full-text index: {had_fts_index}");
    let (usage, note) = v2_usage_and_note();

    let store = Store::open(&path).unwrap();
    assert_eq!(stored_version(&store), vec!["3"]);
    // The old graph, its staged sites and its runs are gone: the next index run rebuilds them.
    assert_empty_graph(&store);
    assert!(fts_indexes(&store).is_empty(), "the index went with its table");
    // What cannot be rebuilt is kept, and reads as a current record.
    assert_eq!(store.list_usage(0).unwrap(), vec![usage.clone()]);
    assert_eq!(store.latest_handoffs(10).unwrap(), vec![note.clone()]);

    // The tables have the columns of version 3, and the new tables are there.
    for query in [
        "MATCH (s:Symbol) RETURN s.body, s.search_text, s.rank, s.community, s.degree, s.embedding;",
        "MATCH (u:UsageRecord) RETURN u.recommended_level, u.mode;",
        "MATCH (m:Memory) RETURN m.id, m.kind, m.text, m.key, m.tags, m.tags_text, m.source, m.created_at, \
         m.valid_at, m.invalid_at, m.expired_at, m.proof_count, m.links, m.signals, m.embedding;",
        "MATCH (p:Page) RETURN p.slug, p.id, p.title, p.body, p.tags, p.kind, p.updated_at, p.last_memory_seen_at;",
        "MATCH (:Memory)-[r:MENTIONS]->(:Symbol) RETURN count(r);",
        "MATCH (:Memory)-[r:MENTIONS]->(:File) RETURN count(r);",
        "MATCH (:Memory)-[r:MENTIONS]->(:Service) RETURN count(r);",
        "MATCH ()-[r:CALLS]->() RETURN r.confidence, r.method, r.evidence_file, r.start_line, r.end_line, r.commit;",
    ] {
        store.raw(query).unwrap_or_else(|e| panic!("{query}: {e:#}"));
    }

    // It is a current store in every respect: the graph, and what version 3 added.
    assert_usable(&store);
    if store.fts_enabled() {
        assert_eq!(
            fts_indexes(&store),
            vec!["Symbol|sym_fts|[name,qualified_name,signature,doc,search_text]"]
        );
    }
    assert_eq!(store.search("order handler", 5).unwrap()[0].kind, "function");
    let fx = fixture();
    let create = fx.id("orders_api.routes.create_order");
    store
        .set_metrics(&[SymbolMetrics {
            id: create.clone(),
            rank: 0.5,
            community: 2,
            degree: 4,
        }])
        .unwrap();
    assert_eq!(store.top_ranked(None, None, 1).unwrap()[0].id, create);
    let fact = MemoryRec {
        links: vec![create.clone()],
        ..memory("m1", "Tasks became orders in the rewrite.", 2000)
    };
    store.put_memory(&fact).unwrap();
    assert_eq!(store.get_memories(&ids(&["m1"])).unwrap(), vec![fact]);
    store
        .put_page(&page("history", "History", "Tasks became orders."))
        .unwrap();
    assert_eq!(store.list_pages().unwrap().len(), 1);
    store
        .set_embeddings(VectorTable::Symbol, &[(create.clone(), vec![0.0, 1.0])])
        .unwrap();
    assert_eq!(
        store
            .vector_search(VectorTable::Symbol, &[0.0, 1.0], 3, 0.5)
            .unwrap()
            .len(),
        1
    );
    // The kept usage record takes the new fields like any other.
    let completed = UsageRecord {
        recommended_level: "medium".to_string(),
        mode: "pack".to_string(),
        ..usage.clone()
    };
    store.put_usage(&completed).unwrap();
    assert_eq!(store.list_usage(0).unwrap(), vec![completed.clone()]);

    // Opening the migrated database again changes nothing.
    let stats = store.stats().unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(stored_version(&store), vec!["3"]);
    assert_eq!(store.stats().unwrap(), stats);
    assert_eq!(store.list_usage(0).unwrap(), vec![completed]);
    assert_eq!(store.latest_handoffs(10).unwrap(), vec![note]);
    assert_eq!(store.memory_counts().unwrap(), (1, 0));
}

#[test]
fn a_version_2_database_with_an_index_is_not_migrated_without_the_extension() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    if !create_v2_database(&path) {
        // Without the extension on this machine the old database has no index to be in the way.
        let store = Store::open_without_fts(&path).unwrap();
        assert_eq!(stored_version(&store), vec!["3"]);
        return;
    }

    // The index can only be dropped with the extension loaded, and the tables
    // only without the index: the open fails and says what to do.
    let err = Store::open_without_fts(&path).expect_err("migrated without the extension");
    assert!(format!("{err:#}").contains("fts extension"), "{err:#}");
    // Nothing was dropped on the way to that error.
    {
        let db = Database::new(&path, SystemConfig::default()).unwrap();
        let conn = Connection::new(&db).unwrap();
        let symbols = conn.query("MATCH (s:Symbol) RETURN count(s);").unwrap().to_string();
        assert_eq!(symbols.lines().nth(1), Some("3"), "{symbols}");
        let version = conn
            .query("MATCH (m:Meta) WHERE m.key = 'schema_version' RETURN m.value;")
            .unwrap()
            .to_string();
        assert_eq!(version.lines().nth(1), Some("2"), "{version}");
    }
    // With the extension the same file migrates.
    let store = Store::open(&path).unwrap();
    assert_eq!(stored_version(&store), vec!["3"]);
    assert_empty_graph(&store);
    let (usage, note) = v2_usage_and_note();
    assert_eq!(store.list_usage(0).unwrap(), vec![usage]);
    assert_eq!(store.latest_handoffs(10).unwrap(), vec![note]);
    assert_usable(&store);
}

#[test]
fn a_migration_keeps_the_facts_and_the_pages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let fx = fixture();
    let resolved = resolved(&fx);
    let create = fx.id("orders_api.routes.create_order");
    let service = lge_core::service_id("orders_api");
    let fact = MemoryRec {
        key: "orders.retry-policy".to_string(),
        tags: vec!["decision".to_string()],
        links: vec![create.clone(), service.clone()],
        ..memory("m1", "Orders are retried twice before they are parked.", 100)
    };
    let old = MemoryRec {
        invalid_at: 90,
        expired_at: 95,
        ..memory("m0", "Orders are retried once.", 50)
    };
    let decisions = PageRec {
        tags: vec!["decision".to_string()],
        last_memory_seen_at: 100,
        ..page("decisions", "Decisions", "- Orders are retried twice.")
    };
    let mentions = |store: &Store| -> i64 { count(store, "MATCH (:Memory)-[r:MENTIONS]->() RETURN count(r);") };
    {
        // A database of a later day that a newer schema has to migrate: the
        // current tables, filled, with an older version number.
        let store = Store::open(&path).unwrap();
        assert_usable(&store);
        store.put_memory(&fact).unwrap();
        store.put_memory(&old).unwrap();
        store.put_page(&decisions).unwrap();
        store
            .set_embeddings(VectorTable::Memory, &[("m1".to_string(), vec![1.0, 0.0])])
            .unwrap();
        assert_eq!(mentions(&store), 2);
        store
            .raw("MATCH (m:Meta) WHERE m.key = 'schema_version' SET m.value = '2';")
            .unwrap();
    }

    let store = Store::open(&path).unwrap();
    assert_eq!(stored_version(&store), vec!["3"]);
    // The graph is gone ...
    assert_eq!((stat(&store, "files"), stat(&store, "symbols")), (0, 0));
    assert!(store.file_hashes().unwrap().is_empty());
    assert_eq!(mentions(&store), 0);
    // ... and everything that was remembered is still there. A fact reads
    // without links while there is nothing for them to lead to.
    assert_eq!(store.memory_counts().unwrap(), (1, 1));
    assert_eq!(
        store.get_memories(&ids(&["m1", "m0"])).unwrap(),
        vec![
            MemoryRec {
                links: Vec::new(),
                ..fact.clone()
            },
            old.clone()
        ]
    );
    assert_eq!(store.memories_by_key("orders.retry-policy").unwrap().len(), 1);
    assert_eq!(store.search_memories("retried parked", 5, false).unwrap()[0].0.id, "m1");
    assert_eq!(store.search_memories("retried", 5, true).unwrap().len(), 2);
    assert_eq!(store.list_pages().unwrap(), vec![decisions.clone()]);
    assert_eq!(store.search_pages("retried", 5).unwrap()[0].0, decisions);
    assert_eq!(
        store.vector_search(VectorTable::Memory, &[1.0, 0.0], 5, 0.5).unwrap(),
        vec![("m1".to_string(), 1.0)]
    );
    assert!(store
        .memories_mentioning(std::slice::from_ref(&create), 5)
        .unwrap()
        .is_empty());

    // The next index run brings the code back, and the facts are joined to it again.
    store.set_repository("/repo/polyglot", COMMIT).unwrap();
    store.insert_parsed_bulk(&fx.files, COMMIT, NOW).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    assert_eq!(mentions(&store), 2);
    assert_eq!(store.get_memories(&ids(&["m1"])).unwrap(), vec![fact.clone()]);
    let about = store.memories_mentioning(&[create, service], 5).unwrap();
    assert_eq!(about, vec![(fact, 2)]);
}

#[test]
fn version_2_usage_and_notes_survive_with_empty_new_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    create_v2_database(&path);
    let store = Store::open(&path).unwrap();
    let (usage, note) = v2_usage_and_note();
    assert_eq!(store.list_usage(0).unwrap(), vec![usage.clone()]);
    assert_eq!(store.latest_handoffs(10).unwrap(), vec![note]);
    // The new fields are written and read back after the migration.
    let routed = UsageRecord {
        recommended_level: "medium".to_string(),
        mode: "proxy".to_string(),
        ..usage
    };
    store.put_usage(&routed).unwrap();
    assert_eq!(store.list_usage(0).unwrap(), vec![routed]);
}
