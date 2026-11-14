//! Files, symbols and staged sites: what the indexer writes and reads back.

mod common;

use common::*;
use lge_core::{
    file_id, kind, lang, repository_id, service_id, Direction, EdgeType, FileInfo, NodeLabel, ParsedFile, SymbolRec,
};
use lge_store::{GraphStore, Store, SCHEMA_VERSION};
use std::collections::BTreeSet;

fn insert_all(store: &Store, fx: &Fixture) {
    for p in &fx.files {
        store.insert_parsed(p, COMMIT, NOW).unwrap();
    }
}

#[test]
fn inserted_files_read_back_intact() {
    let (_dir, store) = open();
    let fx = fixture();
    insert_all(&store, &fx);

    let hashes = store.file_hashes().unwrap();
    assert_eq!(hashes.len(), fx.files.len());
    for p in &fx.files {
        let f = p.file.as_ref().unwrap();
        assert_eq!(
            hashes.get(&f.path),
            Some(&(f.id.clone(), f.content_hash.clone())),
            "{}",
            f.path
        );
    }

    let f = fx.get(ROUTES_PY).file.as_ref().unwrap();
    assert_eq!(
        store.file_info(ROUTES_PY).unwrap(),
        Some(FileInfo {
            id: f.id.clone(),
            path: ROUTES_PY.to_string(),
            service: "orders_api".to_string(),
            lang: lang::PYTHON.to_string(),
            content_hash: f.content_hash.clone(),
            indexed_commit: COMMIT.to_string(),
            indexed_at: NOW,
        })
    );
    assert_eq!(store.file_info("orders_api/missing.py").unwrap(), None);

    // Symbols come back in start-line order with every field.
    let expected: Vec<_> = fx.get(ROUTES_PY).symbols.iter().map(node_of).collect();
    assert_eq!(store.symbols_in_file(ROUTES_PY).unwrap(), expected);
    assert!(store.symbols_in_file("orders_api/missing.py").unwrap().is_empty());

    let create_order = fx.symbol("orders_api.routes.create_order");
    assert!(create_order.body.contains("save_order(payload)"));
    assert_eq!(
        store.symbol_body(&create_order.id).unwrap(),
        Some(create_order.body.clone())
    );
    assert_eq!(
        store.symbol_body(&fx.id("orders_api.routes")).unwrap(),
        Some(String::new())
    );
    assert_eq!(store.symbol_body("no-such-id").unwrap(), None);

    assert_eq!(stat(&store, "files"), fx.files.len() as i64);
    assert_eq!(stat(&store, "symbols"), fx.symbol_count() as i64);
    assert_eq!(
        stat(&store, "call_sites"),
        fx.files.iter().map(|p| p.calls.len() as i64).sum::<i64>()
    );
    assert_eq!(
        stat(&store, "fact_sites"),
        fx.files.iter().map(|p| p.facts.len() as i64).sum::<i64>()
    );
    assert_eq!(stat(&store, "services"), 3);
}

#[test]
fn resolve_input_is_everything_that_was_staged() {
    let (_dir, store) = open();
    let fx = fixture();
    // Insert in reverse so the stored order differs from the expected order.
    for p in fx.files.iter().rev() {
        store.insert_parsed(p, COMMIT, NOW).unwrap();
    }
    let input = store.load_resolve_input().unwrap();

    let mut files: Vec<_> = fx.files.iter().map(|p| p.file.clone().unwrap()).collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(input.files, files);

    // Bodies are not loaded; everything else is.
    let mut symbols: Vec<SymbolRec> = fx
        .files
        .iter()
        .flat_map(|p| p.symbols.iter().cloned())
        .map(|s| SymbolRec {
            body: String::new(),
            ..s
        })
        .collect();
    symbols.sort_by(|a, b| (&a.file_path, a.start_line, &a.id).cmp(&(&b.file_path, b.start_line, &b.id)));
    assert_eq!(input.symbols, symbols);

    let mut calls: Vec<_> = fx.files.iter().flat_map(|p| p.calls.iter().cloned()).collect();
    calls.sort_by(|a, b| (&a.file_path, a.line, &a.id).cmp(&(&b.file_path, b.line, &b.id)));
    assert_eq!(input.calls, calls);

    let mut imports: Vec<_> = fx.files.iter().flat_map(|p| p.imports.iter().cloned()).collect();
    imports.sort_by(|a, b| {
        (&a.file_path, &a.module, &a.name, &a.alias, &a.id).cmp(&(&b.file_path, &b.module, &b.name, &b.alias, &b.id))
    });
    assert_eq!(input.imports, imports);

    let mut facts: Vec<_> = fx.files.iter().flat_map(|p| p.facts.iter().cloned()).collect();
    facts.sort_by(|a, b| (&a.file_path, a.line, &a.id).cmp(&(&b.file_path, b.line, &b.id)));
    assert_eq!(input.facts, facts);
    assert!(input.facts.iter().any(|f| !f.literal) && input.facts.iter().any(|f| f.literal));

    // The same database always gives the same input.
    let again = store.load_resolve_input().unwrap();
    assert_eq!(again.symbols, input.symbols);
    assert_eq!(again.calls, input.calls);
}

#[test]
fn placeholders_are_not_resolver_input() {
    let (_dir, store, fx, _) = indexed();
    let input = store.load_resolve_input().unwrap();
    assert_eq!(input.symbols.len(), fx.symbol_count());
    assert!(input.symbols.iter().all(|s| s.kind != kind::UNRESOLVED));
}

#[test]
fn contains_edges_run_from_repository_to_symbol() {
    let (_dir, store) = open();
    let fx = fixture();
    store.set_repository("/repo/polyglot", COMMIT).unwrap();
    insert_all(&store, &fx);
    assert_eq!(
        store.repository().unwrap(),
        Some(("/repo/polyglot".to_string(), COMMIT.to_string()))
    );

    let contains = |id: String, dir| store.edges(&[id], &[EdgeType::Contains], dir).unwrap();

    // Repository -> Service
    let repo = repository_id("/repo/polyglot");
    let services: BTreeSet<String> = contains(repo.clone(), Direction::Out)
        .into_iter()
        .map(|e| e.to)
        .collect();
    let expected: BTreeSet<String> = ["orders_api", "web_gateway", "notifier"]
        .iter()
        .map(|s| service_id(s))
        .collect();
    assert_eq!(services, expected);

    // Service -> File
    let files: BTreeSet<String> = contains(service_id("orders_api"), Direction::Out)
        .into_iter()
        .map(|e| e.to)
        .collect();
    let expected: BTreeSet<String> = [ROUTES_PY, REPO_PY, EVENTS_PY, TEST_PY]
        .iter()
        .map(|p| file_id(p))
        .collect();
    assert_eq!(files, expected);

    // File -> Symbol, with the symbol's place as evidence
    let out = contains(file_id(ROUTES_PY), Direction::Out);
    let symbols: BTreeSet<String> = out.iter().map(|e| e.to.clone()).collect();
    let expected: BTreeSet<String> = fx.get(ROUTES_PY).symbols.iter().map(|s| s.id.clone()).collect();
    assert_eq!(symbols, expected);
    let create_order = fx.symbol("orders_api.routes.create_order");
    let e = out.iter().find(|e| e.to == create_order.id).unwrap();
    assert_eq!(
        (e.from.as_str(), e.edge_type),
        (file_id(ROUTES_PY).as_str(), EdgeType::Contains)
    );
    assert_eq!((e.confidence, e.method.as_str()), (1.0, "static"));
    assert_eq!((e.evidence_file.as_str(), e.start_line, e.end_line), (ROUTES_PY, 5, 12));

    // And back up: Symbol <- File <- Service <- Repository
    let up = contains(create_order.id.clone(), Direction::In);
    assert_eq!(up.len(), 1);
    assert_eq!(
        (up[0].from.clone(), up[0].to.clone()),
        (file_id(ROUTES_PY), create_order.id.clone())
    );
    let up = contains(file_id(ROUTES_PY), Direction::In);
    assert_eq!(
        up.iter().map(|e| e.from.clone()).collect::<Vec<_>>(),
        vec![service_id("orders_api")]
    );
    let up = contains(service_id("orders_api"), Direction::In);
    assert_eq!(up.iter().map(|e| e.from.clone()).collect::<Vec<_>>(), vec![repo]);
}

#[test]
fn repository_set_after_indexing_adopts_the_services() {
    let (_dir, store) = open();
    let fx = fixture();
    assert_eq!(store.repository().unwrap(), None);
    insert_all(&store, &fx);

    store.set_repository("/repo/a", "aaa").unwrap();
    let repo_a = repository_id("/repo/a");
    assert_eq!(
        store
            .edges(std::slice::from_ref(&repo_a), &[EdgeType::Contains], Direction::Out)
            .unwrap()
            .len(),
        3
    );

    // Same repository, new head: updated in place, edges not doubled.
    store.set_repository("/repo/a", "bbb").unwrap();
    assert_eq!(
        store.repository().unwrap(),
        Some(("/repo/a".to_string(), "bbb".to_string()))
    );
    assert_eq!(
        store
            .edges(std::slice::from_ref(&repo_a), &[EdgeType::Contains], Direction::Out)
            .unwrap()
            .len(),
        3
    );

    // Another path replaces the repository: there is one per database.
    store.set_repository("/repo/b", "ccc").unwrap();
    assert_eq!(
        store.repository().unwrap(),
        Some(("/repo/b".to_string(), "ccc".to_string()))
    );
    let repos = store.nodes_by_label(NodeLabel::Repository, 10).unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(
        (
            repos[0].name.as_str(),
            repos[0].qualified_name.as_str(),
            repos[0].kind.as_str()
        ),
        ("/repo/b", "/repo/b", "ccc")
    );
    assert!(store
        .edges(&[repo_a], &[EdgeType::Contains], Direction::Out)
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .edges(&[repository_id("/repo/b")], &[EdgeType::Contains], Direction::Out)
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn service_root_is_its_directory_or_empty() {
    let (_dir, store) = open();
    // A service per top-level directory: the directory is the root.
    let f = file("orders_api/routes.py", "orders_api", lang::PYTHON);
    store.insert_parsed(&parsed(f, vec![]), COMMIT, NOW).unwrap();
    // The whole repository as one service, named after the repository root.
    let f = file("pkg/util.py", "monorepo", lang::PYTHON);
    store.insert_parsed(&parsed(f, vec![]), COMMIT, NOW).unwrap();
    // A service that owns a directory of its own name and a file outside it.
    let f = file("tool/main.py", "tool", lang::PYTHON);
    store.insert_parsed(&parsed(f, vec![]), COMMIT, NOW).unwrap();
    let f = file("setup.py", "tool", lang::PYTHON);
    store.insert_parsed(&parsed(f, vec![]), COMMIT, NOW).unwrap();

    let services = store.nodes_by_label(NodeLabel::Service, 10).unwrap();
    let roots: Vec<(&str, &str, &str)> = services
        .iter()
        .map(|s| (s.name.as_str(), s.qualified_name.as_str(), s.kind.as_str()))
        .collect();
    assert_eq!(
        roots,
        vec![("monorepo", "", ""), ("orders_api", "orders_api", ""), ("tool", "", "")]
    );
    assert!(services
        .iter()
        .all(|s| s.label == NodeLabel::Service && s.id == service_id(&s.name)));
}

#[test]
fn delete_file_removes_the_file_and_all_that_came_from_it() {
    let (_dir, store, fx, _) = indexed();
    let handle = fx.id("notifier.src.consumer.handleOrderCreated");
    let send_email = fx.id("notifier.src.mailer.sendEmail");
    let before = store.load_resolve_input().unwrap();
    assert!(store
        .edges(std::slice::from_ref(&send_email), &[EdgeType::Calls], Direction::In)
        .unwrap()
        .iter()
        .any(|e| e.from == handle));
    assert!(!store
        .edges(&[file_id(CONSUMER_TS)], &[EdgeType::Imports], Direction::Out)
        .unwrap()
        .is_empty());

    store.delete_file(CONSUMER_TS).unwrap();

    assert!(!store.file_hashes().unwrap().contains_key(CONSUMER_TS));
    assert_eq!(store.file_info(CONSUMER_TS).unwrap(), None);
    assert!(store.symbols_in_file(CONSUMER_TS).unwrap().is_empty());
    assert_eq!(store.symbol_body(&handle).unwrap(), None);
    assert!(store.nodes(&[handle.clone(), file_id(CONSUMER_TS)]).unwrap().is_empty());
    assert!(store.find("handleOrderCreated").unwrap().is_empty());

    // No staged site of the file is left; the other files keep theirs.
    let after = store.load_resolve_input().unwrap();
    let gone = fx.get(CONSUMER_TS);
    assert!(after.files.iter().all(|f| f.path != CONSUMER_TS));
    assert!(after.symbols.iter().all(|s| s.file_path != CONSUMER_TS));
    assert!(after.calls.iter().all(|c| c.file_path != CONSUMER_TS));
    assert!(after.imports.iter().all(|i| i.file_path != CONSUMER_TS));
    assert!(after.facts.iter().all(|f| f.file_path != CONSUMER_TS));
    assert_eq!(after.files.len(), before.files.len() - 1);
    assert_eq!(after.symbols.len(), before.symbols.len() - gone.symbols.len());
    assert_eq!(after.calls.len(), before.calls.len() - gone.calls.len());
    assert_eq!(after.imports.len(), before.imports.len() - gone.imports.len());
    assert_eq!(after.facts.len(), before.facts.len() - gone.facts.len());

    // Every edge that touched its symbols or the file itself is gone, in both directions.
    for dir in [Direction::Out, Direction::In] {
        assert!(store
            .edges(&[handle.clone(), file_id(CONSUMER_TS)], &EdgeType::ALL, dir)
            .unwrap()
            .is_empty());
    }
    assert!(store
        .edges(&[send_email], &[EdgeType::Calls], Direction::In)
        .unwrap()
        .is_empty());
    assert!(store
        .callers(&fx.id("notifier.src.audit.AuditLog.recordDelivery"), 3)
        .unwrap()
        .is_empty());

    // The service still has files, so it stays; deleting the rest removes it
    // together with its edges.
    assert_eq!(stat(&store, "services"), 3);
    for path in [MAILER_TS, TEMPLATES_TS, AUDIT_TS] {
        store.delete_file(path).unwrap();
    }
    assert_eq!(stat(&store, "services"), 2);
    assert!(store.nodes(&[service_id("notifier")]).unwrap().is_empty());
    assert!(store
        .edges(&[service_id("orders_api")], &[EdgeType::DependsOn], Direction::In)
        .unwrap()
        .iter()
        .all(|e| e.from != service_id("notifier")));
    let repo = repository_id("/repo/polyglot");
    assert_eq!(
        store
            .edges(&[repo], &[EdgeType::Contains], Direction::Out)
            .unwrap()
            .len(),
        2
    );

    // Deleting a file that is not indexed is not an error.
    store.delete_file(CONSUMER_TS).unwrap();
    store.delete_file("never/indexed.py").unwrap();
}

#[test]
fn inserting_an_indexed_file_again_replaces_it() {
    let (_dir, store) = open();
    let fx = fixture();
    insert_all(&store, &fx);

    let mut changed = fx.get(ROUTES_PY).clone();
    changed.file.as_mut().unwrap().content_hash = "changed".to_string();
    changed.symbols.retain(|s| s.name != "get_order");
    changed.calls.retain(|c| c.name != "load_order");
    changed.facts.clear();
    store.insert_parsed(&changed, "newcommit", NOW + 60).unwrap();

    let info = store.file_info(ROUTES_PY).unwrap().unwrap();
    assert_eq!(
        (
            info.content_hash.as_str(),
            info.indexed_commit.as_str(),
            info.indexed_at
        ),
        ("changed", "newcommit", NOW + 60)
    );
    let names: Vec<String> = store
        .symbols_in_file(ROUTES_PY)
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert_eq!(names, vec!["routes", "create_order"]);
    let input = store.load_resolve_input().unwrap();
    assert_eq!(input.calls.iter().filter(|c| c.file_path == ROUTES_PY).count(), 2);
    assert_eq!(input.facts.iter().filter(|f| f.file_path == ROUTES_PY).count(), 0);
    assert_eq!(stat(&store, "files"), fx.files.len() as i64);
    assert_eq!(stat(&store, "services"), 3);
    // The file is still contained exactly once.
    assert_eq!(
        store
            .edges(&[file_id(ROUTES_PY)], &[EdgeType::Contains], Direction::In)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_repeated_id_inside_one_file_is_written_once() {
    let (_dir, store) = open();
    let f = file("svc/overloads.ts", "svc", lang::TYPESCRIPT);
    let module = sym(&f, "svc.overloads", kind::MODULE, 1, 9);
    let first = sym(&f, "svc.overloads.parse", kind::FUNCTION, 2, 2);
    let mut second = sym(&f, "svc.overloads.parse", kind::FUNCTION, 4, 8);
    second.signature = "function parse(x: any)".to_string();
    assert_eq!(first.id, second.id);
    let c = call(&first, "helper", "", 5);
    let i = import(&f, "./helper", "helper", "helper");
    let x = fact_site(&first, lge_core::fact::REFERENCE, "helper", "", 6, true);
    let p = ParsedFile {
        file: Some(f),
        symbols: vec![module, first.clone(), second],
        calls: vec![c.clone(), c],
        imports: vec![i.clone(), i],
        facts: vec![x.clone(), x],
    };
    store.insert_parsed(&p, COMMIT, NOW).unwrap();

    let symbols = store.symbols_in_file("svc/overloads.ts").unwrap();
    assert_eq!(symbols.len(), 2);
    assert_eq!(symbols[1], node_of(&first), "the first definition is the one kept");
    let input = store.load_resolve_input().unwrap();
    assert_eq!((input.calls.len(), input.imports.len(), input.facts.len()), (1, 1, 1));
}

#[test]
fn a_parsed_file_without_a_file_record_is_rejected() {
    let (_dir, store) = open();
    let fx = fixture();
    let bad = ParsedFile {
        symbols: fx.get(ROUTES_PY).symbols.clone(),
        ..ParsedFile::default()
    };
    assert!(store.insert_parsed(&bad, COMMIT, NOW).is_err());
    assert_eq!(stat(&store, "symbols"), 0);

    // In a bulk insert the batch is one transaction: nothing of it is stored.
    let batch = vec![fx.get(REPO_PY).clone(), bad];
    assert!(store.insert_parsed_bulk(&batch, COMMIT, NOW).is_err());
    assert_eq!(stat(&store, "files"), 0);
    assert_eq!(stat(&store, "symbols"), 0);
    assert_eq!(stat(&store, "services"), 0);

    // The store is still usable afterwards.
    store.insert_parsed(fx.get(REPO_PY), COMMIT, NOW).unwrap();
    assert_eq!(stat(&store, "files"), 1);
}

/// Many small files, enough to cross the batch boundary of a bulk insert.
fn synthetic(n: usize) -> Vec<ParsedFile> {
    (0..n)
        .map(|i| {
            let service = format!("svc{}", i % 7);
            let f = file(&format!("{service}/pkg/mod_{i}.py"), &service, lang::PYTHON);
            let module = sym(&f, &format!("{service}.pkg.mod_{i}"), kind::MODULE, 1, 30);
            let a = sym(&f, &format!("{service}.pkg.mod_{i}.handler_{i}"), kind::FUNCTION, 3, 10);
            let b = sym(&f, &format!("{service}.pkg.mod_{i}.helper_{i}"), kind::FUNCTION, 12, 30);
            ParsedFile {
                calls: vec![call(&a, &format!("helper_{i}"), "", 5), call(&b, "log", "self", 14)],
                imports: vec![import(&f, "os", "", "os")],
                facts: vec![fact_site(&a, lge_core::fact::DB_READ, "orders", "", 6, i % 2 == 0)],
                file: Some(f),
                symbols: vec![module, a, b],
            }
        })
        .collect()
}

#[test]
fn bulk_insert_stores_the_same_rows_as_one_file_at_a_time() {
    let mut files = fixture().files;
    files.extend(synthetic(450));

    let (_d1, one_by_one) = open();
    let (_d2, bulk) = open();
    for s in [&one_by_one, &bulk] {
        s.set_repository("/repo/polyglot", COMMIT).unwrap();
    }
    for p in &files {
        one_by_one.insert_parsed(p, COMMIT, NOW).unwrap();
    }
    bulk.insert_parsed_bulk(&files, COMMIT, NOW).unwrap();

    assert_eq!(one_by_one.stats().unwrap(), bulk.stats().unwrap());
    assert_eq!(stat(&bulk, "files"), files.len() as i64);
    assert_eq!(one_by_one.file_hashes().unwrap(), bulk.file_hashes().unwrap());

    let (a, b) = (
        one_by_one.load_resolve_input().unwrap(),
        bulk.load_resolve_input().unwrap(),
    );
    assert_eq!(a.files, b.files);
    assert_eq!(a.symbols, b.symbols);
    assert_eq!(a.calls, b.calls);
    assert_eq!(a.imports, b.imports);
    assert_eq!(a.facts, b.facts);

    for label in [
        NodeLabel::Repository,
        NodeLabel::Service,
        NodeLabel::File,
        NodeLabel::Symbol,
    ] {
        assert_eq!(
            one_by_one.nodes_by_label(label, 10_000).unwrap(),
            bulk.nodes_by_label(label, 10_000).unwrap(),
            "{label:?}"
        );
    }
    for p in &files {
        let path = &p.file.as_ref().unwrap().path;
        assert_eq!(one_by_one.file_info(path).unwrap(), bulk.file_info(path).unwrap());
        for s in &p.symbols {
            assert_eq!(bulk.symbol_body(&s.id).unwrap(), Some(s.body.clone()));
        }
    }

    // The whole containment tree is the same, top to bottom.
    let mut level = vec![repository_id("/repo/polyglot")];
    let mut total = 0;
    while !level.is_empty() {
        let x = one_by_one.edges(&level, &[EdgeType::Contains], Direction::Out).unwrap();
        let y = bulk.edges(&level, &[EdgeType::Contains], Direction::Out).unwrap();
        assert_eq!(x, y);
        total += y.len();
        level = y.into_iter().map(|e| e.to).collect();
    }
    let symbols: usize = files.iter().map(|p| p.symbols.len()).sum();
    assert_eq!(
        total,
        3 + 7 + files.len() + symbols,
        "services, files and symbols are each contained once"
    );
}

#[test]
fn index_runs_are_recorded_in_order() {
    let (_dir, store) = open();
    assert_eq!(store.last_run().unwrap(), None);

    let first = store.begin_run(NOW).unwrap();
    let run = store.last_run().unwrap().unwrap();
    assert_eq!(
        (run.id.as_str(), run.status.as_str(), run.started_at, run.finished_at),
        (first.as_str(), "running", NOW, 0)
    );
    assert_eq!((run.files_changed, run.schema_version), (0, SCHEMA_VERSION));

    store.finish_run(&first, NOW + 9, "ok", 11).unwrap();
    let run = store.last_run().unwrap().unwrap();
    assert_eq!(
        (run.id.as_str(), run.status.as_str(), run.finished_at, run.files_changed),
        (first.as_str(), "ok", NOW + 9, 11)
    );

    // A second run in the same second is still the last one.
    let second = store.begin_run(NOW).unwrap();
    assert_ne!(first, second);
    assert_eq!(store.last_run().unwrap().unwrap().id, second);
    store.finish_run(&second, NOW + 1, "failed", 0).unwrap();
    let run = store.last_run().unwrap().unwrap();
    assert_eq!((run.id, run.status), (second, "failed".to_string()));

    assert!(store.finish_run("no-such-run", NOW, "ok", 0).is_err());
}

#[test]
fn threads_can_share_one_store() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store>();

    let (_dir, store) = open();
    let files = synthetic(40);
    std::thread::scope(|scope| {
        for chunk in files.chunks(10) {
            let store = &store;
            scope.spawn(move || {
                for p in chunk {
                    store.insert_parsed(p, COMMIT, NOW).unwrap();
                    // Readers run next to the writers.
                    store.file_hashes().unwrap();
                }
            });
        }
    });
    // Other crates hold the store as a trait object.
    let shared: &dyn GraphStore = &store;
    assert_eq!(shared.file_hashes().unwrap().len(), 40);
    assert!(format!("{store:?}").contains("LadybugStore"));
    assert_eq!(stat(&store, "files"), 40);
    assert_eq!(stat(&store, "symbols"), 120);
    assert_eq!(stat(&store, "services"), 7);
}
